use std::net::SocketAddr;

use axum::http::StatusCode;
use tracing::{debug, warn};

use onair_obs::observe::debug_capture::RequestCapture;
use onair_obs::observe::{ClientInfo, RequestTimeline, TimelineSnapshot};
use onair_proxy::routing::SelectedRoute;

use super::ProxyContext;
use super::upstream::backend_target;

pub(super) fn socket_addr_or_none(address: Option<SocketAddr>) -> String {
    address
        .map(|address| address.to_string())
        .unwrap_or_else(|| "none".to_owned())
}

pub(super) fn debug_capture_id(capture: Option<&RequestCapture>) -> &str {
    capture.map(RequestCapture::id).unwrap_or("none")
}

pub(super) struct PreflightFailureLog<'a> {
    pub(super) timeline: &'a RequestTimeline,
    pub(super) route: &'a str,
    pub(super) identity: &'a str,
    pub(super) model: Option<&'a str>,
    pub(super) stream: bool,
    pub(super) status: StatusCode,
    pub(super) stage: &'static str,
    pub(super) client_info: &'a ClientInfo,
}

pub(super) fn warn_preflight_failure(failure: PreflightFailureLog<'_>) {
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

pub(super) fn warn_proxy_failure(
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

pub(super) fn warn_proxy_retry(
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

pub(super) fn debug_timeline_fields(snapshot: TimelineSnapshot, message: &'static str) {
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
