use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, OriginalUri, Path, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use reqwest::Client;
use tower_http::trace::TraceLayer;
use tracing::{debug, warn};

use crate::auth::authenticate;
use crate::config::{Config, ConfigStore};
use crate::error::{ApiError, Result};
use crate::metrics::{MetricLabels, Metrics, RequestTimer};
use crate::openai;
use crate::proxy;

pub struct AppState {
    pub config: ConfigStore,
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
            config: ConfigStore::new(config),
            http,
            metrics,
        })
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let body_limit = state.config.snapshot().server.request_body_limit_bytes;
    Router::new()
        .route("/healthz", get(healthz))
        .route("/props", get(props))
        .route("/v1/props", get(props))
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::body::{Body, to_bytes};
    use axum::extract::State;
    use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
    use axum::http::{Request, StatusCode};
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::{Value, json};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;
    use tower::ServiceExt;

    use super::*;
    use crate::config::{
        Config, DebugCaptureConfig, ModelRoute, ResolvedBackend, ResolvedClient, RoutingConfig,
        RoutingStrategy, ServerConfig, TelemetryConfig,
    };

    const CLIENT_KEY: &str = "sk-test";
    const PUBLIC_MODEL: &str = "gpt-public";
    const BACKEND_MODEL: &str = "backend-private";

    #[tokio::test]
    async fn responses_forwards_prompt_cache_fields_and_rewrites_model() {
        let backend = TestBackend::spawn("backend-a").await;
        let state = test_state(
            RoutingStrategy::Priority,
            vec![test_backend("backend-a", backend.base_url())],
        );
        let app = router(state);

        let response = app
            .oneshot(json_request(
                "/v1/responses",
                json!({
                    "model": PUBLIC_MODEL,
                    "input": "hello",
                    "prompt_cache_key": "tenant-a:prefix",
                    "prompt_cache_retention": "24h"
                }),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let response_body = json_body(response).await;
        assert_eq!(response_body["model"], PUBLIC_MODEL);
        assert_eq!(
            response_body["usage"]["input_tokens_details"]["cached_tokens"],
            7
        );

        let captured = backend.requests();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0]["model"], BACKEND_MODEL);
        assert_eq!(captured[0]["prompt_cache_key"], "tenant-a:prefix");
        assert_eq!(captured[0]["prompt_cache_retention"], "24h");

        backend.abort();
    }

    #[tokio::test]
    async fn sticky_routing_reuses_backend_for_same_prompt_cache_key() {
        let backend_a = TestBackend::spawn("backend-a").await;
        let backend_b = TestBackend::spawn("backend-b").await;
        let state = test_state(
            RoutingStrategy::Sticky,
            vec![
                test_backend("backend-a", backend_a.base_url()),
                test_backend("backend-b", backend_b.base_url()),
            ],
        );
        let app = router(state);

        for _ in 0..2 {
            let response = app
                .clone()
                .oneshot(json_request(
                    "/v1/responses",
                    json!({
                        "model": PUBLIC_MODEL,
                        "input": "same prefix",
                        "prompt_cache_key": "cache-affinity-key"
                    }),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }

        let hits_a = backend_a.hits();
        let hits_b = backend_b.hits();
        assert!(
            (hits_a == 2 && hits_b == 0) || (hits_a == 0 && hits_b == 2),
            "expected sticky routing to select one backend twice, got a={hits_a}, b={hits_b}"
        );

        backend_a.abort();
        backend_b.abort();
    }

    #[tokio::test]
    async fn disallowed_model_returns_404_without_calling_backend() {
        let backend = TestBackend::spawn("backend-a").await;
        let state = test_state(
            RoutingStrategy::Priority,
            vec![test_backend("backend-a", backend.base_url())],
        );
        let app = router(state);

        let response = app
            .oneshot(json_request(
                "/v1/responses",
                json!({
                    "model": "not-allowed",
                    "input": "hello"
                }),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(backend.hits(), 0);

        backend.abort();
    }

    #[tokio::test]
    async fn model_required_endpoint_without_model_returns_400_without_calling_backend() {
        let backend = TestBackend::spawn("backend-a").await;
        let state = test_state(
            RoutingStrategy::Priority,
            vec![test_backend("backend-a", backend.base_url())],
        );
        let app = router(state);

        let response = app
            .oneshot(json_request(
                "/v1/responses",
                json!({
                    "input": "hello"
                }),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let response_body = json_body(response).await;
        assert_eq!(response_body["error"]["param"], "model");
        assert_eq!(backend.hits(), 0);

        backend.abort();
    }

    #[tokio::test]
    async fn debug_capture_writes_inbound_and_upstream_request_bodies() {
        let backend = TestBackend::spawn("backend-a").await;
        let capture_dir = temp_capture_dir("request-bodies");
        let state = test_state_with_debug_capture(
            RoutingStrategy::Priority,
            vec![test_backend("backend-a", backend.base_url())],
            DebugCaptureConfig {
                enabled: true,
                directory: capture_dir.clone(),
            },
        );
        let app = router(state);

        let response = app
            .oneshot(json_request(
                "/v1/responses?metadata=keep",
                json!({
                    "model": PUBLIC_MODEL,
                    "input": "long context goes here"
                }),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let _ = json_body(response).await;

        let capture_path = only_capture_path(&capture_dir);
        let inbound_body: Value =
            serde_json::from_slice(&std::fs::read(capture_path.join("inbound.body")).unwrap())
                .unwrap();
        let upstream_body: Value =
            serde_json::from_slice(&std::fs::read(capture_path.join("upstream.body")).unwrap())
                .unwrap();
        let metadata: Value =
            serde_json::from_slice(&std::fs::read(capture_path.join("metadata.json")).unwrap())
                .unwrap();

        assert_eq!(inbound_body["model"], PUBLIC_MODEL);
        assert_eq!(upstream_body["model"], BACKEND_MODEL);
        assert_eq!(metadata["identity"], "dev");
        assert_eq!(metadata["route"], "responses");
        assert_eq!(metadata["backend"], "backend-a");
        assert_eq!(metadata["client_query"], "metadata=keep");
        assert_eq!(metadata["outcome"]["kind"], "success");
        assert_eq!(metadata["outcome"]["upstream_status"], 200);

        backend.abort();
        std::fs::remove_dir_all(capture_dir).unwrap();
    }

    #[tokio::test]
    async fn models_respect_context_length_output_policy() {
        let state = test_state_with_client_models(
            RoutingStrategy::Priority,
            vec![ResolvedBackend {
                id: "metadata-only".to_owned(),
                base_url: "http://127.0.0.1:9".to_owned(),
                api_key: None,
                timeout: std::time::Duration::from_secs(5),
                capabilities: btree_set(["responses"]),
                models: vec![
                    ModelRoute {
                        public: PUBLIC_MODEL.to_owned(),
                        backend: BACKEND_MODEL.to_owned(),
                        context_length: Some(131_072),
                        endpoints: btree_set(["responses"]),
                    },
                    ModelRoute {
                        public: "gpt-no-context".to_owned(),
                        backend: "backend-no-context".to_owned(),
                        context_length: None,
                        endpoints: btree_set(["responses"]),
                    },
                ],
            }],
            btree_set([PUBLIC_MODEL, "gpt-no-context"]),
        );
        let app = router(state);

        let response = app.clone().oneshot(authed_get("/v1/models")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response_body = json_body(response).await;
        let models = response_body["data"].as_array().unwrap();
        let model_with_context = models
            .iter()
            .find(|model| model["id"] == PUBLIC_MODEL)
            .unwrap();
        assert_eq!(model_with_context["meta"]["n_ctx"], 131_072);
        assert_eq!(model_with_context["meta"]["n_ctx_train"], 131_072);
        assert!(model_with_context.get("context_length").is_none());
        let model_without_context = models
            .iter()
            .find(|model| model["id"] == "gpt-no-context")
            .unwrap();
        assert!(model_without_context.get("meta").is_none());

        let response = app
            .clone()
            .oneshot(authed_get(&format!("/v1/models/{PUBLIC_MODEL}")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response_body = json_body(response).await;
        assert_eq!(response_body["meta"]["n_ctx"], 131_072);
        assert_eq!(response_body["meta"]["n_ctx_train"], 131_072);

        let response = app
            .oneshot(authed_get(&format!("/props?model={PUBLIC_MODEL}")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response_body = json_body(response).await;
        assert_eq!(
            response_body["default_generation_settings"]["n_ctx"],
            131_072
        );
        assert_eq!(response_body["model_alias"], PUBLIC_MODEL);

        let response = router(test_state_with_client_models(
            RoutingStrategy::Priority,
            vec![],
            BTreeSet::new(),
        ))
        .oneshot(authed_get("/props"))
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response_body = json_body(response).await;
        assert_eq!(response_body["default_generation_settings"]["n_ctx"], 0);
        assert_eq!(response_body["model_alias"], "llama-server");
        assert_eq!(response_body["role"], "router");
    }

    fn test_state(strategy: RoutingStrategy, backends: Vec<ResolvedBackend>) -> Arc<AppState> {
        test_state_with_client_models(strategy, backends, btree_set([PUBLIC_MODEL]))
    }

    fn test_state_with_debug_capture(
        strategy: RoutingStrategy,
        backends: Vec<ResolvedBackend>,
        debug_capture: DebugCaptureConfig,
    ) -> Arc<AppState> {
        test_state_with_config(strategy, backends, btree_set([PUBLIC_MODEL]), debug_capture)
    }

    fn test_state_with_client_models(
        strategy: RoutingStrategy,
        backends: Vec<ResolvedBackend>,
        client_models: BTreeSet<String>,
    ) -> Arc<AppState> {
        test_state_with_config(
            strategy,
            backends,
            client_models,
            DebugCaptureConfig::default(),
        )
    }

    fn test_state_with_config(
        strategy: RoutingStrategy,
        backends: Vec<ResolvedBackend>,
        client_models: BTreeSet<String>,
        debug_capture: DebugCaptureConfig,
    ) -> Arc<AppState> {
        Arc::new(
            AppState::new(
                Config {
                    server: ServerConfig::default(),
                    telemetry: TelemetryConfig::default(),
                    debug_capture,
                    routing: RoutingConfig { strategy },
                    clients: vec![ResolvedClient {
                        id: "dev".to_owned(),
                        api_key: CLIENT_KEY.to_owned(),
                        models: client_models,
                    }],
                    backends,
                },
                Metrics::new(),
            )
            .unwrap(),
        )
    }

    fn test_backend(id: &str, base_url: String) -> ResolvedBackend {
        ResolvedBackend {
            id: id.to_owned(),
            base_url,
            api_key: None,
            timeout: std::time::Duration::from_secs(5),
            capabilities: btree_set(["responses", "streaming"]),
            models: vec![ModelRoute {
                public: PUBLIC_MODEL.to_owned(),
                backend: BACKEND_MODEL.to_owned(),
                context_length: None,
                endpoints: btree_set(["responses"]),
            }],
        }
    }

    fn json_request(uri: &str, body: Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(AUTHORIZATION, format!("Bearer {CLIENT_KEY}"))
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn authed_get(uri: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(uri)
            .header(AUTHORIZATION, format!("Bearer {CLIENT_KEY}"))
            .body(Body::empty())
            .unwrap()
    }

    async fn json_body(response: Response<Body>) -> Value {
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn btree_set<const N: usize>(values: [&str; N]) -> BTreeSet<String> {
        values.into_iter().map(str::to_owned).collect()
    }

    fn temp_capture_dir(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "onair-debug-capture-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn only_capture_path(capture_dir: &std::path::Path) -> std::path::PathBuf {
        let entries = std::fs::read_dir(capture_dir)
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(entries.len(), 1);
        entries[0].path()
    }

    #[derive(Clone)]
    struct BackendState {
        name: String,
        requests: Arc<Mutex<Vec<Value>>>,
        hits: Arc<AtomicUsize>,
    }

    struct TestBackend {
        address: SocketAddr,
        state: BackendState,
        handle: JoinHandle<()>,
    }

    impl TestBackend {
        async fn spawn(name: &str) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let state = BackendState {
                name: name.to_owned(),
                requests: Arc::new(Mutex::new(Vec::new())),
                hits: Arc::new(AtomicUsize::new(0)),
            };
            let app = Router::new()
                .route("/v1/responses", post(backend_responses))
                .with_state(state.clone());
            let handle = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            Self {
                address,
                state,
                handle,
            }
        }

        fn base_url(&self) -> String {
            format!("http://{}", self.address)
        }

        fn hits(&self) -> usize {
            self.state.hits.load(Ordering::SeqCst)
        }

        fn requests(&self) -> Vec<Value> {
            self.state.requests.lock().unwrap().clone()
        }

        fn abort(self) {
            self.handle.abort();
        }
    }

    async fn backend_responses(
        State(state): State<BackendState>,
        Json(payload): Json<Value>,
    ) -> Json<Value> {
        state.hits.fetch_add(1, Ordering::SeqCst);
        state.requests.lock().unwrap().push(payload.clone());
        Json(json!({
            "id": format!("resp_{}", state.name),
            "object": "response",
            "model": payload["model"],
            "output": [],
            "usage": {
                "input_tokens": 13,
                "input_tokens_details": {
                    "cached_tokens": 7
                },
                "output_tokens": 3
            }
        }))
    }
}
