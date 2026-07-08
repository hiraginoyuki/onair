use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use rand::Rng;

use onair_core::compat::{
    CHAT_COMPLETIONS_VIA_MESSAGES, CHAT_COMPLETIONS_VIA_RESPONSES, RESPONSES_VIA_CHAT_COMPLETIONS,
};
use onair_core::config::{
    ChatStreamUsagePolicy, ResolvedBackend, ResolvedRoute, ResponsesMaxOutputTokensPolicy,
    ResponsesStorePolicy, RoutingStrategy, ToolSchemaMode,
};
use onair_core::error::ApiError;
use onair_core::openai::RequestMode;

#[cfg(test)]
pub use onair_core::{KNOWN_MARKERS, is_known_marker};

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
    /// Human-readable route label used in warn logs. Format: `public=...`
    /// for public routes, `path=...` for model-less ones, or `none` if
    /// the route is not part of a configured `[[route]]` block.
    pub route_key_label: String,
    /// Per-route upstream request body overrides, merged on top of
    /// the bound backend's defaults. See `docs/configuration.md`.
    pub extra_body: BTreeMap<String, onair_core::TomlValue>,
    /// Per-route extra headers injected into the upstream request
    /// with override semantics. See `docs/configuration.md`.
    pub request_headers: BTreeMap<String, String>,
    /// Resolved per-route value: forward non-2xx upstream
    /// responses (status mapped via `map_upstream_status`, body
    /// capped at 1 MiB, strict header allowlist) to the client
    /// instead of replacing them with the sanitized OpenAI error
    /// envelope. See `docs/configuration.md` and
    /// `docs/security.md`.
    pub expose_backend_errors: bool,
    /// Resolved per-route value: record per-event SSE / chunk
    /// captures for streaming responses to `upstream_response.ndjson`
    /// and `client_response.ndjson` in the debug capture directory.
    /// See `.local/decisions/2026-06-27-streaming-debug-capture.md`.
    pub stream_capture: bool,
    /// Default `max_tokens` for Anthropic Messages API requests
    /// when the client omits it.
    pub anthropic_max_tokens: Option<u32>,
}

pub struct NonEmptyVec<T> {
    head: T,
    tail: Vec<T>,
}

impl<T> NonEmptyVec<T> {
    pub fn from_vec(vec: Vec<T>) -> Option<Self> {
        let mut iter = vec.into_iter();
        let head = iter.next()?;
        Some(Self {
            head,
            tail: iter.collect(),
        })
    }

    pub fn head(&self) -> &T {
        &self.head
    }

    pub fn tail(&self) -> &[T] {
        &self.tail
    }

    pub fn len(&self) -> usize {
        1 + self.tail.len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn iter(&self) -> std::iter::Chain<std::iter::Once<&T>, std::slice::Iter<'_, T>> {
        std::iter::once(&self.head).chain(self.tail.iter())
    }
}

impl<T> IntoIterator for NonEmptyVec<T> {
    type Item = T;
    type IntoIter = std::iter::Chain<std::iter::Once<T>, std::vec::IntoIter<T>>;

    fn into_iter(self) -> Self::IntoIter {
        std::iter::once(self.head).chain(self.tail)
    }
}

pub struct RoundRobinCounters {
    inner: Arc<Mutex<HashMap<String, u64>>>,
}

impl Clone for RoundRobinCounters {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[cfg(test)]
impl Default for RoundRobinCounters {
    fn default() -> Self {
        Self::new()
    }
}

impl RoundRobinCounters {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
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
    routes: &[ResolvedRoute],
    strategy: RoutingStrategy,
    path: &str,
    model: Option<&str>,
    stream: bool,
    tools: bool,
    sticky_key: Option<&str>,
    round_robin: &RoundRobinCounters,
) -> Result<NonEmptyVec<SelectedRoute>, ApiError> {
    BackendSelector::new(BackendSelectionRequest {
        backends,
        routes,
        strategy,
        path,
        model,
        stream,
        tools,
        sticky_key,
        round_robin,
    })
    .select()
}

#[derive(Clone, Copy)]
pub struct BackendSelectionRequest<'a> {
    pub backends: &'a [ResolvedBackend],
    pub routes: &'a [ResolvedRoute],
    pub strategy: RoutingStrategy,
    pub path: &'a str,
    pub model: Option<&'a str>,
    pub stream: bool,
    pub tools: bool,
    pub sticky_key: Option<&'a str>,
    pub round_robin: &'a RoundRobinCounters,
}

pub struct BackendSelector<'a> {
    request: BackendSelectionRequest<'a>,
    path_candidates: BTreeSet<String>,
}

impl<'a> BackendSelector<'a> {
    pub fn new(request: BackendSelectionRequest<'a>) -> Self {
        let path_candidates = path_capability_candidates(request.path);
        Self {
            request,
            path_candidates,
        }
    }

    pub fn select(self) -> Result<NonEmptyVec<SelectedRoute>, ApiError> {
        let path = self.request.path;
        let strategy = self.request.strategy;
        let model = self.request.model;
        let mut candidates = Vec::new();
        let mut tool_incompatible_candidates = false;

        if let Some(requested_model) = model {
            let Some(route) = self
                .request
                .routes
                .iter()
                .find(|r| matches!(&r.key, onair_core::config::RouteKey::Public(public) if public == requested_model))
            else {
                return Err(ApiError::model_not_found(requested_model));
            };
            for binding in &route.backends {
                let Some(backend) = self
                    .request
                    .backends
                    .iter()
                    .find(|b| b.id == binding.backend_id)
                else {
                    continue;
                };
                let Some(request_mode) =
                    self.is_eligible(backend, Some(route), &mut tool_incompatible_candidates)
                else {
                    continue;
                };
                candidates.push(self.build_candidate(
                    backend,
                    Some(route),
                    Some(binding),
                    Some(requested_model),
                    request_mode,
                ));
            }
        } else {
            let route = self
                .request
                .routes
                .iter()
                .find(|r| matches!(&r.key, onair_core::config::RouteKey::Path(p) if p.trim_end_matches('/') == path.trim_end_matches('/')));
            for backend in self.request.backends {
                if let Some(route) = route
                    && !route.backends.iter().any(|b| b.backend_id == backend.id)
                {
                    continue;
                }
                let Some(request_mode) =
                    self.is_eligible(backend, route, &mut tool_incompatible_candidates)
                else {
                    continue;
                };
                candidates.push(self.build_candidate(backend, route, None, None, request_mode));
            }
        }

        if candidates.is_empty() {
            if self.request.tools && tool_incompatible_candidates {
                return Err(ApiError::bad_request(
                    "The selected model does not support tool calling.",
                    Some("tools".to_owned()),
                ));
            }
            if let Some(requested_model) = model {
                return Err(ApiError::endpoint_unavailable(path, Some(requested_model)));
            }
            return Err(ApiError::endpoint_unavailable(path, None));
        }

        match strategy {
            RoutingStrategy::Priority => {}
            RoutingStrategy::Sticky => {
                let index = sticky_index(self.request.sticky_key.unwrap_or(path), candidates.len());
                candidates.rotate_left(index);
            }
            RoutingStrategy::RoundRobin => {
                let key = model.unwrap_or(path);
                let index = self.request.round_robin.next_index(key, candidates.len());
                candidates.rotate_left(index);
            }
            RoutingStrategy::WeightedRandom => {
                weighted_rotate(&mut candidates);
            }
        }

        debug_assert!(
            !candidates.is_empty(),
            "candidates remained non-empty after rotation"
        );
        Ok(NonEmptyVec::from_vec(candidates).expect("candidates is non-empty"))
    }

    fn is_eligible(
        &self,
        backend: &ResolvedBackend,
        route: Option<&ResolvedRoute>,
        tool_incompatible: &mut bool,
    ) -> Option<RequestMode> {
        if self.request.stream && !has_capability(&backend.supports, "streaming") {
            return None;
        }
        let request_mode = match route {
            Some(route) => request_mode_for_route(
                self.request.path,
                &self.path_candidates,
                &backend.supports,
                Some(&route.expose),
            ),
            None => request_mode_for_backend(
                self.request.path,
                &self.path_candidates,
                &backend.supports,
            ),
        }?;
        if self.request.tools && !supports_tools(&backend.supports, route.map(|r| &r.expose)) {
            *tool_incompatible = true;
            return None;
        }
        Some(request_mode)
    }

    fn build_candidate(
        &self,
        backend: &ResolvedBackend,
        route: Option<&ResolvedRoute>,
        binding: Option<&onair_core::config::RouteBackendBinding>,
        model: Option<&str>,
        request_mode: RequestMode,
    ) -> SelectedRoute {
        let public_model = model.map(str::to_owned);
        let backend_model = binding.map(|b| b.backend_model.clone());
        let (tool_schema_mode, responses_store, responses_max_output_tokens, chat_stream_usage) =
            match route {
                Some(route) => (
                    route.tool_schema_mode,
                    route.responses_store,
                    route.responses_max_output_tokens,
                    route.chat_stream_usage,
                ),
                None => (
                    backend.tool_schema_mode,
                    backend.responses_store,
                    backend.responses_max_output_tokens,
                    backend.chat_stream_usage,
                ),
            };
        let route_key_label = match route {
            Some(route) => match &route.key {
                onair_core::config::RouteKey::Public(public) => format!("public={public}"),
                onair_core::config::RouteKey::Path(path) => format!("path={path}"),
            },
            None => "none".to_owned(),
        };
        let extra_body = route.map(|r| r.extra_body.clone()).unwrap_or_default();
        let request_headers = route.map(|r| r.request_headers.clone()).unwrap_or_default();
        let expose_backend_errors = route.map(|r| r.expose_backend_errors).unwrap_or(false);
        let stream_capture = route.map(|r| r.stream_capture).unwrap_or(false);
        let anthropic_max_tokens = route.and_then(|r| r.anthropic_max_tokens);
        SelectedRoute {
            backend_id: backend.id.clone(),
            base_url: backend.base_url.clone(),
            api_key: backend.api_key.clone(),
            timeout: backend.timeout,
            public_model,
            backend_model,
            request_mode,
            tool_schema_mode,
            responses_store,
            responses_max_output_tokens,
            chat_stream_usage,
            weight: backend.weight,
            route_key_label,
            extra_body,
            request_headers,
            expose_backend_errors,
            stream_capture,
            anthropic_max_tokens,
        }
    }
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

fn request_mode_for_route(
    path: &str,
    path_candidates: &BTreeSet<String>,
    backend_supports: &BTreeSet<String>,
    route_expose: Option<&BTreeSet<String>>,
) -> Option<RequestMode> {
    request_mode_for_path(path, path_candidates, backend_supports, route_expose)
}

fn request_mode_for_backend(
    path: &str,
    path_candidates: &BTreeSet<String>,
    backend_supports: &BTreeSet<String>,
) -> Option<RequestMode> {
    request_mode_for_path(path, path_candidates, backend_supports, None)
}

fn request_mode_for_path(
    path: &str,
    path_candidates: &BTreeSet<String>,
    backend_supports: &BTreeSet<String>,
    route_expose: Option<&BTreeSet<String>>,
) -> Option<RequestMode> {
    match path.trim_end_matches('/') {
        "/v1/responses" => request_mode_for_responses(backend_supports, route_expose),
        "/v1/chat/completions" | "/v1/chat/completion" => {
            request_mode_for_chat_completions(backend_supports, route_expose)
        }
        "/v1/messages" => request_mode_for_messages(backend_supports, route_expose),
        _ => {
            if !supports_candidates(backend_supports, path_candidates) {
                return None;
            }
            if let Some(expose) = route_expose
                && !expose.is_empty()
                && !supports_candidates(expose, path_candidates)
            {
                return None;
            }
            Some(RequestMode::Native)
        }
    }
}

fn request_mode_for_responses(
    backend_supports: &BTreeSet<String>,
    route_expose: Option<&BTreeSet<String>>,
) -> Option<RequestMode> {
    if supports_responses(backend_supports) && route_supports_responses(route_expose) {
        return Some(RequestMode::Native);
    }
    if supports_chat_compat(backend_supports)
        && route_supports_compat_marker(
            backend_supports,
            route_expose,
            RESPONSES_VIA_CHAT_COMPLETIONS,
        )
    {
        return Some(RequestMode::ResponsesViaChatCompletions);
    }
    None
}

fn request_mode_for_chat_completions(
    backend_supports: &BTreeSet<String>,
    route_expose: Option<&BTreeSet<String>>,
) -> Option<RequestMode> {
    if supports_chat_compat(backend_supports) && route_supports_chat_compat(route_expose) {
        return Some(RequestMode::Native);
    }
    if supports_responses(backend_supports)
        && route_supports_compat_marker(
            backend_supports,
            route_expose,
            CHAT_COMPLETIONS_VIA_RESPONSES,
        )
    {
        return Some(RequestMode::ChatCompletionsViaResponses);
    }
    if supports_messages(backend_supports)
        && route_supports_compat_marker(
            backend_supports,
            route_expose,
            CHAT_COMPLETIONS_VIA_MESSAGES,
        )
    {
        return Some(RequestMode::ChatCompletionsViaMessages);
    }
    None
}

fn request_mode_for_messages(
    backend_supports: &BTreeSet<String>,
    route_expose: Option<&BTreeSet<String>>,
) -> Option<RequestMode> {
    if supports_messages(backend_supports) && route_supports_messages(route_expose) {
        return Some(RequestMode::AnthropicMessagesNative);
    }
    None
}

fn supports_messages(supports: &BTreeSet<String>) -> bool {
    has_capability(supports, "messages")
}

fn route_supports_messages(route_expose: Option<&BTreeSet<String>>) -> bool {
    route_expose.is_none_or(|expose| expose.is_empty() || has_capability(expose, "messages"))
}

fn route_supports_responses(route_expose: Option<&BTreeSet<String>>) -> bool {
    route_expose.is_none_or(|expose| {
        expose.is_empty()
            || has_capability(expose, "responses")
            || has_capability(expose, "response")
    })
}

fn route_supports_chat_compat(route_expose: Option<&BTreeSet<String>>) -> bool {
    route_expose.is_none_or(|expose| expose.is_empty() || supports_chat_compat(expose))
}

fn route_supports_compat_marker(
    backend_supports: &BTreeSet<String>,
    route_expose: Option<&BTreeSet<String>>,
    marker: &str,
) -> bool {
    match route_expose {
        Some(expose) if !expose.is_empty() => expose.contains(marker),
        _ => backend_supports.contains(marker),
    }
}

fn supports_responses(supports: &BTreeSet<String>) -> bool {
    has_capability(supports, "responses") || has_capability(supports, "response")
}

fn supports_chat_compat(supports: &BTreeSet<String>) -> bool {
    has_capability(supports, "chat")
        || has_capability(supports, "chat_completions")
        || has_capability(supports, "completions")
}

fn supports_tools(
    backend_supports: &BTreeSet<String>,
    route_expose: Option<&BTreeSet<String>>,
) -> bool {
    has_tool_capability(backend_supports) && route_supports_tools(route_expose)
}

fn route_supports_tools(route_expose: Option<&BTreeSet<String>>) -> bool {
    route_expose.is_none_or(|expose| expose.is_empty() || has_tool_capability(expose))
}

fn has_tool_capability(supports: &BTreeSet<String>) -> bool {
    has_capability(supports, "tools")
        || has_capability(supports, "tool_calls")
        || has_capability(supports, "function_calling")
        || has_capability(supports, "functions")
}

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

struct FnvHasher {
    state: u64,
    started: bool,
}

impl Default for FnvHasher {
    fn default() -> Self {
        Self {
            state: FNV_OFFSET_BASIS,
            started: false,
        }
    }
}

impl Hasher for FnvHasher {
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.state ^= u64::from(*byte);
            self.state = self.state.wrapping_mul(FNV_PRIME);
            self.started = true;
        }
    }

    fn finish(&self) -> u64 {
        if self.started {
            self.state
        } else {
            FNV_OFFSET_BASIS
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
    let normalized = path.trim_end_matches('/');
    if normalized.ends_with("/chat/completions") {
        return "chat_completions".to_owned();
    }
    if normalized.ends_with("/chat/completion") {
        return "chat_completions".to_owned();
    }
    if normalized.ends_with("/responses") {
        return "responses".to_owned();
    }
    if normalized.ends_with("/messages") {
        return "messages".to_owned();
    }
    path_capability_candidates(path)
        .iter()
        .next()
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
            | "/v1/messages"
            | "/v1/embeddings"
            | "/v1/audio/transcriptions"
            | "/v1/audio/translations"
            | "/v1/images/generations"
            | "/v1/images/edits"
            | "/v1/images/variations"
    )
}

pub fn path_capability_candidates(path: &str) -> BTreeSet<String> {
    let trimmed = path
        .strip_prefix("/v1/")
        .unwrap_or(path)
        .trim_start_matches('/');
    if trimmed.is_empty() {
        return BTreeSet::new();
    }

    let segments = trimmed
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(normalize_segment)
        .collect::<Vec<_>>();
    if segments.is_empty() {
        return BTreeSet::new();
    }

    let mut candidates = BTreeSet::new();
    candidates.insert(segments[0].clone());
    if segments[0].ends_with('s') && segments[0].len() > 1 {
        candidates.insert(singularize(&segments[0]));
    }
    if let Some(second_segment) = segments.get(1) {
        candidates.insert(second_segment.clone());
        candidates.insert(format!("{}_{}", segments[0], second_segment));
    }
    if let Some(third_segment) = segments.get(2) {
        candidates.insert(format!("{}_{}", segments[0], third_segment));
        candidates.insert(third_segment.clone());
    }

    match segments[0].as_str() {
        "chat" => {
            candidates.insert("chat_completions".to_owned());
            candidates.insert("completions".to_owned());
        }
        "responses" => {
            candidates.insert(RESPONSES_VIA_CHAT_COMPLETIONS.to_owned());
        }
        "messages" => {
            candidates.insert("messages".to_owned());
        }
        "images" => {
            candidates.insert("image".to_owned());
        }
        "files" => {
            candidates.insert("file".to_owned());
        }
        "models" => {
            candidates.insert("model".to_owned());
        }
        "audio" => {
            candidates.insert("audio".to_owned());
        }
        _ => {}
    }

    candidates
}

fn supports_candidates(capabilities: &BTreeSet<String>, candidates: &BTreeSet<String>) -> bool {
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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::time::Duration;

    use onair_core::config::{
        ChatStreamUsagePolicy, ContextLengthSpec, ResolvedBackend, ResolvedRoute,
        ResponsesMaxOutputTokensPolicy, ResponsesStorePolicy, RouteBackendBinding, RouteKey,
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
        let routes = default_routes(&backends);
        let sticky_key = sticky_routing_key(
            "client-a",
            "/v1/responses",
            Some("public-model"),
            Some("cache-key"),
        );

        let priority = select_backend_candidates(
            &backends,
            &routes,
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
            &routes,
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
        let expected = priority.iter().nth(selected_index).unwrap();
        assert_eq!(sticky.head().backend_id, expected.backend_id);
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
        let backends = vec![backend_with_supports("chat-backend", &["chat"])];
        let routes = vec![route_for_public(
            "public-model",
            &["chat"],
            &[("backend-private", "chat-backend")],
        )];

        let error = match select_backend_candidates(
            &backends,
            &routes,
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
        let backends = vec![backend_with_supports("chat-backend", &["chat"])];
        let routes = vec![route_for_public(
            "public-model",
            &["chat"],
            &[("backend-private", "chat-backend")],
        )];

        let error = match select_backend_candidates(
            &backends,
            &routes,
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
        let backends = vec![backend_with_supports("chat-backend", &["chat"])];
        let routes = vec![route_for_public(
            "configured-model",
            &["chat"],
            &[("backend-private", "chat-backend")],
        )];

        let error = match select_backend_candidates(
            &backends,
            &routes,
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
        let backends = vec![backend_with_supports("chat-backend", &["chat"])];
        let routes = vec![route_for_public(
            "public-model",
            &[RESPONSES_VIA_CHAT_COMPLETIONS],
            &[("backend-private", "chat-backend")],
        )];

        let selected = select_backend_candidates(
            &backends,
            &routes,
            RoutingStrategy::Priority,
            "/v1/responses",
            Some("public-model"),
            false,
            false,
            None,
            &RoundRobinCounters::new(),
        )
        .unwrap();

        assert_eq!(selected.len(), 1);
        assert_eq!(selected.head().backend_id, "chat-backend");
        assert_eq!(
            selected.head().request_mode,
            RequestMode::ResponsesViaChatCompletions
        );
    }

    #[test]
    fn native_responses_capability_prevents_chat_compat_mode() {
        let backends = vec![backend_with_supports(
            "native-backend",
            &["responses", "chat"],
        )];
        let routes = vec![route_for_public(
            "public-model",
            &["responses", "chat", RESPONSES_VIA_CHAT_COMPLETIONS],
            &[("backend-private", "native-backend")],
        )];

        let selected = select_backend_candidates(
            &backends,
            &routes,
            RoutingStrategy::Priority,
            "/v1/responses",
            Some("public-model"),
            false,
            false,
            None,
            &RoundRobinCounters::new(),
        )
        .unwrap();

        assert_eq!(selected.len(), 1);
        assert_eq!(selected.head().backend_id, "native-backend");
        assert_eq!(selected.head().request_mode, RequestMode::Native);
    }

    #[test]
    fn route_endpoint_can_force_chat_compat_without_responses() {
        let backends = vec![backend_with_supports(
            "mixed-backend",
            &["responses", "chat"],
        )];
        let routes = vec![route_for_public(
            "public-model",
            &[RESPONSES_VIA_CHAT_COMPLETIONS],
            &[("backend-private", "mixed-backend")],
        )];

        let selected = select_backend_candidates(
            &backends,
            &routes,
            RoutingStrategy::Priority,
            "/v1/responses",
            Some("public-model"),
            false,
            false,
            None,
            &RoundRobinCounters::new(),
        )
        .unwrap();

        assert_eq!(selected.len(), 1);
        assert_eq!(selected.head().backend_id, "mixed-backend");
        assert_eq!(
            selected.head().request_mode,
            RequestMode::ResponsesViaChatCompletions
        );
    }

    #[test]
    fn chat_completions_can_route_to_explicit_responses_compat_backend() {
        let backends = vec![backend_with_supports("responses-backend", &["responses"])];
        let routes = vec![route_for_public(
            "public-model",
            &[CHAT_COMPLETIONS_VIA_RESPONSES],
            &[("backend-private", "responses-backend")],
        )];

        let selected = select_backend_candidates(
            &backends,
            &routes,
            RoutingStrategy::Priority,
            "/v1/chat/completions",
            Some("public-model"),
            false,
            false,
            None,
            &RoundRobinCounters::new(),
        )
        .unwrap();

        assert_eq!(selected.len(), 1);
        assert_eq!(selected.head().backend_id, "responses-backend");
        assert_eq!(
            selected.head().request_mode,
            RequestMode::ChatCompletionsViaResponses
        );
    }

    #[test]
    fn native_chat_capability_prevents_responses_compat_mode() {
        let backends = vec![backend_with_supports(
            "native-chat-backend",
            &["responses", "chat"],
        )];
        let routes = vec![route_for_public(
            "public-model",
            &["chat", CHAT_COMPLETIONS_VIA_RESPONSES],
            &[("backend-private", "native-chat-backend")],
        )];

        let selected = select_backend_candidates(
            &backends,
            &routes,
            RoutingStrategy::Priority,
            "/v1/chat/completions",
            Some("public-model"),
            false,
            false,
            None,
            &RoundRobinCounters::new(),
        )
        .unwrap();

        assert_eq!(selected.len(), 1);
        assert_eq!(selected.head().backend_id, "native-chat-backend");
        assert_eq!(selected.head().request_mode, RequestMode::Native);
    }

    #[test]
    fn route_endpoint_can_force_responses_compat_without_chat() {
        let backends = vec![backend_with_supports(
            "mixed-backend",
            &["responses", "chat"],
        )];
        let routes = vec![route_for_public(
            "public-model",
            &[CHAT_COMPLETIONS_VIA_RESPONSES],
            &[("backend-private", "mixed-backend")],
        )];

        let selected = select_backend_candidates(
            &backends,
            &routes,
            RoutingStrategy::Priority,
            "/v1/chat/completions",
            Some("public-model"),
            false,
            false,
            None,
            &RoundRobinCounters::new(),
        )
        .unwrap();

        assert_eq!(selected.len(), 1);
        assert_eq!(selected.head().backend_id, "mixed-backend");
        assert_eq!(
            selected.head().request_mode,
            RequestMode::ChatCompletionsViaResponses
        );
    }

    #[test]
    fn chat_completions_can_route_to_explicit_messages_compat_backend() {
        let backends = vec![backend_with_supports("messages-backend", &["messages"])];
        let routes = vec![route_for_public(
            "public-model",
            &[CHAT_COMPLETIONS_VIA_MESSAGES],
            &[("backend-private", "messages-backend")],
        )];

        let selected = select_backend_candidates(
            &backends,
            &routes,
            RoutingStrategy::Priority,
            "/v1/chat/completions",
            Some("public-model"),
            false,
            false,
            None,
            &RoundRobinCounters::new(),
        )
        .unwrap();

        assert_eq!(selected.len(), 1);
        assert_eq!(selected.head().backend_id, "messages-backend");
        assert_eq!(
            selected.head().request_mode,
            RequestMode::ChatCompletionsViaMessages
        );
    }

    #[test]
    fn chat_completions_prefers_responses_compat_before_messages_compat() {
        let backends = vec![
            backend_with_supports("responses-backend", &["responses"]),
            backend_with_supports("messages-backend", &["messages"]),
        ];
        let routes = vec![route_for_public(
            "public-model",
            &[
                CHAT_COMPLETIONS_VIA_RESPONSES,
                CHAT_COMPLETIONS_VIA_MESSAGES,
            ],
            &[
                ("responses-private", "responses-backend"),
                ("messages-private", "messages-backend"),
            ],
        )];

        let selected = select_backend_candidates(
            &backends,
            &routes,
            RoutingStrategy::Priority,
            "/v1/chat/completions",
            Some("public-model"),
            false,
            false,
            None,
            &RoundRobinCounters::new(),
        )
        .unwrap();

        assert_eq!(selected.head().backend_id, "responses-backend");
        assert_eq!(
            selected.head().request_mode,
            RequestMode::ChatCompletionsViaResponses
        );
    }

    #[test]
    fn chat_completions_does_not_implicitly_use_messages_backend() {
        let backends = vec![backend_with_supports("messages-backend", &["messages"])];
        let routes = vec![route_for_public(
            "public-model",
            &["chat"],
            &[("messages-private", "messages-backend")],
        )];

        let error = match select_backend_candidates(
            &backends,
            &routes,
            RoutingStrategy::Priority,
            "/v1/chat/completions",
            Some("public-model"),
            false,
            false,
            None,
            &RoundRobinCounters::new(),
        ) {
            Ok(_) => panic!("expected chat completions routing to fail"),
            Err(error) => error,
        };

        assert_eq!(error.code.as_deref(), Some("endpoint_unavailable"));
    }

    #[test]
    fn tool_requests_require_backend_and_route_capability() {
        let backends = vec![
            backend_with_supports("unsupported-backend", &["responses", "chat"]),
            backend_with_supports("supported-backend", &["responses", "chat", "tools"]),
        ];
        let routes = vec![route_for_public(
            "public-model",
            &["responses", "chat", "tools"],
            &[
                ("unsupported-private", "unsupported-backend"),
                ("supported-private", "supported-backend"),
            ],
        )];

        let selected = select_backend_candidates(
            &backends,
            &routes,
            RoutingStrategy::Priority,
            "/v1/responses",
            Some("public-model"),
            false,
            true,
            None,
            &RoundRobinCounters::new(),
        )
        .unwrap();

        assert_eq!(selected.len(), 1);
        assert_eq!(selected.head().backend_id, "supported-backend");
    }

    #[test]
    fn tool_requests_fail_when_only_endpoint_matches() {
        let backends = vec![backend_with_supports(
            "plain-backend",
            &["responses", "chat"],
        )];
        let routes = vec![route_for_public(
            "public-model",
            &["responses", "chat"],
            &[("plain-private", "plain-backend")],
        )];

        let error = match select_backend_candidates(
            &backends,
            &routes,
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
        let routes = default_routes(&backends);
        let counters = RoundRobinCounters::new();
        let selected = select_backend_candidates(
            &backends,
            &routes,
            RoutingStrategy::RoundRobin,
            "/v1/responses",
            Some("public-model"),
            false,
            false,
            None,
            &counters,
        )
        .unwrap();
        assert_eq!(selected.len(), 3);
        let selected_ids: BTreeSet<&str> = selected
            .iter()
            .map(|route| route.backend_id.as_str())
            .collect();
        let original: BTreeSet<&str> = backends.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(selected_ids, original);
    }

    #[test]
    fn round_robin_uses_model_key() {
        let counters = RoundRobinCounters::new();
        let backends_a = vec![backend("a"), backend("b"), backend("c")];
        let routes_for_a = vec![route_for_public(
            "model-a",
            &["responses"],
            &[("a-private", "a"), ("b-private", "b"), ("c-private", "c")],
        )];
        let selected_a = select_backend_candidates(
            &backends_a,
            &routes_for_a,
            RoutingStrategy::RoundRobin,
            "/v1/responses",
            Some("model-a"),
            false,
            false,
            None,
            &counters,
        )
        .unwrap();
        let backends_b = vec![backend("a"), backend("b")];
        let routes_for_b = vec![route_for_public(
            "model-b",
            &["responses"],
            &[("a-private", "a"), ("b-private", "b")],
        )];
        let selected_b = select_backend_candidates(
            &backends_b,
            &routes_for_b,
            RoutingStrategy::RoundRobin,
            "/v1/responses",
            Some("model-b"),
            false,
            false,
            None,
            &counters,
        )
        .unwrap();
        assert_eq!(selected_a.head().backend_id, "a");
        assert_eq!(selected_b.head().backend_id, "a");
        let selected_b_second = select_backend_candidates(
            &backends_b,
            &routes_for_b,
            RoutingStrategy::RoundRobin,
            "/v1/responses",
            Some("model-b"),
            false,
            false,
            None,
            &counters,
        )
        .unwrap();
        assert_eq!(selected_b_second.head().backend_id, "b");
    }

    #[test]
    fn model_less_unavailable_path_uses_endpoint_unavailable_code() {
        let backends = vec![model_less_backend("a", &["embeddings", "streaming"])];
        let error = match select_backend_candidates(
            &backends,
            &[],
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
        let selected_a = select_backend_candidates(
            &backends,
            &[],
            RoutingStrategy::RoundRobin,
            "/v1/embeddings",
            None,
            false,
            false,
            None,
            &counters,
        )
        .unwrap();
        let selected_b = select_backend_candidates(
            &backends,
            &[],
            RoutingStrategy::RoundRobin,
            "/v1/embeddings",
            None,
            false,
            false,
            None,
            &counters,
        )
        .unwrap();
        assert_eq!(selected_a.head().backend_id, "a");
        assert_eq!(selected_b.head().backend_id, "b");
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
        let routes = default_routes(&backends);
        for _ in 0..200 {
            let selected = select_backend_candidates(
                &backends,
                &routes,
                RoutingStrategy::WeightedRandom,
                "/v1/responses",
                Some("public-model"),
                false,
                false,
                None,
                &RoundRobinCounters::new(),
            )
            .unwrap();
            assert!(selected.head().backend_id == "a" || selected.head().backend_id == "c");
        }
    }

    #[test]
    fn weighted_random_single_candidate_noop() {
        let backends = vec![backend_with_weight("only", 5)];
        let routes = default_routes(&backends);
        for _ in 0..10 {
            let selected = select_backend_candidates(
                &backends,
                &routes,
                RoutingStrategy::WeightedRandom,
                "/v1/responses",
                Some("public-model"),
                false,
                false,
                None,
                &RoundRobinCounters::new(),
            )
            .unwrap();
            assert_eq!(selected.head().backend_id, "only");
        }
    }

    #[test]
    fn weighted_random_preserves_fallback_set() {
        let backends = vec![
            backend_with_weight("backend-a", 1),
            backend_with_weight("backend-b", 1),
            backend_with_weight("backend-c", 1),
        ];
        let routes = default_routes(&backends);
        let selected = select_backend_candidates(
            &backends,
            &routes,
            RoutingStrategy::WeightedRandom,
            "/v1/responses",
            Some("public-model"),
            false,
            false,
            None,
            &RoundRobinCounters::new(),
        )
        .unwrap();
        assert_eq!(selected.len(), 3);
        let selected_ids: BTreeSet<&str> = selected
            .iter()
            .map(|route| route.backend_id.as_str())
            .collect();
        let original: BTreeSet<&str> = backends.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(selected_ids, original);
    }

    #[test]
    fn strategy_round_robin_serializes_via_selected_route_weight() {
        let backends = vec![backend_with_weight("a", 7)];
        let routes = default_routes(&backends);
        let selected = select_backend_candidates(
            &backends,
            &routes,
            RoutingStrategy::Priority,
            "/v1/responses",
            Some("public-model"),
            false,
            false,
            None,
            &RoundRobinCounters::new(),
        )
        .unwrap();
        assert_eq!(selected.head().weight, 7);
    }

    fn backend(id: &str) -> ResolvedBackend {
        backend_with_supports(id, &["responses", "streaming"])
    }

    fn backend_with_supports(id: &str, supports: &[&str]) -> ResolvedBackend {
        ResolvedBackend {
            id: id.to_owned(),
            base_url: format!("http://{id}.example.invalid"),
            api_key: None,
            timeout: Duration::from_secs(5),
            supports: btree_set_from(supports),
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            weight: 1,
            extra_body: BTreeMap::new(),
            expose_backend_errors: false,
            stream_capture: false,
        }
    }

    fn btree_set_from(values: &[&str]) -> BTreeSet<String> {
        values.iter().copied().map(str::to_owned).collect()
    }

    fn default_routes(backends: &[ResolvedBackend]) -> Vec<ResolvedRoute> {
        let bindings: Vec<(String, String)> = backends
            .iter()
            .map(|b| (format!("{}-private", b.id), b.id.clone()))
            .collect();
        let bindings_ref: Vec<(&str, &str)> = bindings
            .iter()
            .map(|(model, backend)| (model.as_str(), backend.as_str()))
            .collect();
        vec![route_for_public(
            "public-model",
            &["responses"],
            &bindings_ref,
        )]
    }

    fn route_for_public(public: &str, expose: &[&str], bindings: &[(&str, &str)]) -> ResolvedRoute {
        ResolvedRoute {
            key: RouteKey::Public(public.to_owned()),
            expose: btree_set_from(expose),
            context_length: ContextLengthSpec::None,
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            backends: bindings
                .iter()
                .map(|(model, backend_id)| RouteBackendBinding {
                    backend_id: (*backend_id).to_owned(),
                    backend_model: (*model).to_owned(),
                })
                .collect(),
            extra_body: BTreeMap::new(),
            request_headers: BTreeMap::new(),
            expose_backend_errors: false,
            stream_capture: false,
            anthropic_max_tokens: None,
        }
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
            "messages",
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

    #[test]
    fn messages_path_requires_model() {
        assert!(path_requires_model("/v1/messages"));
        assert!(path_requires_model("/v1/messages/"));
    }

    #[test]
    fn messages_path_metric_name() {
        assert_eq!(path_metric_name("/v1/messages"), "messages");
        assert_eq!(path_metric_name("/v1/messages/"), "messages");
    }

    #[test]
    fn native_messages_route_selects_anthropic_messages_native() {
        let backends = vec![backend_with_supports("anthropic-backend", &["messages"])];
        let routes = vec![route_for_public(
            "public-model",
            &["messages"],
            &[("backend-private", "anthropic-backend")],
        )];

        let selected = select_backend_candidates(
            &backends,
            &routes,
            RoutingStrategy::Priority,
            "/v1/messages",
            Some("public-model"),
            false,
            false,
            None,
            &RoundRobinCounters::new(),
        )
        .unwrap();

        assert_eq!(selected.len(), 1);
        assert_eq!(selected.head().backend_id, "anthropic-backend");
        assert_eq!(
            selected.head().request_mode,
            RequestMode::AnthropicMessagesNative
        );
    }

    #[test]
    fn missing_messages_capability_returns_none() {
        let backends = vec![backend_with_supports("chat-backend", &["chat"])];
        let routes = vec![route_for_public(
            "public-model",
            &["chat"],
            &[("backend-private", "chat-backend")],
        )];

        let error = match select_backend_candidates(
            &backends,
            &routes,
            RoutingStrategy::Priority,
            "/v1/messages",
            Some("public-model"),
            false,
            false,
            None,
            &RoundRobinCounters::new(),
        ) {
            Ok(_) => panic!("expected messages routing to fail"),
            Err(error) => error,
        };

        assert_eq!(error.status, axum::http::StatusCode::NOT_FOUND);
        assert_eq!(error.code.as_deref(), Some("endpoint_unavailable"));
    }

    fn backend_with_weight(id: &str, weight: u32) -> ResolvedBackend {
        ResolvedBackend {
            weight,
            ..backend(id)
        }
    }

    fn model_less_backend(id: &str, capabilities: &[&str]) -> ResolvedBackend {
        backend_with_supports(id, capabilities)
    }
}
