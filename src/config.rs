use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use notify::event::{AccessKind, AccessMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use url::Url;

use crate::error::{Error, Result};
use crate::observe::{IpCidr, debug_capture, inspector};

const CONFIG_RELOAD_DEBOUNCE: Duration = Duration::from_millis(250);
const CONFIG_RELOAD_RETRY_DELAY: Duration = Duration::from_millis(250);
const CONFIG_RELOAD_MAX_ATTEMPTS: usize = 5;
const MAX_FALLBACK_ATTEMPTS: usize = 16;

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
        self.inner
            .read()
            .expect("config store lock poisoned")
            .clone()
    }

    pub fn replace(&self, config: Config) -> Arc<Config> {
        let config = Arc::new(config);
        *self.inner.write().expect("config store lock poisoned") = config.clone();
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
    pub directory: PathBuf,
}

impl Default for DebugCaptureConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            directory: PathBuf::from("onair-debug-captures"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InspectorConfig {
    pub enabled: bool,
    pub retention_requests: usize,
    pub allow_remote: bool,
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
            retention_requests: inspector::default_retention_requests(),
            allow_remote: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingConfig {
    pub strategy: RoutingStrategy,
    pub fallback_attempts: usize,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            strategy: RoutingStrategy::Priority,
            fallback_attempts: 1,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoutingStrategy {
    #[default]
    Priority,
    Sticky,
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
    pub context_length: Option<u64>,
    pub tool_schema_mode: ToolSchemaMode,
    #[serde(alias = "capability")]
    pub capabilities: BTreeSet<String>,
    #[serde(rename = "model")]
    pub models: Vec<ModelRouteConfig>,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            base_url: String::new(),
            api_key: None,
            api_key_env: None,
            timeout_ms: 120_000,
            context_length: None,
            tool_schema_mode: ToolSchemaMode::Preserve,
            capabilities: BTreeSet::new(),
            models: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolSchemaMode {
    #[default]
    Preserve,
    LlamacppCompat,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRouteConfig {
    pub public: String,
    pub backend: Option<String>,
    #[serde(default)]
    pub context_length: ContextLengthConfig,
    #[serde(default)]
    pub tool_schema_mode: Option<ToolSchemaMode>,
    #[serde(default)]
    pub endpoints: BTreeSet<String>,
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
    Inherit,
}

#[derive(Clone)]
pub struct ResolvedBackend {
    pub id: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub timeout: Duration,
    pub capabilities: BTreeSet<String>,
    pub tool_schema_mode: ToolSchemaMode,
    pub models: Vec<ModelRoute>,
}

#[derive(Debug, Clone)]
pub struct ModelRoute {
    pub public: String,
    pub backend: String,
    pub context_length: Option<u64>,
    pub tool_schema_mode: ToolSchemaMode,
    pub endpoints: BTreeSet<String>,
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
        debug_capture::validate_config(&file.debug_capture)?;
        inspector::validate_config(&file.inspector)?;
        validate_health_config(&file.health)?;
        validate_routing_config(&file.routing)?;

        let mut client_ids = BTreeSet::new();
        let mut clients = Vec::with_capacity(file.clients.len());
        for client in file.clients {
            if client.id.trim().is_empty() {
                return Err(Error::Config("client id must not be empty".to_owned()));
            }
            if !client_ids.insert(client.id.clone()) {
                return Err(Error::Config(format!(
                    "duplicate client id '{}'",
                    client.id
                )));
            }
            let api_key = resolve_secret(
                client.api_key,
                client.api_key_env,
                &format!("client '{}' api key", client.id),
            )?;
            let mut models = BTreeSet::new();
            models.extend(file.access.default_models.iter().cloned());
            models.extend(client.models);
            clients.push(ResolvedClient {
                id: client.id,
                api_key,
                models,
            });
        }

        if clients.is_empty() {
            return Err(Error::Config(
                "at least one [[client]] is required".to_owned(),
            ));
        }

        let mut backend_ids = BTreeSet::new();
        let mut public_models = BTreeSet::new();
        let mut backends = Vec::with_capacity(file.backends.len());
        for backend in file.backends {
            if backend.id.trim().is_empty() {
                return Err(Error::Config("backend id must not be empty".to_owned()));
            }
            if !backend_ids.insert(backend.id.clone()) {
                return Err(Error::Config(format!(
                    "duplicate backend id '{}'",
                    backend.id
                )));
            }
            let base_url = normalize_backend_base_url(&backend.base_url, &backend.id)?;
            let api_key = resolve_optional_secret(backend.api_key, backend.api_key_env)?;
            let models = backend
                .models
                .into_iter()
                .map(|model| {
                    if model.public.trim().is_empty() {
                        return Err(Error::Config(format!(
                            "backend '{}' has an empty public model name",
                            backend.id
                        )));
                    }
                    public_models.insert(model.public.clone());
                    let context_length = resolve_context_length(
                        &model.context_length,
                        backend.context_length,
                        &backend.id,
                        &model.public,
                    )?;
                    Ok(ModelRoute {
                        backend: model.backend.unwrap_or_else(|| model.public.clone()),
                        public: model.public,
                        context_length,
                        tool_schema_mode: model
                            .tool_schema_mode
                            .unwrap_or(backend.tool_schema_mode),
                        endpoints: model.endpoints,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            backends.push(ResolvedBackend {
                id: backend.id,
                base_url,
                api_key,
                timeout: Duration::from_millis(backend.timeout_ms),
                capabilities: backend.capabilities,
                tool_schema_mode: backend.tool_schema_mode,
                models,
            });
        }

        if backends.is_empty() {
            return Err(Error::Config(
                "at least one [[backend]] is required".to_owned(),
            ));
        }

        validate_allowed_models(&clients, &public_models)?;

        Ok(Self {
            server: file.server,
            telemetry: file.telemetry,
            debug_capture: file.debug_capture,
            inspector: file.inspector,
            health: file.health,
            routing: file.routing,
            clients,
            backends,
        })
    }

    pub fn public_model_context_lengths(&self) -> BTreeMap<String, Option<u64>> {
        let mut models = BTreeMap::new();
        for backend in &self.backends {
            for model in &backend.models {
                models
                    .entry(model.public.clone())
                    .or_insert(model.context_length);
            }
        }
        models
    }
}

fn resolve_secret(
    api_key: Option<String>,
    api_key_env: Option<String>,
    label: &str,
) -> Result<String> {
    match (api_key, api_key_env) {
        (Some(_), Some(_)) => Err(Error::Config(format!(
            "{label} must use api_key or api_key_env, not both"
        ))),
        (Some(value), None) if !value.trim().is_empty() => Ok(value),
        (None, Some(name)) if !name.trim().is_empty() => {
            env::var(&name).map_err(|_| Error::MissingEnv(name))
        }
        _ => Err(Error::Config(format!("{label} is required"))),
    }
}

fn resolve_optional_secret(
    api_key: Option<String>,
    api_key_env: Option<String>,
) -> Result<Option<String>> {
    match (api_key, api_key_env) {
        (Some(_), Some(_)) => Err(Error::Config(
            "backend must use api_key or api_key_env, not both".to_owned(),
        )),
        (Some(value), None) if !value.trim().is_empty() => Ok(Some(value)),
        (None, Some(name)) if !name.trim().is_empty() => env::var(&name)
            .map(Some)
            .map_err(|_| Error::MissingEnv(name)),
        _ => Ok(None),
    }
}

fn validate_allowed_models(
    clients: &[ResolvedClient],
    public_models: &BTreeSet<String>,
) -> Result<()> {
    for client in clients {
        for model in &client.models {
            if !public_models.contains(model) {
                return Err(Error::Config(format!(
                    "client '{}' allows unknown model '{}'",
                    client.id, model
                )));
            }
        }
    }
    Ok(())
}

fn validate_health_config(config: &HealthConfig) -> Result<()> {
    if config.interval_ms == 0 {
        return Err(Error::Config(
            "health.interval_ms must be greater than zero".to_owned(),
        ));
    }
    if config.timeout_ms == 0 {
        return Err(Error::Config(
            "health.timeout_ms must be greater than zero".to_owned(),
        ));
    }
    if !config.path.starts_with('/') {
        return Err(Error::Config(
            "health.path must start with '/' and be relative to backend base_url".to_owned(),
        ));
    }
    if config.path.starts_with("//") || config.path.contains("://") {
        return Err(Error::Config(
            "health.path must be a relative path, not an absolute URL".to_owned(),
        ));
    }
    if config.path.chars().any(char::is_control) {
        return Err(Error::Config(
            "health.path must not contain control characters".to_owned(),
        ));
    }
    Ok(())
}

fn validate_routing_config(config: &RoutingConfig) -> Result<()> {
    if config.fallback_attempts > MAX_FALLBACK_ATTEMPTS {
        return Err(Error::Config(format!(
            "routing.fallback_attempts must be at most {MAX_FALLBACK_ATTEMPTS}"
        )));
    }
    Ok(())
}

fn resolve_context_length(
    config: &ContextLengthConfig,
    backend_context_length: Option<u64>,
    backend_id: &str,
    public_model: &str,
) -> Result<Option<u64>> {
    match config {
        ContextLengthConfig::Value(value) => Ok(Some(*value)),
        ContextLengthConfig::Mode(ContextLengthMode::None) => Ok(None),
        ContextLengthConfig::Mode(ContextLengthMode::Inherit) => backend_context_length
            .map(Some)
            .ok_or_else(|| {
                Error::Config(format!(
                    "backend '{}' model '{}' uses context_length = 'inherit' but backend context_length is not set",
                    backend_id, public_model
                ))
            }),
    }
}

fn normalize_backend_base_url(base_url: &str, backend_id: &str) -> Result<String> {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(Error::Config(format!(
            "backend '{backend_id}' base_url must not be empty"
        )));
    }

    let parsed = Url::parse(trimmed).map_err(|source| {
        Error::Config(format!(
            "backend '{backend_id}' base_url is invalid: {source}"
        ))
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(Error::Config(format!(
            "backend '{backend_id}' base_url must use http or https"
        )));
    }
    if parsed.host_str().is_none() {
        return Err(Error::Config(format!(
            "backend '{backend_id}' base_url must include a host"
        )));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(Error::Config(format!(
            "backend '{backend_id}' base_url must not contain credentials"
        )));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(Error::Config(format!(
            "backend '{backend_id}' base_url must not contain a query string or fragment"
        )));
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
    fn context_length_policies_resolve_from_config() {
        let config = parse_config(
            r#"
            [access]
            default_models = ["public-inherit", "public-specific", "public-none"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            context_length = 131072
            capabilities = ["responses"]

            [[backend.model]]
            public = "public-inherit"
            backend = "private-inherit"
            context_length = "inherit"

            [[backend.model]]
            public = "public-specific"
            backend = "private-specific"
            context_length = 8192

            [[backend.model]]
            public = "public-none"
            backend = "private-none"
            context_length = "none"
            "#,
        );

        let models = config.public_model_context_lengths();
        assert_eq!(models.get("public-inherit"), Some(&Some(131_072)));
        assert_eq!(models.get("public-specific"), Some(&Some(8_192)));
        assert_eq!(models.get("public-none"), Some(&None));
    }

    #[test]
    fn inherited_context_length_requires_backend_context_length() {
        let file: ConfigFile = toml::from_str(
            r#"
            [access]
            default_models = ["public-inherit"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            capabilities = ["responses"]

            [[backend.model]]
            public = "public-inherit"
            backend = "private-inherit"
            context_length = "inherit"
            "#,
        )
        .unwrap();

        let error = resolve_error(file);
        assert!(error.contains("uses context_length = 'inherit'"));
    }

    #[test]
    fn tool_schema_mode_can_be_set_per_backend_and_model() {
        let config = parse_config(
            r#"
            [access]
            default_models = ["public-inherit", "public-override"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            capabilities = ["chat", "tools"]
            tool_schema_mode = "llamacpp_compat"

            [[backend.model]]
            public = "public-inherit"
            backend = "private-inherit"
            endpoints = ["chat", "tools"]

            [[backend.model]]
            public = "public-override"
            backend = "private-override"
            endpoints = ["chat", "tools"]
            tool_schema_mode = "preserve"
            "#,
        );

        let backend = &config.backends[0];
        assert_eq!(backend.tool_schema_mode, ToolSchemaMode::LlamacppCompat);
        assert_eq!(
            backend.models[0].tool_schema_mode,
            ToolSchemaMode::LlamacppCompat
        );
        assert_eq!(backend.models[1].tool_schema_mode, ToolSchemaMode::Preserve);
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
            directory = "onair-debug-captures"

            [access]
            default_models = ["public-model"]

            [[client]]
            id = "dev"
            api_key = "sk-test"

            [[backend]]
            id = "backend-a"
            base_url = "http://127.0.0.1:8000"
            capabilities = ["responses"]

            [[backend.model]]
            public = "public-model"
            backend = "private-model"
            "#,
        );

        assert!(config.debug_capture.enabled);
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
            capabilities = ["responses"]

            [[backend.model]]
            public = "public-model"
            backend = "private-model"
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
            capabilities = ["responses"]

            [[backend.model]]
            public = "public-model"
            backend = "private-model"
            "#,
        );

        assert_eq!(config.routing.strategy, RoutingStrategy::Sticky);
        assert_eq!(config.routing.fallback_attempts, 2);
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
            capabilities = ["responses"]

            [[backend.model]]
            public = "public-model"
            backend = "private-model"
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
            capabilities = ["responses"]

            [[backend.model]]
            public = "public-model"
            backend = "private-model"
            "#,
        );

        assert!(config.inspector.enabled);
        assert!(config.inspector.allow_remote);
        assert_eq!(config.inspector.retention_requests, 128);
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
            capabilities = ["responses"]

            [[backend.model]]
            public = "public-model"
            backend = "private-model"
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
            capabilities = ["responses"]

            [[backend.model]]
            public = "public-model"
            backend = "private-model"
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
            capabilities = ["responses"]

            [[backend.model]]
            public = "public-model"
            backend = "private-model"
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
            capabilities = ["responses"]

            [[backend.model]]
            public = "public-model"
            backend = "private-model"
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
            capabilities = ["responses"]

            [[backend.model]]
            public = "public-model"
            backend = "private-model"
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
            capabilities = ["responses"]

            [[backend.model]]
            public = "{model}"
            backend = "private-{model}"
            "#
        )
    }
}
