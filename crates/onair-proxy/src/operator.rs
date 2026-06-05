use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use onair_core::config::{
    Config, ContextLengthPolicy, DebugCaptureConfig, HealthConfig, InspectorConfig,
    ResolvedBackend, ResolvedClient, ResolvedContextLength, ResolvedRoute, RouteKey, RoutingConfig,
    RoutingStrategy, ServerConfig, TelemetryConfig, TelemetryExporter,
};
use onair_obs::observe::{BackendHealthSnapshot as ObservedBackendHealth, BackendHealthStore};

#[derive(Debug, Serialize)]
pub struct OperatorRuntimeSnapshot {
    pub now_unix_ms: u64,
    pub started_at_unix_ms: u64,
    pub uptime_ms: u64,
    pub clients: usize,
    pub backends: usize,
    pub public_models: usize,
    pub routes: usize,
    pub inspector_retained_requests: usize,
    pub telemetry: TelemetrySnapshot,
}

#[derive(Debug, Serialize)]
pub struct OperatorConfigSnapshot {
    pub server: ServerSnapshot,
    pub telemetry: TelemetrySnapshot,
    pub debug_capture: DebugCaptureSnapshot,
    pub inspector: InspectorSnapshot,
    pub health: HealthConfigSnapshot,
    pub routing: RoutingSnapshot,
    pub clients: Vec<ClientSnapshot>,
    pub backends: Vec<BackendSnapshot>,
    pub routes: Vec<RouteSnapshot>,
}

#[derive(Debug, Serialize)]
pub struct OperatorModelsSnapshot {
    pub public_models: Vec<PublicModelSnapshot>,
    pub clients: Vec<ClientSnapshot>,
}

#[derive(Debug, Serialize)]
pub struct OperatorHealthSnapshot {
    pub backends: Vec<BackendHealthSnapshot>,
}

#[derive(Debug, Serialize)]
pub struct ServerSnapshot {
    pub bind: String,
    pub request_body_limit_bytes: usize,
    pub trusted_proxy_cidrs: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct TelemetrySnapshot {
    pub service_name: String,
    pub exporter: &'static str,
    pub otlp_endpoint_configured: bool,
    pub export_interval_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct DebugCaptureSnapshot {
    pub enabled: bool,
    pub mode: onair_core::config::DebugCaptureMode,
    pub directory: String,
}

#[derive(Debug, Serialize)]
pub struct InspectorSnapshot {
    pub enabled: bool,
    pub retention_requests: usize,
    pub allow_remote: bool,
}

#[derive(Debug, Serialize)]
pub struct HealthConfigSnapshot {
    pub active: bool,
    pub interval_ms: u64,
    pub timeout_ms: u64,
    pub path: String,
}

#[derive(Debug, Serialize)]
pub struct RoutingSnapshot {
    pub strategy: &'static str,
    pub fallback_attempts: usize,
}

#[derive(Debug, Serialize)]
pub struct RouteSnapshot {
    pub public: Option<String>,
    pub path: Option<String>,
    pub expose: Vec<String>,
    pub context_length: Option<u64>,
    pub context_length_source: &'static str,
    pub tool_schema_mode: onair_core::config::ToolSchemaMode,
    pub responses_store: onair_core::config::ResponsesStorePolicy,
    pub responses_max_output_tokens: onair_core::config::ResponsesMaxOutputTokensPolicy,
    pub chat_stream_usage: onair_core::config::ChatStreamUsagePolicy,
    pub backends: Vec<RouteBindingSnapshot>,
}

#[derive(Debug, Serialize)]
pub struct RouteBindingSnapshot {
    pub backend: String,
    pub backend_model: String,
}

#[derive(Debug, Serialize)]
pub struct ClientSnapshot {
    pub id: String,
    pub models: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct BackendSnapshot {
    pub id: String,
    pub base_url: String,
    pub api_key_configured: bool,
    pub timeout_ms: u64,
    pub tool_schema_mode: onair_core::config::ToolSchemaMode,
    pub responses_store: onair_core::config::ResponsesStorePolicy,
    pub responses_max_output_tokens: onair_core::config::ResponsesMaxOutputTokensPolicy,
    pub chat_stream_usage: onair_core::config::ChatStreamUsagePolicy,
    pub supports: Vec<String>,
    pub weight: u32,
}

#[derive(Debug, Serialize)]
pub struct PublicModelSnapshot {
    pub public: String,
    pub context_length: Option<u64>,
    pub context_length_source: &'static str,
    pub context_length_last_fetch_unix_ms: Option<u64>,
    pub clients: Vec<String>,
    pub routes: Vec<RouteBindingSnapshot>,
}

#[derive(Debug, Serialize)]
pub struct BackendHealthSnapshot {
    pub backend: String,
    pub status: &'static str,
    pub successes: u64,
    pub failures: u64,
    pub traffic_successes: u64,
    pub traffic_failures: u64,
    pub probe_successes: u64,
    pub probe_failures: u64,
    pub consecutive_failures: u64,
    pub last_success_unix_ms: Option<u64>,
    pub last_failure_unix_ms: Option<u64>,
    pub last_observed_unix_ms: Option<u64>,
    pub last_status: Option<u16>,
    pub last_error_kind: Option<String>,
    pub last_latency_ms: Option<u64>,
    pub last_source: Option<&'static str>,
}

pub fn runtime_snapshot(
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
        routes: config.routes.len(),
        inspector_retained_requests,
        telemetry: telemetry_snapshot(&config.telemetry),
    }
}

pub fn context_length_source(policy: &ContextLengthPolicy) -> &'static str {
    match policy {
        ContextLengthPolicy::None => "none",
        ContextLengthPolicy::Static(_) => "static",
        ContextLengthPolicy::Upstream { .. } => "upstream",
    }
}

pub fn config_snapshot(config: &Config) -> OperatorConfigSnapshot {
    OperatorConfigSnapshot {
        server: server_snapshot(&config.server),
        telemetry: telemetry_snapshot(&config.telemetry),
        debug_capture: debug_capture_snapshot(&config.debug_capture),
        inspector: inspector_snapshot(&config.inspector),
        health: health_config_snapshot(&config.health),
        routing: routing_snapshot(&config.routing),
        clients: clients_snapshot(&config.clients),
        backends: config.backends.iter().map(backend_snapshot).collect(),
        routes: config.routes.iter().map(route_snapshot).collect(),
    }
}

pub fn models_snapshot(
    config: &Config,
    context_sizes: &onair_core::ContextSizeCache,
) -> OperatorModelsSnapshot {
    let mut models = BTreeMap::<String, PublicModelSnapshot>::new();
    for (public, resolved) in config.public_model_context_lengths_with_cache(context_sizes) {
        let (context_length, context_length_source, context_length_last_fetch_unix_ms) =
            match &resolved {
                ResolvedContextLength::None => (None, "none", None),
                ResolvedContextLength::Static { n_ctx } => (Some(*n_ctx), "static", None),
                ResolvedContextLength::Upstream { n_ctx } => {
                    let last_fetch = context_sizes
                        .entry(&public)
                        .and_then(|entry| entry.last_success_unix_ms);
                    (*n_ctx, "upstream", last_fetch)
                }
            };
        models.insert(
            public.clone(),
            PublicModelSnapshot {
                public,
                context_length,
                context_length_source,
                context_length_last_fetch_unix_ms,
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

    for route in &config.routes {
        if let RouteKey::Public(public) = &route.key
            && let Some(public_model) = models.get_mut(public)
        {
            for binding in &route.backends {
                public_model.routes.push(RouteBindingSnapshot {
                    backend: binding.backend_id.clone(),
                    backend_model: binding.backend_model.clone(),
                });
            }
        }
    }

    OperatorModelsSnapshot {
        public_models: models.into_values().collect(),
        clients: clients_snapshot(&config.clients),
    }
}

pub fn health_snapshot(config: &Config, health: &BackendHealthStore) -> OperatorHealthSnapshot {
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
        mode: config.mode,
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
            RoutingStrategy::RoundRobin => "round_robin",
            RoutingStrategy::WeightedRandom => "weighted_random",
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
        tool_schema_mode: backend.tool_schema_mode,
        responses_store: backend.responses_store,
        responses_max_output_tokens: backend.responses_max_output_tokens,
        chat_stream_usage: backend.chat_stream_usage,
        supports: sorted_strings(&backend.supports),
        weight: backend.weight,
    }
}

fn route_snapshot(route: &ResolvedRoute) -> RouteSnapshot {
    let (public, path) = match &route.key {
        RouteKey::Public(p) => (Some(p.clone()), None),
        RouteKey::Path(p) => (None, Some(p.clone())),
    };
    RouteSnapshot {
        public,
        path,
        expose: sorted_strings(&route.expose),
        context_length: static_context_length(&route.context_length),
        context_length_source: context_length_source(&route.context_length),
        tool_schema_mode: route.tool_schema_mode,
        responses_store: route.responses_store,
        responses_max_output_tokens: route.responses_max_output_tokens,
        chat_stream_usage: route.chat_stream_usage,
        backends: route
            .backends
            .iter()
            .map(|binding| RouteBindingSnapshot {
                backend: binding.backend_id.clone(),
                backend_model: binding.backend_model.clone(),
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

fn static_context_length(policy: &ContextLengthPolicy) -> Option<u64> {
    match policy {
        ContextLengthPolicy::None => None,
        ContextLengthPolicy::Static(value) => Some(*value),
        ContextLengthPolicy::Upstream { .. } => None,
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

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}
