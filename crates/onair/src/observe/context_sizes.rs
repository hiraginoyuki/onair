use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Client;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

#[cfg(test)]
use crate::config::{
    ChatStreamUsagePolicy, DebugCaptureConfig, HealthConfig, InspectorConfig, ModelRoute,
    ResolvedBackend, ResponsesMaxOutputTokensPolicy, ResponsesStorePolicy, RoutingConfig,
    ServerConfig, TelemetryConfig, ToolSchemaMode,
};
use crate::config::{Config, ConfigStore, ContextLengthPolicy};

pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
pub(crate) const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextSizeEntry {
    pub value: Option<u64>,
    pub last_success_unix_ms: Option<u64>,
    pub last_failure_unix_ms: Option<u64>,
    pub last_error_kind: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ContextSizeCache {
    inner: Arc<Mutex<BTreeMap<String, ContextSizeEntry>>>,
}

impl ContextSizeCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn lookup(&self, public_model: &str) -> Option<u64> {
        let map = self.inner.lock().expect("context size cache lock poisoned");
        map.get(public_model).and_then(|entry| entry.value)
    }

    pub fn entry(&self, public_model: &str) -> Option<ContextSizeEntry> {
        let map = self.inner.lock().expect("context size cache lock poisoned");
        map.get(public_model).cloned()
    }

    pub fn set(&self, public_model: &str, value: Option<u64>, error_kind: Option<&str>) {
        if let (None, None) = (value, error_kind) {
            return;
        }
        let now_unix_ms = unix_millis();
        let mut map = self.inner.lock().expect("context size cache lock poisoned");
        let entry = map.entry(public_model.to_owned()).or_default();
        entry.value = value;
        match (value, error_kind) {
            (Some(_), None) => {
                entry.last_success_unix_ms = Some(now_unix_ms);
                entry.last_error_kind = None;
            }
            (None, Some(kind)) => {
                entry.last_failure_unix_ms = Some(now_unix_ms);
                entry.last_error_kind = Some(kind.to_owned());
            }
            _ => {}
        }
    }

    pub fn prune(&self, active: &BTreeSet<String>) {
        let mut map = self.inner.lock().expect("context size cache lock poisoned");
        map.retain(|public_model, _| active.contains(public_model));
    }
}

pub(crate) struct ContextSizeRefreshTask {
    task: JoinHandle<()>,
}

impl ContextSizeRefreshTask {
    pub(crate) fn start(config: ConfigStore, http: Client, cache: ContextSizeCache) -> Self {
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

pub(crate) async fn refresh_once(http: &Client, cache: &ContextSizeCache, config: &Config) {
    let targets = collect_upstream_targets(config);
    let mut active = BTreeSet::new();
    for (public, backend_id, backend_model) in &targets {
        active.insert(public.clone());
        let backend = match config.backends.iter().find(|b| &b.id == backend_id) {
            Some(backend) => backend,
            None => {
                warn!(
                    public = %public,
                    backend = %backend_id,
                    "context-size refresh skipped: backend not found"
                );
                continue;
            }
        };
        let result = fetch_upstream_n_ctx(
            http,
            &backend.base_url,
            backend.api_key.as_deref(),
            backend_model,
        )
        .await;
        match result {
            FetchResult::Ok(value) => {
                debug!(
                    public = %public,
                    backend = %backend_id,
                    n_ctx = value,
                    "context-size refresh succeeded"
                );
                cache.set(public, Some(value), None);
            }
            FetchResult::Err(kind) => {
                warn!(
                    public = %public,
                    backend = %backend_id,
                    error_kind = %kind,
                    "context-size refresh failed"
                );
                cache.set(public, None, Some(kind));
            }
        }
    }
    cache.prune(&active);
}

fn collect_upstream_targets(config: &Config) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for backend in &config.backends {
        for model in &backend.models {
            if let ContextLengthPolicy::Upstream {
                backend_id,
                backend_model,
            } = &model.context_length
                && seen.insert(model.public.clone())
            {
                out.push((
                    model.public.clone(),
                    backend_id.clone(),
                    backend_model.clone(),
                ));
            }
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

pub(crate) fn build_props_url(base_url: &str, backend_model: &str) -> Option<String> {
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

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
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
                capabilities: BTreeSet::new(),
                tool_schema_mode: ToolSchemaMode::Preserve,
                responses_store: ResponsesStorePolicy::Preserve,
                responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                weight: 1,
                models: vec![
                    ModelRoute {
                        public: "shared".to_owned(),
                        backend: "shared-a".to_owned(),
                        context_length: ContextLengthPolicy::Upstream {
                            backend_id: "backend-a".to_owned(),
                            backend_model: "shared-a".to_owned(),
                        },
                        tool_schema_mode: ToolSchemaMode::Preserve,
                        responses_store: ResponsesStorePolicy::Preserve,
                        responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                        chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                        endpoints: BTreeSet::new(),
                    },
                    ModelRoute {
                        public: "shared".to_owned(),
                        backend: "shared-b".to_owned(),
                        context_length: ContextLengthPolicy::Upstream {
                            backend_id: "backend-b".to_owned(),
                            backend_model: "shared-b".to_owned(),
                        },
                        tool_schema_mode: ToolSchemaMode::Preserve,
                        responses_store: ResponsesStorePolicy::Preserve,
                        responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                        chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                        endpoints: BTreeSet::new(),
                    },
                ],
            }],
        };
        let targets = collect_upstream_targets(&config);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].0, "shared");
        assert_eq!(targets[0].1, "backend-a");
        assert_eq!(targets[0].2, "shared-a");
    }
}
