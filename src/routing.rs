use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};

use crate::config::{
    ChatStreamUsagePolicy, ResolvedBackend, ResponsesMaxOutputTokensPolicy, ResponsesStorePolicy,
    RoutingStrategy, ToolSchemaMode,
};
use crate::error::ApiError;
use crate::openai::RequestMode;

#[derive(Clone)]
pub struct SelectedRoute {
    pub backend_id: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub timeout: std::time::Duration,
    pub public_model: Option<String>,
    pub backend_model: Option<String>,
    pub request_mode: RequestMode,
    pub tool_schema_mode: ToolSchemaMode,
    pub responses_store: ResponsesStorePolicy,
    pub responses_max_output_tokens: ResponsesMaxOutputTokensPolicy,
    pub chat_stream_usage: ChatStreamUsagePolicy,
}

pub fn select_backend_candidates(
    backends: &[ResolvedBackend],
    strategy: RoutingStrategy,
    path: &str,
    model: Option<&str>,
    stream: bool,
    tools: bool,
    sticky_key: Option<&str>,
) -> Result<Vec<SelectedRoute>, ApiError> {
    let path_candidates = path_capability_candidates(path);
    let mut candidates = Vec::new();
    let mut tool_incompatible_candidates = false;

    for backend in backends {
        if stream && !has_capability(&backend.capabilities, "streaming") {
            continue;
        }
        if !supports_candidates(&backend.capabilities, &path_candidates) {
            continue;
        }

        if let Some(requested_model) = model {
            for route in &backend.models {
                if route.public != requested_model {
                    continue;
                }
                if !route.endpoints.is_empty()
                    && !supports_candidates(&route.endpoints, &path_candidates)
                {
                    continue;
                }
                if tools && !supports_tools(&backend.capabilities, Some(&route.endpoints)) {
                    tool_incompatible_candidates = true;
                    continue;
                }
                candidates.push(SelectedRoute {
                    backend_id: backend.id.clone(),
                    base_url: backend.base_url.clone(),
                    api_key: backend.api_key.clone(),
                    timeout: backend.timeout,
                    public_model: Some(route.public.clone()),
                    backend_model: Some(route.backend.clone()),
                    request_mode: request_mode_for_backend(
                        path,
                        &backend.capabilities,
                        Some(&route.endpoints),
                    ),
                    tool_schema_mode: route.tool_schema_mode,
                    responses_store: route.responses_store,
                    responses_max_output_tokens: route.responses_max_output_tokens,
                    chat_stream_usage: route.chat_stream_usage,
                });
            }
            continue;
        }

        if tools && !supports_tools(&backend.capabilities, None) {
            tool_incompatible_candidates = true;
            continue;
        }
        candidates.push(SelectedRoute {
            backend_id: backend.id.clone(),
            base_url: backend.base_url.clone(),
            api_key: backend.api_key.clone(),
            timeout: backend.timeout,
            public_model: None,
            backend_model: None,
            request_mode: request_mode_for_backend(path, &backend.capabilities, None),
            tool_schema_mode: backend.tool_schema_mode,
            responses_store: backend.responses_store,
            responses_max_output_tokens: backend.responses_max_output_tokens,
            chat_stream_usage: backend.chat_stream_usage,
        });
    }

    if candidates.is_empty() {
        if tools && tool_incompatible_candidates {
            return Err(ApiError::bad_request(
                "The selected model does not support tool calling.",
                Some("tools".to_owned()),
            ));
        }
        if let Some(requested_model) = model {
            return Err(ApiError::model_not_found(requested_model));
        }
        return Err(ApiError::not_found(format!(
            "The requested endpoint '{path}' is unavailable."
        )));
    }

    match strategy {
        RoutingStrategy::Priority => {}
        RoutingStrategy::Sticky => {
            let index = sticky_index(sticky_key.unwrap_or(path), candidates.len());
            candidates.rotate_left(index);
        }
    }

    Ok(candidates)
}

fn sticky_index(key: &str, count: usize) -> usize {
    if count <= 1 {
        return 0;
    }

    let mut hasher = FnvHasher::default();
    key.hash(&mut hasher);
    (hasher.finish() as usize) % count
}

fn request_mode_for_backend(
    path: &str,
    backend_capabilities: &BTreeSet<String>,
    route_endpoints: Option<&BTreeSet<String>>,
) -> RequestMode {
    if path.trim_end_matches('/') != "/v1/responses" {
        return RequestMode::Native;
    }
    if supports_responses(backend_capabilities) && route_supports_responses(route_endpoints) {
        return RequestMode::Native;
    }
    if supports_chat_compat(backend_capabilities) && route_supports_chat_compat(route_endpoints) {
        RequestMode::ResponsesViaChatCompletions
    } else {
        RequestMode::Native
    }
}

fn route_supports_responses(route_endpoints: Option<&BTreeSet<String>>) -> bool {
    route_endpoints.is_none_or(|endpoints| {
        endpoints.is_empty()
            || has_capability(endpoints, "responses")
            || has_capability(endpoints, "response")
    })
}

fn route_supports_chat_compat(route_endpoints: Option<&BTreeSet<String>>) -> bool {
    route_endpoints.is_none_or(|endpoints| endpoints.is_empty() || supports_chat_compat(endpoints))
}

fn supports_responses(capabilities: &BTreeSet<String>) -> bool {
    has_capability(capabilities, "responses") || has_capability(capabilities, "response")
}

fn supports_chat_compat(capabilities: &BTreeSet<String>) -> bool {
    has_capability(capabilities, "chat")
        || has_capability(capabilities, "chat_completions")
        || has_capability(capabilities, "completions")
}

fn supports_tools(
    backend_capabilities: &BTreeSet<String>,
    route_endpoints: Option<&BTreeSet<String>>,
) -> bool {
    has_tool_capability(backend_capabilities) && route_supports_tools(route_endpoints)
}

fn route_supports_tools(route_endpoints: Option<&BTreeSet<String>>) -> bool {
    route_endpoints.is_none_or(|endpoints| endpoints.is_empty() || has_tool_capability(endpoints))
}

fn has_tool_capability(capabilities: &BTreeSet<String>) -> bool {
    has_capability(capabilities, "tools")
        || has_capability(capabilities, "tool_calls")
        || has_capability(capabilities, "function_calling")
        || has_capability(capabilities, "functions")
}

#[derive(Default)]
struct FnvHasher(u64);

impl Hasher for FnvHasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut hash = if self.0 == 0 {
            0xcbf29ce484222325
        } else {
            self.0
        };
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        self.0 = hash;
    }

    fn finish(&self) -> u64 {
        if self.0 == 0 {
            0xcbf29ce484222325
        } else {
            self.0
        }
    }
}

pub fn sticky_routing_key(
    identity: &str,
    path: &str,
    model: Option<&str>,
    prompt_cache_key: Option<&str>,
) -> String {
    format!(
        "{identity}\u{0}{path}\u{0}{}\u{0}{}",
        model.unwrap_or(""),
        prompt_cache_key.unwrap_or("")
    )
}

pub fn path_metric_name(path: &str) -> String {
    if path.ends_with("/chat/completions") {
        return "chat_completions".to_owned();
    }
    if path.ends_with("/chat/completion") {
        return "chat_completions".to_owned();
    }
    if path.ends_with("/responses") {
        return "responses".to_owned();
    }
    let candidates = path_capability_candidates(path);
    candidates
        .first()
        .cloned()
        .unwrap_or_else(|| "unknown".to_owned())
}

pub fn path_requires_model(path: &str) -> bool {
    let normalized = path.trim_end_matches('/');
    matches!(
        normalized,
        "/v1/chat/completions"
            | "/v1/chat/completion"
            | "/v1/responses"
            | "/v1/embeddings"
            | "/v1/audio/transcriptions"
            | "/v1/audio/translations"
            | "/v1/images/generations"
            | "/v1/images/edits"
            | "/v1/images/variations"
    )
}

pub fn path_capability_candidates(path: &str) -> Vec<String> {
    let trimmed = path
        .strip_prefix("/v1/")
        .unwrap_or(path)
        .trim_start_matches('/');
    if trimmed.is_empty() {
        return Vec::new();
    }

    let segments = trimmed
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(normalize_segment)
        .collect::<Vec<_>>();
    if segments.is_empty() {
        return Vec::new();
    }

    let mut candidates = Vec::new();
    push_unique(&mut candidates, segments[0].clone());
    if segments[0].ends_with('s') && segments[0].len() > 1 {
        push_unique(&mut candidates, singularize(&segments[0]));
    }
    if let Some(second_segment) = segments.get(1) {
        push_unique(&mut candidates, second_segment.clone());
        push_unique(
            &mut candidates,
            format!("{}_{}", segments[0], second_segment),
        );
    }
    if let Some(third_segment) = segments.get(2) {
        push_unique(
            &mut candidates,
            format!("{}_{}", segments[0], third_segment),
        );
        push_unique(&mut candidates, third_segment.clone());
    }

    match segments[0].as_str() {
        "chat" => {
            push_unique(&mut candidates, "chat_completions".to_owned());
            push_unique(&mut candidates, "completions".to_owned());
        }
        "responses" => {
            push_unique(&mut candidates, "chat".to_owned());
            push_unique(&mut candidates, "chat_completions".to_owned());
            push_unique(&mut candidates, "completions".to_owned());
        }
        "images" => {
            push_unique(&mut candidates, "image".to_owned());
        }
        "files" => {
            push_unique(&mut candidates, "file".to_owned());
        }
        "models" => {
            push_unique(&mut candidates, "model".to_owned());
        }
        "audio" => {
            push_unique(&mut candidates, "audio".to_owned());
        }
        _ => {}
    }

    candidates
}

fn supports_candidates(capabilities: &BTreeSet<String>, candidates: &[String]) -> bool {
    candidates
        .iter()
        .any(|candidate| has_capability(capabilities, candidate))
}

fn has_capability(capabilities: &BTreeSet<String>, capability: &str) -> bool {
    capabilities.contains(capability) || capabilities.contains("all")
}

fn normalize_segment(segment: &str) -> String {
    segment
        .chars()
        .map(|character| match character {
            '-' => '_',
            character => character.to_ascii_lowercase(),
        })
        .collect()
}

fn singularize(segment: &str) -> String {
    segment.strip_suffix('s').unwrap_or(segment).to_owned()
}

fn push_unique(candidates: &mut Vec<String>, candidate: String) {
    if !candidate.is_empty() && !candidates.iter().any(|value| value == &candidate) {
        candidates.push(candidate);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::Duration;

    use crate::config::{
        ModelRoute, ResolvedBackend, ResponsesMaxOutputTokensPolicy, ResponsesStorePolicy,
        ToolSchemaMode,
    };

    use super::*;

    #[test]
    fn sticky_candidates_start_with_hashed_backend_and_keep_fallbacks() {
        let backends = vec![
            backend("backend-a"),
            backend("backend-b"),
            backend("backend-c"),
        ];
        let sticky_key = sticky_routing_key(
            "client-a",
            "/v1/responses",
            Some("public-model"),
            Some("cache-key"),
        );

        let priority = select_backend_candidates(
            &backends,
            RoutingStrategy::Priority,
            "/v1/responses",
            Some("public-model"),
            false,
            false,
            Some(&sticky_key),
        )
        .unwrap();
        let sticky = select_backend_candidates(
            &backends,
            RoutingStrategy::Sticky,
            "/v1/responses",
            Some("public-model"),
            false,
            false,
            Some(&sticky_key),
        )
        .unwrap();

        let selected_index = sticky_index(&sticky_key, priority.len());
        assert_eq!(sticky[0].backend_id, priority[selected_index].backend_id);
        assert_eq!(sticky.len(), priority.len());
        assert_eq!(
            sticky
                .iter()
                .map(|route| route.backend_id.as_str())
                .collect::<BTreeSet<_>>(),
            priority
                .iter()
                .map(|route| route.backend_id.as_str())
                .collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn responses_can_route_to_chat_backend() {
        let backend = ResolvedBackend {
            id: "chat-backend".to_owned(),
            base_url: "http://chat-backend.example.invalid".to_owned(),
            api_key: None,
            timeout: Duration::from_secs(5),
            capabilities: btree_set(["chat"]),
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            models: vec![ModelRoute {
                public: "public-model".to_owned(),
                backend: "backend-private".to_owned(),
                context_length: None,
                tool_schema_mode: ToolSchemaMode::Preserve,
                responses_store: ResponsesStorePolicy::Preserve,
                responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                endpoints: btree_set(["chat"]),
            }],
        };

        let routes = select_backend_candidates(
            &[backend],
            RoutingStrategy::Priority,
            "/v1/responses",
            Some("public-model"),
            false,
            false,
            None,
        )
        .unwrap();

        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].backend_id, "chat-backend");
        assert_eq!(
            routes[0].request_mode,
            RequestMode::ResponsesViaChatCompletions
        );
    }

    #[test]
    fn native_responses_capability_prevents_chat_compat_mode() {
        let backend = ResolvedBackend {
            id: "native-backend".to_owned(),
            base_url: "http://native-backend.example.invalid".to_owned(),
            api_key: None,
            timeout: Duration::from_secs(5),
            capabilities: btree_set(["responses", "chat"]),
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            models: vec![ModelRoute {
                public: "public-model".to_owned(),
                backend: "backend-private".to_owned(),
                context_length: None,
                tool_schema_mode: ToolSchemaMode::Preserve,
                responses_store: ResponsesStorePolicy::Preserve,
                responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                endpoints: btree_set(["responses", "chat"]),
            }],
        };

        let routes = select_backend_candidates(
            &[backend],
            RoutingStrategy::Priority,
            "/v1/responses",
            Some("public-model"),
            false,
            false,
            None,
        )
        .unwrap();

        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].backend_id, "native-backend");
        assert_eq!(routes[0].request_mode, RequestMode::Native);
    }

    #[test]
    fn route_endpoint_can_choose_chat_compat_without_responses() {
        let backend = ResolvedBackend {
            id: "mixed-backend".to_owned(),
            base_url: "http://mixed-backend.example.invalid".to_owned(),
            api_key: None,
            timeout: Duration::from_secs(5),
            capabilities: btree_set(["responses", "chat"]),
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            models: vec![ModelRoute {
                public: "public-model".to_owned(),
                backend: "backend-private".to_owned(),
                context_length: None,
                tool_schema_mode: ToolSchemaMode::Preserve,
                responses_store: ResponsesStorePolicy::Preserve,
                responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                endpoints: btree_set(["chat"]),
            }],
        };

        let routes = select_backend_candidates(
            &[backend],
            RoutingStrategy::Priority,
            "/v1/responses",
            Some("public-model"),
            false,
            false,
            None,
        )
        .unwrap();

        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].backend_id, "mixed-backend");
        assert_eq!(
            routes[0].request_mode,
            RequestMode::ResponsesViaChatCompletions
        );
    }

    #[test]
    fn tool_requests_require_backend_and_route_capability() {
        let unsupported = ResolvedBackend {
            id: "unsupported-backend".to_owned(),
            base_url: "http://unsupported-backend.example.invalid".to_owned(),
            api_key: None,
            timeout: Duration::from_secs(5),
            capabilities: btree_set(["responses", "chat"]),
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            models: vec![ModelRoute {
                public: "public-model".to_owned(),
                backend: "unsupported-private".to_owned(),
                context_length: None,
                tool_schema_mode: ToolSchemaMode::Preserve,
                responses_store: ResponsesStorePolicy::Preserve,
                responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                endpoints: btree_set(["responses", "chat"]),
            }],
        };
        let supported = ResolvedBackend {
            id: "supported-backend".to_owned(),
            base_url: "http://supported-backend.example.invalid".to_owned(),
            api_key: None,
            timeout: Duration::from_secs(5),
            capabilities: btree_set(["responses", "chat", "tools"]),
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            models: vec![ModelRoute {
                public: "public-model".to_owned(),
                backend: "supported-private".to_owned(),
                context_length: None,
                tool_schema_mode: ToolSchemaMode::Preserve,
                responses_store: ResponsesStorePolicy::Preserve,
                responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                endpoints: btree_set(["responses", "chat", "tools"]),
            }],
        };

        let routes = select_backend_candidates(
            &[unsupported, supported],
            RoutingStrategy::Priority,
            "/v1/responses",
            Some("public-model"),
            false,
            true,
            None,
        )
        .unwrap();

        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].backend_id, "supported-backend");
    }

    #[test]
    fn tool_requests_fail_when_only_endpoint_matches() {
        let backend = ResolvedBackend {
            id: "plain-backend".to_owned(),
            base_url: "http://plain-backend.example.invalid".to_owned(),
            api_key: None,
            timeout: Duration::from_secs(5),
            capabilities: btree_set(["responses", "chat"]),
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            models: vec![ModelRoute {
                public: "public-model".to_owned(),
                backend: "plain-private".to_owned(),
                context_length: None,
                tool_schema_mode: ToolSchemaMode::Preserve,
                responses_store: ResponsesStorePolicy::Preserve,
                responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                endpoints: btree_set(["responses", "chat"]),
            }],
        };

        let error = match select_backend_candidates(
            &[backend],
            RoutingStrategy::Priority,
            "/v1/responses",
            Some("public-model"),
            false,
            true,
            None,
        ) {
            Ok(_) => panic!("expected tool request routing to fail"),
            Err(error) => error,
        };

        assert_eq!(error.status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(error.param.as_deref(), Some("tools"));
    }

    fn backend(id: &str) -> ResolvedBackend {
        ResolvedBackend {
            id: id.to_owned(),
            base_url: format!("http://{id}.example.invalid"),
            api_key: None,
            timeout: Duration::from_secs(5),
            capabilities: btree_set(["responses", "streaming"]),
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            models: vec![ModelRoute {
                public: "public-model".to_owned(),
                backend: format!("{id}-private"),
                context_length: None,
                tool_schema_mode: ToolSchemaMode::Preserve,
                responses_store: ResponsesStorePolicy::Preserve,
                responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                endpoints: btree_set(["responses"]),
            }],
        }
    }

    fn btree_set<const N: usize>(values: [&str; N]) -> BTreeSet<String> {
        values.into_iter().map(str::to_owned).collect()
    }
}
