use std::sync::Arc;
use std::time::Instant;

use async_stream::try_stream;
use axum::body::{Body, Bytes};
use axum::http::header::{
    ACCEPT, AUTHORIZATION, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE, HeaderName,
    HeaderValue,
};
use axum::http::{HeaderMap, Method, Response, StatusCode, Uri};
use futures_util::{StreamExt, TryStreamExt};
use tracing::{Instrument, info_span, warn};

use crate::app::AppState;
use crate::auth::authenticate;
use crate::error::ApiError;
use crate::metrics::{MetricLabels, RequestTimer};
use crate::openai::{self, SseNormalizer, UsageTotals};
use crate::routing::{self, SelectedRoute};

pub const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

pub async fn proxy_v1(
    state: Arc<AppState>,
    headers: HeaderMap,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> Result<Response<Body>, ApiError> {
    let request_timer = RequestTimer::start();
    let path = uri.path().to_owned();
    let route_name = routing::path_metric_name(&path);

    let identity = match authenticate(&headers, &state.config.clients) {
        Ok(identity) => identity,
        Err(error) => {
            record_preflight_failure(
                &state,
                &route_name,
                "unknown",
                None,
                false,
                error.status,
                request_timer.elapsed(),
            );
            return Err(error);
        }
    };

    let content_type = header_str(&headers, &CONTENT_TYPE).map(str::to_owned);
    let request_shape = openai::inspect_request(&body, content_type.as_deref(), uri.query());
    if let Some(model) = request_shape.model.as_deref() {
        if !identity.models.contains(model) {
            record_preflight_failure(
                &state,
                &route_name,
                &identity.id,
                Some(model),
                request_shape.stream,
                StatusCode::NOT_FOUND,
                request_timer.elapsed(),
            );
            return Err(ApiError::model_not_found(model));
        }
    }

    let route = match routing::select_backend(
        &state.config.backends,
        &path,
        request_shape.model.as_deref(),
        request_shape.stream,
    ) {
        Ok(route) => route,
        Err(error) => {
            record_preflight_failure(
                &state,
                &route_name,
                &identity.id,
                request_shape.model.as_deref(),
                request_shape.stream,
                error.status,
                request_timer.elapsed(),
            );
            return Err(error);
        }
    };

    let labels = MetricLabels {
        route: route_name,
        identity: identity.id,
        public_model: route
            .public_model
            .clone()
            .or_else(|| request_shape.model.clone())
            .unwrap_or_else(|| "none".to_owned()),
        backend: route.backend_id.clone(),
        stream: request_shape.stream,
    };
    state.metrics.record_backend_attempt(&labels);

    let span = info_span!(
        "proxy_v1",
        route = %labels.route,
        model = %labels.public_model,
        backend = %route.backend_id,
        stream = request_shape.stream,
        method = %method,
        path = %path,
    );
    do_proxy(
        state,
        headers,
        method,
        uri,
        body,
        content_type,
        request_shape.stream,
        route,
        labels,
        request_timer,
    )
    .instrument(span)
    .await
}

async fn do_proxy(
    state: Arc<AppState>,
    client_headers: HeaderMap,
    method: Method,
    uri: Uri,
    body: Bytes,
    content_type: Option<String>,
    stream: bool,
    route: SelectedRoute,
    labels: MetricLabels,
    request_timer: RequestTimer,
) -> Result<Response<Body>, ApiError> {
    let outbound_body = openai::rewrite_request_body(
        &body,
        content_type.as_deref(),
        route.backend_model.as_deref(),
    );
    let upstream_url = upstream_url(
        &route.base_url,
        uri.path(),
        uri.query(),
        route.backend_model.as_deref(),
    );

    let mut request = state
        .http
        .request(method, upstream_url)
        .timeout(route.timeout);

    if !outbound_body.is_empty() {
        request = request.body(outbound_body);
        if let Some(content_type) = client_headers
            .get(CONTENT_TYPE)
            .and_then(valid_header_value)
            .cloned()
        {
            request = request.header(CONTENT_TYPE, content_type);
        }
    }
    if stream {
        request = request.header(ACCEPT, "text/event-stream");
    }
    if let Some(api_key) = &route.api_key {
        request = request.header(AUTHORIZATION, format!("Bearer {api_key}"));
    }
    if let Some(request_id) = client_headers
        .get(X_REQUEST_ID)
        .and_then(valid_header_value)
        .cloned()
    {
        request = request.header(X_REQUEST_ID, request_id);
    }

    let upstream = match request.send().await {
        Ok(response) => response,
        Err(error) if error.is_timeout() => {
            state.metrics.record_request(
                &labels,
                StatusCode::GATEWAY_TIMEOUT.as_u16(),
                request_timer.elapsed(),
            );
            return Err(ApiError::timeout());
        }
        Err(error) => {
            warn!(?error, "upstream request failed");
            state.metrics.record_request(
                &labels,
                StatusCode::BAD_GATEWAY.as_u16(),
                request_timer.elapsed(),
            );
            return Err(ApiError::upstream(StatusCode::BAD_GATEWAY));
        }
    };

    let upstream_status = upstream.status();
    if !upstream_status.is_success() {
        let api_error = ApiError::upstream(upstream_status);
        state
            .metrics
            .record_request(&labels, api_error.status.as_u16(), request_timer.elapsed());
        return Err(api_error);
    }

    let upstream_content_type = header_str(upstream.headers(), &CONTENT_TYPE).map(str::to_owned);
    if stream || openai::is_event_stream_content_type(upstream_content_type.as_deref()) {
        Ok(streaming_response(
            state,
            client_headers,
            upstream,
            route,
            labels,
            request_timer,
        ))
    } else {
        buffered_response(
            state,
            client_headers,
            upstream,
            route,
            labels,
            request_timer,
        )
        .await
    }
}

async fn buffered_response(
    state: Arc<AppState>,
    client_headers: HeaderMap,
    upstream: reqwest::Response,
    route: SelectedRoute,
    labels: MetricLabels,
    request_timer: RequestTimer,
) -> Result<Response<Body>, ApiError> {
    let upstream_status = upstream.status();
    let upstream_headers = upstream.headers().clone();
    let content_type = header_str(&upstream_headers, &CONTENT_TYPE).map(str::to_owned);
    let bytes = upstream.bytes().await.map_err(|error| {
        warn!(?error, "failed to read upstream body");
        state.metrics.record_request(
            &labels,
            StatusCode::BAD_GATEWAY.as_u16(),
            request_timer.elapsed(),
        );
        ApiError::upstream(StatusCode::BAD_GATEWAY)
    })?;

    let (response_bytes, usage) = openai::rewrite_response_body(
        &bytes,
        content_type.as_deref(),
        route.backend_model.as_deref(),
        route.public_model.as_deref(),
    );

    state.metrics.record_usage(&labels, usage);
    state
        .metrics
        .record_request(&labels, upstream_status.as_u16(), request_timer.elapsed());

    response_builder(
        upstream_status,
        &client_headers,
        &upstream_headers,
        content_type.as_deref(),
        false,
    )
    .body(Body::from(response_bytes))
    .map_err(|_| ApiError::internal())
}

fn streaming_response(
    state: Arc<AppState>,
    client_headers: HeaderMap,
    upstream: reqwest::Response,
    route: SelectedRoute,
    labels: MetricLabels,
    request_timer: RequestTimer,
) -> Response<Body> {
    let upstream_status = upstream.status();
    let upstream_headers = upstream.headers().clone();
    let content_type = header_str(&upstream_headers, &CONTENT_TYPE).map(str::to_owned);
    let normalize_sse = openai::is_event_stream_content_type(content_type.as_deref());

    state
        .metrics
        .record_request(&labels, upstream_status.as_u16(), request_timer.elapsed());
    let stream_metrics = StreamMetrics::new(
        state.metrics.clone(),
        labels.clone(),
        upstream_status.as_u16(),
    );
    let backend_model = route.backend_model;
    let public_model = route.public_model;
    let stream = try_stream! {
        let mut stream_metrics = stream_metrics;
        let mut chunks = upstream.bytes_stream();

        if normalize_sse {
            let mut normalizer = SseNormalizer::new(backend_model, public_model);
            while let Some(chunk) = chunks.next().await {
                let chunk = chunk.map_err(|error| {
                    warn!(?error, "upstream stream chunk failed");
                    error
                })?;
                let normalized = normalizer.push(&chunk);
                if !normalized.is_empty() {
                    stream_metrics.add_usage(normalizer.usage);
                    normalizer.usage = UsageTotals::default();
                    yield Bytes::from(normalized);
                }
            }
            let tail = normalizer.finish();
            if !tail.is_empty() {
                stream_metrics.add_usage(normalizer.usage);
                yield Bytes::from(tail);
            }
        } else {
            while let Some(chunk) = chunks.next().await {
                let chunk = chunk.map_err(|error| {
                    warn!(?error, "upstream stream chunk failed");
                    error
                })?;
                yield chunk;
            }
        }
    }
    .map_err(|error: reqwest::Error| -> axum::BoxError { Box::new(error) });

    response_builder(
        upstream_status,
        &client_headers,
        &upstream_headers,
        content_type.as_deref().or(Some("text/event-stream")),
        true,
    )
    .body(Body::from_stream(stream))
    .expect("stream response builder is valid")
}

fn upstream_url(
    base_url: &str,
    path: &str,
    query: Option<&str>,
    backend_model: Option<&str>,
) -> String {
    let mut upstream_url = format!("{}{}", base_url, upstream_path(path));
    if let Some(query) = openai::rewrite_query_model(query, backend_model) {
        if !query.is_empty() {
            upstream_url.push('?');
            upstream_url.push_str(&query);
        }
    }
    upstream_url
}

fn upstream_path(path: &str) -> &str {
    match path {
        "/v1/chat/completion" => "/v1/chat/completions",
        path => path,
    }
}

fn response_builder(
    status: StatusCode,
    client_headers: &HeaderMap,
    upstream_headers: &HeaderMap,
    fallback_content_type: Option<&str>,
    streaming: bool,
) -> axum::http::response::Builder {
    let mut builder = Response::builder()
        .status(status)
        .header(CACHE_CONTROL, "no-cache");

    if let Some(content_type) = upstream_headers
        .get(CONTENT_TYPE)
        .and_then(valid_header_value)
        .cloned()
    {
        builder = builder.header(CONTENT_TYPE, content_type);
    } else if let Some(content_type) = fallback_content_type {
        builder = builder.header(CONTENT_TYPE, content_type);
    }

    if let Some(content_disposition) = upstream_headers
        .get(CONTENT_DISPOSITION)
        .and_then(valid_header_value)
        .cloned()
    {
        builder = builder.header(CONTENT_DISPOSITION, content_disposition);
    }

    if let Some(request_id) = client_headers
        .get(X_REQUEST_ID)
        .and_then(valid_header_value)
        .cloned()
    {
        builder = builder.header(X_REQUEST_ID, request_id);
    }

    if streaming {
        builder = builder.header("x-accel-buffering", "no");
    }

    builder
}

pub fn attach_request_id(
    mut response: Response<Body>,
    client_headers: &HeaderMap,
) -> Response<Body> {
    if let Some(request_id) = client_headers
        .get(X_REQUEST_ID)
        .and_then(valid_header_value)
        .cloned()
    {
        response.headers_mut().insert(X_REQUEST_ID, request_id);
    }
    response
}

fn header_str<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn valid_header_value(value: &HeaderValue) -> Option<&HeaderValue> {
    value.to_str().ok()?;
    Some(value)
}

fn record_preflight_failure(
    state: &AppState,
    route: &str,
    identity: &str,
    model: Option<&str>,
    stream: bool,
    status: StatusCode,
    duration: std::time::Duration,
) {
    let labels = MetricLabels {
        route: route.to_owned(),
        identity: identity.to_owned(),
        public_model: model.unwrap_or("none").to_owned(),
        backend: "none".to_owned(),
        stream,
    };
    state
        .metrics
        .record_request(&labels, status.as_u16(), duration);
}

struct StreamMetrics {
    metrics: crate::metrics::Metrics,
    labels: MetricLabels,
    status_code: u16,
    started: Instant,
    usage: UsageTotals,
}

impl StreamMetrics {
    fn new(metrics: crate::metrics::Metrics, labels: MetricLabels, status_code: u16) -> Self {
        Self {
            metrics,
            labels,
            status_code,
            started: Instant::now(),
            usage: UsageTotals::default(),
        }
    }

    fn add_usage(&mut self, usage: UsageTotals) {
        self.usage.input += usage.input;
        self.usage.cached_input += usage.cached_input;
        self.usage.output += usage.output;
    }
}

impl Drop for StreamMetrics {
    fn drop(&mut self) {
        if !self.usage.is_empty() {
            self.metrics.record_usage(&self.labels, self.usage);
        }
        self.metrics
            .record_stream(&self.labels, self.status_code, self.started.elapsed());
    }
}
