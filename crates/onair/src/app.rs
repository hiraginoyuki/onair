use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, DefaultBodyLimit, OriginalUri, Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Json, Router};
use reqwest::Client;
use reqwest::redirect::Policy;
use tokio::sync::watch;
use tower_http::trace::TraceLayer;
use tracing::{debug, warn};
use url::form_urlencoded;

use onair_core::ContextSizeCache;
use onair_core::auth::{Identity, authenticate};
use onair_core::config::{Config, ConfigStore, ContextLengthSpec};
use onair_core::error::{ApiError, Result};
use onair_core::openai;
use onair_obs::metrics::{MetricLabels, Metrics, RequestTimer};
use onair_obs::observe::{
    BackendHealthStore, ClientInfo, ContextSizeRefreshTask, HealthProbeTask,
    InspectorRequestRecord, InspectorStore, inspector,
};
use onair_proxy::operator;
use onair_proxy::proxy::{self, PropagateRequestIdLayer};
use onair_proxy::proxy_state::ProxyState;
use onair_proxy::routing::RoundRobinCounters;

const DEFAULT_INSPECTOR_SNAPSHOT_LIMIT: usize = 1_000;
const MAX_INSPECTOR_SNAPSHOT_LIMIT: usize = 10_000;

pub struct AppState {
    pub config: ConfigStore,
    pub health: BackendHealthStore,
    pub inspector: InspectorStore,
    pub metrics: Metrics,
    pub context_sizes: ContextSizeCache,
    proxy_state: Arc<ProxyState>,
    shutdown: watch::Sender<bool>,
    _health_probe: HealthProbeTask,
    _context_size_refresh: ContextSizeRefreshTask,
    started: Instant,
    started_at_unix_ms: u64,
}

impl AppState {
    pub fn new(config: Config, metrics: Metrics, shutdown: watch::Sender<bool>) -> Result<Self> {
        let started = Instant::now();
        let started_at_unix_ms = unix_millis();
        let inspector = InspectorStore::from_config(&config.inspector)?;
        let config = ConfigStore::new(config);
        let http = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(std::time::Duration::from_secs(10))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .build()?;
        let health = BackendHealthStore::new();
        let health_probe = HealthProbeTask::start(config.clone(), http.clone(), health.clone());
        let round_robin = RoundRobinCounters::new();
        let context_sizes = ContextSizeCache::new();
        let context_size_refresh =
            ContextSizeRefreshTask::start(config.clone(), http.clone(), context_sizes.clone());
        let proxy_state = Arc::new(ProxyState::from_app_state(
            Arc::new(config.clone()),
            Arc::new(http),
            Arc::new(inspector.clone()),
            Arc::new(metrics.clone()),
            Arc::new(health.clone()),
            Arc::new(round_robin),
            shutdown.subscribe(),
        ));
        Ok(Self {
            config,
            health,
            inspector,
            metrics,
            context_sizes,
            proxy_state,
            shutdown,
            _health_probe: health_probe,
            _context_size_refresh: context_size_refresh,
            started,
            started_at_unix_ms,
        })
    }

    fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn shutdown_receiver(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    pub fn proxy_state(&self) -> Arc<ProxyState> {
        self.proxy_state.clone()
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let body_limit = state.config.snapshot().server.request_body_limit_bytes;
    Router::new()
        .route("/healthz", get(healthz))
        .route("/props", get(props))
        .route("/v1/props", get(props))
        .route("/_onair/inspector", get(inspector_ui))
        .route("/_onair/inspector/events", get(inspector_events))
        .route("/_onair/inspector/requests", get(inspector_requests))
        .route(
            "/_onair/inspector/requests/{*request_id}",
            get(inspector_request),
        )
        .route("/_onair/operator/config", get(operator_config))
        .route("/_onair/operator/models", get(operator_models))
        .route("/_onair/operator/health", get(operator_health))
        .route("/_onair/operator/runtime", get(operator_runtime))
        .route("/v1/models", get(models))
        .route("/v1/models/{*model}", get(model))
        .route("/v1/{*path}", any(v1_proxy))
        .fallback(fallback)
        .layer(DefaultBodyLimit::max(body_limit))
        .layer(PropagateRequestIdLayer)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn healthz() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn models(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response<Body> {
    authed_handler(state, &headers, "models", "none", |identity, available| {
        let models = identity
            .models
            .iter()
            .cloned()
            .filter_map(|model| {
                available.get(&model).map(|resolved| match resolved {
                    ContextLengthSpec::None => openai::ModelObject::new(model, None),
                    ContextLengthSpec::Static { n_ctx } => {
                        openai::ModelObject::new_static(model, *n_ctx)
                    }
                    ContextLengthSpec::Upstream {
                        n_ctx: Some(n_ctx), ..
                    } => openai::ModelObject::new(model, Some(*n_ctx)),
                    ContextLengthSpec::Upstream { n_ctx: None, .. } => {
                        openai::ModelObject::new(model, None)
                    }
                })
            })
            .collect::<Vec<_>>();
        openai::models_response(models).into_response()
    })
    .await
}

async fn model(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(model): Path<String>,
) -> Response<Body> {
    authed_handler(
        state,
        &headers,
        "models_retrieve",
        &model,
        |_identity, available| match available.get(&model) {
            Some(ContextLengthSpec::None) => {
                openai::model_response(model.clone(), None).into_response()
            }
            Some(ContextLengthSpec::Static { n_ctx }) => {
                openai::model_response_with_n_ctx_train(model.clone(), *n_ctx).into_response()
            }
            Some(ContextLengthSpec::Upstream { n_ctx, .. }) => {
                openai::model_response(model.clone(), *n_ctx).into_response()
            }
            None => ApiError::model_not_found(&model).into_response(),
        },
    )
    .await
}

async fn props(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response<Body> {
    let query_model = uri.query().and_then(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(key, _)| key == "model")
            .map(|(_, value)| value.into_owned())
    });
    let label = query_model.clone().unwrap_or_else(|| "none".to_owned());
    authed_handler(
        state,
        &headers,
        "props",
        &label,
        move |_identity, available| match query_model.as_deref() {
            Some(model) => match available.get(model) {
                Some(resolved) => {
                    let n_ctx = match resolved {
                        ContextLengthSpec::None => 0,
                        ContextLengthSpec::Static { n_ctx, .. } => *n_ctx,
                        ContextLengthSpec::Upstream { n_ctx, .. } => n_ctx.unwrap_or(0),
                    };
                    openai::props_response(Some("router"), Some(model.to_owned()), n_ctx)
                        .into_response()
                }
                None => ApiError::model_not_found(model).into_response(),
            },
            None => openai::props_response(Some("router"), Some("llama-server".to_owned()), 0)
                .into_response(),
        },
    )
    .await
}

async fn authed_handler<F>(
    state: Arc<AppState>,
    headers: &HeaderMap,
    route_name: &'static str,
    model_label: &str,
    handle: F,
) -> Response<Body>
where
    F: FnOnce(&Identity, &BTreeMap<String, ContextLengthSpec>) -> Response<Body>,
{
    let timer = RequestTimer::start();
    let config = state.config.snapshot();
    match authenticate(headers, &config.clients) {
        Ok(identity) => {
            let available = config.public_model_context_lengths_with_cache(&state.context_sizes);
            let response = handle(&identity, &available);
            state.metrics.record_request(
                &model_route_labels(route_name, &identity.id, model_label),
                response.status().as_u16(),
                timer.elapsed(),
            );
            debug!(
                identity = %identity.id,
                route = route_name,
                model = model_label,
                response_status = response.status().as_u16(),
                "authed response completed"
            );
            response
        }
        Err(error) => {
            state.metrics.record_request(
                &model_route_labels(route_name, "unknown", model_label),
                error.status.as_u16(),
                timer.elapsed(),
            );
            error.into_response()
        }
    }
}

async fn v1_proxy(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    method: Method,
    OriginalUri(uri): OriginalUri,
    body: Bytes,
) -> Response<Body> {
    let proxy_state = state.proxy_state();
    match proxy::proxy_v1(
        proxy_state,
        peer_addr,
        headers.clone(),
        method,
        uri,
        body,
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            warn!(status = error.status.as_u16(), kind = %error.kind, "request failed before proxy response");
            error.into_response()
        }
    }
}

async fn inspector_ui(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
) -> Response<Body> {
    if let Some(response) = local_operator_gate(&state, &request) {
        return response;
    }

    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(inspector::ui_html()))
        .expect("inspector UI response builder is valid");
    add_inspector_headers(&mut response);
    response.headers_mut().insert(
        CONTENT_SECURITY_POLICY,
        "default-src 'none'; connect-src 'self'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
            .parse()
            .expect("static content security policy is valid"),
    );
    response
}

async fn inspector_requests(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
) -> Response<Body> {
    if let Some(response) = local_operator_gate(&state, &request) {
        return response;
    }

    let limit = inspector_limit(&request, &["limit"]);
    let mut records = state.inspector.records_limited(limit);
    records.reverse();
    let mut response = Json(records).into_response();
    add_inspector_headers(&mut response);
    response
}

async fn inspector_request(
    State(state): State<Arc<AppState>>,
    Path(request_id): Path<String>,
    request: Request<Body>,
) -> Response<Body> {
    if let Some(response) = local_operator_gate(&state, &request) {
        return response;
    }

    let mut response = state
        .inspector
        .get(&request_id)
        .map(Json)
        .map(IntoResponse::into_response)
        .unwrap_or_else(|| {
            ApiError::not_found("The requested inspector record does not exist.").into_response()
        });
    add_inspector_headers(&mut response);
    response
}

async fn inspector_events(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
) -> Response<Body> {
    if let Some(response) = local_operator_gate(&state, &request) {
        return response;
    }

    let snapshot_limit = inspector_limit(&request, &["snapshot_limit", "limit"]);
    let mut receiver = state.inspector.subscribe();
    let mut shutdown = state.shutdown_receiver();
    let mut snapshot = state.inspector.records_limited(snapshot_limit);
    snapshot.reverse();
    let stream = async_stream::stream! {
        for record in snapshot {
            yield Ok::<_, Infallible>(inspector_sse_event("snapshot", record));
        }

        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                message = receiver.recv() => {
                    match message {
                        Ok(record) => yield Ok::<_, Infallible>(inspector_sse_event("request", record)),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            let event = Event::default()
                                .event("lagged")
                                .data(skipped.to_string());
                            yield Ok::<_, Infallible>(event);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
    };

    let mut response = Sse::new(stream)
        .keep_alive(
            KeepAlive::default()
                .interval(Duration::from_secs(15))
                .text("keepalive"),
        )
        .into_response();
    add_inspector_headers(&mut response);
    response
}

fn inspector_sse_event(event_name: &'static str, record: InspectorRequestRecord) -> Event {
    let data =
        serde_json::to_string(&record).expect("InspectorRequestRecord is always serializable");
    Event::default().event(event_name).data(data)
}

fn inspector_limit(request: &Request<Body>, names: &[&str]) -> usize {
    let requested = request.uri().query().and_then(|query| {
        form_urlencoded::parse(query.as_bytes()).find_map(|(key, value)| {
            names
                .contains(&key.as_ref())
                .then(|| value.parse::<usize>().ok())
                .flatten()
        })
    });

    requested
        .unwrap_or(DEFAULT_INSPECTOR_SNAPSHOT_LIMIT)
        .clamp(1, MAX_INSPECTOR_SNAPSHOT_LIMIT)
}

async fn operator_runtime(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
) -> Response<Body> {
    if let Some(response) = local_operator_gate(&state, &request) {
        return response;
    }

    operator::runtime_snapshot(
        &state.config.snapshot(),
        state.started_at_unix_ms,
        state.uptime(),
        state.inspector.retained_len(),
    )
    .into_response()
}

async fn operator_config(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
) -> Response<Body> {
    if let Some(response) = local_operator_gate(&state, &request) {
        return response;
    }

    operator::config_snapshot(&state.config.snapshot()).into_response()
}

async fn operator_models(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
) -> Response<Body> {
    if let Some(response) = local_operator_gate(&state, &request) {
        return response;
    }

    operator::models_snapshot(&state.config.snapshot(), &state.context_sizes).into_response()
}

async fn operator_health(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
) -> Response<Body> {
    if let Some(response) = local_operator_gate(&state, &request) {
        return response;
    }

    operator::health_snapshot(&state.config.snapshot(), &state.health).into_response()
}

fn local_operator_gate(state: &AppState, request: &Request<Body>) -> Option<Response<Body>> {
    let config = state.config.snapshot();
    if !config.inspector.enabled {
        return Some(inspector_not_found());
    }
    if !config.inspector.allow_remote {
        let peer_addr = request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|connect_info| connect_info.0);
        let client_info = ClientInfo::from_headers(
            request.headers(),
            peer_addr,
            &config.server.trusted_proxy_cidrs,
        );
        if !client_info.effective_client_is_loopback() {
            return Some(inspector_not_found());
        }
    }
    None
}

fn inspector_not_found() -> Response<Body> {
    let mut response =
        ApiError::not_found("The requested endpoint does not exist.").into_response();
    add_inspector_headers(&mut response);
    response
}

fn add_inspector_headers(response: &mut Response<Body>) {
    response.headers_mut().insert(
        CACHE_CONTROL,
        "no-store".parse().expect("static header is valid"),
    );
    response.headers_mut().insert(
        "x-content-type-options",
        "nosniff".parse().expect("static header is valid"),
    );
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

async fn fallback(State(state): State<Arc<AppState>>) -> Response<Body> {
    let timer = RequestTimer::start();
    let error = ApiError::not_found("The requested endpoint does not exist.");
    state.metrics.record_request(
        &model_route_labels("fallback", "unknown", "none"),
        error.status.as_u16(),
        timer.elapsed(),
    );
    error.into_response()
}

fn model_route_labels(route: &str, identity: &str, model: &str) -> MetricLabels {
    MetricLabels {
        route: route.to_owned(),
        identity: identity.to_owned(),
        public_model: model.to_owned(),
        backend: "none".to_owned(),
        stream: false,
    }
}

#[cfg(test)]
mod tests;
