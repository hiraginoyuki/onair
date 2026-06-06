use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::event::{AccessKind, AccessMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use url::Url;

use crate::error::{ConfigError, Error, Result};
use crate::{ContextSizeCache, IpCidr};

const CONFIG_RELOAD_DEBOUNCE: Duration = Duration::from_millis(250);
const CONFIG_RELOAD_RETRY_DELAY: Duration = Duration::from_millis(250);
const CONFIG_RELOAD_MAX_ATTEMPTS: usize = 5;
const MAX_FALLBACK_ATTEMPTS: usize = 16;
const MAX_INSPECTOR_RETENTION_REQUESTS: usize = 1_000_000;
const DEFAULT_INSPECTOR_RETENTION_REQUESTS: usize = 10_000;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    #[serde(default)]
    pub debug_capture: DebugCaptureConfig,
    #[serde(default)]
    pub inspector: InspectorConfig,
    #[serde(default)]
    pub health: HealthConfig,
    #[serde(default)]
    pub routing: RoutingConfig,
    #[serde(default)]
    pub access: AccessConfig,
    #[serde(default, rename = "client")]
    pub clients: Vec<ClientConfig>,
    #[serde(default, rename = "backend")]
    pub backends: Vec<BackendConfig>,
    #[serde(default, rename = "route")]
    pub routes: Vec<RouteConfig>,
}

#[derive(Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub telemetry: TelemetryConfig,
    pub debug_capture: DebugCaptureConfig,
    pub inspector: InspectorConfig,
    pub health: HealthConfig,
    pub routing: RoutingConfig,
    pub clients: Vec<ResolvedClient>,
    pub backends: Vec<ResolvedBackend>,
    pub routes: Vec<ResolvedRoute>,
}

#[derive(Clone)]
pub struct ConfigStore {
    inner: Arc<RwLock<Arc<Config>>>,
}

impl ConfigStore {
    pub fn new(config: Config) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Arc::new(config))),
        }
    }

    pub fn snapshot(&self) -> Arc<Config> {
        self.inner.read().clone()
    }

    pub fn replace(&self, config: Config) -> Arc<Config> {
        let config = Arc::new(config);
        *self.inner.write() = config.clone();
        config
    }
}

pub struct ConfigWatcher {
    _watcher: RecommendedWatcher,
    task: JoinHandle<()>,
}

impl ConfigWatcher {
    pub fn start(path: impl AsRef<Path>, store: ConfigStore) -> Result<Self> {
        let path = absolute_path(path.as_ref())?;
        let directory = path.parent().ok_or_else(|| {
            Error::ConfigWatch(format!("config path '{}' has no parent", path.display()))
        })?;
        let filename = path
            .file_name()
            .ok_or_else(|| {
                Error::ConfigWatch(format!("config path '{}' has no filename", path.display()))
            })?
            .to_owned();

        let callback_path = path.clone();
        let callback_filename = filename.clone();
        let (tx, rx) = mpsc::unbounded_channel::<notify::Result<Event>>();
        let mut watcher = notify::recommended_watcher(move |event| {
            let event = match event {
                Ok(event) if is_reload_event(&event, &callback_path, &callback_filename) => {
                    Ok(event)
                }
                Ok(event) => {
                    debug!(?event, "ignored config watch event");
                    return;
                }
                Err(error) => Err(error),
            };
            if let Err(error) = tx.send(event) {
                warn!(?error, "failed to enqueue config reload event");
            }
        })
        .map_err(|error| Error::ConfigWatch(error.to_string()))?;
        watcher
            .watch(directory, RecursiveMode::NonRecursive)
            .map_err(|error| Error::ConfigWatch(error.to_string()))?;

        let task_path = path.clone();
        let task = tokio::spawn(process_config_watch_events(rx, task_path, filename, store));

        Ok(Self {
            _watcher: watcher,
            task,
        })
    }
}

impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn wait_for_reload_quiet(
    rx: &mut mpsc::UnboundedReceiver<notify::Result<Event>>,
    path: &Path,
    filename: &std::ffi::OsStr,
) -> bool {
    loop {
        tokio::time::sleep(CONFIG_RELOAD_DEBOUNCE).await;
        let mut saw_reload_event = false;

        loop {
            match rx.try_recv() {
                Ok(Ok(event)) if is_reload_event(&event, path, filename) => {
                    saw_reload_event = true;
                }
                Ok(Ok(event)) => {
                    debug!(?event, "ignored config watch event");
                }
                Ok(Err(error)) => {
                    warn!(?error, "config watch event failed");
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return false,
            }
        }

        if !saw_reload_event {
            return true;
        }
    }
}

async fn process_config_watch_events(
    mut rx: mpsc::UnboundedReceiver<notify::Result<Event>>,
    task_path: PathBuf,
    filename: OsString,
    store: ConfigStore,
) {
    info!(path = %task_path.display(), "watching config file");
    while let Some(event) = rx.recv().await {
        match event {
            Ok(event) if is_reload_event(&event, &task_path, &filename) => {
                if !wait_for_reload_quiet(&mut rx, &task_path, &filename).await {
                    return;
                }
                reload_config_with_retries(&task_path, &store).await;
            }
            Ok(event) => {
                debug!(?event, "ignored config watch event");
            }
            Err(error) => {
                warn!(?error, "config watch event failed");
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub request_body_limit_bytes: usize,
    pub trusted_proxy_cidrs: Vec<IpCidr>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080"
                .parse()
                .expect("valid default bind address"),
            request_body_limit_bytes: 2 * 1024 * 1024,
            trusted_proxy_cidrs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TelemetryConfig {
    pub service_name: String,
    pub exporter: TelemetryExporter,
    pub otlp_endpoint: Option<String>,
    pub export_interval_ms: u64,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            service_name: "onair".to_owned(),
            exporter: TelemetryExporter::None,
            otlp_endpoint: None,
            export_interval_ms: 30_000,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryExporter {
    #[default]
    None,
    Otlp,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DebugCaptureConfig {
    pub enabled: bool,
    pub mode: DebugCaptureMode,
    pub directory: PathBuf,
}

impl Default for DebugCaptureConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: DebugCaptureMode::All,
            directory: PathBuf::from("onair-debug-captures"),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DebugCaptureMode {
    #[default]
    All,
    Failures,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InspectorConfig {
    pub enabled: bool,
    pub retention_requests: usize,
    pub allow_remote: bool,
    pub persistence: InspectorPersistenceConfig,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct InspectorPersistenceConfig {
    pub enabled: bool,
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HealthConfig {
    pub active: bool,
    pub interval_ms: u64,
    pub timeout_ms: u64,
    pub path: String,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            active: false,
            interval_ms: 30_000,
            timeout_ms: 2_000,
            path: "/v1/models".to_owned(),
        }
    }
}

impl Default for InspectorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            retention_requests: DEFAULT_INSPECTOR_RETENTION_REQUESTS,
            allow_remote: false,
            persistence: InspectorPersistenceConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingConfig {
    pub strategy: RoutingStrategy,
    pub fallback_attempts: usize,
    pub unknown_capability_policy: UnknownMarkerPolicy,
    pub unknown_endpoint_policy: UnknownMarkerPolicy,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            strategy: RoutingStrategy::Priority,
            fallback_attempts: 1,
            unknown_capability_policy: UnknownMarkerPolicy::Warn,
            unknown_endpoint_policy: UnknownMarkerPolicy::Warn,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoutingStrategy {
    #[default]
    Priority,
    Sticky,
    RoundRobin,
    WeightedRandom,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UnknownMarkerPolicy {
    #[default]
    Warn,
    Error,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccessConfig {
    pub default_models: Vec<String>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    pub id: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
}

#[derive(Clone)]
pub struct ResolvedClient {
    pub id: String,
    pub api_key: String,
    pub models: BTreeSet<String>,
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BackendConfig {
    pub id: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub timeout_ms: u64,
    pub tool_schema_mode: ToolSchemaMode,
    pub responses_store: ResponsesStorePolicy,
    pub responses_max_output_tokens: ResponsesMaxOutputTokensPolicy,
    pub chat_stream_usage: ChatStreamUsagePolicy,
    pub supports: BTreeSet<String>,
    #[serde(default = "one_u32")]
    pub weight: u32,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            base_url: String::new(),
            api_key: None,
            api_key_env: None,
            timeout_ms: 120_000,
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            supports: BTreeSet::new(),
            weight: 1,
        }
    }
}

fn one_u32() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolSchemaMode {
    #[default]
    Preserve,
    LlamacppCompat,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResponsesStorePolicy {
    #[default]
    Preserve,
    ForceFalse,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResponsesMaxOutputTokensPolicy {
    #[default]
    Preserve,
    Drop,
    RenameToMaxTokens,
    RenameToMaxCompletionTokens,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChatStreamUsagePolicy {
    #[default]
    Preserve,
    Insert,
    ForceTrue,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    pub public: Option<String>,
    pub path: Option<String>,
    #[serde(default)]
    pub expose: BTreeSet<String>,
    #[serde(default)]
    pub backends: Vec<String>,
    #[serde(default)]
    pub context_length: ContextLengthConfig,
    #[serde(default)]
    pub tool_schema_mode: Option<ToolSchemaMode>,
    #[serde(default)]
    pub responses_store: Option<ResponsesStorePolicy>,
    #[serde(default)]
    pub responses_max_output_tokens: Option<ResponsesMaxOutputTokensPolicy>,
    #[serde(default)]
    pub chat_stream_usage: Option<ChatStreamUsagePolicy>,
}

impl RouteConfig {
    pub fn route_key(&self) -> Option<RouteKey> {
        match (self.public.as_deref(), self.path.as_deref()) {
            (Some(public), None) => Some(RouteKey::Public(public.to_owned())),
            (None, Some(path)) => Some(RouteKey::Path(path.to_owned())),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RouteKey {
    Public(String),
    Path(String),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ContextLengthConfig {
    Value(u64),
    Mode(ContextLengthMode),
}

impl Default for ContextLengthConfig {
    fn default() -> Self {
        Self::Mode(ContextLengthMode::None)
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextLengthMode {
    None,
    Upstream,
}

#[derive(Debug, Clone)]
pub enum ContextLengthPolicy {
    None,
    Static(u64),
    Upstream {
        backend_id: String,
        backend_model: String,
    },
}

/// Resolved form of [`ContextLengthPolicy`], carrying the live
/// upstream `n_ctx` once it has been observed.
#[derive(Debug, Clone)]
pub enum ResolvedContextLength {
    None,
    Static { n_ctx: u64 },
    Upstream { n_ctx: Option<u64> },
}

/// The collapsed single-enum target for the B5 cleanup.
///
/// Carries both the routing info from [`ContextLengthPolicy`] and the
/// live `n_ctx` from [`ResolvedContextLength`] on the upstream
/// variant. The onair-proxy and onair match sites need to be
/// migrated to this shape before the old enums can be removed; in
/// the meantime, [`ContextLengthPolicy`] and
/// [`ResolvedContextLength`] remain the public types.
#[derive(Debug, Clone)]
pub enum ContextLengthSpec {
    None,
    Static { n_ctx: u64 },
    Upstream {
        backend_id: String,
        backend_model: String,
        n_ctx: Option<u64>,
    },
}

impl From<&ContextLengthPolicy> for ContextLengthSpec {
    fn from(policy: &ContextLengthPolicy) -> Self {
        match policy {
            ContextLengthPolicy::None => Self::None,
            ContextLengthPolicy::Static(value) => Self::Static { n_ctx: *value },
            ContextLengthPolicy::Upstream {
                backend_id,
                backend_model,
            } => Self::Upstream {
                backend_id: backend_id.clone(),
                backend_model: backend_model.clone(),
                n_ctx: None,
            },
        }
    }
}

#[derive(Clone)]
pub struct ResolvedBackend {
    pub id: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub timeout: Duration,
    pub supports: BTreeSet<String>,
    pub tool_schema_mode: ToolSchemaMode,
    pub responses_store: ResponsesStorePolicy,
    pub responses_max_output_tokens: ResponsesMaxOutputTokensPolicy,
    pub chat_stream_usage: ChatStreamUsagePolicy,
    pub weight: u32,
}

#[derive(Debug, Clone)]
pub struct RouteBackendBinding {
    pub backend_id: String,
    pub backend_model: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedRoute {
    pub key: RouteKey,
    pub expose: BTreeSet<String>,
    pub context_length: ContextLengthPolicy,
    pub tool_schema_mode: ToolSchemaMode,
    pub responses_store: ResponsesStorePolicy,
    pub responses_max_output_tokens: ResponsesMaxOutputTokensPolicy,
    pub chat_stream_usage: ChatStreamUsagePolicy,
    pub backends: Vec<RouteBackendBinding>,
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let path_display = path.display().to_string();
        let raw = std::fs::read_to_string(path).map_err(|source| Error::ConfigRead {
            path: path_display.clone(),
            source,
        })?;
        let file: ConfigFile = toml::from_str(&raw).map_err(|source| Error::ConfigParse {
            path: path_display,
            source,
        })?;
        Self::resolve(file)
    }

    fn resolve(file: ConfigFile) -> Result<Self> {
        validate_top_level(&file)?;

        let clients = resolve_clients(file.clients, &file.access)?;
        let backends = resolve_backends(file.backends, file.routing.unknown_capability_policy)?;
        let routes = resolve_routes(&file.routes, &backends, &file.routing)?;

        validate_allowed_routes(&clients, &routes)?;

        Ok(Self {
            server: file.server,
            telemetry: file.telemetry,
            debug_capture: file.debug_capture,
            inspector: file.inspector,
            health: file.health,
            routing: file.routing,
            clients,
            backends,
            routes,
        })
    }

    pub fn public_model_context_lengths(&self) -> BTreeMap<String, ResolvedContextLength> {
        let mut models = BTreeMap::new();
        for route in &self.routes {
            if let RouteKey::Public(public) = &route.key {
                let resolved = match &route.context_length {
                    ContextLengthPolicy::None => ResolvedContextLength::None,
                    ContextLengthPolicy::Static(value) => {
                        ResolvedContextLength::Static { n_ctx: *value }
                    }
                    ContextLengthPolicy::Upstream { .. } => {
                        ResolvedContextLength::Upstream { n_ctx: None }
                    }
                };
                models.entry(public.clone()).or_insert(resolved);
            }
        }
        models
    }

    pub fn public_model_context_lengths_with_cache(
        &self,
        cache: &ContextSizeCache,
    ) -> BTreeMap<String, ResolvedContextLength> {
        let mut models = BTreeMap::new();
        for route in &self.routes {
            if let RouteKey::Public(public) = &route.key {
                let resolved = match &route.context_length {
                    ContextLengthPolicy::None => ResolvedContextLength::None,
                    ContextLengthPolicy::Static(value) => {
                        ResolvedContextLength::Static { n_ctx: *value }
                    }
                    ContextLengthPolicy::Upstream { .. } => ResolvedContextLength::Upstream {
                        n_ctx: cache.lookup(public),
                    },
                };
                models.entry(public.clone()).or_insert(resolved);
            }
        }
        models
    }
}

fn resolve_routes(
    route_configs: &[RouteConfig],
    backends: &[ResolvedBackend],
    routing_config: &RoutingConfig,
) -> Result<Vec<ResolvedRoute>> {
    let validated = validate_routes(route_configs, routing_config)?;
    let mut resolved_routes = Vec::with_capacity(validated.len());
    for (key, route_config) in validated {
        let bindings = bind_routes(&key, &route_config.backends, backends)?;
        warn_about_unreachable_backends(&key, &route_config.expose, &bindings, backends);
        let tool_schema_mode = route_config
            .tool_schema_mode
            .unwrap_or(ToolSchemaMode::Preserve);
        let responses_store = route_config
            .responses_store
            .unwrap_or(ResponsesStorePolicy::Preserve);
        let responses_max_output_tokens = route_config
            .responses_max_output_tokens
            .unwrap_or(ResponsesMaxOutputTokensPolicy::Preserve);
        let chat_stream_usage = route_config
            .chat_stream_usage
            .unwrap_or(ChatStreamUsagePolicy::Preserve);
        let expose = route_config.expose.clone();
        let context_length =
            resolve_context_lengths(&key, &route_config.context_length, &bindings)?;
        resolved_routes.push(ResolvedRoute {
            key: key.clone(),
            expose,
            context_length,
            tool_schema_mode,
            responses_store,
            responses_max_output_tokens,
            chat_stream_usage,
            backends: bindings,
        });
    }
    Ok(resolved_routes)
}

fn validate_routes(
    route_configs: &[RouteConfig],
    routing_config: &RoutingConfig,
) -> Result<Vec<(RouteKey, RouteConfig)>> {
    let mut seen_keys: std::collections::HashSet<RouteKey> = std::collections::HashSet::new();
    let mut validated = Vec::with_capacity(route_configs.len());
    for route_config in route_configs {
        let key = route_config.route_key().ok_or_else(|| {
            Error::Config(ConfigError::Message(format!(
                "each [[route]] must declare exactly one of `public` or `path`; got public={:?} path={:?}",
                route_config.public, route_config.path
            )))
        })?;
        if !seen_keys.insert(key.clone()) {
            return Err(Error::Config(ConfigError::Message(format!(
                "duplicate route declaration for '{}'",
                format_route_key(&key)
            ))));
        }
        validate_markers(
            &route_config.expose,
            routing_config.unknown_endpoint_policy,
            MarkerKind::Endpoint,
            &format!("route '{}'", format_route_key(&key)),
        )?;
        validated.push((key, route_config.clone()));
    }
    Ok(validated)
}

fn bind_routes(
    key: &RouteKey,
    entries: &[String],
    backends: &[ResolvedBackend],
) -> Result<Vec<RouteBackendBinding>> {
    let mut bindings = Vec::with_capacity(entries.len());
    for entry in entries {
        let (model, backend_id) = parse_route_backend(entry)?;
        let backend = backends
            .iter()
            .find(|b| b.id == backend_id)
            .ok_or_else(|| {
                Error::Config(ConfigError::Message(format!(
                    "route '{}' references unknown backend '{}'",
                    format_route_key(key),
                    backend_id
                )))
            })?;
        let backend_model = model.clone().unwrap_or_else(|| match key {
            RouteKey::Public(public) => public.clone(),
            RouteKey::Path(_) => backend_id.clone(),
        });
        bindings.push(RouteBackendBinding {
            backend_id: backend.id.clone(),
            backend_model,
        });
    }
    Ok(bindings)
}

fn warn_about_unreachable_backends(
    key: &RouteKey,
    expose: &BTreeSet<String>,
    bindings: &[RouteBackendBinding],
    backends: &[ResolvedBackend],
) {
    for binding in bindings {
        // The unknown-backend check inside bind_routes already rejected
        // any binding whose backend_id is not in `backends`, so this
        // lookup is guaranteed to succeed; if it ever fails we log and
        // bail out so a misconfigured hot reload does not crash the
        // whole server.
        let Some(backend) = backends.iter().find(|b| b.id == binding.backend_id) else {
            debug_assert!(
                false,
                "binding references a known backend (validator above)"
            );
            return;
        };
        if !route_can_serve(expose, backend) {
            let route_label = format_route_key(key);
            let backend_supports = sorted_marker_list(&backend.supports);
            let route_exposes = sorted_marker_list(expose);
            warn!(
                route = %route_label,
                backend = %backend.id,
                route_expose = %route_exposes,
                backend_supports = %backend_supports,
                "route backend cannot serve any of the route's expose markers; it will never be selected",
            );
        }
    }
}

fn resolve_context_lengths(
    key: &RouteKey,
    config: &ContextLengthConfig,
    bindings: &[RouteBackendBinding],
) -> Result<ContextLengthPolicy> {
    match key {
        RouteKey::Path(_) => Ok(ContextLengthPolicy::None),
        RouteKey::Public(public) => {
            let first = bindings.first();
            let backend_id = first.map(|b| b.backend_id.as_str()).unwrap_or("");
            let model_for_lookup = first
                .map(|b| b.backend_model.clone())
                .unwrap_or_else(|| public.clone());
            resolve_context_length_policy(config, backend_id, &model_for_lookup)
        }
    }
}

fn format_route_key(key: &RouteKey) -> String {
    match key {
        RouteKey::Public(p) => format!("public={p}"),
        RouteKey::Path(p) => format!("path={p}"),
    }
}

fn sorted_marker_list(set: &BTreeSet<String>) -> String {
    let mut v: Vec<&str> = set.iter().map(String::as_str).collect();
    v.sort_unstable();
    v.join(",")
}

fn route_can_serve(expose: &BTreeSet<String>, backend: &ResolvedBackend) -> bool {
    for marker in expose {
        if backend.supports.contains(marker) {
            return true;
        }
        if is_compat_marker_pair(marker, &backend.supports) {
            return true;
        }
    }
    false
}

fn is_compat_marker_pair(marker: &str, backend_supports: &BTreeSet<String>) -> bool {
    if marker == crate::compat::RESPONSES_VIA_CHAT_COMPLETIONS {
        return has_marker(backend_supports, "chat")
            || has_marker(backend_supports, "chat_completions")
            || has_marker(backend_supports, "completions");
    }
    if marker == crate::compat::CHAT_COMPLETIONS_VIA_RESPONSES {
        return has_marker(backend_supports, "responses")
            || has_marker(backend_supports, "response");
    }
    false
}

fn has_marker(set: &BTreeSet<String>, marker: &str) -> bool {
    set.contains(marker) || set.contains("all")
}

fn parse_route_backend(entry: &str) -> Result<(Option<String>, String)> {
    if let Some(at_pos) = entry.find('@') {
        let model = entry[..at_pos].to_owned();
        let backend_id = entry[at_pos + 1..].to_owned();
        if backend_id.is_empty() {
            return Err(Error::Config(ConfigError::Message(format!(
                "route backend entry '{entry}' is missing the backend id after '@'"
            ))));
        }
        if model.is_empty() {
            return Err(Error::Config(ConfigError::Message(format!(
                "route backend entry '{entry}' uses '@' but the model part is empty; use a bare backend id for model-less routes"
            ))));
        }
        Ok((Some(model), backend_id))
    } else {
        Ok((None, entry.to_owned()))
    }
}

fn validate_top_level(file: &ConfigFile) -> Result<()> {
    validate_debug_capture_config(&file.debug_capture)?;
    validate_inspector_config(&file.inspector)?;
    validate_health_config(&file.health)?;
    validate_routing_config(&file.routing)?;
    Ok(())
}

fn resolve_clients(
    raw_clients: Vec<ClientConfig>,
    access: &AccessConfig,
) -> Result<Vec<ResolvedClient>> {
    let mut client_ids = BTreeSet::new();
    let mut clients = Vec::with_capacity(raw_clients.len());
    for client in raw_clients {
        if client.id.trim().is_empty() {
            return Err(Error::Config(ConfigError::Message(
                "client id must not be empty".to_owned(),
            )));
        }
        if !client_ids.insert(client.id.clone()) {
            return Err(Error::Config(ConfigError::Message(format!(
                "duplicate client id '{}'",
                client.id
            ))));
        }
        let api_key = resolve_secret(
            client.api_key,
            client.api_key_env,
            &format!("client '{}' api key", client.id),
        )?;
        let mut models = BTreeSet::new();
        models.extend(access.default_models.iter().cloned());
        models.extend(client.models);
        clients.push(ResolvedClient {
            id: client.id,
            api_key,
            models,
        });
    }

    if clients.is_empty() {
        return Err(Error::Config(ConfigError::Message(
            "at least one [[client]] is required".to_owned(),
        )));
    }

    Ok(clients)
}

fn resolve_backends(
    raw_backends: Vec<BackendConfig>,
    unknown_capability_policy: UnknownMarkerPolicy,
) -> Result<Vec<ResolvedBackend>> {
    let mut backend_ids = BTreeSet::new();
    let mut backends = Vec::with_capacity(raw_backends.len());
    for backend in raw_backends {
        if backend.id.trim().is_empty() {
            return Err(Error::Config(ConfigError::Message(
                "backend id must not be empty".to_owned(),
            )));
        }
        if !backend_ids.insert(backend.id.clone()) {
            return Err(Error::Config(ConfigError::Message(format!(
                "duplicate backend id '{}'",
                backend.id
            ))));
        }
        if backend.weight == 0 {
            return Err(Error::Config(ConfigError::Message(format!(
                "backend '{}' weight must be greater than zero",
                backend.id
            ))));
        }
        let base_url = normalize_backend_base_url(&backend.base_url, &backend.id)?;
        let api_key = resolve_optional_secret(backend.api_key, backend.api_key_env)?;
        validate_markers(
            &backend.supports,
            unknown_capability_policy,
            MarkerKind::Capability,
            &format!("backend '{}'", backend.id),
        )?;
        backends.push(ResolvedBackend {
            id: backend.id,
            base_url,
            api_key,
            timeout: Duration::from_millis(backend.timeout_ms),
            supports: backend.supports,
            tool_schema_mode: backend.tool_schema_mode,
            responses_store: backend.responses_store,
            responses_max_output_tokens: backend.responses_max_output_tokens,
            chat_stream_usage: backend.chat_stream_usage,
            weight: backend.weight,
        });
    }

    if backends.is_empty() {
        return Err(Error::Config(ConfigError::Message(
            "at least one [[backend]] is required".to_owned(),
        )));
    }

    Ok(backends)
}

fn validate_allowed_routes(clients: &[ResolvedClient], routes: &[ResolvedRoute]) -> Result<()> {
    for client in clients {
        for model in &client.models {
            let found = routes.iter().any(|route| match &route.key {
                RouteKey::Public(public) => public == model,
                RouteKey::Path(_) => false,
            });
            if !found {
                return Err(Error::Config(ConfigError::Message(format!(
                    "client '{}' references public model '{model}' which has no [[route]] declaration; add a [[route]] block or remove the model from the client",
                    client.id
                ))));
            }
        }
    }
    Ok(())
}

fn resolve_secret(
    api_key: Option<String>,
    api_key_env: Option<String>,
    label: &str,
) -> Result<String> {
    match (api_key, api_key_env) {
        (Some(_), Some(_)) => Err(Error::Config(ConfigError::Message(format!(
            "{label} must use api_key or api_key_env, not both"
        )))),
        (Some(value), None) if !value.trim().is_empty() => Ok(value),
        (None, Some(name)) if !name.trim().is_empty() => {
            env::var(&name).map_err(|_| Error::MissingEnv(name))
        }
        _ => Err(Error::Config(ConfigError::Message(format!(
            "{label} is required"
        )))),
    }
}

fn resolve_optional_secret(
    api_key: Option<String>,
    api_key_env: Option<String>,
) -> Result<Option<String>> {
    match (api_key, api_key_env) {
        (Some(_), Some(_)) => Err(Error::Config(ConfigError::Message(
            "backend must use api_key or api_key_env, not both".to_owned(),
        ))),
        (Some(value), None) if !value.trim().is_empty() => Ok(Some(value)),
        (None, Some(name)) if !name.trim().is_empty() => env::var(&name)
            .map(Some)
            .map_err(|_| Error::MissingEnv(name)),
        _ => Ok(None),
    }
}

fn validate_health_config(config: &HealthConfig) -> Result<()> {
    if config.interval_ms == 0 {
        return Err(Error::Config(ConfigError::Message(
            "health.interval_ms must be greater than zero".to_owned(),
        )));
    }
    if config.timeout_ms == 0 {
        return Err(Error::Config(ConfigError::Message(
            "health.timeout_ms must be greater than zero".to_owned(),
        )));
    }
    if !config.path.starts_with('/') {
        return Err(Error::Config(ConfigError::Message(
            "health.path must start with '/' and be relative to backend base_url".to_owned(),
        )));
    }
    if config.path.starts_with("//") || config.path.contains("://") {
        return Err(Error::Config(ConfigError::Message(
            "health.path must be a relative path, not an absolute URL".to_owned(),
        )));
    }
    if config.path.chars().any(char::is_control) {
        return Err(Error::Config(ConfigError::Message(
            "health.path must not contain control characters".to_owned(),
        )));
    }
    Ok(())
}

fn validate_inspector_config(config: &InspectorConfig) -> Result<()> {
    if config.retention_requests == 0 {
        return Err(Error::Config(ConfigError::Message(
            "inspector.retention_requests must be greater than zero".to_owned(),
        )));
    }
    if config.retention_requests > MAX_INSPECTOR_RETENTION_REQUESTS {
        return Err(Error::Config(ConfigError::Message(format!(
            "inspector.retention_requests must be at most {MAX_INSPECTOR_RETENTION_REQUESTS}"
        ))));
    }
    if config.persistence.enabled {
        let Some(path) = config.persistence.path.as_ref() else {
            return Err(Error::Config(ConfigError::Message(
                "inspector.persistence.path is required when persistence is enabled".to_owned(),
            )));
        };
        if path.as_os_str().is_empty() {
            return Err(Error::Config(ConfigError::Message(
                "inspector.persistence.path must not be empty".to_owned(),
            )));
        }
    }
    Ok(())
}

fn validate_debug_capture_config(config: &DebugCaptureConfig) -> Result<()> {
    use std::path::Component;

    if !config.enabled {
        return Ok(());
    }

    if config.directory.as_os_str().is_empty() {
        return Err(Error::Config(ConfigError::Message(
            "debug_capture.directory must not be empty when debug capture is enabled".to_owned(),
        )));
    }

    if config
        .directory
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(Error::Config(ConfigError::Message(
            "debug_capture.directory must not contain '..' components".to_owned(),
        )));
    }

    if !config
        .directory
        .components()
        .any(|component| matches!(component, Component::Normal(_)))
    {
        return Err(Error::Config(ConfigError::Message(
            "debug_capture.directory must include a directory name".to_owned(),
        )));
    }

    Ok(())
}

fn validate_routing_config(config: &RoutingConfig) -> Result<()> {
    if config.fallback_attempts > MAX_FALLBACK_ATTEMPTS {
        return Err(Error::Config(ConfigError::Message(format!(
            "routing.fallback_attempts must be at most {MAX_FALLBACK_ATTEMPTS}"
        ))));
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum MarkerKind {
    Capability,
    Endpoint,
}

impl MarkerKind {
    fn as_str(self) -> &'static str {
        match self {
            MarkerKind::Capability => "capability",
            MarkerKind::Endpoint => "endpoint",
        }
    }
}

fn validate_markers(
    values: &BTreeSet<String>,
    policy: UnknownMarkerPolicy,
    kind: MarkerKind,
    location: &str,
) -> Result<()> {
    for value in values {
        if crate::is_known_marker(value) {
            continue;
        }
        match policy {
            UnknownMarkerPolicy::Warn => {
                warn!(
                    marker_kind = kind.as_str(),
                    unknown_marker = value.as_str(),
                    location,
                    "unknown marker; not recognized by the router",
                );
            }
            UnknownMarkerPolicy::Error => {
                return Err(Error::Config(ConfigError::Message(format!(
                    "{location} {} '{value}' is not a recognized marker; allowed: {}",
                    kind.as_str(),
                    crate::KNOWN_MARKERS.join(", "),
                ))));
            }
        }
    }
    Ok(())
}

fn resolve_context_length_policy(
    config: &ContextLengthConfig,
    backend_id: &str,
    backend_model: &str,
) -> Result<ContextLengthPolicy> {
    match config {
        ContextLengthConfig::Value(value) => Ok(ContextLengthPolicy::Static(*value)),
        ContextLengthConfig::Mode(ContextLengthMode::None) => Ok(ContextLengthPolicy::None),
        ContextLengthConfig::Mode(ContextLengthMode::Upstream) => {
            Ok(ContextLengthPolicy::Upstream {
                backend_id: backend_id.to_owned(),
                backend_model: backend_model.to_owned(),
            })
        }
    }
}

fn normalize_backend_base_url(base_url: &str, backend_id: &str) -> Result<String> {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(Error::Config(ConfigError::Message(format!(
            "backend '{backend_id}' base_url must not be empty"
        ))));
    }

    let parsed = Url::parse(trimmed).map_err(|source| {
        Error::Config(ConfigError::Message(format!(
            "backend '{backend_id}' base_url is invalid: {source}"
        )))
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(Error::Config(ConfigError::Message(format!(
            "backend '{backend_id}' base_url must use http or https"
        ))));
    }
    if parsed.host_str().is_none() {
        return Err(Error::Config(ConfigError::Message(format!(
            "backend '{backend_id}' base_url must include a host"
        ))));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(Error::Config(ConfigError::Message(format!(
            "backend '{backend_id}' base_url must not contain credentials"
        ))));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(Error::Config(ConfigError::Message(format!(
            "backend '{backend_id}' base_url must not contain a query string or fragment"
        ))));
    }

    Ok(trimmed.to_owned())
}

#[cfg(test)]
fn reload_config(path: &Path, store: &ConfigStore) {
    if let Err(error) = try_reload_config(path, store) {
        warn!(?error, "config reload failed; keeping previous config");
    }
}

async fn reload_config_with_retries(path: &Path, store: &ConfigStore) {
    for attempt in 1..=CONFIG_RELOAD_MAX_ATTEMPTS {
        match try_reload_config(path, store) {
            Ok(()) => return,
            Err(error) if attempt == CONFIG_RELOAD_MAX_ATTEMPTS => {
                warn!(?error, "config reload failed; keeping previous config");
            }
            Err(error) => {
                debug!(
                    ?error,
                    attempt,
                    max_attempts = CONFIG_RELOAD_MAX_ATTEMPTS,
                    "config reload attempt failed; retrying"
                );
                tokio::time::sleep(CONFIG_RELOAD_RETRY_DELAY).await;
            }
        }
    }
}

fn try_reload_config(path: &Path, store: &ConfigStore) -> Result<()> {
    let config = Config::load(path)?;
    apply_reloaded_config(config, store);
    Ok(())
}

fn apply_reloaded_config(config: Config, store: &ConfigStore) {
    let previous = store.snapshot();
    let next = store.replace(config);
    if previous.server.bind != next.server.bind {
        warn!(
            old = %previous.server.bind,
            new = %next.server.bind,
            "config reload changed server.bind; listener address is only applied on restart"
        );
    }
    if previous.server.request_body_limit_bytes != next.server.request_body_limit_bytes {
        warn!(
            old = previous.server.request_body_limit_bytes,
            new = next.server.request_body_limit_bytes,
            "config reload changed server.request_body_limit_bytes; body limit is only applied on restart"
        );
    }
    if previous.telemetry.exporter != next.telemetry.exporter
        || previous.telemetry.otlp_endpoint != next.telemetry.otlp_endpoint
        || previous.telemetry.service_name != next.telemetry.service_name
        || previous.telemetry.export_interval_ms != next.telemetry.export_interval_ms
    {
        warn!(
            "config reload changed telemetry settings; telemetry exporter is only applied on restart"
        );
    }
    if previous.inspector.persistence != next.inspector.persistence {
        warn!(
            "config reload changed inspector persistence settings; persistence settings are only applied on restart"
        );
    }
    info!(
        clients = next.clients.len(),
        backends = next.backends.len(),
        "config reloaded"
    );
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

fn is_reload_event(event: &Event, path: &Path, filename: &std::ffi::OsStr) -> bool {
    if !matches!(
        event.kind,
        EventKind::Any
            | EventKind::Create(_)
            | EventKind::Modify(_)
            | EventKind::Remove(_)
            | EventKind::Access(AccessKind::Close(AccessMode::Write | AccessMode::Any))
    ) {
        return false;
    }

    event.paths.is_empty()
        || event.paths.iter().any(|event_path| {
            event_path == path
                || event_path
                    .file_name()
                    .is_some_and(|event_filename| event_filename == filename)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_store_lock_survives_panic_in_holder() {
        use std::io::Write;

        let path = env::temp_dir().join(format!(
            "onair-config-store-lock-test-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(
            br#"
            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["responses"]

            [[route]]
            public = "public-model"
            expose = ["responses"]
            backends = ["private-model@backend-a"]
            "#,
        )
        .unwrap();
        let store = ConfigStore::new(Config::load(&path).unwrap());
        let _ = std::fs::remove_file(&path);

        let writer = store.clone();
        let _ = std::thread::spawn(move || {
            // parking_lot guards are unwind-safe by design; if a task
            // that held the read/write lock panicked, subsequent
            // accessors must still succeed.
            let _guard = writer.inner.write();
            panic!("simulated panic in lock holder");
        })
        .join();

        let snapshot = store.snapshot();
        assert!(!snapshot.clients.is_empty());
    }

    #[test]
    fn context_length_policies_resolve_from_config() {
        let config = parse_config(
            r#"
            [access]
            default_models = ["public-upstream", "public-specific", "public-none"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["responses"]

            [[route]]
            public = "public-upstream"
            expose = ["responses"]
            backends = ["private-upstream@backend-a"]
            context_length = "upstream"

            [[route]]
            public = "public-specific"
            expose = ["responses"]
            backends = ["private-specific@backend-a"]
            context_length = 8192

            [[route]]
            public = "public-none"
            expose = ["responses"]
            backends = ["private-none@backend-a"]
            context_length = "none"
            "#,
        );

        let cache = ContextSizeCache::new();
        let models = config.public_model_context_lengths_with_cache(&cache);
        match models.get("public-upstream").unwrap() {
            ResolvedContextLength::Upstream { n_ctx } => assert_eq!(*n_ctx, None),
            other => panic!("expected Upstream, got {other:?}"),
        }
        match models.get("public-specific").unwrap() {
            ResolvedContextLength::Static { n_ctx } => {
                assert_eq!(*n_ctx, 8_192);
            }
            other => panic!("expected Static, got {other:?}"),
        }
        match models.get("public-none").unwrap() {
            ResolvedContextLength::None => {}
            other => panic!("expected None, got {other:?}"),
        }
    }

    #[test]
    fn upstream_context_length_resolves_without_backend_default() {
        let config = parse_config(
            r#"
            [access]
            default_models = ["public-upstream"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["responses"]

            [[route]]
            public = "public-upstream"
            expose = ["responses"]
            backends = ["private-upstream@backend-a"]
            context_length = "upstream"
            "#,
        );

        let route = &config.routes[0];
        match &route.context_length {
            ContextLengthPolicy::Upstream {
                backend_id,
                backend_model,
            } => {
                assert_eq!(backend_id, "backend-a");
                assert_eq!(backend_model, "private-upstream");
            }
            other => panic!("expected Upstream policy, got {other:?}"),
        }
    }

    #[test]
    fn backend_context_length_field_is_rejected() {
        let result: std::result::Result<ConfigFile, _> = toml::from_str(
            r#"
            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            context_length = 131072
            supports = ["responses"]

            [[route]]
            public = "public-model"
            expose = ["responses"]
            backends = ["private-model@backend-a"]
            "#,
        );
        let error = match result {
            Ok(_) => panic!("expected backend context_length to be rejected"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("context_length"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn tool_schema_mode_can_be_set_per_backend_and_route() {
        let config = parse_config(
            r#"
            [access]
            default_models = ["public-default", "public-override"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["chat", "tools"]
            tool_schema_mode = "llamacpp_compat"

            [[route]]
            public = "public-default"
            expose = ["chat", "tools"]
            backends = ["private-default@backend-a"]

            [[route]]
            public = "public-override"
            expose = ["chat", "tools"]
            backends = ["private-override@backend-a"]
            tool_schema_mode = "llamacpp_compat"
            "#,
        );

        let backend = &config.backends[0];
        assert_eq!(backend.tool_schema_mode, ToolSchemaMode::LlamacppCompat);
        let default_route = route_by_public(&config, "public-default");
        let override_route = route_by_public(&config, "public-override");
        assert_eq!(default_route.tool_schema_mode, ToolSchemaMode::Preserve);
        assert_eq!(
            override_route.tool_schema_mode,
            ToolSchemaMode::LlamacppCompat
        );
    }

    #[test]
    fn responses_store_policy_can_be_set_per_backend_and_route() {
        let config = parse_config(
            r#"
            [access]
            default_models = ["public-default", "public-override"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["responses"]
            responses_store = "force_false"

            [[route]]
            public = "public-default"
            expose = ["responses"]
            backends = ["private-default@backend-a"]

            [[route]]
            public = "public-override"
            expose = ["responses"]
            backends = ["private-override@backend-a"]
            responses_store = "force_false"
            "#,
        );

        let backend = &config.backends[0];
        assert_eq!(backend.responses_store, ResponsesStorePolicy::ForceFalse);
        let default_route = route_by_public(&config, "public-default");
        let override_route = route_by_public(&config, "public-override");
        assert_eq!(
            default_route.responses_store,
            ResponsesStorePolicy::Preserve
        );
        assert_eq!(
            override_route.responses_store,
            ResponsesStorePolicy::ForceFalse
        );
    }

    #[test]
    fn responses_max_output_tokens_policy_can_be_set_per_backend_and_route() {
        let config = parse_config(
            r#"
            [access]
            default_models = ["public-default", "public-override"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["responses"]
            responses_max_output_tokens = "rename_to_max_tokens"

            [[route]]
            public = "public-default"
            expose = ["responses"]
            backends = ["private-default@backend-a"]

            [[route]]
            public = "public-override"
            expose = ["responses"]
            backends = ["private-override@backend-a"]
            responses_max_output_tokens = "drop"
            "#,
        );

        let backend = &config.backends[0];
        assert_eq!(
            backend.responses_max_output_tokens,
            ResponsesMaxOutputTokensPolicy::RenameToMaxTokens
        );
        let default_route = route_by_public(&config, "public-default");
        let override_route = route_by_public(&config, "public-override");
        assert_eq!(
            default_route.responses_max_output_tokens,
            ResponsesMaxOutputTokensPolicy::Preserve
        );
        assert_eq!(
            override_route.responses_max_output_tokens,
            ResponsesMaxOutputTokensPolicy::Drop
        );
    }

    #[test]
    fn chat_stream_usage_policy_can_be_set_per_backend_and_route() {
        let config = parse_config(
            r#"
            [access]
            default_models = ["public-default", "public-override"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["chat", "streaming"]
            chat_stream_usage = "force_true"

            [[route]]
            public = "public-default"
            expose = ["chat"]
            backends = ["private-default@backend-a"]

            [[route]]
            public = "public-override"
            expose = ["chat"]
            backends = ["private-override@backend-a"]
            chat_stream_usage = "insert"
            "#,
        );

        let backend = &config.backends[0];
        assert_eq!(backend.chat_stream_usage, ChatStreamUsagePolicy::ForceTrue);
        let default_route = route_by_public(&config, "public-default");
        let override_route = route_by_public(&config, "public-override");
        assert_eq!(
            default_route.chat_stream_usage,
            ChatStreamUsagePolicy::Preserve
        );
        assert_eq!(
            override_route.chat_stream_usage,
            ChatStreamUsagePolicy::Insert
        );
    }

    #[test]
    fn backend_base_url_rejects_credentials_and_query_strings() {
        let credentials_error = resolve_error(config_file_with_base_url(
            "http://user:password@127.0.0.1:8000",
        ));
        assert!(credentials_error.contains("must not contain credentials"));

        let query_error = resolve_error(config_file_with_base_url(
            "http://127.0.0.1:8000?api_key=secret",
        ));
        assert!(query_error.contains("must not contain a query string or fragment"));
    }

    #[test]
    fn debug_capture_config_resolves_when_enabled() {
        let config = parse_config(
            r#"
            [debug_capture]
            enabled = true
            mode = "failures"
            directory = "onair-debug-captures"

            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["responses"]

            [[route]]
            public = "public-model"
            expose = ["responses"]
            backends = ["private-model@backend-a"]
            "#,
        );

        assert!(config.debug_capture.enabled);
        assert_eq!(config.debug_capture.mode, DebugCaptureMode::Failures);
        assert_eq!(
            config.debug_capture.directory,
            PathBuf::from("onair-debug-captures")
        );
    }

    #[test]
    fn health_config_resolves_when_enabled() {
        let config = parse_config(
            r#"
            [health]
            active = true
            interval_ms = 5000
            timeout_ms = 750
            path = "/v1/models"

            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["responses"]

            [[route]]
            public = "public-model"
            expose = ["responses"]
            backends = ["private-model@backend-a"]
            "#,
        );

        assert!(config.health.active);
        assert_eq!(config.health.interval_ms, 5000);
        assert_eq!(config.health.timeout_ms, 750);
        assert_eq!(config.health.path, "/v1/models");
    }

    #[test]
    fn routing_config_resolves_fallback_attempts() {
        let config = parse_config(
            r#"
            [routing]
            strategy = "sticky"
            fallback_attempts = 2

            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["responses"]

            [[route]]
            public = "public-model"
            expose = ["responses"]
            backends = ["private-model@backend-a"]
            "#,
        );

        assert_eq!(config.routing.strategy, RoutingStrategy::Sticky);
        assert_eq!(config.routing.fallback_attempts, 2);
    }

    #[test]
    fn routing_strategy_round_robin_and_weighted_random_parse() {
        let config = parse_config(
            r#"
            [routing]
            strategy = "round_robin"

            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["responses"]

            [[route]]
            public = "public-model"
            expose = ["responses"]
            backends = ["private-model@backend-a"]
            "#,
        );
        assert_eq!(config.routing.strategy, RoutingStrategy::RoundRobin);

        let config = parse_config(
            r#"
            [routing]
            strategy = "weighted_random"

            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["responses"]

            [[route]]
            public = "public-model"
            expose = ["responses"]
            backends = ["private-model@backend-a"]
            "#,
        );
        assert_eq!(config.routing.strategy, RoutingStrategy::WeightedRandom);
    }

    #[test]
    fn unknown_marker_policy_defaults_to_warn() {
        let config = parse_config(
            r#"
            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["responses"]

            [[route]]
            public = "public-model"
            expose = ["responses"]
            backends = ["private-model@backend-a"]
            "#,
        );
        assert_eq!(
            config.routing.unknown_capability_policy,
            UnknownMarkerPolicy::Warn
        );
        assert_eq!(
            config.routing.unknown_endpoint_policy,
            UnknownMarkerPolicy::Warn
        );
    }

    #[test]
    fn unknown_marker_policy_parses_error_value() {
        let config = parse_config(
            r#"
            [routing]
            unknown_capability_policy = "error"
            unknown_endpoint_policy = "error"

            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["responses"]

            [[route]]
            public = "public-model"
            expose = ["responses"]
            backends = ["private-model@backend-a"]
            "#,
        );
        assert_eq!(
            config.routing.unknown_capability_policy,
            UnknownMarkerPolicy::Error
        );
        assert_eq!(
            config.routing.unknown_endpoint_policy,
            UnknownMarkerPolicy::Error
        );
    }

    #[test]
    fn unknown_marker_policy_rejects_unknown_value() {
        let result: std::result::Result<ConfigFile, _> = toml::from_str(
            r#"
            [routing]
            unknown_capability_policy = "explode"

            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["responses"]

            [[route]]
            public = "public-model"
            expose = ["responses"]
            backends = ["private-model@backend-a"]
            "#,
        );
        let error = result
            .err()
            .expect("expected unknown policy value to fail to parse");
        assert!(
            error.to_string().contains("unknown_capability_policy"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn unknown_endpoint_marker_passes_under_warn_policy() {
        let config = parse_config(
            r#"
            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["chat", "streaming"]

            [[route]]
            public = "public-model"
            expose = ["chat", "responses_via_chat_completion"]
            backends = ["private-model@backend-a"]
            "#,
        );
        let route = route_by_public(&config, "public-model");
        let endpoint_value = route
            .expose
            .iter()
            .find(|value| value.as_str() == "responses_via_chat_completion")
            .expect("typo marker should be preserved under warn policy");
        assert_eq!(endpoint_value, "responses_via_chat_completion");
    }

    #[test]
    fn unknown_endpoint_marker_fails_under_error_policy() {
        let result: std::result::Result<ConfigFile, _> = toml::from_str(
            r#"
            [routing]
            unknown_endpoint_policy = "error"

            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["chat", "streaming"]

            [[route]]
            public = "public-model"
            expose = ["chat", "responses_via_chat_completion"]
            backends = ["private-model@backend-a"]
            "#,
        );
        let file = result.expect("config should parse at the toml level");
        let error = Config::resolve(file)
            .err()
            .expect("expected unknown endpoint marker to fail under error policy");
        let message = error.to_string();
        assert!(
            message.contains("route 'public=public-model'"),
            "missing location in error: {message}"
        );
        assert!(
            message.contains("responses_via_chat_completion"),
            "missing offending value in error: {message}"
        );
        assert!(
            message.contains("not a recognized marker"),
            "missing explanation in error: {message}"
        );
    }

    #[test]
    fn unknown_capability_marker_fails_under_error_policy() {
        let result: std::result::Result<ConfigFile, _> = toml::from_str(
            r#"
            [routing]
            unknown_capability_policy = "error"

            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["respons"]

            [[route]]
            public = "public-model"
            expose = ["responses"]
            backends = ["private-model@backend-a"]
            "#,
        );
        let file = result.expect("config should parse at the toml level");
        let error = Config::resolve(file)
            .err()
            .expect("expected unknown capability to fail under error policy");
        let message = error.to_string();
        assert!(
            message.contains("backend 'backend-a'"),
            "missing location in error: {message}"
        );
        assert!(
            message.contains("'respons'"),
            "missing offending value in error: {message}"
        );
    }

    #[test]
    fn known_aliases_and_compat_markers_are_accepted() {
        let config = parse_config(
            r#"
            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = [
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
                "audio",
                "files",
                "models",
                "batches",
                "fine_tuning",
                "assistants",
                "threads",
                "vector_stores",
                "uploads",
            ]

            [[route]]
            public = "public-model"
            expose = [
                "chat",
                "responses_via_chat_completions",
                "chat_completions_via_responses",
                "tools",
                "embeddings",
            ]
            backends = ["private-model@backend-a"]
            "#,
        );
        let supports = &config.backends[0].supports;
        for known in [
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
            "audio",
            "files",
            "models",
            "batches",
            "fine_tuning",
            "assistants",
            "threads",
            "vector_stores",
            "uploads",
        ] {
            assert!(
                supports.contains(known),
                "expected capability '{known}' to be accepted"
            );
        }
        let expose = &route_by_public(&config, "public-model").expose;
        for known in [
            "chat",
            "responses_via_chat_completions",
            "chat_completions_via_responses",
            "tools",
            "embeddings",
        ] {
            assert!(
                expose.contains(known),
                "expected endpoint '{known}' to be accepted"
            );
        }
    }

    #[test]
    fn marker_policies_are_independent() {
        let file: ConfigFile = toml::from_str(
            r#"
            [routing]
            unknown_capability_policy = "error"
            unknown_endpoint_policy = "warn"

            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["respons"]

            [[route]]
            public = "public-model"
            expose = ["responses_via_chat_completion"]
            backends = ["private-model@backend-a"]
            "#,
        )
        .expect("config should parse at the toml level");
        let error = Config::resolve(file)
            .err()
            .expect("unknown capability should fail under its own error policy");
        assert!(
            error.to_string().contains("'respons'"),
            "capability error should fire even when endpoint policy is warn: {error}"
        );
    }

    #[test]
    fn backend_config_default_weight_is_one() {
        let config = parse_config(
            r#"
            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["responses"]

            [[route]]
            public = "public-model"
            expose = ["responses"]
            backends = ["private-model@backend-a"]
            "#,
        );
        assert_eq!(config.backends[0].weight, 1);
    }

    #[test]
    fn backend_config_parses_explicit_weight() {
        let config = parse_config(
            r#"
            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["responses"]
            weight = 5

            [[route]]
            public = "public-model"
            expose = ["responses"]
            backends = ["private-model@backend-a"]
            "#,
        );
        assert_eq!(config.backends[0].weight, 5);
    }

    #[test]
    fn config_rejects_zero_weight() {
        let file: ConfigFile = toml::from_str(
            r#"
            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["responses"]
            weight = 0

            [[route]]
            public = "public-model"
            expose = ["responses"]
            backends = ["private-model@backend-a"]
            "#,
        )
        .unwrap();
        let error = resolve_error(file);
        assert!(error.contains("weight must be greater than zero"));
    }

    #[test]
    fn routing_config_rejects_excessive_fallback_attempts() {
        let file: ConfigFile = toml::from_str(
            r#"
            [routing]
            fallback_attempts = 17

            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["responses"]

            [[route]]
            public = "public-model"
            expose = ["responses"]
            backends = ["private-model@backend-a"]
            "#,
        )
        .unwrap();

        let error = resolve_error(file);
        assert!(error.contains("routing.fallback_attempts must be at most 16"));
    }

    #[test]
    fn inspector_config_resolves_when_enabled() {
        let config = parse_config(
            r#"
            [inspector]
            enabled = true
            retention_requests = 128
            allow_remote = true

            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["responses"]

            [[route]]
            public = "public-model"
            expose = ["responses"]
            backends = ["private-model@backend-a"]
            "#,
        );

        assert!(config.inspector.enabled);
        assert!(config.inspector.allow_remote);
        assert_eq!(config.inspector.retention_requests, 128);
        assert!(!config.inspector.persistence.enabled);
        assert!(config.inspector.persistence.path.is_none());
    }

    #[test]
    fn inspector_persistence_config_resolves_when_enabled_with_path() {
        let config = parse_config(
            r#"
            [inspector]
            enabled = true
            retention_requests = 128

            [inspector.persistence]
            enabled = true
            path = ".local/inspector.sqlite"

            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["responses"]

            [[route]]
            public = "public-model"
            expose = ["responses"]
            backends = ["private-model@backend-a"]
            "#,
        );

        assert!(config.inspector.persistence.enabled);
        assert_eq!(
            config.inspector.persistence.path.unwrap(),
            PathBuf::from(".local/inspector.sqlite")
        );
    }

    #[test]
    fn inspector_persistence_rejects_enabled_without_path() {
        let file: ConfigFile = toml::from_str(
            r#"
            [inspector.persistence]
            enabled = true

            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["responses"]

            [[route]]
            public = "public-model"
            expose = ["responses"]
            backends = ["private-model@backend-a"]
            "#,
        )
        .unwrap();

        let error = resolve_error(file);
        assert!(error.contains("inspector.persistence.path is required"));
    }

    #[test]
    fn inspector_config_rejects_zero_retention_requests() {
        let file: ConfigFile = toml::from_str(
            r#"
            [inspector]
            enabled = true
            retention_requests = 0

            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["responses"]

            [[route]]
            public = "public-model"
            expose = ["responses"]
            backends = ["private-model@backend-a"]
            "#,
        )
        .unwrap();

        let error = resolve_error(file);
        assert!(error.contains("inspector.retention_requests must be greater than zero"));
    }

    #[test]
    fn health_config_rejects_invalid_path() {
        let file: ConfigFile = toml::from_str(
            r#"
            [health]
            active = true
            path = "v1/models"

            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["responses"]

            [[route]]
            public = "public-model"
            expose = ["responses"]
            backends = ["private-model@backend-a"]
            "#,
        )
        .unwrap();

        let error = resolve_error(file);
        assert!(error.contains("health.path must start with '/'"));
    }

    #[test]
    fn trusted_proxy_cidrs_resolve_from_server_config() {
        let config = parse_config(
            r#"
            [server]
            trusted_proxy_cidrs = ["127.0.0.1/32", "::1/128"]

            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["responses"]

            [[route]]
            public = "public-model"
            expose = ["responses"]
            backends = ["private-model@backend-a"]
            "#,
        );

        assert_eq!(config.server.trusted_proxy_cidrs.len(), 2);
        assert!(config.server.trusted_proxy_cidrs[0].contains("127.0.0.1".parse().unwrap()));
        assert!(config.server.trusted_proxy_cidrs[1].contains("::1".parse().unwrap()));
    }

    #[test]
    fn debug_capture_rejects_parent_directory_components() {
        let file = toml::from_str(
            r#"
            [debug_capture]
            enabled = true
            directory = "../captures"

            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["responses"]

            [[route]]
            public = "public-model"
            expose = ["responses"]
            backends = ["private-model@backend-a"]
            "#,
        )
        .unwrap();
        let error = resolve_error(file);

        assert!(error.contains("must not contain '..' components"));
    }

    #[tokio::test]
    async fn watcher_reloads_config_after_file_changes() {
        let path = temp_config_path("watch");
        std::fs::write(&path, config_with_model("first-model")).unwrap();
        let store = ConfigStore::new(Config::load(&path).unwrap());
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(process_config_watch_events(
            rx,
            path.clone(),
            path.file_name().unwrap().to_owned(),
            store.clone(),
        ));

        std::fs::write(&path, config_with_model("second-model")).unwrap();
        send_reload_event(
            &tx,
            EventKind::Access(AccessKind::Close(AccessMode::Write)),
            &path,
        );
        wait_for_model(&store, "second-model").await;
        assert!(
            !store
                .snapshot()
                .public_model_context_lengths()
                .contains_key("first-model")
        );

        let replacement = path.with_extension("replacement.toml");
        std::fs::write(&replacement, config_with_model("third-model")).unwrap();
        #[cfg(not(unix))]
        {
            let _ = std::fs::remove_file(&path);
        }
        std::fs::rename(&replacement, &path).unwrap();
        send_reload_event(
            &tx,
            EventKind::Create(notify::event::CreateKind::File),
            &path,
        );
        wait_for_model(&store, "third-model").await;
        assert!(
            !store
                .snapshot()
                .public_model_context_lengths()
                .contains_key("second-model")
        );

        drop(tx);
        task.await.unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reload_keeps_previous_config_when_new_config_is_invalid() {
        let path = temp_config_path("reload");
        std::fs::write(&path, config_with_model("first-model")).unwrap();
        let store = ConfigStore::new(Config::load(&path).unwrap());

        assert!(
            store
                .snapshot()
                .public_model_context_lengths()
                .contains_key("first-model")
        );

        std::fs::write(&path, "not valid toml = ]").unwrap();
        reload_config(&path, &store);
        assert!(
            store
                .snapshot()
                .public_model_context_lengths()
                .contains_key("first-model")
        );

        std::fs::write(&path, config_with_model("second-model")).unwrap();
        reload_config(&path, &store);
        let models = store.snapshot().public_model_context_lengths();
        assert!(!models.contains_key("first-model"));
        assert!(models.contains_key("second-model"));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reload_applies_unknown_marker_under_warn_policy() {
        let path = temp_config_path("reload-warn-marker");
        let initial = r#"
            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["chat", "streaming"]

            [[route]]
            public = "public-model"
            expose = ["chat"]
            backends = ["private-model@backend-a"]
            "#;
        std::fs::write(&path, initial).unwrap();
        let store = ConfigStore::new(Config::load(&path).unwrap());

        assert!(
            !store.snapshot().routes[0]
                .expose
                .iter()
                .any(|value| value == "responses_via_chat_completion")
        );

        let updated = r#"
            [routing]
            unknown_endpoint_policy = "warn"

            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["chat", "streaming"]

            [[route]]
            public = "public-model"
            expose = ["chat", "responses_via_chat_completion"]
            backends = ["private-model@backend-a"]
            "#;
        std::fs::write(&path, updated).unwrap();
        reload_config(&path, &store);

        let snapshot = store.snapshot();
        assert!(
            snapshot.routes[0]
                .expose
                .iter()
                .any(|value| value == "responses_via_chat_completion"),
            "warn policy should preserve the unknown endpoint marker across reload",
        );
        assert_eq!(
            snapshot.routing.unknown_endpoint_policy,
            UnknownMarkerPolicy::Warn
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reload_events_include_close_after_write() {
        let path = PathBuf::from("/tmp/onair.toml");
        let filename = path.file_name().unwrap();
        let write_close = Event::new(EventKind::Access(AccessKind::Close(AccessMode::Write)))
            .add_path(path.clone());
        let read_close = Event::new(EventKind::Access(AccessKind::Close(AccessMode::Read)))
            .add_path(path.clone());

        assert!(is_reload_event(&write_close, &path, filename));
        assert!(!is_reload_event(&read_close, &path, filename));
    }

    fn send_reload_event(
        tx: &mpsc::UnboundedSender<notify::Result<Event>>,
        kind: EventKind,
        path: &Path,
    ) {
        tx.send(Ok(Event::new(kind).add_path(path.to_path_buf())))
            .unwrap();
    }

    async fn wait_for_model(store: &ConfigStore, model: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if store
                    .snapshot()
                    .public_model_context_lengths()
                    .contains_key(model)
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for model '{model}' to reload"));
    }

    fn temp_config_path(label: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "onair-config-{label}-test-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn parse_config(raw: &str) -> Config {
        Config::resolve(toml::from_str(raw).unwrap()).unwrap()
    }

    fn resolve_error(file: ConfigFile) -> String {
        match Config::resolve(file) {
            Ok(_) => panic!("expected config resolution to fail"),
            Err(error) => error.to_string(),
        }
    }

    fn config_file_with_base_url(base_url: &str) -> ConfigFile {
        toml::from_str(&format!(
            r#"
            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "{base_url}"
            supports = ["responses"]

            [[route]]
            public = "public-model"
            expose = ["responses"]
            backends = ["private-model@backend-a"]
            "#
        ))
        .unwrap()
    }

    fn config_with_model(model: &str) -> String {
        format!(
            r#"
            [access]
            default_models = ["{model}"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            supports = ["responses"]

            [[route]]
            public = "{model}"
            expose = ["responses"]
            backends = ["private-{model}@backend-a"]
            "#
        )
    }

    fn route_by_public<'a>(config: &'a Config, public: &str) -> &'a ResolvedRoute {
        config
            .routes
            .iter()
            .find(|route| matches!(&route.key, RouteKey::Public(name) if name == public))
            .unwrap_or_else(|| panic!("no route with public={public}"))
    }
}
