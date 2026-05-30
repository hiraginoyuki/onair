use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::config::{
    Config, DebugCaptureConfig, HealthConfig, InspectorConfig, ResolvedBackend, ResolvedClient,
    RoutingConfig, RoutingStrategy, ServerConfig, TelemetryConfig, TelemetryExporter,
};
use crate::observe::{BackendHealthSnapshot as ObservedBackendHealth, BackendHealthStore};

#[derive(Debug, Serialize)]
pub(crate) struct OperatorRuntimeSnapshot {
    pub(crate) now_unix_ms: u64,
    pub(crate) started_at_unix_ms: u64,
    pub(crate) uptime_ms: u64,
    pub(crate) clients: usize,
    pub(crate) backends: usize,
    pub(crate) public_models: usize,
    pub(crate) inspector_retained_requests: usize,
    pub(crate) telemetry: TelemetrySnapshot,
}

#[derive(Debug, Serialize)]
pub(crate) struct OperatorConfigSnapshot {
    pub(crate) server: ServerSnapshot,
    pub(crate) telemetry: TelemetrySnapshot,
    pub(crate) debug_capture: DebugCaptureSnapshot,
    pub(crate) inspector: InspectorSnapshot,
    pub(crate) health: HealthConfigSnapshot,
    pub(crate) routing: RoutingSnapshot,
    pub(crate) clients: Vec<ClientSnapshot>,
    pub(crate) backends: Vec<BackendSnapshot>,
}

#[derive(Debug, Serialize)]
pub(crate) struct OperatorModelsSnapshot {
    pub(crate) public_models: Vec<PublicModelSnapshot>,
    pub(crate) clients: Vec<ClientSnapshot>,
}

#[derive(Debug, Serialize)]
pub(crate) struct OperatorHealthSnapshot {
    pub(crate) backends: Vec<BackendHealthSnapshot>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ServerSnapshot {
    pub(crate) bind: String,
    pub(crate) request_body_limit_bytes: usize,
    pub(crate) trusted_proxy_cidrs: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct TelemetrySnapshot {
    pub(crate) service_name: String,
    pub(crate) exporter: &'static str,
    pub(crate) otlp_endpoint_configured: bool,
    pub(crate) export_interval_ms: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct DebugCaptureSnapshot {
    pub(crate) enabled: bool,
    pub(crate) directory: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct InspectorSnapshot {
    pub(crate) enabled: bool,
    pub(crate) retention_requests: usize,
    pub(crate) allow_remote: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct HealthConfigSnapshot {
    pub(crate) active: bool,
    pub(crate) interval_ms: u64,
    pub(crate) timeout_ms: u64,
    pub(crate) path: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct RoutingSnapshot {
    pub(crate) strategy: &'static str,
    pub(crate) fallback_attempts: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct ClientSnapshot {
    pub(crate) id: String,
    pub(crate) models: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct BackendSnapshot {
    pub(crate) id: String,
    pub(crate) base_url: String,
    pub(crate) api_key_configured: bool,
    pub(crate) timeout_ms: u64,
    pub(crate) capabilities: Vec<String>,
    pub(crate) models: Vec<BackendModelSnapshot>,
}

#[derive(Debug, Serialize)]
pub(crate) struct BackendModelSnapshot {
    pub(crate) public: String,
    pub(crate) backend: String,
    pub(crate) context_length: Option<u64>,
    pub(crate) endpoints: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct PublicModelSnapshot {
    pub(crate) public: String,
    pub(crate) context_length: Option<u64>,
    pub(crate) clients: Vec<String>,
    pub(crate) routes: Vec<ModelRouteSnapshot>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ModelRouteSnapshot {
    pub(crate) backend: String,
    pub(crate) backend_model: String,
    pub(crate) endpoints: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct BackendHealthSnapshot {
    pub(crate) backend: String,
    pub(crate) status: &'static str,
    pub(crate) successes: u64,
    pub(crate) failures: u64,
    pub(crate) traffic_successes: u64,
    pub(crate) traffic_failures: u64,
    pub(crate) probe_successes: u64,
    pub(crate) probe_failures: u64,
    pub(crate) consecutive_failures: u64,
    pub(crate) last_success_unix_ms: Option<u64>,
    pub(crate) last_failure_unix_ms: Option<u64>,
    pub(crate) last_observed_unix_ms: Option<u64>,
    pub(crate) last_status: Option<u16>,
    pub(crate) last_error_kind: Option<String>,
    pub(crate) last_latency_ms: Option<u64>,
    pub(crate) last_source: Option<&'static str>,
}

pub(crate) fn runtime_snapshot(
    config: &Config,
    started_at_unix_ms: u64,
    uptime: Duration,
    inspector_retained_requests: usize,
) -> OperatorRuntimeSnapshot {
    OperatorRuntimeSnapshot {
        now_unix_ms: unix_millis(),
        started_at_unix_ms,
        uptime_ms: duration_millis(uptime),
        clients: config.clients.len(),
        backends: config.backends.len(),
        public_models: config.public_model_context_lengths().len(),
        inspector_retained_requests,
        telemetry: telemetry_snapshot(&config.telemetry),
    }
}

pub(crate) fn config_snapshot(config: &Config) -> OperatorConfigSnapshot {
    OperatorConfigSnapshot {
        server: server_snapshot(&config.server),
        telemetry: telemetry_snapshot(&config.telemetry),
        debug_capture: debug_capture_snapshot(&config.debug_capture),
        inspector: inspector_snapshot(&config.inspector),
        health: health_config_snapshot(&config.health),
        routing: routing_snapshot(&config.routing),
        clients: clients_snapshot(&config.clients),
        backends: config.backends.iter().map(backend_snapshot).collect(),
    }
}

pub(crate) fn models_snapshot(config: &Config) -> OperatorModelsSnapshot {
    let mut models = BTreeMap::<String, PublicModelSnapshot>::new();
    for (public, context_length) in config.public_model_context_lengths() {
        models.insert(
            public.clone(),
            PublicModelSnapshot {
                public,
                context_length,
                clients: Vec::new(),
                routes: Vec::new(),
            },
        );
    }

    for client in &config.clients {
        for model in &client.models {
            if let Some(public_model) = models.get_mut(model) {
                public_model.clients.push(client.id.clone());
            }
        }
    }

    for backend in &config.backends {
        for model in &backend.models {
            if let Some(public_model) = models.get_mut(&model.public) {
                public_model.routes.push(ModelRouteSnapshot {
                    backend: backend.id.clone(),
                    backend_model: model.backend.clone(),
                    endpoints: sorted_strings(&model.endpoints),
                });
            }
        }
    }

    OperatorModelsSnapshot {
        public_models: models.into_values().collect(),
        clients: clients_snapshot(&config.clients),
    }
}

pub(crate) fn health_snapshot(
    config: &Config,
    health: &BackendHealthStore,
) -> OperatorHealthSnapshot {
    let backend_ids = config
        .backends
        .iter()
        .map(|backend| backend.id.clone())
        .collect::<Vec<_>>();
    OperatorHealthSnapshot {
        backends: health
            .snapshot(&backend_ids)
            .into_iter()
            .map(backend_health_snapshot)
            .collect(),
    }
}

fn server_snapshot(config: &ServerConfig) -> ServerSnapshot {
    ServerSnapshot {
        bind: config.bind.to_string(),
        request_body_limit_bytes: config.request_body_limit_bytes,
        trusted_proxy_cidrs: config
            .trusted_proxy_cidrs
            .iter()
            .map(ToString::to_string)
            .collect(),
    }
}

fn telemetry_snapshot(config: &TelemetryConfig) -> TelemetrySnapshot {
    TelemetrySnapshot {
        service_name: config.service_name.clone(),
        exporter: telemetry_exporter(config.exporter),
        otlp_endpoint_configured: config.otlp_endpoint.is_some(),
        export_interval_ms: config.export_interval_ms,
    }
}

fn debug_capture_snapshot(config: &DebugCaptureConfig) -> DebugCaptureSnapshot {
    DebugCaptureSnapshot {
        enabled: config.enabled,
        directory: config.directory.display().to_string(),
    }
}

fn inspector_snapshot(config: &InspectorConfig) -> InspectorSnapshot {
    InspectorSnapshot {
        enabled: config.enabled,
        retention_requests: config.retention_requests,
        allow_remote: config.allow_remote,
    }
}

fn health_config_snapshot(config: &HealthConfig) -> HealthConfigSnapshot {
    HealthConfigSnapshot {
        active: config.active,
        interval_ms: config.interval_ms,
        timeout_ms: config.timeout_ms,
        path: config.path.clone(),
    }
}

fn routing_snapshot(config: &RoutingConfig) -> RoutingSnapshot {
    RoutingSnapshot {
        strategy: match config.strategy {
            RoutingStrategy::Priority => "priority",
            RoutingStrategy::Sticky => "sticky",
        },
        fallback_attempts: config.fallback_attempts,
    }
}

fn clients_snapshot(clients: &[ResolvedClient]) -> Vec<ClientSnapshot> {
    clients
        .iter()
        .map(|client| ClientSnapshot {
            id: client.id.clone(),
            models: sorted_strings(&client.models),
        })
        .collect()
}

fn backend_snapshot(backend: &ResolvedBackend) -> BackendSnapshot {
    BackendSnapshot {
        id: backend.id.clone(),
        base_url: backend.base_url.clone(),
        api_key_configured: backend.api_key.is_some(),
        timeout_ms: duration_millis(backend.timeout),
        capabilities: sorted_strings(&backend.capabilities),
        models: backend
            .models
            .iter()
            .map(|model| BackendModelSnapshot {
                public: model.public.clone(),
                backend: model.backend.clone(),
                context_length: model.context_length,
                endpoints: sorted_strings(&model.endpoints),
            })
            .collect(),
    }
}

fn backend_health_snapshot(observed: ObservedBackendHealth) -> BackendHealthSnapshot {
    BackendHealthSnapshot {
        backend: observed.backend,
        status: observed.status,
        successes: observed.successes,
        failures: observed.failures,
        traffic_successes: observed.traffic_successes,
        traffic_failures: observed.traffic_failures,
        probe_successes: observed.probe_successes,
        probe_failures: observed.probe_failures,
        consecutive_failures: observed.consecutive_failures,
        last_success_unix_ms: observed.last_success_unix_ms,
        last_failure_unix_ms: observed.last_failure_unix_ms,
        last_observed_unix_ms: observed.last_observed_unix_ms,
        last_status: observed.last_status,
        last_error_kind: observed.last_error_kind,
        last_latency_ms: observed.last_latency_ms,
        last_source: observed.last_source,
    }
}

fn telemetry_exporter(exporter: TelemetryExporter) -> &'static str {
    match exporter {
        TelemetryExporter::None => "none",
        TelemetryExporter::Otlp => "otlp",
    }
}

fn sorted_strings(values: &std::collections::BTreeSet<String>) -> Vec<String> {
    values.iter().cloned().collect()
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}
