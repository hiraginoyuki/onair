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
use tracing::{Instrument, debug, info_span, warn};

use crate::app::AppState;
use crate::auth::authenticate;
use crate::config::DebugCaptureConfig;
use crate::debug_capture::{self, CaptureOutcome, CaptureRequest, RequestCapture};
use crate::error::ApiError;
use crate::metrics::{MetricLabels, RequestTimer};
use crate::openai::{self, SseNormalizer, UsageTotals};
use crate::routing::{self, SelectedRoute};
use crate::timeline::{RequestTimeline, TimelineEvent, TimelineSnapshot};

pub const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

pub async fn proxy_v1(
    state: Arc<AppState>,
    headers: HeaderMap,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> Result<Response<Body>, ApiError> {
    let request_timer = RequestTimer::start();
    let mut timeline = RequestTimeline::start();
    let path = uri.path().to_owned();
    let route_name = routing::path_metric_name(&path);
    let config = state.config.snapshot();

    let identity = match authenticate(&headers, &config.clients) {
        Ok(identity) => {
            timeline.mark(TimelineEvent::AuthDone);
            identity
        }
        Err(error) => {
            timeline.mark(TimelineEvent::AuthDone);
            record_preflight_failure(
                &state,
                &route_name,
                "unknown",
                None,
                false,
                error.status,
                request_timer.elapsed(),
            );
            warn_preflight_failure(
                &timeline,
                &route_name,
                "unknown",
                None,
                false,
                error.status,
                "auth",
            );
            return Err(error);
        }
    };

    let content_type = header_str(&headers, &CONTENT_TYPE).map(str::to_owned);
    let request_shape = openai::inspect_request(&body, content_type.as_deref(), uri.query());
    timeline.mark(TimelineEvent::RequestInspected);
    if request_shape.model.is_none() && routing::path_requires_model(&path) {
        let error = ApiError::bad_request(
            "Missing required parameter: model.",
            Some("model".to_owned()),
        );
        record_preflight_failure(
            &state,
            &route_name,
            &identity.id,
            None,
            request_shape.stream,
            error.status,
            request_timer.elapsed(),
        );
        warn_preflight_failure(
            &timeline,
            &route_name,
            &identity.id,
            None,
            request_shape.stream,
            error.status,
            "inspect",
        );
        return Err(error);
    }
    if let Some(model) = request_shape.model.as_deref()
        && !identity.models.contains(model)
    {
        record_preflight_failure(
            &state,
            &route_name,
            &identity.id,
            Some(model),
            request_shape.stream,
            StatusCode::NOT_FOUND,
            request_timer.elapsed(),
        );
        warn_preflight_failure(
            &timeline,
            &route_name,
            &identity.id,
            Some(model),
            request_shape.stream,
            StatusCode::NOT_FOUND,
            "access",
        );
        return Err(ApiError::model_not_found(model));
    }

    let sticky_key = routing::sticky_routing_key(
        &identity.id,
        &path,
        request_shape.model.as_deref(),
        request_shape.prompt_cache_key.as_deref(),
    );
    let route = match routing::select_backend(
        &config.backends,
        config.routing.strategy,
        &path,
        request_shape.model.as_deref(),
        request_shape.stream,
        Some(&sticky_key),
    ) {
        Ok(route) => {
            timeline.mark(TimelineEvent::RouteSelected);
            route
        }
        Err(error) => {
            timeline.mark(TimelineEvent::RouteSelected);
            record_preflight_failure(
                &state,
                &route_name,
                &identity.id,
                request_shape.model.as_deref(),
                request_shape.stream,
                error.status,
                request_timer.elapsed(),
            );
            warn_preflight_failure(
                &timeline,
                &route_name,
                &identity.id,
                request_shape.model.as_deref(),
                request_shape.stream,
                error.status,
                "route",
            );
            return Err(error);
        }
    };
    let model_log_fields = ModelLogFields::from_route(request_shape.model.as_deref(), &route);
    let request_body_bytes = body.len();

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
        requested_model = %model_log_fields.requested,
        public_model = %model_log_fields.public,
        backend_model = %model_log_fields.backend,
        backend = %route.backend_id,
        stream = request_shape.stream,
        method = %method,
        path = %path,
        request_body_bytes,
    );
    let request = ProxyRequest {
        method,
        uri,
        body,
        content_type,
        stream: request_shape.stream,
    };
    let context = ProxyContext {
        state,
        client_headers: headers,
        debug_capture_config: config.debug_capture.clone(),
        debug_capture: None,
        route,
        labels,
        model_log_fields,
        request_body_bytes,
        request_timer,
        timeline,
    };

    do_proxy(context, request).instrument(span).await
}

struct ProxyRequest {
    method: Method,
    uri: Uri,
    body: Bytes,
    content_type: Option<String>,
    stream: bool,
}

struct ProxyContext {
    state: Arc<AppState>,
    client_headers: HeaderMap,
    debug_capture_config: DebugCaptureConfig,
    debug_capture: Option<RequestCapture>,
    route: SelectedRoute,
    labels: MetricLabels,
    model_log_fields: ModelLogFields,
    request_body_bytes: usize,
    request_timer: RequestTimer,
    timeline: RequestTimeline,
}

async fn do_proxy(
    mut context: ProxyContext,
    request: ProxyRequest,
) -> Result<Response<Body>, ApiError> {
    let ProxyRequest {
        method,
        uri,
        body,
        content_type,
        stream,
    } = request;
    let request_path = uri.path().to_owned();
    let request_query = uri.query().map(str::to_owned);
    let outbound_body = openai::rewrite_request_body(
        &body,
        content_type.as_deref(),
        context.route.backend_model.as_deref(),
    );
    context.timeline.mark(TimelineEvent::RequestRewritten);
    let upstream_path = upstream_path(&request_path).to_owned();
    let upstream_query = openai::rewrite_query_model(
        request_query.as_deref(),
        context.route.backend_model.as_deref(),
    )
    .filter(|query| !query.is_empty());
    let upstream_url = upstream_url(
        &context.route.base_url,
        &upstream_path,
        upstream_query.as_deref(),
    );

    let request_id = context
        .client_headers
        .get(X_REQUEST_ID)
        .and_then(header_str_value)
        .map(str::to_owned);
    context.debug_capture = debug_capture::capture_request(
        &context.debug_capture_config,
        CaptureRequest {
            method: &method,
            client_path: &request_path,
            client_query: request_query.as_deref(),
            upstream_path: &upstream_path,
            upstream_query: upstream_query.as_deref(),
            content_type: content_type.as_deref(),
            request_id: request_id.as_deref(),
            labels: &context.labels,
            requested_model: &context.model_log_fields.requested,
            public_model: &context.model_log_fields.public,
            backend_model: &context.model_log_fields.backend,
            inbound_body: &body,
            upstream_body: &outbound_body,
        },
    );
    context.timeline.mark(TimelineEvent::DebugCaptureDone);

    let mut request = context
        .state
        .http
        .request(method, upstream_url)
        .timeout(context.route.timeout);

    if !outbound_body.is_empty() {
        request = request.body(outbound_body);
        if let Some(content_type) = context
            .client_headers
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
    if let Some(api_key) = &context.route.api_key {
        request = request.header(AUTHORIZATION, format!("Bearer {api_key}"));
    }
    if let Some(request_id) = context
        .client_headers
        .get(X_REQUEST_ID)
        .and_then(valid_header_value)
        .cloned()
    {
        request = request.header(X_REQUEST_ID, request_id);
    }

    context.timeline.mark(TimelineEvent::BackendForwardStart);
    let upstream = match request.send().await {
        Ok(response) => {
            context.timeline.mark(TimelineEvent::BackendHeadersReceived);
            response
        }
        Err(error) if error.is_timeout() => {
            context.state.metrics.record_request(
                &context.labels,
                StatusCode::GATEWAY_TIMEOUT.as_u16(),
                context.request_timer.elapsed(),
            );
            if let Some(capture) = &mut context.debug_capture {
                capture.record_outcome(CaptureOutcome::UpstreamTimeout {
                    client_status: StatusCode::GATEWAY_TIMEOUT.as_u16(),
                });
            }
            warn_proxy_failure(
                &context,
                StatusCode::GATEWAY_TIMEOUT,
                "timeout",
                "upstream request timed out",
            );
            return Err(ApiError::timeout());
        }
        Err(error) => {
            let error_kind = upstream_error_kind(&error);
            warn!(error_kind = error_kind, "upstream request failed");
            context.state.metrics.record_request(
                &context.labels,
                StatusCode::BAD_GATEWAY.as_u16(),
                context.request_timer.elapsed(),
            );
            if let Some(capture) = &mut context.debug_capture {
                capture.record_outcome(CaptureOutcome::UpstreamRequestFailed {
                    client_status: StatusCode::BAD_GATEWAY.as_u16(),
                    error_kind,
                });
            }
            warn_proxy_failure(
                &context,
                StatusCode::BAD_GATEWAY,
                error_kind,
                "upstream request failed",
            );
            return Err(ApiError::upstream(StatusCode::BAD_GATEWAY));
        }
    };

    let upstream_status = upstream.status();
    if !upstream_status.is_success() {
        let api_error = ApiError::upstream(upstream_status);
        warn!(
            upstream_status = upstream_status.as_u16(),
            client_status = api_error.status.as_u16(),
            backend = %context.labels.backend,
            route = %context.labels.route,
            requested_model = %context.model_log_fields.requested,
            public_model = %context.model_log_fields.public,
            backend_model = %context.model_log_fields.backend,
            path = %request_path,
            request_body_bytes = context.request_body_bytes,
            "upstream returned non-success status"
        );
        context.state.metrics.record_request(
            &context.labels,
            api_error.status.as_u16(),
            context.request_timer.elapsed(),
        );
        if let Some(capture) = &mut context.debug_capture {
            capture.record_outcome(CaptureOutcome::UpstreamNonSuccess {
                upstream_status: upstream_status.as_u16(),
                client_status: api_error.status.as_u16(),
            });
        }
        warn_proxy_failure(
            &context,
            api_error.status,
            "upstream_non_success",
            "upstream returned non-success status",
        );
        return Err(api_error);
    }

    let upstream_content_type = header_str(upstream.headers(), &CONTENT_TYPE).map(str::to_owned);
    if stream || openai::is_event_stream_content_type(upstream_content_type.as_deref()) {
        Ok(streaming_response(context, upstream))
    } else {
        buffered_response(context, upstream).await
    }
}

async fn buffered_response(
    context: ProxyContext,
    upstream: reqwest::Response,
) -> Result<Response<Body>, ApiError> {
    let ProxyContext {
        state,
        client_headers,
        debug_capture_config: _,
        mut debug_capture,
        route,
        labels,
        model_log_fields,
        request_body_bytes,
        request_timer,
        mut timeline,
    } = context;
    let upstream_status = upstream.status();
    let upstream_headers = upstream.headers().clone();
    let content_type = header_str(&upstream_headers, &CONTENT_TYPE).map(str::to_owned);
    let bytes = read_buffered_upstream_body(upstream, &mut timeline)
        .await
        .map_err(|error| {
            let error_kind = upstream_error_kind(&error);
            warn!(error_kind = error_kind, "failed to read upstream body");
            state.metrics.record_request(
                &labels,
                StatusCode::BAD_GATEWAY.as_u16(),
                request_timer.elapsed(),
            );
            if let Some(capture) = &mut debug_capture {
                capture.record_outcome(CaptureOutcome::UpstreamBodyReadFailed {
                    client_status: StatusCode::BAD_GATEWAY.as_u16(),
                    error_kind,
                });
            }
            ApiError::upstream(StatusCode::BAD_GATEWAY)
        })?;

    let (response_bytes, usage) = openai::rewrite_response_body(
        &bytes,
        content_type.as_deref(),
        route.backend_model.as_deref(),
        route.public_model.as_deref(),
    );
    timeline.mark(TimelineEvent::ResponseRewritten);

    state.metrics.record_usage(&labels, usage);
    state
        .metrics
        .record_request(&labels, upstream_status.as_u16(), request_timer.elapsed());
    debug!(
        upstream_status = upstream_status.as_u16(),
        response_bytes = response_bytes.len(),
        backend = %labels.backend,
        route = %labels.route,
        requested_model = %model_log_fields.requested,
        public_model = %model_log_fields.public,
        backend_model = %model_log_fields.backend,
        request_body_bytes,
        "buffered response completed"
    );
    if let Some(capture) = &mut debug_capture {
        capture.record_outcome(CaptureOutcome::Success {
            upstream_status: upstream_status.as_u16(),
        });
    }
    timeline.mark(TimelineEvent::ClientResponseReady);
    let timeline_snapshot = timeline.snapshot();
    debug_timeline_fields(timeline_snapshot, "buffered response timeline snapshot");

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

fn streaming_response(context: ProxyContext, upstream: reqwest::Response) -> Response<Body> {
    let ProxyContext {
        state,
        client_headers,
        debug_capture_config: _,
        debug_capture,
        route,
        labels,
        model_log_fields,
        request_body_bytes,
        request_timer,
        mut timeline,
    } = context;
    let upstream_status = upstream.status();
    let upstream_headers = upstream.headers().clone();
    let content_type = header_str(&upstream_headers, &CONTENT_TYPE).map(str::to_owned);
    let normalize_sse = openai::is_event_stream_content_type(content_type.as_deref());

    state
        .metrics
        .record_request(&labels, upstream_status.as_u16(), request_timer.elapsed());
    timeline.mark(TimelineEvent::ClientResponseReady);
    let stream_metrics = StreamMetrics::new(
        state.metrics.clone(),
        labels.clone(),
        upstream_status.as_u16(),
        model_log_fields.clone(),
        request_body_bytes,
        debug_capture,
        timeline,
    );
    debug!(
        upstream_status = upstream_status.as_u16(),
        backend = %labels.backend,
        route = %labels.route,
        requested_model = %model_log_fields.requested,
        public_model = %model_log_fields.public,
        backend_model = %model_log_fields.backend,
        request_body_bytes,
        "streaming response started"
    );
    let backend_model = route.backend_model;
    let public_model = route.public_model;
    let stream = try_stream! {
        let mut stream_metrics = stream_metrics;
        let mut chunks = upstream.bytes_stream();

        if normalize_sse {
            let mut normalizer = SseNormalizer::new(backend_model, public_model);
            while let Some(chunk) = chunks.next().await {
                let chunk = chunk.inspect_err(|error| {
                    warn!(
                        error_kind = upstream_error_kind(error),
                        "upstream stream chunk failed"
                    );
                })?;
                stream_metrics.mark_body_chunk();
                let normalized = normalizer.push(&chunk);
                if !normalized.is_empty() {
                    stream_metrics.add_usage(normalizer.usage);
                    normalizer.usage = UsageTotals::default();
                    yield Bytes::from(normalized);
                }
            }
            stream_metrics.mark_body_complete();
            let tail = normalizer.finish();
            if !tail.is_empty() {
                stream_metrics.add_usage(normalizer.usage);
                yield Bytes::from(tail);
            }
        } else {
            while let Some(chunk) = chunks.next().await {
                let chunk = chunk.inspect_err(|error| {
                    warn!(
                        error_kind = upstream_error_kind(error),
                        "upstream stream chunk failed"
                    );
                })?;
                stream_metrics.mark_body_chunk();
                yield chunk;
            }
            stream_metrics.mark_body_complete();
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

async fn read_buffered_upstream_body(
    upstream: reqwest::Response,
    timeline: &mut RequestTimeline,
) -> std::result::Result<Bytes, reqwest::Error> {
    let mut bytes = Vec::new();
    let mut chunks = upstream.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk?;
        timeline.mark(TimelineEvent::BackendBodyFirstChunk);
        bytes.extend_from_slice(&chunk);
    }
    timeline.mark(TimelineEvent::BackendBodyComplete);
    Ok(Bytes::from(bytes))
}

fn upstream_url(base_url: &str, path: &str, query: Option<&str>) -> String {
    let mut upstream_url = format!("{base_url}{path}");
    if let Some(query) = query {
        upstream_url.push('?');
        upstream_url.push_str(query);
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

fn header_str_value(value: &HeaderValue) -> Option<&str> {
    value.to_str().ok()
}

fn valid_header_value(value: &HeaderValue) -> Option<&HeaderValue> {
    value.to_str().ok()?;
    Some(value)
}

fn upstream_error_kind(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_request() {
        "request"
    } else if error.is_body() {
        "body"
    } else if error.is_decode() {
        "decode"
    } else if error.is_redirect() {
        "redirect"
    } else {
        "unknown"
    }
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

fn warn_preflight_failure(
    timeline: &RequestTimeline,
    route: &str,
    identity: &str,
    model: Option<&str>,
    stream: bool,
    status: StatusCode,
    stage: &'static str,
) {
    let snapshot = timeline.snapshot();
    warn!(
        route,
        identity,
        model = model.unwrap_or("none"),
        stream,
        status = status.as_u16(),
        stage,
        timeline_started_unix_ms = snapshot.started_unix_ms,
        timeline_total_us = snapshot.total_us,
        timeline_proxy_entry_us = snapshot.proxy_entry_us,
        timeline_auth_done_us = ?snapshot.auth_done_us,
        timeline_request_inspected_us = ?snapshot.request_inspected_us,
        timeline_route_selected_us = ?snapshot.route_selected_us,
        "request failed before upstream attempt"
    );
}

fn warn_proxy_failure(
    context: &ProxyContext,
    client_status: StatusCode,
    error_kind: &'static str,
    message: &'static str,
) {
    let snapshot = context.timeline.snapshot();
    warn!(
        client_status = client_status.as_u16(),
        error_kind,
        backend = %context.labels.backend,
        route = %context.labels.route,
        requested_model = %context.model_log_fields.requested,
        public_model = %context.model_log_fields.public,
        backend_model = %context.model_log_fields.backend,
        stream = context.labels.stream,
        request_body_bytes = context.request_body_bytes,
        timeline_started_unix_ms = snapshot.started_unix_ms,
        timeline_total_us = snapshot.total_us,
        timeline_proxy_entry_us = snapshot.proxy_entry_us,
        timeline_auth_done_us = ?snapshot.auth_done_us,
        timeline_request_inspected_us = ?snapshot.request_inspected_us,
        timeline_route_selected_us = ?snapshot.route_selected_us,
        timeline_request_rewritten_us = ?snapshot.request_rewritten_us,
        timeline_debug_capture_done_us = ?snapshot.debug_capture_done_us,
        timeline_backend_forward_start_us = ?snapshot.backend_forward_start_us,
        timeline_backend_headers_received_us = ?snapshot.backend_headers_received_us,
        event = message,
        "proxy failure timeline"
    );
}

fn debug_timeline_fields(snapshot: TimelineSnapshot, message: &'static str) {
    debug!(
        timeline_started_unix_ms = snapshot.started_unix_ms,
        timeline_total_us = snapshot.total_us,
        timeline_proxy_entry_us = snapshot.proxy_entry_us,
        timeline_auth_done_us = ?snapshot.auth_done_us,
        timeline_request_inspected_us = ?snapshot.request_inspected_us,
        timeline_route_selected_us = ?snapshot.route_selected_us,
        timeline_request_rewritten_us = ?snapshot.request_rewritten_us,
        timeline_debug_capture_done_us = ?snapshot.debug_capture_done_us,
        timeline_backend_forward_start_us = ?snapshot.backend_forward_start_us,
        timeline_backend_headers_received_us = ?snapshot.backend_headers_received_us,
        timeline_backend_body_first_chunk_us = ?snapshot.backend_body_first_chunk_us,
        timeline_backend_body_complete_us = ?snapshot.backend_body_complete_us,
        timeline_response_rewritten_us = ?snapshot.response_rewritten_us,
        timeline_client_response_ready_us = ?snapshot.client_response_ready_us,
        timeline_stream_complete_us = ?snapshot.stream_complete_us,
        event = message,
        "request timeline snapshot"
    );
}

#[derive(Debug, Clone)]
struct ModelLogFields {
    requested: String,
    public: String,
    backend: String,
}

impl ModelLogFields {
    fn from_route(requested_model: Option<&str>, route: &SelectedRoute) -> Self {
        Self {
            requested: requested_model.unwrap_or("none").to_owned(),
            public: route.public_model.as_deref().unwrap_or("none").to_owned(),
            backend: route.backend_model.as_deref().unwrap_or("none").to_owned(),
        }
    }
}

struct StreamMetrics {
    metrics: crate::metrics::Metrics,
    labels: MetricLabels,
    status_code: u16,
    model_log_fields: ModelLogFields,
    request_body_bytes: usize,
    debug_capture: Option<RequestCapture>,
    timeline: RequestTimeline,
    started: Instant,
    usage: UsageTotals,
}

impl StreamMetrics {
    fn new(
        metrics: crate::metrics::Metrics,
        labels: MetricLabels,
        status_code: u16,
        model_log_fields: ModelLogFields,
        request_body_bytes: usize,
        debug_capture: Option<RequestCapture>,
        timeline: RequestTimeline,
    ) -> Self {
        Self {
            metrics,
            labels,
            status_code,
            model_log_fields,
            request_body_bytes,
            debug_capture,
            timeline,
            started: Instant::now(),
            usage: UsageTotals::default(),
        }
    }

    fn mark_body_chunk(&mut self) {
        self.timeline.mark(TimelineEvent::BackendBodyFirstChunk);
    }

    fn mark_body_complete(&mut self) {
        self.timeline.mark(TimelineEvent::BackendBodyComplete);
    }

    fn add_usage(&mut self, usage: UsageTotals) {
        self.usage.input += usage.input;
        self.usage.cached_input += usage.cached_input;
        self.usage.output += usage.output;
    }
}

impl Drop for StreamMetrics {
    fn drop(&mut self) {
        let duration = self.started.elapsed();
        self.timeline.mark(TimelineEvent::StreamComplete);
        if !self.usage.is_empty() {
            self.metrics.record_usage(&self.labels, self.usage);
        }
        debug!(
            upstream_status = self.status_code,
            backend = %self.labels.backend,
            route = %self.labels.route,
            requested_model = %self.model_log_fields.requested,
            public_model = %self.model_log_fields.public,
            backend_model = %self.model_log_fields.backend,
            request_body_bytes = self.request_body_bytes,
            stream_duration_ms = duration.as_millis() as u64,
            input_tokens = self.usage.input,
            cached_input_tokens = self.usage.cached_input,
            output_tokens = self.usage.output,
            "streaming response completed"
        );
        if let Some(capture) = &mut self.debug_capture {
            capture.record_outcome(CaptureOutcome::StreamCompleted {
                upstream_status: self.status_code,
                stream_duration_ms: duration.as_millis(),
                input_tokens: self.usage.input,
                cached_input_tokens: self.usage.cached_input,
                output_tokens: self.usage.output,
            });
        }
        debug_timeline_fields(
            self.timeline.snapshot(),
            "streaming response timeline snapshot",
        );
        self.metrics
            .record_stream(&self.labels, self.status_code, duration);
    }
}
