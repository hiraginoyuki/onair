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

use crate::auth::authenticate;
use crate::config::{Config, ConfigStore};
use crate::error::{ApiError, Result};
use crate::metrics::{MetricLabels, Metrics, RequestTimer};
use crate::observe::{
    BackendHealthStore, ClientInfo, HealthProbeTask, InspectorRequestRecord, InspectorStore,
    inspector,
};
use crate::openai;
use crate::operator;
use crate::proxy;

const DEFAULT_INSPECTOR_SNAPSHOT_LIMIT: usize = 1_000;
const MAX_INSPECTOR_SNAPSHOT_LIMIT: usize = 10_000;

pub struct AppState {
    pub config: ConfigStore,
    pub http: Client,
    pub health: BackendHealthStore,
    pub inspector: InspectorStore,
    pub metrics: Metrics,
    shutdown: watch::Sender<bool>,
    _health_probe: HealthProbeTask,
    started: Instant,
    started_at_unix_ms: u64,
}

impl AppState {
    pub fn new(config: Config, metrics: Metrics, shutdown: watch::Sender<bool>) -> Result<Self> {
        let started = Instant::now();
        let started_at_unix_ms = unix_millis();
        let config = ConfigStore::new(config);
        let http = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(std::time::Duration::from_secs(10))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .build()?;
        let health = BackendHealthStore::new();
        let health_probe = HealthProbeTask::start(config.clone(), http.clone(), health.clone());
        Ok(Self {
            config,
            http,
            health,
            inspector: InspectorStore::new(),
            metrics,
            shutdown,
            _health_probe: health_probe,
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
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn healthz() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn models(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response<Body> {
    let timer = RequestTimer::start();
    let config = state.config.snapshot();
    let response = match authenticate(&headers, &config.clients) {
        Ok(identity) => {
            let available = config.public_model_context_lengths();
            let identity_id = identity.id.clone();
            let models = identity
                .models
                .into_iter()
                .filter_map(|model| {
                    available
                        .get(&model)
                        .copied()
                        .map(|context_length| openai::ModelObject::new(model, context_length))
                })
                .collect::<Vec<_>>();
            let model_count = models.len();
            let response = openai::models_response(models).into_response();
            state.metrics.record_request(
                &model_route_labels("models", &identity_id, "none"),
                response.status().as_u16(),
                timer.elapsed(),
            );
            debug!(
                identity = %identity_id,
                response_status = response.status().as_u16(),
                model_count,
                "models response completed"
            );
            response
        }
        Err(error) => {
            state.metrics.record_request(
                &model_route_labels("models", "unknown", "none"),
                error.status.as_u16(),
                timer.elapsed(),
            );
            error.into_response()
        }
    };
    proxy::attach_request_id(response, &headers)
}

async fn model(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(model): Path<String>,
) -> Response<Body> {
    let timer = RequestTimer::start();
    let config = state.config.snapshot();
    let response = match authenticate(&headers, &config.clients) {
        Ok(identity) => {
            let available = config.public_model_context_lengths();
            if identity.models.contains(&model) {
                if let Some(context_length) = available.get(&model).copied() {
                    let response =
                        openai::model_response(model.clone(), context_length).into_response();
                    state.metrics.record_request(
                        &model_route_labels("models_retrieve", &identity.id, &model),
                        response.status().as_u16(),
                        timer.elapsed(),
                    );
                    debug!(
                        identity = %identity.id,
                        model = %model,
                        context_length = ?context_length,
                        response_status = response.status().as_u16(),
                        "model response completed"
                    );
                    response
                } else {
                    let error = ApiError::model_not_found(&model);
                    state.metrics.record_request(
                        &model_route_labels("models_retrieve", &identity.id, &model),
                        error.status.as_u16(),
                        timer.elapsed(),
                    );
                    error.into_response()
                }
            } else {
                let error = ApiError::model_not_found(&model);
                state.metrics.record_request(
                    &model_route_labels("models_retrieve", &identity.id, &model),
                    error.status.as_u16(),
                    timer.elapsed(),
                );
                error.into_response()
            }
        }
        Err(error) => {
            state.metrics.record_request(
                &model_route_labels("models_retrieve", "unknown", &model),
                error.status.as_u16(),
                timer.elapsed(),
            );
            error.into_response()
        }
    };
    proxy::attach_request_id(response, &headers)
}

async fn props(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response<Body> {
    let timer = RequestTimer::start();
    let query_model = uri.query().and_then(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(key, _)| key == "model")
            .map(|(_, value)| value.into_owned())
    });

    let config = state.config.snapshot();
    let response = match authenticate(&headers, &config.clients) {
        Ok(identity) => {
            let available = config.public_model_context_lengths();
            let response = match query_model.as_deref() {
                Some(model) => {
                    if identity.models.contains(model) {
                        if let Some(context_length) = available.get(model).copied() {
                            openai::props_response(
                                Some("router"),
                                Some(model.to_owned()),
                                context_length.unwrap_or(0),
                            )
                            .into_response()
                        } else {
                            ApiError::model_not_found(model).into_response()
                        }
                    } else {
                        ApiError::model_not_found(model).into_response()
                    }
                }
                None => openai::props_response(Some("router"), Some("llama-server".to_owned()), 0)
                    .into_response(),
            };
            state.metrics.record_request(
                &model_route_labels(
                    "props",
                    &identity.id,
                    query_model.as_deref().unwrap_or("none"),
                ),
                response.status().as_u16(),
                timer.elapsed(),
            );
            debug!(
                identity = %identity.id,
                response_status = response.status().as_u16(),
                model = query_model.as_deref().unwrap_or("none"),
                "props response completed"
            );
            response
        }
        Err(error) => {
            state.metrics.record_request(
                &model_route_labels("props", "unknown", query_model.as_deref().unwrap_or("none")),
                error.status.as_u16(),
                timer.elapsed(),
            );
            error.into_response()
        }
    };
    proxy::attach_request_id(response, &headers)
}

async fn v1_proxy(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    method: Method,
    OriginalUri(uri): OriginalUri,
    body: Bytes,
) -> Response<Body> {
    match proxy::proxy_v1(state, Some(peer_addr), headers.clone(), method, uri, body).await {
        Ok(response) => response,
        Err(error) => {
            warn!(status = error.status.as_u16(), kind = %error.kind, "request failed before proxy response");
            proxy::attach_request_id(error.into_response(), &headers)
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
        .body(Body::from(inspector::UI_HTML))
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
    let snapshot = state.inspector.records_limited(snapshot_limit);
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
    let data = serde_json::to_string(&record).unwrap_or_else(|_| "{}".to_owned());
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

    let config = state.config.snapshot();
    let mut response = Json(operator::runtime_snapshot(
        &config,
        state.started_at_unix_ms,
        state.uptime(),
        state.inspector.retained_len(),
    ))
    .into_response();
    add_inspector_headers(&mut response);
    response
}

async fn operator_config(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
) -> Response<Body> {
    if let Some(response) = local_operator_gate(&state, &request) {
        return response;
    }

    let config = state.config.snapshot();
    let mut response = Json(operator::config_snapshot(&config)).into_response();
    add_inspector_headers(&mut response);
    response
}

async fn operator_models(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
) -> Response<Body> {
    if let Some(response) = local_operator_gate(&state, &request) {
        return response;
    }

    let config = state.config.snapshot();
    let mut response = Json(operator::models_snapshot(&config)).into_response();
    add_inspector_headers(&mut response);
    response
}

async fn operator_health(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
) -> Response<Body> {
    if let Some(response) = local_operator_gate(&state, &request) {
        return response;
    }

    let config = state.config.snapshot();
    let mut response = Json(operator::health_snapshot(&config, &state.health)).into_response();
    add_inspector_headers(&mut response);
    response
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

async fn fallback(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response<Body> {
    let timer = RequestTimer::start();
    let error = ApiError::not_found("The requested endpoint does not exist.");
    state.metrics.record_request(
        &model_route_labels("fallback", "unknown", "none"),
        error.status.as_u16(),
        timer.elapsed(),
    );
    proxy::attach_request_id(error.into_response(), &headers)
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
