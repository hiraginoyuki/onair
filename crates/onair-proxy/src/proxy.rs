use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::http::header::{
    ACCEPT, AUTHORIZATION, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE, HeaderName,
    HeaderValue,
};
use axum::http::{HeaderMap, Method, Response, StatusCode, Uri};
use tracing::{Instrument, info_span, warn};

use crate::proxy_state::ProxyState;
use crate::routing::{self, SelectedRoute};
use onair_core::auth::authenticate;
use onair_core::config::DebugCaptureMode;
use onair_core::error::ApiError;
use onair_core::openai;
use onair_obs::metrics::{MetricLabels, RequestTimer};
use onair_obs::observe::debug_capture::CaptureOutcome;
use onair_obs::observe::{
    ClientInfo, InspectorOutcome, InspectorStore, InspectorTokenCounts, LiveRecord,
    RequestTimeline, TimelineEvent,
};

pub mod attempt;
pub mod context;
pub mod inspector;
pub mod logging;
pub mod response;
pub mod upstream;

use self::attempt::{InspectorAttemptBuilder, InspectorAttemptInit};
pub(super) use self::context::{
    ModelLogFields, PendingDebugCapture, ProxyContext, ProxyRequest, ensure_failure_debug_capture,
    refresh_live_record,
};
use self::inspector::{
    PreflightInspectorRecord, RequestObservationBase, initial_live_record,
    record_context_inspector, record_preflight_inspector, routed_inspector_base,
};
use self::logging::{
    PreflightFailureLog, debug_capture_id, warn_preflight_failure, warn_proxy_failure,
    warn_proxy_retry,
};
use self::response::{buffered_response, streaming_response};
use self::upstream::{
    BufferedBodyReadError, UpstreamSendError, backend_target, read_capped_upstream_error_body,
    retryable_send_error, send_upstream_request, upstream_error_kind, upstream_path, upstream_url,
};

pub const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
const MAX_INSPECTOR_TEXT_CHARS: usize = 512;

pub async fn proxy_v1(
    state: Arc<ProxyState>,
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

    let live_record = LiveRecord::new(
        (*state.inspector).clone(),
        observation.inspector_enabled,
        observation.inspector_retention_requests,
        initial_live_record(&observation, &timeline, &route_name),
    );

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
        &config.routes,
        config.routing.strategy,
        &path,
        request_shape.model.as_deref(),
        request_shape.stream,
        request_shape.has_tools,
        Some(&sticky_key),
        &state.round_robin,
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
    live_record.update(|record| {
        record.base = inspector_base.clone();
        record.timeline = timeline.snapshot();
    });
    live_record.publish_initial();

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
    let shutdown = state.shutdown.clone();
    let context = ProxyContext {
        state,
        client_headers: headers,
        debug_capture_config: config.debug_capture.clone(),
        debug_capture: None,
        pending_debug_capture: None,
        inspector_base,
        inspector_enabled: observation.inspector_enabled,
        inspector_retention_requests: observation.inspector_retention_requests,
        live_record: Some(live_record),
        client_info,
        shutdown,
        backend_target,
        backend_remote_addr: None,
        route,
        labels,
        model_log_fields,
        requested_model: request_shape.model.clone(),
        client_stream_usage_requested: request_shape.stream_usage_requested,
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
        let mut attempt_record = InspectorAttemptBuilder::new(InspectorAttemptInit {
            attempt: context.attempt,
            backend: context.labels.backend.clone(),
            backend_target: context.backend_target.clone(),
            backend_remote_addr: context.backend_remote_addr,
            debug_capture: context.debug_capture.as_ref(),
            started_us: context.timeline.elapsed_us(),
        });
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
                chat_stream_usage: context.route.chat_stream_usage,
            },
        )
        .map_err(|error| ApiError::bad_request(error.message(), error.param()))?;
        attempt_record
            .mark_request_rewritten(context.timeline.mark(TimelineEvent::RequestRewritten));
        context.live_upsert(refresh_live_record);
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
        if context.debug_capture.is_some() {
            attempt_record
                .mark_debug_capture_done(context.timeline.mark(TimelineEvent::DebugCaptureDone));
            context.live_upsert(refresh_live_record);
        }

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

        attempt_record
            .mark_backend_forward_start(context.timeline.mark(TimelineEvent::BackendForwardStart));
        context.live_upsert(refresh_live_record);
        match send_upstream_request(upstream_request, &mut context.shutdown).await {
            Ok(response) => {
                attempt_record.mark_backend_headers_received(
                    context.timeline.mark(TimelineEvent::BackendHeadersReceived),
                );
                context.backend_remote_addr = response.remote_addr();
                attempt_record.set_backend_remote_addr(context.backend_remote_addr);
                context.current_attempt = Some(attempt_record);
                context.live_upsert(refresh_live_record);
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
                warn_proxy_failure(
                    &context,
                    StatusCode::GATEWAY_TIMEOUT,
                    "timeout",
                    "upstream request timed out",
                );
                record_context_inspector(
                    context,
                    InspectorOutcome::UpstreamTimeout,
                    StatusCode::GATEWAY_TIMEOUT,
                    None,
                    None,
                    InspectorTokenCounts::default(),
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
                warn_proxy_failure(
                    &context,
                    StatusCode::BAD_GATEWAY,
                    error_kind,
                    "upstream request failed",
                );
                record_context_inspector(
                    context,
                    InspectorOutcome::UpstreamRequestFailed,
                    StatusCode::BAD_GATEWAY,
                    Some(error_kind),
                    None,
                    InspectorTokenCounts::default(),
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
        warn_proxy_failure(
            &context,
            api_error.status,
            "upstream_non_success",
            "upstream returned non-success status",
        );
        record_context_inspector(
            context,
            InspectorOutcome::UpstreamNonSuccess,
            api_error.status,
            None,
            None,
            InspectorTokenCounts::default(),
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

    if streaming {
        builder = builder.header(
            CONTENT_TYPE,
            fallback_content_type.unwrap_or("text/event-stream"),
        );
    } else if let Some(content_type) = upstream_headers
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

fn record_preflight_failure(
    state: &ProxyState,
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
