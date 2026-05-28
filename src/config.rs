use std::collections::BTreeSet;
use std::env;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
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
    pub clients: Vec<ResolvedClient>,
    pub backends: Vec<ResolvedBackend>,
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
    pub endpoints: BTreeSet<String>,
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
                    Ok(ModelRoute {
                        backend: model.backend.unwrap_or_else(|| model.public.clone()),
                        public: model.public,
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
            clients,
            backends,
        })
    }

    pub fn public_model_ids(&self) -> BTreeSet<String> {
        self.backends
            .iter()
            .flat_map(|backend| backend.models.iter().map(|model| model.public.clone()))
            .collect()
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
