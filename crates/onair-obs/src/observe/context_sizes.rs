use std::collections::BTreeSet;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use futures_util::FutureExt;
use onair_core::ContextSizeCache;
use onair_core::config::{Config, ConfigStore, ContextLengthSpec, RouteKey};
use reqwest::Client;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

#[cfg(test)]
use onair_core::config::{
    ChatStreamUsagePolicy, DebugCaptureConfig, HealthConfig, InspectorConfig, ResolvedBackend,
    ResolvedRoute, ResponsesMaxOutputTokensPolicy, ResponsesStorePolicy, RouteBackendBinding,
    RoutingConfig, ServerConfig, TelemetryConfig, ToolSchemaMode,
};

pub const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

pub struct ContextSizeRefreshTask {
    task: JoinHandle<()>,
}

impl ContextSizeRefreshTask {
    pub fn start(config: ConfigStore, http: Client, cache: ContextSizeCache) -> Self {
        let task = tokio::spawn(async move {
            loop {
                let snapshot = config.snapshot();
                refresh_once(&http, &cache, &snapshot).await;
                tokio::time::sleep(REFRESH_INTERVAL).await;
            }
        });
        Self { task }
    }
}

impl Drop for ContextSizeRefreshTask {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn refresh_once(http: &Client, cache: &ContextSizeCache, config: &Config) {
    let targets = collect_upstream_targets(config);
    let active: BTreeSet<String> = targets
        .iter()
        .map(|(public, _, _)| public.clone())
        .collect();

    let mut handles = Vec::with_capacity(targets.len());
    for (public, backend_id, backend_model) in &targets {
        let Some(backend) = config.backends.iter().find(|b| &b.id == backend_id) else {
            warn!(
                public = %public,
                backend = %backend_id,
                "context-size refresh skipped: backend not found"
            );
            continue;
        };
        let backend_url = backend.base_url.clone();
        let backend_api_key = backend.api_key.clone();
        let backend_id = backend_id.clone();
        let public = public.clone();
        let backend_model = backend_model.clone();
        let http = http.clone();
        let cache = cache.clone();
        handles.push(tokio::spawn(async move {
            let public = public;
            let backend_id = backend_id;
            let backend_model = backend_model;
            let result = AssertUnwindSafe(fetch_upstream_n_ctx(
                &http,
                &backend_url,
                backend_api_key.as_deref(),
                &backend_model,
            ))
            .catch_unwind()
            .await;
            match result {
                Ok(FetchResult::Ok(value)) => {
                    debug!(
                        public = %public,
                        backend = %backend_id,
                        n_ctx = value,
                        "context-size refresh succeeded"
                    );
                    cache.set(&public, Some(value), None);
                }
                Ok(FetchResult::Err(kind)) => {
                    warn!(
                        public = %public,
                        backend = %backend_id,
                        error_kind = %kind,
                        "context-size refresh failed"
                    );
                    cache.set(&public, None, Some(kind));
                }
                Err(panic) => {
                    let message = panic_message(&panic);
                    warn!(
                        public = %public,
                        backend = %backend_id,
                        panic = %message,
                        "context-size refresh panicked; isolating the failure"
                    );
                    cache.set(&public, None, Some("panic"));
                }
            }
        }));
    }

    for handle in handles {
        if let Err(error) = handle.await {
            warn!(?error, "context-size refresh task join failed");
        }
    }

    cache.prune(&active);
}

fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = panic.downcast_ref::<&'static str>() {
        (*message).to_owned()
    } else if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_owned()
    }
}

fn collect_upstream_targets(config: &Config) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for route in &config.routes {
        if let (
            RouteKey::Public(public),
            ContextLengthSpec::Upstream {
                backend_id,
                backend_model,
                ..
            },
        ) = (&route.key, &route.context_length)
            && seen.insert(public.clone())
        {
            out.push((public.clone(), backend_id.clone(), backend_model.clone()));
        }
    }
    out
}

enum FetchResult {
    Ok(u64),
    Err(&'static str),
}

async fn fetch_upstream_n_ctx(
    http: &Client,
    base_url: &str,
    api_key: Option<&str>,
    backend_model: &str,
) -> FetchResult {
    let url = match build_props_url(base_url, backend_model) {
        Some(url) => url,
        None => return FetchResult::Err("url"),
    };
    let mut request = http.get(&url).timeout(FETCH_TIMEOUT);
    if let Some(api_key) = api_key {
        request = request.bearer_auth(api_key);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => return FetchResult::Err(classify_error(&error)),
    };
    if !response.status().is_success() {
        return FetchResult::Err("status");
    }
    let body: serde_json::Value = match response.json().await {
        Ok(body) => body,
        Err(_) => return FetchResult::Err("decode"),
    };
    let n_ctx = body
        .get("default_generation_settings")
        .and_then(|settings| settings.get("n_ctx"))
        .and_then(|value| value.as_u64());
    match n_ctx {
        Some(value) => FetchResult::Ok(value),
        None => FetchResult::Err("missing"),
    }
}

pub fn build_props_url(base_url: &str, backend_model: &str) -> Option<String> {
    let trimmed = base_url.trim_end_matches('/');
    let mut url = url::Url::parse(&format!("{trimmed}/props")).ok()?;
    url.query_pairs_mut().append_pair("model", backend_model);
    Some(url.to_string())
}

fn classify_error(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_request() {
        "request"
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_returns_none_for_missing_key() {
        let cache = ContextSizeCache::new();
        assert_eq!(cache.lookup("missing"), None);
        assert_eq!(cache.entry("missing"), None);
    }

    #[test]
    fn set_updates_value_and_stamps_success() {
        let cache = ContextSizeCache::new();
        cache.set("public", Some(131072), None);
        assert_eq!(cache.lookup("public"), Some(131072));
        let entry = cache.entry("public").unwrap();
        assert_eq!(entry.value, Some(131072));
        assert!(entry.last_success_unix_ms.is_some());
        assert!(entry.last_failure_unix_ms.is_none());
        assert!(entry.last_error_kind.is_none());
    }

    #[test]
    fn set_with_error_stamps_failure_only() {
        let cache = ContextSizeCache::new();
        cache.set("public", None, Some("timeout"));
        assert_eq!(cache.lookup("public"), None);
        let entry = cache.entry("public").unwrap();
        assert_eq!(entry.value, None);
        assert!(entry.last_success_unix_ms.is_none());
        assert!(entry.last_failure_unix_ms.is_some());
        assert_eq!(entry.last_error_kind.as_deref(), Some("timeout"));
    }

    #[test]
    fn set_with_neither_value_nor_error_is_a_noop() {
        let cache = ContextSizeCache::new();
        cache.set("public", None, None);
        assert!(cache.entry("public").is_none());
    }

    #[test]
    fn prune_removes_inactive_keys_only() {
        let cache = ContextSizeCache::new();
        cache.set("a", Some(1), None);
        cache.set("b", Some(2), None);
        cache.set("c", Some(3), None);
        let active = BTreeSet::from(["a".to_owned(), "c".to_owned()]);
        cache.prune(&active);
        assert_eq!(cache.lookup("a"), Some(1));
        assert_eq!(cache.lookup("b"), None);
        assert_eq!(cache.lookup("c"), Some(3));
    }

    #[test]
    fn build_props_url_trims_trailing_slash() {
        assert_eq!(
            build_props_url("http://127.0.0.1:8000/", "gpt-4"),
            Some("http://127.0.0.1:8000/props?model=gpt-4".to_owned())
        );
        assert_eq!(
            build_props_url("http://127.0.0.1:8000", "gpt-4"),
            Some("http://127.0.0.1:8000/props?model=gpt-4".to_owned())
        );
    }

    #[test]
    fn build_props_url_percent_encodes_reserved_chars() {
        let url = build_props_url("http://x", "a/b c").unwrap();
        assert!(
            url.contains("model=a%2Fb+c") || url.contains("model=a%2Fb%20c"),
            "url: {url}"
        );
    }

    #[test]
    fn build_props_url_returns_none_for_invalid_base() {
        assert_eq!(build_props_url("not a url", "gpt-4"), None);
    }

    #[test]
    fn collect_upstream_targets_returns_first_match_per_public_model() {
        let config = Config {
            server: ServerConfig::default(),
            telemetry: TelemetryConfig::default(),
            debug_capture: DebugCaptureConfig::default(),
            inspector: InspectorConfig::default(),
            health: HealthConfig::default(),
            routing: RoutingConfig::default(),
            clients: Vec::new(),
            backends: vec![ResolvedBackend {
                id: "backend-a".to_owned(),
                base_url: "http://a".to_owned(),
                api_key: None,
                timeout: Duration::from_secs(5),
                supports: BTreeSet::new(),
                tool_schema_mode: ToolSchemaMode::Preserve,
                responses_store: ResponsesStorePolicy::Preserve,
                responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                weight: 1,
            }],
            routes: vec![
                ResolvedRoute {
                    key: RouteKey::Public("shared".to_owned()),
                    expose: BTreeSet::new(),
                    context_length: ContextLengthSpec::Upstream {
                        backend_id: "backend-a".to_owned(),
                        backend_model: "shared-a".to_owned(),
                        n_ctx: None,
                    },
                    tool_schema_mode: ToolSchemaMode::Preserve,
                    responses_store: ResponsesStorePolicy::Preserve,
                    responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                    chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                    backends: vec![RouteBackendBinding {
                        backend_id: "backend-a".to_owned(),
                        backend_model: "shared-a".to_owned(),
                    }],
                },
                ResolvedRoute {
                    key: RouteKey::Public("shared".to_owned()),
                    expose: BTreeSet::new(),
                    context_length: ContextLengthSpec::Upstream {
                        backend_id: "backend-b".to_owned(),
                        backend_model: "shared-b".to_owned(),
                        n_ctx: None,
                    },
                    tool_schema_mode: ToolSchemaMode::Preserve,
                    responses_store: ResponsesStorePolicy::Preserve,
                    responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                    chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                    backends: vec![RouteBackendBinding {
                        backend_id: "backend-b".to_owned(),
                        backend_model: "shared-b".to_owned(),
                    }],
                },
            ],
        };
        let targets = collect_upstream_targets(&config);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].0, "shared");
        assert_eq!(targets[0].1, "backend-a");
        assert_eq!(targets[0].2, "shared-a");
    }
}
