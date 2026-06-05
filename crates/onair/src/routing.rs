use std::collections::{BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

use rand::Rng;

use crate::openai::RequestMode;
#[cfg(test)]
use onair_core::config::ContextLengthPolicy;
use onair_core::config::{
    ChatStreamUsagePolicy, ResolvedBackend, ResponsesMaxOutputTokensPolicy, ResponsesStorePolicy,
    RoutingStrategy, ToolSchemaMode,
};
use onair_core::error::ApiError;

const RESPONSES_VIA_CHAT_COMPLETIONS: &str = "responses_via_chat_completions";
const CHAT_COMPLETIONS_VIA_RESPONSES: &str = "chat_completions_via_responses";

#[cfg(test)]
pub use onair_core::{is_known_marker, KNOWN_MARKERS};

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
    pub weight: u32,
}

pub struct RoundRobinCounters {
    inner: Mutex<HashMap<String, u64>>,
}

impl Default for RoundRobinCounters {
    fn default() -> Self {
        Self::new()
    }
}

impl RoundRobinCounters {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Returns the index to promote to primary for the given model/path key.
    /// Increments the counter (wrapping) for next call.
    /// Entries are created lazily and persist across requests.
    /// Returns `0` if `count` is `0`; this is defensive because empty
    /// candidate lists are normally rejected before the call site.
    pub fn next_index(&self, key: &str, count: usize) -> usize {
        if count == 0 {
            return 0;
        }
        let mut map = self.inner.lock().expect("round-robin lock");
        let counter = map.entry(key.to_owned()).or_insert(0);
        let index = *counter as usize % count;
        *counter = counter.wrapping_add(1);
        index
    }
}

#[allow(clippy::too_many_arguments)]
pub fn select_backend_candidates(
    backends: &[ResolvedBackend],
    strategy: RoutingStrategy,
    path: &str,
    model: Option<&str>,
    stream: bool,
    tools: bool,
    sticky_key: Option<&str>,
    round_robin: &RoundRobinCounters,
) -> Result<Vec<SelectedRoute>, ApiError> {
    let path_candidates = path_capability_candidates(path);
    let mut candidates = Vec::new();
    let mut tool_incompatible_candidates = false;

    for backend in backends {
        if stream && !has_capability(&backend.capabilities, "streaming") {
            continue;
        }

        if let Some(requested_model) = model {
            for route in &backend.models {
                if route.public != requested_model {
                    continue;
                }
                let Some(request_mode) = request_mode_for_route(
                    path,
                    &path_candidates,
                    &backend.capabilities,
                    &route.endpoints,
                ) else {
                    continue;
                };
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
                    request_mode,
                    tool_schema_mode: route.tool_schema_mode,
                    responses_store: route.responses_store,
                    responses_max_output_tokens: route.responses_max_output_tokens,
                    chat_stream_usage: route.chat_stream_usage,
                    weight: backend.weight,
                });
            }
            continue;
        }

        let Some(request_mode) =
            request_mode_for_backend(path, &path_candidates, &backend.capabilities)
        else {
            continue;
        };
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
            request_mode,
            tool_schema_mode: backend.tool_schema_mode,
            responses_store: backend.responses_store,
            responses_max_output_tokens: backend.responses_max_output_tokens,
            chat_stream_usage: backend.chat_stream_usage,
            weight: backend.weight,
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
            if model_is_known(backends, requested_model) {
                return Err(ApiError::endpoint_unavailable(path, Some(requested_model)));
            }
            return Err(ApiError::model_not_found(requested_model));
        }
        return Err(ApiError::endpoint_unavailable(path, None));
    }

    match strategy {
        RoutingStrategy::Priority => {}
        RoutingStrategy::Sticky => {
            let index = sticky_index(sticky_key.unwrap_or(path), candidates.len());
            candidates.rotate_left(index);
        }
        RoutingStrategy::RoundRobin => {
            let key = model.unwrap_or(path);
            let index = round_robin.next_index(key, candidates.len());
            candidates.rotate_left(index);
        }
        RoutingStrategy::WeightedRandom => {
            weighted_rotate(&mut candidates);
        }
    }

    Ok(candidates)
}

fn weighted_rotate(candidates: &mut [SelectedRoute]) {
    if candidates.len() <= 1 {
        return;
    }
    let total: u64 = candidates.iter().map(|c| u64::from(c.weight)).sum();
    let pick = rand::thread_rng().gen_range(0..total);
    let mut cumulative = 0u64;
    for (i, candidate) in candidates.iter().enumerate() {
        cumulative += u64::from(candidate.weight);
        if pick < cumulative {
            candidates.rotate_left(i);
            return;
        }
    }
}

fn sticky_index(key: &str, count: usize) -> usize {
    if count <= 1 {
        return 0;
    }

    let mut hasher = FnvHasher::default();
    key.hash(&mut hasher);
    (hasher.finish() as usize) % count
}

fn model_is_known(backends: &[ResolvedBackend], requested_model: &str) -> bool {
    backends.iter().any(|backend| {
        backend
            .models
            .iter()
            .any(|route| route.public == requested_model)
    })
}

fn request_mode_for_route(
    path: &str,
    path_candidates: &[String],
    backend_capabilities: &BTreeSet<String>,
    route_endpoints: &BTreeSet<String>,
) -> Option<RequestMode> {
    request_mode_for_path(
        path,
        path_candidates,
        backend_capabilities,
        Some(route_endpoints),
    )
}

fn request_mode_for_backend(
    path: &str,
    path_candidates: &[String],
    backend_capabilities: &BTreeSet<String>,
) -> Option<RequestMode> {
    request_mode_for_path(path, path_candidates, backend_capabilities, None)
}

fn request_mode_for_path(
    path: &str,
    path_candidates: &[String],
    backend_capabilities: &BTreeSet<String>,
    route_endpoints: Option<&BTreeSet<String>>,
) -> Option<RequestMode> {
    match path.trim_end_matches('/') {
        "/v1/responses" => request_mode_for_responses(backend_capabilities, route_endpoints),
        "/v1/chat/completions" | "/v1/chat/completion" => {
            request_mode_for_chat_completions(backend_capabilities, route_endpoints)
        }
        _ => {
            if !supports_candidates(backend_capabilities, path_candidates) {
                return None;
            }
            if let Some(endpoints) = route_endpoints
                && !endpoints.is_empty()
                && !supports_candidates(endpoints, path_candidates)
            {
                return None;
            }
            Some(RequestMode::Native)
        }
    }
}

fn request_mode_for_responses(
    backend_capabilities: &BTreeSet<String>,
    route_endpoints: Option<&BTreeSet<String>>,
) -> Option<RequestMode> {
    if supports_responses(backend_capabilities) && route_supports_responses(route_endpoints) {
        return Some(RequestMode::Native);
    }
    if supports_chat_compat(backend_capabilities)
        && route_supports_compat_marker(
            backend_capabilities,
            route_endpoints,
            RESPONSES_VIA_CHAT_COMPLETIONS,
        )
    {
        return Some(RequestMode::ResponsesViaChatCompletions);
    }
    None
}

fn request_mode_for_chat_completions(
    backend_capabilities: &BTreeSet<String>,
    route_endpoints: Option<&BTreeSet<String>>,
) -> Option<RequestMode> {
    if supports_chat_compat(backend_capabilities) && route_supports_chat_compat(route_endpoints) {
        return Some(RequestMode::Native);
    }
    if supports_responses(backend_capabilities)
        && route_supports_compat_marker(
            backend_capabilities,
            route_endpoints,
            CHAT_COMPLETIONS_VIA_RESPONSES,
        )
    {
        return Some(RequestMode::ChatCompletionsViaResponses);
    }
    None
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

fn route_supports_compat_marker(
    backend_capabilities: &BTreeSet<String>,
    route_endpoints: Option<&BTreeSet<String>>,
    marker: &str,
) -> bool {
    match route_endpoints {
        Some(endpoints) if !endpoints.is_empty() => endpoints.contains(marker),
        _ => backend_capabilities.contains(marker),
    }
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
            push_unique(&mut candidates, RESPONSES_VIA_CHAT_COMPLETIONS.to_owned());
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

    use onair_core::config::{
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
            &RoundRobinCounters::new(),
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
            &RoundRobinCounters::new(),
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
    fn responses_requires_explicit_chat_compat_marker() {
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
            weight: 1,
            models: vec![ModelRoute {
                public: "public-model".to_owned(),
                backend: "backend-private".to_owned(),
                context_length: ContextLengthPolicy::None,
                tool_schema_mode: ToolSchemaMode::Preserve,
                responses_store: ResponsesStorePolicy::Preserve,
                responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                endpoints: btree_set(["chat"]),
            }],
        };

        let error = match select_backend_candidates(
            &[backend],
            RoutingStrategy::Priority,
            "/v1/responses",
            Some("public-model"),
            false,
            false,
            None,
            &RoundRobinCounters::new(),
        ) {
            Ok(_) => panic!("expected implicit chat compatibility to be rejected"),
            Err(error) => error,
        };

        assert_eq!(error.status, axum::http::StatusCode::NOT_FOUND);
        assert_eq!(error.code.as_deref(), Some("endpoint_unavailable"));
        assert_eq!(error.param.as_deref(), Some("endpoint"));
        assert!(error.message.contains("public-model"));
        assert!(error.message.contains("/v1/responses"));
    }

    #[test]
    fn responses_404_for_known_model_uses_endpoint_unavailable_code() {
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
            weight: 1,
            models: vec![ModelRoute {
                public: "public-model".to_owned(),
                backend: "backend-private".to_owned(),
                context_length: ContextLengthPolicy::None,
                tool_schema_mode: ToolSchemaMode::Preserve,
                responses_store: ResponsesStorePolicy::Preserve,
                responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                endpoints: btree_set(["chat"]),
            }],
        };

        let error = match select_backend_candidates(
            &[backend],
            RoutingStrategy::Priority,
            "/v1/responses",
            Some("public-model"),
            false,
            false,
            None,
            &RoundRobinCounters::new(),
        ) {
            Ok(_) => panic!("expected routing failure"),
            Err(error) => error,
        };

        assert_eq!(error.status, axum::http::StatusCode::NOT_FOUND);
        assert_eq!(error.code.as_deref(), Some("endpoint_unavailable"));
        assert_eq!(error.param.as_deref(), Some("endpoint"));
        assert!(error.message.contains("public-model"));
    }

    #[test]
    fn responses_404_for_unknown_model_uses_model_not_found_code() {
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
            weight: 1,
            models: vec![ModelRoute {
                public: "configured-model".to_owned(),
                backend: "backend-private".to_owned(),
                context_length: ContextLengthPolicy::None,
                tool_schema_mode: ToolSchemaMode::Preserve,
                responses_store: ResponsesStorePolicy::Preserve,
                responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                endpoints: btree_set(["chat"]),
            }],
        };

        let error = match select_backend_candidates(
            &[backend],
            RoutingStrategy::Priority,
            "/v1/responses",
            Some("unknown-model"),
            false,
            false,
            None,
            &RoundRobinCounters::new(),
        ) {
            Ok(_) => panic!("expected routing failure"),
            Err(error) => error,
        };

        assert_eq!(error.status, axum::http::StatusCode::NOT_FOUND);
        assert_eq!(error.code.as_deref(), Some("model_not_found"));
        assert_eq!(error.param.as_deref(), Some("model"));
        assert!(error.message.contains("unknown-model"));
    }

    #[test]
    fn responses_can_route_to_explicit_chat_compat_backend() {
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
            weight: 1,
            models: vec![ModelRoute {
                public: "public-model".to_owned(),
                backend: "backend-private".to_owned(),
                context_length: ContextLengthPolicy::None,
                tool_schema_mode: ToolSchemaMode::Preserve,
                responses_store: ResponsesStorePolicy::Preserve,
                responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                endpoints: btree_set([RESPONSES_VIA_CHAT_COMPLETIONS]),
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
            &RoundRobinCounters::new(),
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
            weight: 1,
            models: vec![ModelRoute {
                public: "public-model".to_owned(),
                backend: "backend-private".to_owned(),
                context_length: ContextLengthPolicy::None,
                tool_schema_mode: ToolSchemaMode::Preserve,
                responses_store: ResponsesStorePolicy::Preserve,
                responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                endpoints: btree_set(["responses", "chat", RESPONSES_VIA_CHAT_COMPLETIONS]),
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
            &RoundRobinCounters::new(),
        )
        .unwrap();

        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].backend_id, "native-backend");
        assert_eq!(routes[0].request_mode, RequestMode::Native);
    }

    #[test]
    fn route_endpoint_can_force_chat_compat_without_responses() {
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
            weight: 1,
            models: vec![ModelRoute {
                public: "public-model".to_owned(),
                backend: "backend-private".to_owned(),
                context_length: ContextLengthPolicy::None,
                tool_schema_mode: ToolSchemaMode::Preserve,
                responses_store: ResponsesStorePolicy::Preserve,
                responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                endpoints: btree_set([RESPONSES_VIA_CHAT_COMPLETIONS]),
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
            &RoundRobinCounters::new(),
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
    fn chat_completions_can_route_to_explicit_responses_compat_backend() {
        let backend = ResolvedBackend {
            id: "responses-backend".to_owned(),
            base_url: "http://responses-backend.example.invalid".to_owned(),
            api_key: None,
            timeout: Duration::from_secs(5),
            capabilities: btree_set(["responses"]),
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            weight: 1,
            models: vec![ModelRoute {
                public: "public-model".to_owned(),
                backend: "backend-private".to_owned(),
                context_length: ContextLengthPolicy::None,
                tool_schema_mode: ToolSchemaMode::Preserve,
                responses_store: ResponsesStorePolicy::Preserve,
                responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                endpoints: btree_set([CHAT_COMPLETIONS_VIA_RESPONSES]),
            }],
        };

        let routes = select_backend_candidates(
            &[backend],
            RoutingStrategy::Priority,
            "/v1/chat/completions",
            Some("public-model"),
            false,
            false,
            None,
            &RoundRobinCounters::new(),
        )
        .unwrap();

        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].backend_id, "responses-backend");
        assert_eq!(
            routes[0].request_mode,
            RequestMode::ChatCompletionsViaResponses
        );
    }

    #[test]
    fn native_chat_capability_prevents_responses_compat_mode() {
        let backend = ResolvedBackend {
            id: "native-chat-backend".to_owned(),
            base_url: "http://native-chat-backend.example.invalid".to_owned(),
            api_key: None,
            timeout: Duration::from_secs(5),
            capabilities: btree_set(["responses", "chat"]),
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            weight: 1,
            models: vec![ModelRoute {
                public: "public-model".to_owned(),
                backend: "backend-private".to_owned(),
                context_length: ContextLengthPolicy::None,
                tool_schema_mode: ToolSchemaMode::Preserve,
                responses_store: ResponsesStorePolicy::Preserve,
                responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                endpoints: btree_set(["chat", CHAT_COMPLETIONS_VIA_RESPONSES]),
            }],
        };

        let routes = select_backend_candidates(
            &[backend],
            RoutingStrategy::Priority,
            "/v1/chat/completions",
            Some("public-model"),
            false,
            false,
            None,
            &RoundRobinCounters::new(),
        )
        .unwrap();

        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].backend_id, "native-chat-backend");
        assert_eq!(routes[0].request_mode, RequestMode::Native);
    }

    #[test]
    fn route_endpoint_can_force_responses_compat_without_chat() {
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
            weight: 1,
            models: vec![ModelRoute {
                public: "public-model".to_owned(),
                backend: "backend-private".to_owned(),
                context_length: ContextLengthPolicy::None,
                tool_schema_mode: ToolSchemaMode::Preserve,
                responses_store: ResponsesStorePolicy::Preserve,
                responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                endpoints: btree_set([CHAT_COMPLETIONS_VIA_RESPONSES]),
            }],
        };

        let routes = select_backend_candidates(
            &[backend],
            RoutingStrategy::Priority,
            "/v1/chat/completions",
            Some("public-model"),
            false,
            false,
            None,
            &RoundRobinCounters::new(),
        )
        .unwrap();

        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].backend_id, "mixed-backend");
        assert_eq!(
            routes[0].request_mode,
            RequestMode::ChatCompletionsViaResponses
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
            weight: 1,
            models: vec![ModelRoute {
                public: "public-model".to_owned(),
                backend: "unsupported-private".to_owned(),
                context_length: ContextLengthPolicy::None,
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
            weight: 1,
            models: vec![ModelRoute {
                public: "public-model".to_owned(),
                backend: "supported-private".to_owned(),
                context_length: ContextLengthPolicy::None,
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
            &RoundRobinCounters::new(),
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
            weight: 1,
            models: vec![ModelRoute {
                public: "public-model".to_owned(),
                backend: "plain-private".to_owned(),
                context_length: ContextLengthPolicy::None,
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
            &RoundRobinCounters::new(),
        ) {
            Ok(_) => panic!("expected tool request routing to fail"),
            Err(error) => error,
        };

        assert_eq!(error.status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(error.param.as_deref(), Some("tools"));
    }

    #[test]
    fn round_robin_advances_per_model() {
        let counters = RoundRobinCounters::new();
        assert_eq!(counters.next_index("public-model", 3), 0);
        assert_eq!(counters.next_index("public-model", 3), 1);
        assert_eq!(counters.next_index("public-model", 3), 2);
        assert_eq!(counters.next_index("public-model", 3), 0);
        assert_eq!(counters.next_index("other-model", 3), 0);
        assert_eq!(counters.next_index("other-model", 3), 1);
        assert_eq!(counters.next_index("public-model", 3), 1);
    }

    #[test]
    fn round_robin_single_candidate_noop() {
        let counters = RoundRobinCounters::new();
        for _ in 0..5 {
            assert_eq!(counters.next_index("public-model", 1), 0);
        }
    }

    #[test]
    fn round_robin_wraps() {
        let counters = RoundRobinCounters::new();
        for _ in 0..3 {
            assert_eq!(counters.next_index("k", 2), 0);
            assert_eq!(counters.next_index("k", 2), 1);
        }
    }

    #[test]
    fn round_robin_preserves_fallback_set() {
        let backends = vec![
            backend("backend-a"),
            backend("backend-b"),
            backend("backend-c"),
        ];
        let counters = RoundRobinCounters::new();
        let routes = select_backend_candidates(
            &backends,
            RoutingStrategy::RoundRobin,
            "/v1/responses",
            Some("public-model"),
            false,
            false,
            None,
            &counters,
        )
        .unwrap();
        assert_eq!(routes.len(), 3);
        let selected: BTreeSet<&str> = routes
            .iter()
            .map(|route| route.backend_id.as_str())
            .collect();
        let original: BTreeSet<&str> = backends.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(selected, original);
    }

    #[test]
    fn round_robin_uses_model_key() {
        let counters = RoundRobinCounters::new();
        let routes_a = select_backend_candidates(
            &[
                backend_with_public_model("a", "model-a"),
                backend_with_public_model("b", "model-a"),
                backend_with_public_model("c", "model-a"),
            ],
            RoutingStrategy::RoundRobin,
            "/v1/responses",
            Some("model-a"),
            false,
            false,
            None,
            &counters,
        )
        .unwrap();
        let routes_b = select_backend_candidates(
            &[
                backend_with_public_model("a", "model-b"),
                backend_with_public_model("b", "model-b"),
            ],
            RoutingStrategy::RoundRobin,
            "/v1/responses",
            Some("model-b"),
            false,
            false,
            None,
            &counters,
        )
        .unwrap();
        assert_eq!(routes_a[0].backend_id, "a");
        assert_eq!(routes_b[0].backend_id, "a");
        let routes_b_second = select_backend_candidates(
            &[
                backend_with_public_model("a", "model-b"),
                backend_with_public_model("b", "model-b"),
            ],
            RoutingStrategy::RoundRobin,
            "/v1/responses",
            Some("model-b"),
            false,
            false,
            None,
            &counters,
        )
        .unwrap();
        assert_eq!(routes_b_second[0].backend_id, "b");
    }

    #[test]
    fn model_less_unavailable_path_uses_endpoint_unavailable_code() {
        let backends = vec![model_less_backend("a", &["embeddings", "streaming"])];
        let error = match select_backend_candidates(
            &backends,
            RoutingStrategy::Priority,
            "/v1/audio/transcriptions",
            None,
            false,
            false,
            None,
            &RoundRobinCounters::new(),
        ) {
            Ok(_) => panic!("expected model-less routing failure"),
            Err(error) => error,
        };

        assert_eq!(error.status, axum::http::StatusCode::NOT_FOUND);
        assert_eq!(error.code.as_deref(), Some("endpoint_unavailable"));
        assert_eq!(error.param.as_deref(), Some("endpoint"));
        assert!(error.message.contains("/v1/audio/transcriptions"));
    }

    #[test]
    fn round_robin_path_key_for_model_less() {
        let counters = RoundRobinCounters::new();
        let backends = vec![
            model_less_backend("a", &["embeddings", "streaming"]),
            model_less_backend("b", &["embeddings", "streaming"]),
        ];
        let routes_a = select_backend_candidates(
            &backends,
            RoutingStrategy::RoundRobin,
            "/v1/embeddings",
            None,
            false,
            false,
            None,
            &counters,
        )
        .unwrap();
        let routes_b = select_backend_candidates(
            &backends,
            RoutingStrategy::RoundRobin,
            "/v1/embeddings",
            None,
            false,
            false,
            None,
            &counters,
        )
        .unwrap();
        assert_eq!(routes_a[0].backend_id, "a");
        assert_eq!(routes_b[0].backend_id, "b");
    }

    #[test]
    fn round_robin_shrinking_candidate_list() {
        let counters = RoundRobinCounters::new();
        for _ in 0..3 {
            counters.next_index("public-model", 5);
        }
        assert_eq!(counters.next_index("public-model", 2), 3 % 2);
    }

    #[test]
    fn round_robin_zero_count_returns_zero() {
        let counters = RoundRobinCounters::new();
        for _ in 0..3 {
            assert_eq!(counters.next_index("k", 0), 0);
        }
    }

    #[test]
    fn round_robin_default_matches_new() {
        let a = RoundRobinCounters::default();
        let b = RoundRobinCounters::new();
        assert_eq!(a.next_index("k", 3), b.next_index("k", 3));
    }

    #[test]
    fn round_robin_concurrent_threads_cover_all_indices() {
        use std::sync::Arc;
        use std::thread;

        let counters = Arc::new(RoundRobinCounters::new());
        let mut handles = Vec::new();
        for _ in 0..4 {
            let counters = Arc::clone(&counters);
            handles.push(thread::spawn(move || {
                let mut hits = [0usize; 3];
                for _ in 0..3000 {
                    let index = counters.next_index("public-model", 3);
                    hits[index] += 1;
                }
                hits
            }));
        }
        let mut total = [0usize; 3];
        for handle in handles {
            let hits = handle.join().unwrap();
            total[0] += hits[0];
            total[1] += hits[1];
            total[2] += hits[2];
        }
        // 4 threads * 3000 iterations = 12000 total increments. Distributed
        // across 3 indices, the modulo must produce a balanced distribution
        // (each index within +/- 1 of 4000).
        let total_increments: usize = total.iter().sum();
        assert_eq!(total_increments, 12_000);
        for count in total {
            assert!((3999..=4001).contains(&count), "unbalanced: {total:?}");
        }
    }

    #[test]
    fn weighted_random_deterministic_weights_zero_skipped() {
        let backends = vec![
            backend_with_weight("a", 1),
            backend_with_weight("b", 0),
            backend_with_weight("c", 1),
        ];
        for _ in 0..200 {
            let routes = select_backend_candidates(
                &backends,
                RoutingStrategy::WeightedRandom,
                "/v1/responses",
                Some("public-model"),
                false,
                false,
                None,
                &RoundRobinCounters::new(),
            )
            .unwrap();
            assert!(routes[0].backend_id == "a" || routes[0].backend_id == "c");
        }
    }

    #[test]
    fn weighted_random_single_candidate_noop() {
        let backends = vec![backend_with_weight("only", 5)];
        for _ in 0..10 {
            let routes = select_backend_candidates(
                &backends,
                RoutingStrategy::WeightedRandom,
                "/v1/responses",
                Some("public-model"),
                false,
                false,
                None,
                &RoundRobinCounters::new(),
            )
            .unwrap();
            assert_eq!(routes[0].backend_id, "only");
        }
    }

    #[test]
    fn weighted_random_preserves_fallback_set() {
        let backends = vec![
            backend_with_weight("backend-a", 1),
            backend_with_weight("backend-b", 1),
            backend_with_weight("backend-c", 1),
        ];
        let routes = select_backend_candidates(
            &backends,
            RoutingStrategy::WeightedRandom,
            "/v1/responses",
            Some("public-model"),
            false,
            false,
            None,
            &RoundRobinCounters::new(),
        )
        .unwrap();
        assert_eq!(routes.len(), 3);
        let selected: BTreeSet<&str> = routes
            .iter()
            .map(|route| route.backend_id.as_str())
            .collect();
        let original: BTreeSet<&str> = backends.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(selected, original);
    }

    #[test]
    fn strategy_round_robin_serializes_via_selected_route_weight() {
        let backends = vec![backend_with_weight("a", 7)];
        let routes = select_backend_candidates(
            &backends,
            RoutingStrategy::Priority,
            "/v1/responses",
            Some("public-model"),
            false,
            false,
            None,
            &RoundRobinCounters::new(),
        )
        .unwrap();
        assert_eq!(routes[0].weight, 7);
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
                context_length: ContextLengthPolicy::None,
                tool_schema_mode: ToolSchemaMode::Preserve,
                responses_store: ResponsesStorePolicy::Preserve,
                responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                endpoints: btree_set(["responses"]),
            }],
            weight: 1,
        }
    }

    fn btree_set<const N: usize>(values: [&str; N]) -> BTreeSet<String> {
        values.into_iter().map(str::to_owned).collect()
    }

    #[test]
    fn known_markers_allowlist_is_non_empty_and_deduped() {
        let len = KNOWN_MARKERS.len();
        assert!(len > 0, "KNOWN_MARKERS must not be empty");

        let unique: BTreeSet<&str> = KNOWN_MARKERS.iter().copied().collect();
        assert_eq!(
            unique.len(),
            len,
            "KNOWN_MARKERS must not contain duplicates"
        );
    }

    #[test]
    fn known_markers_contain_structural_aliases() {
        let set: BTreeSet<&str> = KNOWN_MARKERS.iter().copied().collect();
        for required in [
            "all",
            "streaming",
            "chat",
            "chat_completions",
            "completions",
            "responses",
            "response",
            "tools",
            "tool_calls",
            "function_calling",
            "functions",
            "responses_via_chat_completions",
            "chat_completions_via_responses",
            "embeddings",
            "images",
            "image",
            "audio",
            "files",
            "file",
            "models",
            "model",
            "batches",
            "fine_tuning",
            "assistants",
            "threads",
            "vector_stores",
            "uploads",
        ] {
            assert!(
                set.contains(required),
                "KNOWN_MARKERS must include the structural marker '{required}'"
            );
        }
    }

    #[test]
    fn is_known_marker_matches_canonical_allowlist() {
        for value in KNOWN_MARKERS {
            assert!(
                is_known_marker(value),
                "is_known_marker must agree with KNOWN_MARKERS for '{value}'"
            );
        }
        for value in [
            "respons",
            "responses_via_chat_completion",
            "straming",
            "tols",
        ] {
            assert!(
                !is_known_marker(value),
                "is_known_marker must reject the typo '{value}'"
            );
        }
    }

    fn backend_with_weight(id: &str, weight: u32) -> ResolvedBackend {
        ResolvedBackend {
            weight,
            ..backend(id)
        }
    }

    fn backend_with_public_model(id: &str, public_model: &str) -> ResolvedBackend {
        let mut backend = backend(id);
        backend.models[0].public = public_model.to_owned();
        backend
    }

    fn model_less_backend(id: &str, capabilities: &[&str]) -> ResolvedBackend {
        ResolvedBackend {
            id: id.to_owned(),
            base_url: format!("http://{id}.example.invalid"),
            api_key: None,
            timeout: Duration::from_secs(5),
            capabilities: capabilities.iter().map(|s| s.to_string()).collect(),
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            models: Vec::new(),
            weight: 1,
        }
    }
}
