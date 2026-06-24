use std::net::SocketAddr;
use std::time::Instant;

use async_stream::try_stream;
use axum::body::{Body, Bytes};
use axum::http::header::CONTENT_TYPE;
use axum::http::{Response, StatusCode};
use futures_util::TryStreamExt;
use tracing::{debug, warn};

use onair_core::config::DebugCaptureConfig;
use onair_core::error::ApiError;
use onair_core::openai::{self, SseStrategy, UsageDiagnostics, UsageTotals};
use onair_obs::metrics::MetricLabels;
use onair_obs::observe::debug_capture::{CaptureOutcome, RequestCapture};
use onair_obs::observe::{
    BackendHealthStore, ClientInfo, InspectorAttemptRecord, InspectorOutcome, InspectorRequestBase,
    InspectorRequestRecord, InspectorRequestRecordInit, InspectorStore, InspectorTokenCounts,
    LiveRecord, RequestTimeline, TimelineEvent,
};

use super::attempt::InspectorAttemptBuilder;
use super::inspector::{InspectorRecord, inspector_tokens, record_inspector_request};
use super::logging::{debug_timeline_fields, socket_addr_or_none};
use super::upstream::{
    BufferedBodyReadError, next_stream_chunk, read_buffered_upstream_body, upstream_error_kind,
};
use super::{
    ModelLogFields, PendingDebugCapture, ProxyContext, ensure_failure_debug_capture, header_str,
    response_builder,
};

pub(super) async fn buffered_response(
    context: ProxyContext<'_>,
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
        live_record,
        client_info,
        mut shutdown,
        backend_target,
        backend_remote_addr,
        route,
        labels,
        model_log_fields,
        requested_model: _,
        client_stream_usage_requested: _,
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
                    client_status: StatusCode::BAD_GATEWAY,
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
    live_record.update(|r| {
        r.timeline = timeline.snapshot();
    });

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
        capture.record_outcome(CaptureOutcome::Success { upstream_status });
    }
    state.health.record_success(
        &labels.backend,
        request_timer.elapsed(),
        upstream_status.as_u16(),
    );
    if let Some(attempt_record) = current_attempt.take() {
        backend_attempts.push(attempt_record.finish_at_body_complete_or(
            upstream_status,
            Some(upstream_status.as_u16()),
            "completed",
            None,
            timeline.elapsed_us(),
        ));
    }
    timeline.mark(TimelineEvent::ClientResponseReady);
    live_record.update(|r| {
        r.timeline = timeline.snapshot();
        r.backend_attempts = backend_attempts.clone();
    });
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
        client_headers,
        &upstream_headers,
        content_type.as_deref(),
        false,
    )
    .body(Body::from(response_bytes))
    .map_err(|_| ApiError::internal())
}

pub(super) fn streaming_response(
    context: ProxyContext<'_>,
    upstream: reqwest::Response,
) -> Response<Body> {
    let ProxyContext {
        state,
        client_headers,
        debug_capture_config,
        debug_capture,
        pending_debug_capture,
        inspector_base,
        inspector_enabled,
        inspector_retention_requests,
        live_record,
        client_info,
        mut shutdown,
        backend_target,
        backend_remote_addr,
        route,
        labels,
        model_log_fields,
        requested_model: _,
        client_stream_usage_requested,
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
    let normalize_sse =
        labels.stream || openai::is_event_stream_content_type(content_type.as_deref());

    state
        .metrics
        .record_request(&labels, upstream_status.as_u16(), request_timer.elapsed());
    timeline.mark(TimelineEvent::ClientResponseReady);
    live_record.update(|r| {
        r.timeline = timeline.snapshot();
    });
    let stream_metrics = StreamMetrics::new(StreamMetricsInit {
        metrics: (*state.metrics).clone(),
        health_store: (*state.health).clone(),
        inspector_store: (*state.inspector).clone(),
        inspector_base,
        inspector_enabled,
        inspector_retention_requests,
        labels: labels.clone(),
        status_code: upstream_status,
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
        live_record,
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
    let emit_usage_to_client = labels.route != "chat_completions" || client_stream_usage_requested;
    let stream = try_stream! {
        let mut stream_metrics = stream_metrics;
        let mut chunks = upstream.bytes_stream();

        if normalize_sse {
            let mut strategy = SseStrategy::new(
                request_mode,
                backend_model,
                public_model,
                emit_usage_to_client,
            );
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
                let normalized = strategy.push(&chunk);
                if !normalized.is_empty() {
                    stream_metrics.add_usage(strategy.usage());
                    stream_metrics.add_usage_diagnostics(strategy.diagnostics());
                    strategy.clear_usage();
                    strategy.clear_diagnostics();
                    yield Bytes::from(normalized);
                }
            }
            stream_metrics.mark_body_complete();
            let tail = strategy.finish();
            if !tail.is_empty() {
                stream_metrics.add_usage(strategy.usage());
                stream_metrics.add_usage_diagnostics(strategy.diagnostics());
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
        client_headers,
        &upstream_headers,
        Some("text/event-stream"),
        true,
    )
    .body(Body::from_stream(stream))
    .expect("stream response builder is valid")
}

struct StreamMetrics {
    metrics: onair_obs::metrics::Metrics,
    health_store: BackendHealthStore,
    inspector_store: InspectorStore,
    inspector_enabled: bool,
    inspector_retention_requests: usize,
    inspector_base: InspectorRequestBase,
    labels: MetricLabels,
    status_code: StatusCode,
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
    usage_diagnostics: UsageDiagnostics,
    body_complete: bool,
    stream_error_kind: Option<&'static str>,
    live_record: LiveRecord,
}

struct StreamMetricsInit {
    metrics: onair_obs::metrics::Metrics,
    health_store: BackendHealthStore,
    inspector_store: InspectorStore,
    inspector_base: InspectorRequestBase,
    inspector_enabled: bool,
    inspector_retention_requests: usize,
    labels: MetricLabels,
    status_code: StatusCode,
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
    live_record: LiveRecord,
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
            usage_diagnostics: UsageDiagnostics::default(),
            body_complete: false,
            stream_error_kind: None,
            live_record: init.live_record,
        }
    }

    fn mark_body_chunk(&mut self) {
        let body_first_chunk_us = self.timeline.mark(TimelineEvent::BackendBodyFirstChunk);
        if let Some(attempt_record) = &mut self.current_attempt {
            attempt_record.mark_body_first_chunk(body_first_chunk_us);
        }
        self.live_record.update(|r| {
            r.timeline = self.timeline.snapshot();
        });
    }

    fn mark_body_complete(&mut self) {
        self.body_complete = true;
        let body_complete_us = self.timeline.mark(TimelineEvent::BackendBodyComplete);
        if let Some(attempt_record) = &mut self.current_attempt {
            attempt_record.mark_body_complete(body_complete_us);
        }
        self.live_record.update(|r| {
            r.timeline = self.timeline.snapshot();
        });
    }

    fn add_usage(&mut self, usage: UsageTotals) {
        self.usage.input += usage.input;
        self.usage.cached_input += usage.cached_input;
        self.usage.output += usage.output;
        self.usage.total += usage.total;
    }

    fn add_usage_diagnostics(&mut self, diagnostics: UsageDiagnostics) {
        self.usage_diagnostics.merge(diagnostics);
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
        self.live_record.update(|r| {
            r.timeline = self.timeline.snapshot();
        });
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
            attempt_record.mark_stream_complete(stream_complete_us);
            self.backend_attempts.push(attempt_record.finish(
                self.status_code,
                Some(self.status_code.as_u16()),
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
            upstream_status = %self.status_code,
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
            total_tokens = self.usage.total,
            stream_usage_object_count = self.usage_diagnostics.usage_object_count,
            stream_usage_keys = ?self.usage_diagnostics.usage_keys,
            stream_event_names = ?self.usage_diagnostics.event_names,
            stream_usage_event_names = ?self.usage_diagnostics.usage_event_names,
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
                self.status_code.as_u16(),
                error_kind,
            );
        } else if self.body_complete {
            self.health_store.record_success(
                &self.labels.backend,
                duration,
                self.status_code.as_u16(),
            );
        }
        if let Some(capture) = &mut self.debug_capture {
            capture.record_stream_usage(self.usage_diagnostics.clone());
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
        self.inspector_store.upsert_final(
            self.inspector_enabled,
            self.inspector_retention_requests,
            InspectorRequestRecord::new(InspectorRequestRecordInit {
                base: inspector_base,
                outcome: inspector_outcome,
                status: self.status_code.as_u16(),
                error_kind: self.stream_error_kind.map(str::to_owned),
                backend_attempts: self.backend_attempts.clone(),
                retried_attempts: self.retried_attempts.clone(),
                response_body_bytes: None,
                tokens: inspector_tokens(self.usage),
                timeline: self.timeline.snapshot(),
            }),
        );
        debug_timeline_fields(
            self.timeline.snapshot(),
            "streaming response timeline snapshot",
        );
        self.metrics
            .record_stream(&self.labels, self.status_code.as_u16(), duration);
    }
}
