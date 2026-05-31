use std::net::SocketAddr;
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
use tokio::sync::watch;
use tracing::{Instrument, debug, info_span, warn};

use crate::app::AppState;
use crate::auth::authenticate;
use crate::config::{DebugCaptureConfig, DebugCaptureMode};
use crate::error::ApiError;
use crate::metrics::{MetricLabels, RequestTimer};
use crate::observe::debug_capture::{self, CaptureOutcome, CaptureRequest, RequestCapture};
use crate::observe::{
    BackendHealthStore, ClientInfo, InspectorAttemptRecord, InspectorOutcome, InspectorRequestBase,
    InspectorRequestRecordInit, InspectorStore, InspectorTokenCounts, RequestTimeline,
    TimelineEvent, TimelineSnapshot,
};
use crate::openai::{self, SseNormalizer, UsageTotals};
use crate::routing::{self, SelectedRoute};

pub const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
const MAX_INSPECTOR_TEXT_CHARS: usize = 512;
const MAX_UPSTREAM_ERROR_CAPTURE_BYTES: usize = 1024 * 1024;

pub async fn proxy_v1(
    state: Arc<AppState>,
    peer_addr: Option<SocketAddr>,
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
    let client_info =
        ClientInfo::from_headers(&headers, peer_addr, &config.server.trusted_proxy_cidrs);
    let request_body_bytes = body.len();
    let client_request_id = header_str(&headers, &X_REQUEST_ID).map(inspector_text);
    let started_at_unix_ms = timeline.snapshot().started_unix_ms;
    let observation = RequestObservationBase {
        inspector_enabled: config.inspector.enabled,
        inspector_retention_requests: config.inspector.retention_requests,
        record_id: InspectorStore::next_record_id(started_at_unix_ms, client_request_id.as_deref()),
        client_request_id,
        started_at_unix_ms,
        method: method.as_str().to_owned(),
        path: inspector_text(&path),
        query: uri.query().map(inspector_text),
        client_info: client_info.clone(),
        request_body_bytes,
    };

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
            warn_preflight_failure(PreflightFailureLog {
                timeline: &timeline,
                route: &route_name,
                identity: "unknown",
                model: None,
                stream: false,
                status: error.status,
                stage: "auth",
                client_info: &client_info,
            });
            record_preflight_inspector(PreflightInspectorRecord {
                state: &state,
                observation: &observation,
                timeline: &timeline,
                route: &route_name,
                identity: "unknown",
                model: None,
                stream: false,
                status: error.status,
                stage: "auth",
            });
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
        warn_preflight_failure(PreflightFailureLog {
            timeline: &timeline,
            route: &route_name,
            identity: &identity.id,
            model: None,
            stream: request_shape.stream,
            status: error.status,
            stage: "inspect",
            client_info: &client_info,
        });
        record_preflight_inspector(PreflightInspectorRecord {
            state: &state,
            observation: &observation,
            timeline: &timeline,
            route: &route_name,
            identity: &identity.id,
            model: None,
            stream: request_shape.stream,
            status: error.status,
            stage: "inspect",
        });
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
        warn_preflight_failure(PreflightFailureLog {
            timeline: &timeline,
            route: &route_name,
            identity: &identity.id,
            model: Some(model),
            stream: request_shape.stream,
            status: StatusCode::NOT_FOUND,
            stage: "access",
            client_info: &client_info,
        });
        record_preflight_inspector(PreflightInspectorRecord {
            state: &state,
            observation: &observation,
            timeline: &timeline,
            route: &route_name,
            identity: &identity.id,
            model: Some(model),
            stream: request_shape.stream,
            status: StatusCode::NOT_FOUND,
            stage: "access",
        });
        return Err(ApiError::model_not_found(model));
    }

    let sticky_key = routing::sticky_routing_key(
        &identity.id,
        &path,
        request_shape.model.as_deref(),
        request_shape.prompt_cache_key.as_deref(),
    );
    let routes = match routing::select_backend_candidates(
        &config.backends,
        config.routing.strategy,
        &path,
        request_shape.model.as_deref(),
        request_shape.stream,
        request_shape.has_tools,
        Some(&sticky_key),
    ) {
        Ok(routes) => {
            timeline.mark(TimelineEvent::RouteSelected);
            routes
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
            warn_preflight_failure(PreflightFailureLog {
                timeline: &timeline,
                route: &route_name,
                identity: &identity.id,
                model: request_shape.model.as_deref(),
                stream: request_shape.stream,
                status: error.status,
                stage: "route",
                client_info: &client_info,
            });
            record_preflight_inspector(PreflightInspectorRecord {
                state: &state,
                observation: &observation,
                timeline: &timeline,
                route: &route_name,
                identity: &identity.id,
                model: request_shape.model.as_deref(),
                stream: request_shape.stream,
                status: error.status,
                stage: "route",
            });
            return Err(error);
        }
    };
    let mut routes = routes.into_iter();
    let route = routes.next().expect("candidate list is not empty");
    let fallback_routes = routes
        .take(config.routing.fallback_attempts)
        .collect::<Vec<_>>();
    let max_attempts = 1 + fallback_routes.len();
    let model_log_fields = ModelLogFields::from_route(request_shape.model.as_deref(), &route);
    let backend_target = backend_target(&route.base_url);

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
    let inspector_base =
        routed_inspector_base(&observation, &labels, &model_log_fields, &backend_target);

    let span = info_span!(
        "proxy_v1",
        route = %labels.route,
        requested_model = %model_log_fields.requested,
        public_model = %model_log_fields.public,
        backend_model = %model_log_fields.backend,
        backend = %route.backend_id,
        attempt = 1usize,
        max_attempts,
        stream = request_shape.stream,
        method = %method,
        path = %path,
        peer_addr = %client_info.peer_addr(),
        effective_client_addr = %client_info.effective_client_addr(),
        trusted_proxy_addr = %client_info.trusted_proxy_addr(),
        forwarded_for = %client_info.forwarded_for(),
        user_agent = %client_info.user_agent(),
        request_body_bytes,
    );
    let request = ProxyRequest {
        method,
        uri,
        body,
        content_type,
        stream: request_shape.stream,
    };
    let shutdown = state.shutdown_receiver();
    let context = ProxyContext {
        state,
        client_headers: headers,
        debug_capture_config: config.debug_capture.clone(),
        debug_capture: None,
        pending_debug_capture: None,
        inspector_base,
        inspector_enabled: observation.inspector_enabled,
        inspector_retention_requests: observation.inspector_retention_requests,
        client_info,
        shutdown,
        backend_target,
        backend_remote_addr: None,
        route,
        labels,
        model_log_fields,
        requested_model: request_shape.model.clone(),
        request_body_bytes,
        request_timer,
        timeline,
        attempt: 1,
        max_attempts,
        backend_attempts: Vec::new(),
        retried_attempts: Vec::new(),
        current_attempt: None,
    };

    do_proxy(context, request, fallback_routes)
        .instrument(span)
        .await
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
    pending_debug_capture: Option<PendingDebugCapture>,
    inspector_base: InspectorRequestBase,
    inspector_enabled: bool,
    inspector_retention_requests: usize,
    client_info: ClientInfo,
    shutdown: watch::Receiver<bool>,
    backend_target: String,
    backend_remote_addr: Option<SocketAddr>,
    route: SelectedRoute,
    labels: MetricLabels,
    model_log_fields: ModelLogFields,
    requested_model: Option<String>,
    request_body_bytes: usize,
    request_timer: RequestTimer,
    timeline: RequestTimeline,
    attempt: usize,
    max_attempts: usize,
    backend_attempts: Vec<InspectorAttemptRecord>,
    retried_attempts: Vec<InspectorAttemptRecord>,
    current_attempt: Option<InspectorAttemptBuilder>,
}

impl ProxyContext {
    fn apply_route(&mut self, route: SelectedRoute) {
        self.backend_target = backend_target(&route.base_url);
        self.backend_remote_addr = None;
        self.debug_capture = None;
        self.pending_debug_capture = None;
        self.model_log_fields = ModelLogFields::from_route(self.requested_model.as_deref(), &route);
        self.labels.backend = route.backend_id.clone();
        self.labels.public_model = route
            .public_model
            .clone()
            .or_else(|| self.requested_model.clone())
            .unwrap_or_else(|| "none".to_owned());
        self.inspector_base.requested_model = self.model_log_fields.requested.clone();
        self.inspector_base.public_model = self.model_log_fields.public.clone();
        self.inspector_base.backend_model = self.model_log_fields.backend.clone();
        self.inspector_base.backend = self.labels.backend.clone();
        self.inspector_base.backend_target = self.backend_target.clone();
        self.inspector_base.backend_remote_addr = None;
        self.inspector_base.debug_capture_id = None;
        self.route = route;
    }

    fn record_retried_attempt(&mut self, attempt: InspectorAttemptRecord) {
        self.backend_attempts.push(attempt.clone());
        self.retried_attempts.push(attempt);
    }

    fn record_final_attempt(
        &mut self,
        status: StatusCode,
        upstream_status: Option<u16>,
        outcome: &'static str,
        error_kind: Option<&'static str>,
    ) {
        if let Some(attempt) = self.current_attempt.take() {
            let ended_us = self.timeline.elapsed_us();
            self.backend_attempts.push(attempt.finish(
                status,
                upstream_status,
                outcome,
                error_kind,
                ended_us,
            ));
        }
    }
}

#[derive(Debug, Clone)]
struct PendingDebugCapture {
    method: Method,
    client_path: String,
    client_query: Option<String>,
    upstream_path: String,
    upstream_query: Option<String>,
    content_type: Option<String>,
    request_id: Option<String>,
    labels: MetricLabels,
    requested_model: String,
    public_model: String,
    backend_model: String,
    inbound_body: Bytes,
    upstream_body: Vec<u8>,
}

impl PendingDebugCapture {
    fn capture(&self, config: &DebugCaptureConfig) -> Option<RequestCapture> {
        debug_capture::capture_request(
            config,
            CaptureRequest {
                method: &self.method,
                client_path: &self.client_path,
                client_query: self.client_query.as_deref(),
                upstream_path: &self.upstream_path,
                upstream_query: self.upstream_query.as_deref(),
                content_type: self.content_type.as_deref(),
                request_id: self.request_id.as_deref(),
                labels: &self.labels,
                requested_model: &self.requested_model,
                public_model: &self.public_model,
                backend_model: &self.backend_model,
                inbound_body: &self.inbound_body,
                upstream_body: &self.upstream_body,
            },
        )
    }
}

fn ensure_failure_debug_capture(
    config: &DebugCaptureConfig,
    pending_debug_capture: &mut Option<PendingDebugCapture>,
    debug_capture: &mut Option<RequestCapture>,
    timeline: &mut RequestTimeline,
    attempt_record: Option<&mut InspectorAttemptBuilder>,
) {
    let had_capture = debug_capture.is_some();
    if debug_capture.is_none()
        && let Some(pending_capture) = pending_debug_capture.take()
    {
        *debug_capture = pending_capture.capture(config);
    }

    if let Some(attempt_record) = attempt_record {
        attempt_record.set_debug_capture(debug_capture.as_ref());
        if !had_capture && debug_capture.is_some() {
            attempt_record.debug_capture_done_us =
                Some(timeline.mark(TimelineEvent::DebugCaptureDone));
        }
    } else if !had_capture && debug_capture.is_some() {
        timeline.mark(TimelineEvent::DebugCaptureDone);
    }
}

#[derive(Debug, Clone)]
struct InspectorAttemptBuilder {
    attempt: usize,
    backend: String,
    backend_target: String,
    backend_remote_addr: Option<String>,
    debug_capture_id: Option<String>,
    started_us: u64,
    request_rewritten_us: Option<u64>,
    debug_capture_done_us: Option<u64>,
    backend_forward_start_us: Option<u64>,
    backend_headers_received_us: Option<u64>,
    backend_body_first_chunk_us: Option<u64>,
    backend_body_complete_us: Option<u64>,
    stream_complete_us: Option<u64>,
}

impl InspectorAttemptBuilder {
    fn new(context: &ProxyContext) -> Self {
        Self {
            attempt: context.attempt,
            backend: context.labels.backend.clone(),
            backend_target: context.backend_target.clone(),
            backend_remote_addr: context
                .backend_remote_addr
                .map(|address| address.to_string()),
            debug_capture_id: context
                .debug_capture
                .as_ref()
                .map(|capture| capture.id().to_owned()),
            started_us: context.timeline.elapsed_us(),
            request_rewritten_us: None,
            debug_capture_done_us: None,
            backend_forward_start_us: None,
            backend_headers_received_us: None,
            backend_body_first_chunk_us: None,
            backend_body_complete_us: None,
            stream_complete_us: None,
        }
    }

    fn set_debug_capture(&mut self, capture: Option<&RequestCapture>) {
        self.debug_capture_id = capture.map(|capture| capture.id().to_owned());
    }

    fn set_backend_remote_addr(&mut self, address: Option<SocketAddr>) {
        self.backend_remote_addr = address.map(|address| address.to_string());
    }

    fn mark_body_first_chunk(&mut self, elapsed_us: u64) {
        if self.backend_body_first_chunk_us.is_none() {
            self.backend_body_first_chunk_us = Some(elapsed_us);
        }
    }

    fn finish(
        self,
        status: StatusCode,
        upstream_status: Option<u16>,
        outcome: &'static str,
        error_kind: Option<&'static str>,
        ended_us: u64,
    ) -> InspectorAttemptRecord {
        let elapsed_us = ended_us.saturating_sub(self.started_us);
        InspectorAttemptRecord {
            attempt: self.attempt,
            backend: self.backend,
            backend_target: self.backend_target,
            backend_remote_addr: self.backend_remote_addr,
            debug_capture_id: self.debug_capture_id,
            status: status.as_u16(),
            outcome: outcome.to_owned(),
            error_kind: error_kind.map(str::to_owned),
            started_us: self.started_us,
            ended_us,
            elapsed_us,
            elapsed_ms: duration_millis(std::time::Duration::from_micros(elapsed_us)),
            upstream_status,
            request_rewritten_us: self.request_rewritten_us,
            debug_capture_done_us: self.debug_capture_done_us,
            backend_forward_start_us: self.backend_forward_start_us,
            backend_headers_received_us: self.backend_headers_received_us,
            backend_body_first_chunk_us: self.backend_body_first_chunk_us,
            backend_body_complete_us: self.backend_body_complete_us,
            stream_complete_us: self.stream_complete_us,
        }
    }
}

async fn do_proxy(
    mut context: ProxyContext,
    request: ProxyRequest,
    fallback_routes: Vec<SelectedRoute>,
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
    let upstream_path = upstream_path(&request_path).to_owned();
    let request_id = context
        .client_headers
        .get(X_REQUEST_ID)
        .and_then(header_str_value)
        .map(str::to_owned);
    let capture_mode = context.debug_capture_config.mode;
    let mut fallback_routes = fallback_routes.into_iter();
    let upstream = loop {
        let attempt_started = Instant::now();
        let mut attempt_record = InspectorAttemptBuilder::new(&context);
        context
            .state
            .metrics
            .record_backend_attempt(&context.labels);
        let outbound_body = openai::rewrite_request_body_for_mode_with_policies(
            &body,
            content_type.as_deref(),
            context.route.backend_model.as_deref(),
            &upstream_path,
            context.route.request_mode,
            openai::RequestRewritePolicies {
                tool_schema_mode: context.route.tool_schema_mode,
                responses_store: context.route.responses_store,
                responses_max_output_tokens: context.route.responses_max_output_tokens,
            },
        )
        .map_err(|error| ApiError::bad_request(error.message(), error.param()))?;
        attempt_record.request_rewritten_us =
            Some(context.timeline.mark(TimelineEvent::RequestRewritten));
        let upstream_query = openai::rewrite_query_model(
            request_query.as_deref(),
            context.route.backend_model.as_deref(),
        )
        .filter(|query| !query.is_empty());
        let actual_upstream_path =
            openai::upstream_path_for_mode(&upstream_path, context.route.request_mode);
        let upstream_url = upstream_url(
            &context.route.base_url,
            actual_upstream_path,
            upstream_query.as_deref(),
        );
        let pending_debug_capture = PendingDebugCapture {
            method: method.clone(),
            client_path: request_path.clone(),
            client_query: request_query.clone(),
            upstream_path: actual_upstream_path.to_owned(),
            upstream_query: upstream_query.clone(),
            content_type: content_type.clone(),
            request_id: request_id.clone(),
            labels: context.labels.clone(),
            requested_model: context.model_log_fields.requested.clone(),
            public_model: context.model_log_fields.public.clone(),
            backend_model: context.model_log_fields.backend.clone(),
            inbound_body: body.clone(),
            upstream_body: outbound_body.clone(),
        };
        if !context.debug_capture_config.enabled {
            context.debug_capture = None;
            context.pending_debug_capture = None;
        } else if capture_mode == DebugCaptureMode::All {
            context.debug_capture = pending_debug_capture.capture(&context.debug_capture_config);
            context.pending_debug_capture = None;
        } else {
            context.debug_capture = None;
            context.pending_debug_capture = Some(pending_debug_capture);
        }
        attempt_record.set_debug_capture(context.debug_capture.as_ref());
        attempt_record.debug_capture_done_us = context
            .debug_capture
            .as_ref()
            .map(|_| context.timeline.mark(TimelineEvent::DebugCaptureDone));

        let mut upstream_request = context
            .state
            .http
            .request(method.clone(), upstream_url)
            .timeout(context.route.timeout);

        if !outbound_body.is_empty() {
            upstream_request = upstream_request.body(outbound_body.clone());
            if let Some(content_type) = context
                .client_headers
                .get(CONTENT_TYPE)
                .and_then(valid_header_value)
                .cloned()
            {
                upstream_request = upstream_request.header(CONTENT_TYPE, content_type);
            }
        }
        if stream {
            upstream_request = upstream_request.header(ACCEPT, "text/event-stream");
        }
        if let Some(api_key) = &context.route.api_key {
            upstream_request = upstream_request.header(AUTHORIZATION, format!("Bearer {api_key}"));
        }
        if let Some(request_id) = context
            .client_headers
            .get(X_REQUEST_ID)
            .and_then(valid_header_value)
            .cloned()
        {
            upstream_request = upstream_request.header(X_REQUEST_ID, request_id);
        }

        attempt_record.backend_forward_start_us =
            Some(context.timeline.mark(TimelineEvent::BackendForwardStart));
        match send_upstream_request(upstream_request, &mut context.shutdown).await {
            Ok(response) => {
                attempt_record.backend_headers_received_us =
                    Some(context.timeline.mark(TimelineEvent::BackendHeadersReceived));
                context.backend_remote_addr = response.remote_addr();
                attempt_record.set_backend_remote_addr(context.backend_remote_addr);
                context.current_attempt = Some(attempt_record);
                break response;
            }
            Err(UpstreamSendError::Shutdown) => {
                warn!("shutdown signaled during upstream request");
                return Err(ApiError::internal());
            }
            Err(UpstreamSendError::Request(error)) if error.is_timeout() => {
                ensure_failure_debug_capture(
                    &context.debug_capture_config,
                    &mut context.pending_debug_capture,
                    &mut context.debug_capture,
                    &mut context.timeline,
                    Some(&mut attempt_record),
                );
                if let Some(next_route) = fallback_routes.next() {
                    let attempt_elapsed = attempt_started.elapsed();
                    if let Some(capture) = &mut context.debug_capture {
                        capture.record_outcome(CaptureOutcome::UpstreamTimeout {
                            client_status: StatusCode::GATEWAY_TIMEOUT.as_u16(),
                        });
                    }
                    context.state.health.record_failure(
                        &context.labels.backend,
                        attempt_elapsed,
                        StatusCode::GATEWAY_TIMEOUT.as_u16(),
                        "timeout",
                    );
                    let attempt_record = attempt_record.finish(
                        StatusCode::GATEWAY_TIMEOUT,
                        None,
                        "upstream_timeout",
                        Some("timeout"),
                        context.timeline.elapsed_us(),
                    );
                    context.record_retried_attempt(attempt_record);
                    warn_proxy_retry(
                        &context,
                        &next_route,
                        StatusCode::GATEWAY_TIMEOUT,
                        "timeout",
                        "upstream request timed out; retrying fallback backend",
                    );
                    context.attempt += 1;
                    context.apply_route(next_route);
                    continue;
                }

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
                context.state.health.record_failure(
                    &context.labels.backend,
                    context.request_timer.elapsed(),
                    StatusCode::GATEWAY_TIMEOUT.as_u16(),
                    "timeout",
                );
                context.backend_attempts.push(attempt_record.finish(
                    StatusCode::GATEWAY_TIMEOUT,
                    None,
                    "upstream_timeout",
                    Some("timeout"),
                    context.timeline.elapsed_us(),
                ));
                record_context_inspector(
                    &context,
                    InspectorOutcome::UpstreamTimeout,
                    StatusCode::GATEWAY_TIMEOUT,
                    None,
                    None,
                    InspectorTokenCounts::default(),
                );
                warn_proxy_failure(
                    &context,
                    StatusCode::GATEWAY_TIMEOUT,
                    "timeout",
                    "upstream request timed out",
                );
                return Err(ApiError::timeout());
            }
            Err(UpstreamSendError::Request(error)) => {
                let error_kind = upstream_error_kind(&error);
                ensure_failure_debug_capture(
                    &context.debug_capture_config,
                    &mut context.pending_debug_capture,
                    &mut context.debug_capture,
                    &mut context.timeline,
                    Some(&mut attempt_record),
                );
                if retryable_send_error(&error)
                    && let Some(next_route) = fallback_routes.next()
                {
                    let attempt_elapsed = attempt_started.elapsed();
                    if let Some(capture) = &mut context.debug_capture {
                        capture.record_outcome(CaptureOutcome::UpstreamRequestFailed {
                            client_status: StatusCode::BAD_GATEWAY.as_u16(),
                            error_kind,
                        });
                    }
                    context.state.health.record_failure(
                        &context.labels.backend,
                        attempt_elapsed,
                        StatusCode::BAD_GATEWAY.as_u16(),
                        error_kind,
                    );
                    let attempt_record = attempt_record.finish(
                        StatusCode::BAD_GATEWAY,
                        None,
                        "upstream_request_failed",
                        Some(error_kind),
                        context.timeline.elapsed_us(),
                    );
                    context.record_retried_attempt(attempt_record);
                    warn_proxy_retry(
                        &context,
                        &next_route,
                        StatusCode::BAD_GATEWAY,
                        error_kind,
                        "upstream request failed before response; retrying fallback backend",
                    );
                    context.attempt += 1;
                    context.apply_route(next_route);
                    continue;
                }

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
                context.state.health.record_failure(
                    &context.labels.backend,
                    context.request_timer.elapsed(),
                    StatusCode::BAD_GATEWAY.as_u16(),
                    error_kind,
                );
                context.backend_attempts.push(attempt_record.finish(
                    StatusCode::BAD_GATEWAY,
                    None,
                    "upstream_request_failed",
                    Some(error_kind),
                    context.timeline.elapsed_us(),
                ));
                record_context_inspector(
                    &context,
                    InspectorOutcome::UpstreamRequestFailed,
                    StatusCode::BAD_GATEWAY,
                    Some(error_kind),
                    None,
                    InspectorTokenCounts::default(),
                );
                warn_proxy_failure(
                    &context,
                    StatusCode::BAD_GATEWAY,
                    error_kind,
                    "upstream request failed",
                );
                return Err(ApiError::upstream(StatusCode::BAD_GATEWAY));
            }
        }
    };

    let upstream_status = upstream.status();
    if !upstream_status.is_success() {
        let upstream_content_type =
            header_str(upstream.headers(), &CONTENT_TYPE).map(str::to_owned);
        ensure_failure_debug_capture(
            &context.debug_capture_config,
            &mut context.pending_debug_capture,
            &mut context.debug_capture,
            &mut context.timeline,
            context.current_attempt.as_mut(),
        );
        if context.debug_capture.is_some() {
            match read_capped_upstream_error_body(
                upstream,
                &mut context.timeline,
                context.current_attempt.as_mut(),
                &mut context.shutdown,
            )
            .await
            {
                Ok(error_body) => {
                    if let Some(capture) = &mut context.debug_capture {
                        capture.record_upstream_error_response(
                            upstream_status.as_u16(),
                            upstream_content_type.as_deref(),
                            &error_body.bytes,
                            error_body.truncated,
                        );
                    }
                }
                Err(BufferedBodyReadError::Shutdown) => {
                    warn!("shutdown signaled while capturing upstream error body");
                }
                Err(BufferedBodyReadError::Upstream(error)) => {
                    let error_kind = upstream_error_kind(&error);
                    warn!(
                        error_kind = error_kind,
                        "failed to capture upstream error body"
                    );
                }
            }
        }
        let api_error = ApiError::upstream(upstream_status);
        warn!(
            upstream_status = upstream_status.as_u16(),
            client_status = api_error.status.as_u16(),
            backend = %context.labels.backend,
            route = %context.labels.route,
            requested_model = %context.model_log_fields.requested,
            public_model = %context.model_log_fields.public,
            backend_model = %context.model_log_fields.backend,
            attempt = context.attempt,
            max_attempts = context.max_attempts,
            path = %request_path,
            request_body_bytes = context.request_body_bytes,
            debug_capture_id = debug_capture_id(context.debug_capture.as_ref()),
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
        context.state.health.record_failure(
            &context.labels.backend,
            context.request_timer.elapsed(),
            upstream_status.as_u16(),
            "upstream_non_success",
        );
        context.record_final_attempt(
            api_error.status,
            Some(upstream_status.as_u16()),
            "upstream_non_success",
            None,
        );
        record_context_inspector(
            &context,
            InspectorOutcome::UpstreamNonSuccess,
            api_error.status,
            None,
            None,
            InspectorTokenCounts::default(),
        );
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
        debug_capture_config,
        mut debug_capture,
        mut pending_debug_capture,
        inspector_base,
        inspector_enabled,
        inspector_retention_requests,
        client_info,
        mut shutdown,
        backend_target,
        backend_remote_addr,
        route,
        labels,
        model_log_fields,
        requested_model: _,
        request_body_bytes,
        request_timer,
        mut timeline,
        attempt,
        max_attempts,
        mut backend_attempts,
        retried_attempts,
        mut current_attempt,
    } = context;
    let upstream_status = upstream.status();
    let upstream_headers = upstream.headers().clone();
    let content_type = header_str(&upstream_headers, &CONTENT_TYPE).map(str::to_owned);
    let bytes = match read_buffered_upstream_body(
        upstream,
        &mut timeline,
        current_attempt.as_mut(),
        &mut shutdown,
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(BufferedBodyReadError::Shutdown) => {
            warn!("shutdown signaled while reading upstream body");
            return Err(ApiError::internal());
        }
        Err(BufferedBodyReadError::Upstream(error)) => {
            let error_kind = upstream_error_kind(&error);
            warn!(error_kind = error_kind, "failed to read upstream body");
            state.metrics.record_request(
                &labels,
                StatusCode::BAD_GATEWAY.as_u16(),
                request_timer.elapsed(),
            );
            ensure_failure_debug_capture(
                &debug_capture_config,
                &mut pending_debug_capture,
                &mut debug_capture,
                &mut timeline,
                current_attempt.as_mut(),
            );
            if let Some(capture) = &mut debug_capture {
                capture.record_outcome(CaptureOutcome::UpstreamBodyReadFailed {
                    client_status: StatusCode::BAD_GATEWAY.as_u16(),
                    error_kind,
                });
            }
            state.health.record_failure(
                &labels.backend,
                request_timer.elapsed(),
                StatusCode::BAD_GATEWAY.as_u16(),
                error_kind,
            );
            if let Some(attempt_record) = current_attempt.take() {
                backend_attempts.push(attempt_record.finish(
                    StatusCode::BAD_GATEWAY,
                    Some(upstream_status.as_u16()),
                    "upstream_body_read_failed",
                    Some(error_kind),
                    timeline.elapsed_us(),
                ));
            }
            let mut inspector_base = inspector_base.clone();
            inspector_base.backend_remote_addr =
                backend_remote_addr.map(|address| address.to_string());
            inspector_base.debug_capture_id = debug_capture
                .as_ref()
                .map(|capture| capture.id().to_owned());
            record_inspector_request(
                &state.inspector,
                inspector_enabled,
                inspector_retention_requests,
                InspectorRecord {
                    base: inspector_base,
                    timeline: &timeline,
                    outcome: InspectorOutcome::UpstreamBodyReadFailed,
                    status: StatusCode::BAD_GATEWAY,
                    error_kind: Some(error_kind),
                    backend_attempts: backend_attempts.clone(),
                    retried_attempts: retried_attempts.clone(),
                    response_body_bytes: None,
                    tokens: InspectorTokenCounts::default(),
                },
            );
            return Err(ApiError::upstream(StatusCode::BAD_GATEWAY));
        }
    };

    let (response_bytes, usage) = openai::rewrite_response_body(
        &bytes,
        content_type.as_deref(),
        route.backend_model.as_deref(),
        route.public_model.as_deref(),
        route.request_mode,
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
        backend_target = %backend_target,
        backend_remote_addr = %socket_addr_or_none(backend_remote_addr),
        route = %labels.route,
        peer_addr = %client_info.peer_addr(),
        effective_client_addr = %client_info.effective_client_addr(),
        trusted_proxy_addr = %client_info.trusted_proxy_addr(),
        forwarded_for = %client_info.forwarded_for(),
        user_agent = %client_info.user_agent(),
        requested_model = %model_log_fields.requested,
        public_model = %model_log_fields.public,
        backend_model = %model_log_fields.backend,
        attempt,
        max_attempts,
        request_body_bytes,
        "buffered response completed"
    );
    if let Some(capture) = &mut debug_capture {
        capture.record_outcome(CaptureOutcome::Success {
            upstream_status: upstream_status.as_u16(),
        });
    }
    state.health.record_success(
        &labels.backend,
        request_timer.elapsed(),
        upstream_status.as_u16(),
    );
    if let Some(attempt_record) = current_attempt.take() {
        let ended_us = attempt_record
            .backend_body_complete_us
            .unwrap_or_else(|| timeline.elapsed_us());
        backend_attempts.push(attempt_record.finish(
            upstream_status,
            Some(upstream_status.as_u16()),
            "completed",
            None,
            ended_us,
        ));
    }
    timeline.mark(TimelineEvent::ClientResponseReady);
    let mut final_inspector_base = inspector_base;
    final_inspector_base.backend_remote_addr =
        backend_remote_addr.map(|address| address.to_string());
    final_inspector_base.debug_capture_id = debug_capture
        .as_ref()
        .map(|capture| capture.id().to_owned());
    record_inspector_request(
        &state.inspector,
        inspector_enabled,
        inspector_retention_requests,
        InspectorRecord {
            base: final_inspector_base,
            timeline: &timeline,
            outcome: InspectorOutcome::Completed,
            status: upstream_status,
            error_kind: None,
            backend_attempts,
            retried_attempts,
            response_body_bytes: Some(response_bytes.len()),
            tokens: inspector_tokens(usage),
        },
    );
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
        debug_capture_config,
        debug_capture,
        pending_debug_capture,
        inspector_base,
        inspector_enabled,
        inspector_retention_requests,
        client_info,
        mut shutdown,
        backend_target,
        backend_remote_addr,
        route,
        labels,
        model_log_fields,
        requested_model: _,
        request_body_bytes,
        request_timer,
        mut timeline,
        attempt,
        max_attempts,
        backend_attempts,
        retried_attempts,
        current_attempt,
    } = context;
    let upstream_status = upstream.status();
    let upstream_headers = upstream.headers().clone();
    let content_type = header_str(&upstream_headers, &CONTENT_TYPE).map(str::to_owned);
    let normalize_sse = openai::is_event_stream_content_type(content_type.as_deref());

    state
        .metrics
        .record_request(&labels, upstream_status.as_u16(), request_timer.elapsed());
    timeline.mark(TimelineEvent::ClientResponseReady);
    let stream_metrics = StreamMetrics::new(StreamMetricsInit {
        metrics: state.metrics.clone(),
        health_store: state.health.clone(),
        inspector_store: state.inspector.clone(),
        inspector_base,
        inspector_enabled,
        inspector_retention_requests,
        labels: labels.clone(),
        status_code: upstream_status.as_u16(),
        model_log_fields: model_log_fields.clone(),
        request_body_bytes,
        debug_capture_config,
        debug_capture,
        pending_debug_capture,
        client_info: client_info.clone(),
        backend_target: backend_target.clone(),
        backend_remote_addr,
        timeline,
        attempt,
        max_attempts,
        backend_attempts,
        retried_attempts,
        current_attempt,
    });
    debug!(
        upstream_status = upstream_status.as_u16(),
        backend = %labels.backend,
        backend_target = %backend_target,
        backend_remote_addr = %socket_addr_or_none(backend_remote_addr),
        route = %labels.route,
        peer_addr = %client_info.peer_addr(),
        effective_client_addr = %client_info.effective_client_addr(),
        trusted_proxy_addr = %client_info.trusted_proxy_addr(),
        forwarded_for = %client_info.forwarded_for(),
        user_agent = %client_info.user_agent(),
        requested_model = %model_log_fields.requested,
        public_model = %model_log_fields.public,
        backend_model = %model_log_fields.backend,
        attempt,
        max_attempts,
        request_body_bytes,
        "streaming response started"
    );
    let request_mode = route.request_mode;
    let backend_model = route.backend_model;
    let public_model = route.public_model;
    let stream = try_stream! {
        let mut stream_metrics = stream_metrics;
        let mut chunks = upstream.bytes_stream();

        if normalize_sse {
            let mut normalizer = if matches!(request_mode, openai::RequestMode::ResponsesViaChatCompletions) {
                EitherNormalizer::Responses(openai::ResponsesSseNormalizer::new(backend_model, public_model))
            } else {
                EitherNormalizer::Native(SseNormalizer::new(backend_model, public_model))
            };
            while let Some(chunk) = next_stream_chunk(&mut chunks, &mut shutdown).await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        let error_kind = upstream_error_kind(&error);
                        stream_metrics.mark_stream_error(error_kind);
                        warn!(error_kind, "upstream stream chunk failed");
                        Err(error)?
                    }
                };
                stream_metrics.mark_body_chunk();
                let normalized = normalizer.push(&chunk);
                if !normalized.is_empty() {
                    stream_metrics.add_usage(normalizer.usage());
                    normalizer.clear_usage();
                    yield Bytes::from(normalized);
                }
            }
            stream_metrics.mark_body_complete();
            let tail = normalizer.finish();
            if !tail.is_empty() {
                stream_metrics.add_usage(normalizer.usage());
                yield Bytes::from(tail);
            }
        } else {
            while let Some(chunk) = next_stream_chunk(&mut chunks, &mut shutdown).await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        let error_kind = upstream_error_kind(&error);
                        stream_metrics.mark_stream_error(error_kind);
                        warn!(error_kind, "upstream stream chunk failed");
                        Err(error)?
                    }
                };
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

enum UpstreamSendError {
    Request(reqwest::Error),
    Shutdown,
}

async fn send_upstream_request(
    request: reqwest::RequestBuilder,
    shutdown: &mut watch::Receiver<bool>,
) -> std::result::Result<reqwest::Response, UpstreamSendError> {
    if *shutdown.borrow() {
        return Err(UpstreamSendError::Shutdown);
    }
    let send = request.send();
    tokio::pin!(send);

    tokio::select! {
        biased;
        changed = shutdown.changed() => {
            if changed.is_err() || *shutdown.borrow() {
                Err(UpstreamSendError::Shutdown)
            } else {
                send.await.map_err(UpstreamSendError::Request)
            }
        }
        result = &mut send => result.map_err(UpstreamSendError::Request),
    }
}

enum EitherNormalizer {
    Native(SseNormalizer),
    Responses(openai::ResponsesSseNormalizer),
}

impl EitherNormalizer {
    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        match self {
            Self::Native(normalizer) => normalizer.push(chunk),
            Self::Responses(normalizer) => normalizer.push(chunk),
        }
    }

    fn finish(&mut self) -> Vec<u8> {
        match self {
            Self::Native(normalizer) => normalizer.finish(),
            Self::Responses(normalizer) => normalizer.finish(),
        }
    }

    fn usage(&self) -> UsageTotals {
        match self {
            Self::Native(normalizer) => normalizer.usage,
            Self::Responses(normalizer) => normalizer.usage,
        }
    }

    fn clear_usage(&mut self) {
        match self {
            Self::Native(normalizer) => normalizer.usage = UsageTotals::default(),
            Self::Responses(normalizer) => normalizer.usage = UsageTotals::default(),
        }
    }
}

async fn next_stream_chunk<S>(
    chunks: &mut S,
    shutdown: &mut watch::Receiver<bool>,
) -> Option<std::result::Result<Bytes, reqwest::Error>>
where
    S: futures_util::Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Unpin,
{
    if *shutdown.borrow() {
        return None;
    }

    tokio::select! {
        biased;
        changed = shutdown.changed() => {
            if changed.is_err() || *shutdown.borrow() {
                None
            } else {
                chunks.next().await
            }
        }
        chunk = chunks.next() => chunk,
    }
}

enum BufferedBodyReadError {
    Upstream(reqwest::Error),
    Shutdown,
}

async fn read_buffered_upstream_body(
    upstream: reqwest::Response,
    timeline: &mut RequestTimeline,
    mut current_attempt: Option<&mut InspectorAttemptBuilder>,
    shutdown: &mut watch::Receiver<bool>,
) -> std::result::Result<Bytes, BufferedBodyReadError> {
    let mut bytes = Vec::new();
    let mut chunks = upstream.bytes_stream();
    loop {
        if *shutdown.borrow() {
            return Err(BufferedBodyReadError::Shutdown);
        }
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Err(BufferedBodyReadError::Shutdown);
                }
            }
            chunk = chunks.next() => {
                let Some(chunk) = chunk else {
                    break;
                };
                let chunk = chunk.map_err(BufferedBodyReadError::Upstream)?;
                let body_first_chunk_us = timeline.mark(TimelineEvent::BackendBodyFirstChunk);
                if let Some(attempt_record) = current_attempt.as_deref_mut() {
                    attempt_record.mark_body_first_chunk(body_first_chunk_us);
                }
                bytes.extend_from_slice(&chunk);
            }
        }
    }
    let body_complete_us = timeline.mark(TimelineEvent::BackendBodyComplete);
    if let Some(attempt_record) = current_attempt {
        attempt_record.backend_body_complete_us = Some(body_complete_us);
    }
    Ok(Bytes::from(bytes))
}

struct CapturedUpstreamErrorBody {
    bytes: Bytes,
    truncated: bool,
}

async fn read_capped_upstream_error_body(
    upstream: reqwest::Response,
    timeline: &mut RequestTimeline,
    mut current_attempt: Option<&mut InspectorAttemptBuilder>,
    shutdown: &mut watch::Receiver<bool>,
) -> std::result::Result<CapturedUpstreamErrorBody, BufferedBodyReadError> {
    let mut bytes = Vec::new();
    let mut chunks = upstream.bytes_stream();
    let mut truncated = false;
    let mut completed = false;

    loop {
        let Some(chunk) = next_stream_chunk(&mut chunks, shutdown).await else {
            if *shutdown.borrow() {
                return Err(BufferedBodyReadError::Shutdown);
            }
            completed = true;
            break;
        };
        let chunk = chunk.map_err(BufferedBodyReadError::Upstream)?;
        let body_first_chunk_us = timeline.mark(TimelineEvent::BackendBodyFirstChunk);
        if let Some(attempt_record) = current_attempt.as_deref_mut() {
            attempt_record.mark_body_first_chunk(body_first_chunk_us);
        }

        let remaining = MAX_UPSTREAM_ERROR_CAPTURE_BYTES.saturating_sub(bytes.len());
        if chunk.len() > remaining {
            bytes.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        bytes.extend_from_slice(&chunk);
        if bytes.len() == MAX_UPSTREAM_ERROR_CAPTURE_BYTES {
            truncated = true;
            break;
        }
    }

    if completed {
        let body_complete_us = timeline.mark(TimelineEvent::BackendBodyComplete);
        if let Some(attempt_record) = current_attempt {
            attempt_record.backend_body_complete_us = Some(body_complete_us);
        }
    }

    Ok(CapturedUpstreamErrorBody {
        bytes: Bytes::from(bytes),
        truncated,
    })
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

fn backend_target(base_url: &str) -> String {
    let Ok(url) = url::Url::parse(base_url) else {
        return "unknown".to_owned();
    };
    let host = url.host_str().unwrap_or("unknown");
    match url.port_or_known_default() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}

fn socket_addr_or_none(address: Option<SocketAddr>) -> String {
    address
        .map(|address| address.to_string())
        .unwrap_or_else(|| "none".to_owned())
}

fn debug_capture_id(capture: Option<&RequestCapture>) -> &str {
    capture.map(RequestCapture::id).unwrap_or("none")
}

fn duration_millis(duration: std::time::Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
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

fn inspector_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                '_'
            } else {
                character
            }
        })
        .take(MAX_INSPECTOR_TEXT_CHARS)
        .collect()
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

fn retryable_send_error(error: &reqwest::Error) -> bool {
    error.is_connect()
        || (error.is_request() && !error.is_body() && !error.is_decode() && !error.is_redirect())
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

struct RequestObservationBase {
    inspector_enabled: bool,
    inspector_retention_requests: usize,
    record_id: String,
    client_request_id: Option<String>,
    started_at_unix_ms: u64,
    method: String,
    path: String,
    query: Option<String>,
    client_info: ClientInfo,
    request_body_bytes: usize,
}

struct PreflightInspectorRecord<'a> {
    state: &'a Arc<AppState>,
    observation: &'a RequestObservationBase,
    timeline: &'a RequestTimeline,
    route: &'a str,
    identity: &'a str,
    model: Option<&'a str>,
    stream: bool,
    status: StatusCode,
    stage: &'static str,
}

fn routed_inspector_base(
    observation: &RequestObservationBase,
    labels: &MetricLabels,
    model_log_fields: &ModelLogFields,
    backend_target: &str,
) -> InspectorRequestBase {
    InspectorRequestBase {
        record_id: observation.record_id.clone(),
        client_request_id: observation.client_request_id.clone(),
        started_at_unix_ms: observation.started_at_unix_ms,
        method: observation.method.clone(),
        path: observation.path.clone(),
        query: observation.query.clone(),
        route: labels.route.clone(),
        identity: labels.identity.clone(),
        requested_model: model_log_fields.requested.clone(),
        public_model: model_log_fields.public.clone(),
        backend_model: model_log_fields.backend.clone(),
        backend: labels.backend.clone(),
        backend_target: backend_target.to_owned(),
        backend_remote_addr: None,
        stream: labels.stream,
        peer_addr: observation.client_info.peer_addr().to_owned(),
        effective_client_addr: observation.client_info.effective_client_addr().to_owned(),
        trusted_proxy_addr: observation.client_info.trusted_proxy_addr().to_owned(),
        forwarded_for: observation.client_info.forwarded_for().to_owned(),
        user_agent: observation.client_info.user_agent().to_owned(),
        request_body_bytes: observation.request_body_bytes,
        debug_capture_id: None,
    }
}

fn preflight_inspector_base(record: &PreflightInspectorRecord<'_>) -> InspectorRequestBase {
    let model = record.model.unwrap_or("none").to_owned();
    InspectorRequestBase {
        record_id: record.observation.record_id.clone(),
        client_request_id: record.observation.client_request_id.clone(),
        started_at_unix_ms: record.observation.started_at_unix_ms,
        method: record.observation.method.clone(),
        path: record.observation.path.clone(),
        query: record.observation.query.clone(),
        route: record.route.to_owned(),
        identity: record.identity.to_owned(),
        requested_model: model.clone(),
        public_model: model,
        backend_model: "none".to_owned(),
        backend: "none".to_owned(),
        backend_target: "none".to_owned(),
        backend_remote_addr: None,
        stream: record.stream,
        peer_addr: record.observation.client_info.peer_addr().to_owned(),
        effective_client_addr: record
            .observation
            .client_info
            .effective_client_addr()
            .to_owned(),
        trusted_proxy_addr: record
            .observation
            .client_info
            .trusted_proxy_addr()
            .to_owned(),
        forwarded_for: record.observation.client_info.forwarded_for().to_owned(),
        user_agent: record.observation.client_info.user_agent().to_owned(),
        request_body_bytes: record.observation.request_body_bytes,
        debug_capture_id: None,
    }
}

fn record_preflight_inspector(record: PreflightInspectorRecord<'_>) {
    if !record.observation.inspector_enabled {
        return;
    }

    record_inspector_request(
        &record.state.inspector,
        record.observation.inspector_enabled,
        record.observation.inspector_retention_requests,
        InspectorRecord {
            base: preflight_inspector_base(&record),
            timeline: record.timeline,
            outcome: InspectorOutcome::Preflight {
                stage: record.stage,
            },
            status: record.status,
            error_kind: None,
            backend_attempts: Vec::new(),
            retried_attempts: Vec::new(),
            response_body_bytes: None,
            tokens: InspectorTokenCounts::default(),
        },
    );
}

fn record_context_inspector(
    context: &ProxyContext,
    outcome: InspectorOutcome,
    status: StatusCode,
    error_kind: Option<&'static str>,
    response_body_bytes: Option<usize>,
    tokens: InspectorTokenCounts,
) {
    let mut base = context.inspector_base.clone();
    base.backend_remote_addr = context
        .backend_remote_addr
        .map(|address| address.to_string());
    base.debug_capture_id = context
        .debug_capture
        .as_ref()
        .map(|capture| capture.id().to_owned());
    record_inspector_request(
        &context.state.inspector,
        context.inspector_enabled,
        context.inspector_retention_requests,
        InspectorRecord {
            base,
            timeline: &context.timeline,
            outcome,
            status,
            error_kind,
            backend_attempts: context.backend_attempts.clone(),
            retried_attempts: context.retried_attempts.clone(),
            response_body_bytes,
            tokens,
        },
    );
}

struct InspectorRecord<'a> {
    base: InspectorRequestBase,
    timeline: &'a RequestTimeline,
    outcome: InspectorOutcome,
    status: StatusCode,
    error_kind: Option<&'static str>,
    backend_attempts: Vec<InspectorAttemptRecord>,
    retried_attempts: Vec<InspectorAttemptRecord>,
    response_body_bytes: Option<usize>,
    tokens: InspectorTokenCounts,
}

fn record_inspector_request(
    store: &InspectorStore,
    enabled: bool,
    retention_requests: usize,
    record: InspectorRecord<'_>,
) {
    if !enabled {
        return;
    }

    store.record(
        enabled,
        retention_requests,
        crate::observe::InspectorRequestRecord::new(InspectorRequestRecordInit {
            base: record.base,
            outcome: record.outcome,
            status: record.status.as_u16(),
            error_kind: record.error_kind.map(str::to_owned),
            backend_attempts: record.backend_attempts,
            retried_attempts: record.retried_attempts,
            response_body_bytes: record.response_body_bytes,
            tokens: record.tokens,
            timeline: record.timeline.snapshot(),
        }),
    );
}

fn inspector_tokens(usage: UsageTotals) -> InspectorTokenCounts {
    InspectorTokenCounts {
        input: usage.input,
        cached_input: usage.cached_input,
        output: usage.output,
    }
}

struct PreflightFailureLog<'a> {
    timeline: &'a RequestTimeline,
    route: &'a str,
    identity: &'a str,
    model: Option<&'a str>,
    stream: bool,
    status: StatusCode,
    stage: &'static str,
    client_info: &'a ClientInfo,
}

fn warn_preflight_failure(failure: PreflightFailureLog<'_>) {
    let snapshot = failure.timeline.snapshot();
    warn!(
        route = failure.route,
        identity = failure.identity,
        model = failure.model.unwrap_or("none"),
        stream = failure.stream,
        status = failure.status.as_u16(),
        stage = failure.stage,
        peer_addr = %failure.client_info.peer_addr(),
        effective_client_addr = %failure.client_info.effective_client_addr(),
        trusted_proxy_addr = %failure.client_info.trusted_proxy_addr(),
        forwarded_for = %failure.client_info.forwarded_for(),
        user_agent = %failure.client_info.user_agent(),
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
        backend_target = %context.backend_target,
        backend_remote_addr = %socket_addr_or_none(context.backend_remote_addr),
        route = %context.labels.route,
        peer_addr = %context.client_info.peer_addr(),
        effective_client_addr = %context.client_info.effective_client_addr(),
        trusted_proxy_addr = %context.client_info.trusted_proxy_addr(),
        forwarded_for = %context.client_info.forwarded_for(),
        user_agent = %context.client_info.user_agent(),
        requested_model = %context.model_log_fields.requested,
        public_model = %context.model_log_fields.public,
        backend_model = %context.model_log_fields.backend,
        attempt = context.attempt,
        max_attempts = context.max_attempts,
        stream = context.labels.stream,
        request_body_bytes = context.request_body_bytes,
        debug_capture_id = debug_capture_id(context.debug_capture.as_ref()),
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

fn warn_proxy_retry(
    context: &ProxyContext,
    next_route: &SelectedRoute,
    client_status: StatusCode,
    error_kind: &'static str,
    message: &'static str,
) {
    let snapshot = context.timeline.snapshot();
    warn!(
        client_status = client_status.as_u16(),
        error_kind,
        backend = %context.labels.backend,
        backend_target = %context.backend_target,
        backend_remote_addr = %socket_addr_or_none(context.backend_remote_addr),
        next_backend = %next_route.backend_id,
        next_backend_target = %backend_target(&next_route.base_url),
        route = %context.labels.route,
        peer_addr = %context.client_info.peer_addr(),
        effective_client_addr = %context.client_info.effective_client_addr(),
        trusted_proxy_addr = %context.client_info.trusted_proxy_addr(),
        forwarded_for = %context.client_info.forwarded_for(),
        user_agent = %context.client_info.user_agent(),
        requested_model = %context.model_log_fields.requested,
        public_model = %context.model_log_fields.public,
        backend_model = %context.model_log_fields.backend,
        attempt = context.attempt,
        max_attempts = context.max_attempts,
        next_attempt = context.attempt + 1,
        stream = context.labels.stream,
        request_body_bytes = context.request_body_bytes,
        debug_capture_id = debug_capture_id(context.debug_capture.as_ref()),
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
        "proxy retry timeline"
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
    health_store: BackendHealthStore,
    inspector_store: InspectorStore,
    inspector_enabled: bool,
    inspector_retention_requests: usize,
    inspector_base: InspectorRequestBase,
    labels: MetricLabels,
    status_code: u16,
    model_log_fields: ModelLogFields,
    request_body_bytes: usize,
    debug_capture_config: DebugCaptureConfig,
    debug_capture: Option<RequestCapture>,
    pending_debug_capture: Option<PendingDebugCapture>,
    client_info: ClientInfo,
    backend_target: String,
    backend_remote_addr: Option<SocketAddr>,
    timeline: RequestTimeline,
    attempt: usize,
    max_attempts: usize,
    backend_attempts: Vec<InspectorAttemptRecord>,
    retried_attempts: Vec<InspectorAttemptRecord>,
    current_attempt: Option<InspectorAttemptBuilder>,
    started: Instant,
    usage: UsageTotals,
    body_complete: bool,
    stream_error_kind: Option<&'static str>,
}

struct StreamMetricsInit {
    metrics: crate::metrics::Metrics,
    health_store: BackendHealthStore,
    inspector_store: InspectorStore,
    inspector_base: InspectorRequestBase,
    inspector_enabled: bool,
    inspector_retention_requests: usize,
    labels: MetricLabels,
    status_code: u16,
    model_log_fields: ModelLogFields,
    request_body_bytes: usize,
    debug_capture_config: DebugCaptureConfig,
    debug_capture: Option<RequestCapture>,
    pending_debug_capture: Option<PendingDebugCapture>,
    client_info: ClientInfo,
    backend_target: String,
    backend_remote_addr: Option<SocketAddr>,
    timeline: RequestTimeline,
    attempt: usize,
    max_attempts: usize,
    backend_attempts: Vec<InspectorAttemptRecord>,
    retried_attempts: Vec<InspectorAttemptRecord>,
    current_attempt: Option<InspectorAttemptBuilder>,
}

impl StreamMetrics {
    fn new(init: StreamMetricsInit) -> Self {
        Self {
            metrics: init.metrics,
            health_store: init.health_store,
            inspector_store: init.inspector_store,
            inspector_base: init.inspector_base,
            inspector_enabled: init.inspector_enabled,
            inspector_retention_requests: init.inspector_retention_requests,
            labels: init.labels,
            status_code: init.status_code,
            model_log_fields: init.model_log_fields,
            request_body_bytes: init.request_body_bytes,
            debug_capture_config: init.debug_capture_config,
            debug_capture: init.debug_capture,
            pending_debug_capture: init.pending_debug_capture,
            client_info: init.client_info,
            backend_target: init.backend_target,
            backend_remote_addr: init.backend_remote_addr,
            timeline: init.timeline,
            attempt: init.attempt,
            max_attempts: init.max_attempts,
            backend_attempts: init.backend_attempts,
            retried_attempts: init.retried_attempts,
            current_attempt: init.current_attempt,
            started: Instant::now(),
            usage: UsageTotals::default(),
            body_complete: false,
            stream_error_kind: None,
        }
    }

    fn mark_body_chunk(&mut self) {
        let body_first_chunk_us = self.timeline.mark(TimelineEvent::BackendBodyFirstChunk);
        if let Some(attempt_record) = &mut self.current_attempt {
            attempt_record.mark_body_first_chunk(body_first_chunk_us);
        }
    }

    fn mark_body_complete(&mut self) {
        self.body_complete = true;
        let body_complete_us = self.timeline.mark(TimelineEvent::BackendBodyComplete);
        if let Some(attempt_record) = &mut self.current_attempt {
            attempt_record.backend_body_complete_us = Some(body_complete_us);
        }
    }

    fn add_usage(&mut self, usage: UsageTotals) {
        self.usage.input += usage.input;
        self.usage.cached_input += usage.cached_input;
        self.usage.output += usage.output;
        self.usage.total += usage.total;
    }

    fn mark_stream_error(&mut self, error_kind: &'static str) {
        self.stream_error_kind = Some(error_kind);
    }

    fn inspect_base(&self) -> InspectorRequestBase {
        let mut base = self.inspector_base.clone();
        base.backend_remote_addr = self.backend_remote_addr.map(|address| address.to_string());
        base
    }
}

impl Drop for StreamMetrics {
    fn drop(&mut self) {
        let duration = self.started.elapsed();
        let stream_complete_us = self.timeline.mark(TimelineEvent::StreamComplete);
        if self.stream_error_kind.is_some() {
            ensure_failure_debug_capture(
                &self.debug_capture_config,
                &mut self.pending_debug_capture,
                &mut self.debug_capture,
                &mut self.timeline,
                self.current_attempt.as_mut(),
            );
        }
        if let Some(mut attempt_record) = self.current_attempt.take() {
            attempt_record.stream_complete_us = Some(stream_complete_us);
            self.backend_attempts.push(attempt_record.finish(
                StatusCode::from_u16(self.status_code).unwrap_or(StatusCode::OK),
                Some(self.status_code),
                match self.stream_error_kind {
                    Some(_) => "upstream_stream_failed",
                    None if !self.body_complete => "stream_incomplete",
                    None => "completed",
                },
                self.stream_error_kind,
                stream_complete_us,
            ));
        }
        if !self.usage.is_empty() {
            self.metrics.record_usage(&self.labels, self.usage);
        }
        debug!(
            upstream_status = self.status_code,
            backend = %self.labels.backend,
            backend_target = %self.backend_target,
            backend_remote_addr = %socket_addr_or_none(self.backend_remote_addr),
            route = %self.labels.route,
            peer_addr = %self.client_info.peer_addr(),
            effective_client_addr = %self.client_info.effective_client_addr(),
            trusted_proxy_addr = %self.client_info.trusted_proxy_addr(),
            forwarded_for = %self.client_info.forwarded_for(),
            user_agent = %self.client_info.user_agent(),
            requested_model = %self.model_log_fields.requested,
            public_model = %self.model_log_fields.public,
            backend_model = %self.model_log_fields.backend,
            attempt = self.attempt,
            max_attempts = self.max_attempts,
            request_body_bytes = self.request_body_bytes,
            stream_duration_ms = duration.as_millis() as u64,
            input_tokens = self.usage.input,
            cached_input_tokens = self.usage.cached_input,
            output_tokens = self.usage.output,
            "streaming response completed"
        );
        let inspector_outcome = match self.stream_error_kind {
            Some(_) => InspectorOutcome::UpstreamStreamFailed,
            None if !self.body_complete => InspectorOutcome::StreamIncomplete,
            None => InspectorOutcome::Completed,
        };
        if let Some(error_kind) = self.stream_error_kind {
            self.health_store.record_failure(
                &self.labels.backend,
                duration,
                self.status_code,
                error_kind,
            );
        } else if self.body_complete {
            self.health_store
                .record_success(&self.labels.backend, duration, self.status_code);
        }
        if let Some(capture) = &mut self.debug_capture {
            let capture_outcome = match self.stream_error_kind {
                Some(error_kind) => CaptureOutcome::UpstreamStreamFailed {
                    upstream_status: self.status_code,
                    stream_duration_ms: duration.as_millis(),
                    error_kind,
                    input_tokens: self.usage.input,
                    cached_input_tokens: self.usage.cached_input,
                    output_tokens: self.usage.output,
                },
                None if !self.body_complete => CaptureOutcome::StreamIncomplete {
                    upstream_status: self.status_code,
                    stream_duration_ms: duration.as_millis(),
                    input_tokens: self.usage.input,
                    cached_input_tokens: self.usage.cached_input,
                    output_tokens: self.usage.output,
                },
                None => CaptureOutcome::StreamCompleted {
                    upstream_status: self.status_code,
                    stream_duration_ms: duration.as_millis(),
                    input_tokens: self.usage.input,
                    cached_input_tokens: self.usage.cached_input,
                    output_tokens: self.usage.output,
                },
            };
            capture.record_outcome(capture_outcome);
        }
        let mut inspector_base = self.inspect_base();
        inspector_base.debug_capture_id = self
            .debug_capture
            .as_ref()
            .map(|capture| capture.id().to_owned());
        record_inspector_request(
            &self.inspector_store,
            self.inspector_enabled,
            self.inspector_retention_requests,
            InspectorRecord {
                base: inspector_base,
                timeline: &self.timeline,
                outcome: inspector_outcome,
                status: StatusCode::from_u16(self.status_code).unwrap_or(StatusCode::OK),
                error_kind: self.stream_error_kind,
                backend_attempts: self.backend_attempts.clone(),
                retried_attempts: self.retried_attempts.clone(),
                response_body_bytes: None,
                tokens: inspector_tokens(self.usage),
            },
        );
        debug_timeline_fields(
            self.timeline.snapshot(),
            "streaming response timeline snapshot",
        );
        self.metrics
            .record_stream(&self.labels, self.status_code, duration);
    }
}
