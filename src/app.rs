use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, OriginalUri, Path, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use reqwest::Client;
use tower_http::trace::TraceLayer;
use tracing::warn;

use crate::auth::authenticate;
use crate::config::Config;
use crate::error::{ApiError, Result};
use crate::metrics::{MetricLabels, Metrics, RequestTimer};
use crate::openai;
use crate::proxy;

#[derive(Debug)]
pub struct AppState {
    pub config: Config,
    pub http: Client,
    pub metrics: Metrics,
}

impl AppState {
    pub fn new(config: Config, metrics: Metrics) -> Result<Self> {
        let http = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .build()?;
        Ok(Self {
            config,
            http,
            metrics,
        })
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let body_limit = state.config.server.request_body_limit_bytes;
    Router::new()
        .route("/healthz", get(healthz))
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
    let response = match authenticate(&headers, &state.config.clients) {
        Ok(identity) => {
            let available = state.config.public_model_ids();
            let identity_id = identity.id.clone();
            let models = identity
                .models
                .into_iter()
                .filter(|model| available.contains(model))
                .collect::<Vec<_>>();
            let response = openai::models_response(models).into_response();
            state.metrics.record_request(
                &model_route_labels("models", &identity_id, "none"),
                response.status().as_u16(),
                timer.elapsed(),
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
    let response = match authenticate(&headers, &state.config.clients) {
        Ok(identity) => {
            let available = state.config.public_model_ids();
            if identity.models.contains(&model) && available.contains(&model) {
                let response = openai::model_response(model.clone()).into_response();
                state.metrics.record_request(
                    &model_route_labels("models_retrieve", &identity.id, &model),
                    response.status().as_u16(),
                    timer.elapsed(),
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

async fn v1_proxy(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    OriginalUri(uri): OriginalUri,
    body: Bytes,
) -> Response<Body> {
    match proxy::proxy_v1(state, headers.clone(), method, uri, body).await {
        Ok(response) => response,
        Err(error) => {
            warn!(status = error.status.as_u16(), kind = %error.kind, "request failed before proxy response");
            proxy::attach_request_id(error.into_response(), &headers)
        }
    }
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
