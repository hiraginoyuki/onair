use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    #[serde(default)]
    pub routing: RoutingConfig,
    #[serde(default)]
    pub access: AccessConfig,
    #[serde(default, rename = "client")]
    pub clients: Vec<ClientConfig>,
    #[serde(default, rename = "backend")]
    pub backends: Vec<BackendConfig>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub telemetry: TelemetryConfig,
    pub routing: RoutingConfig,
    pub clients: Vec<ResolvedClient>,
    pub backends: Vec<ResolvedBackend>,
}

#[derive(Debug, Clone)]
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

        let (tx, mut rx) = mpsc::channel::<notify::Result<Event>>(32);
        let mut watcher = notify::recommended_watcher(move |event| {
            if let Err(error) = tx.try_send(event) {
                warn!(?error, "failed to enqueue config watch event");
            }
        })
        .map_err(|error| Error::ConfigWatch(error.to_string()))?;
        watcher
            .watch(directory, RecursiveMode::NonRecursive)
            .map_err(|error| Error::ConfigWatch(error.to_string()))?;

        let task_path = path.clone();
        let task = tokio::spawn(async move {
            info!(path = %task_path.display(), "watching config file");
            while let Some(event) = rx.recv().await {
                match event {
                    Ok(event) if is_reload_event(&event, &task_path, &filename) => {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        loop {
                            match rx.try_recv() {
                                Ok(Ok(event)) => {
                                    if !is_reload_event(&event, &task_path, &filename) {
                                        continue;
                                    }
                                }
                                Ok(Err(error)) => {
                                    warn!(?error, "config watch event failed");
                                }
                                Err(TryRecvError::Empty) => break,
                                Err(TryRecvError::Disconnected) => return,
                            }
                        }
                        reload_config(&task_path, &store);
                    }
                    Ok(event) => {
                        debug!(?event, "ignored config watch event");
                    }
                    Err(error) => {
                        warn!(?error, "config watch event failed");
                    }
                }
            }
        });

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

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub request_body_limit_bytes: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080"
                .parse()
                .expect("valid default bind address"),
            request_body_limit_bytes: 2 * 1024 * 1024,
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

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryExporter {
    None,
    Otlp,
}

impl Default for TelemetryExporter {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingConfig {
    pub strategy: RoutingStrategy,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            strategy: RoutingStrategy::Priority,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoutingStrategy {
    Priority,
    Sticky,
}

impl Default for RoutingStrategy {
    fn default() -> Self {
        Self::Priority
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccessConfig {
    pub default_models: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    pub id: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedClient {
    pub id: String,
    pub api_key: String,
    pub models: BTreeSet<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BackendConfig {
    pub id: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub timeout_ms: u64,
    pub context_length: Option<u64>,
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
            capabilities: BTreeSet::new(),
            models: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRouteConfig {
    pub public: String,
    pub backend: Option<String>,
    #[serde(default)]
    pub context_length: ContextLengthConfig,
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

#[derive(Debug, Clone)]
pub struct ResolvedBackend {
    pub id: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub timeout: Duration,
    pub capabilities: BTreeSet<String>,
    pub models: Vec<ModelRoute>,
}

#[derive(Debug, Clone)]
pub struct ModelRoute {
    pub public: String,
    pub backend: String,
    pub context_length: Option<u64>,
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
            if backend.base_url.trim().is_empty() {
                return Err(Error::Config(format!(
                    "backend '{}' base_url must not be empty",
                    backend.id
                )));
            }
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
                        endpoints: model.endpoints,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            backends.push(ResolvedBackend {
                id: backend.id,
                base_url: backend.base_url.trim_end_matches('/').to_owned(),
                api_key,
                timeout: Duration::from_millis(backend.timeout_ms),
                capabilities: backend.capabilities,
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

fn reload_config(path: &Path, store: &ConfigStore) {
    match Config::load(path) {
        Ok(config) => {
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
        Err(error) => {
            warn!(?error, "config reload failed; keeping previous config");
        }
    }
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
        EventKind::Any | EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
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

        let error = Config::resolve(file).unwrap_err().to_string();
        assert!(error.contains("uses context_length = 'inherit'"));
    }

    #[test]
    fn reload_keeps_previous_config_when_new_config_is_invalid() {
        let path = env::temp_dir().join(format!(
            "onair-config-reload-test-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
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

    fn parse_config(raw: &str) -> Config {
        Config::resolve(toml::from_str(raw).unwrap()).unwrap()
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
