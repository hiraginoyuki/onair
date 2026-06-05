use std::net::SocketAddr;
use std::time::Duration;

use axum::http::StatusCode;

use onair_obs::observe::InspectorAttemptRecord;
use onair_obs::observe::debug_capture::RequestCapture;

pub(super) struct InspectorAttemptInit<'a> {
    pub(super) attempt: usize,
    pub(super) backend: String,
    pub(super) backend_target: String,
    pub(super) backend_remote_addr: Option<SocketAddr>,
    pub(super) debug_capture: Option<&'a RequestCapture>,
    pub(super) started_us: u64,
}

#[derive(Debug, Clone)]
pub(super) struct InspectorAttemptBuilder {
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
    pub(super) fn new(init: InspectorAttemptInit<'_>) -> Self {
        Self {
            attempt: init.attempt,
            backend: init.backend,
            backend_target: init.backend_target,
            backend_remote_addr: init.backend_remote_addr.map(|address| address.to_string()),
            debug_capture_id: init.debug_capture.map(|capture| capture.id().to_owned()),
            started_us: init.started_us,
            request_rewritten_us: None,
            debug_capture_done_us: None,
            backend_forward_start_us: None,
            backend_headers_received_us: None,
            backend_body_first_chunk_us: None,
            backend_body_complete_us: None,
            stream_complete_us: None,
        }
    }

    pub(super) fn set_debug_capture(&mut self, capture: Option<&RequestCapture>) {
        self.debug_capture_id = capture.map(|capture| capture.id().to_owned());
    }

    pub(super) fn set_backend_remote_addr(&mut self, address: Option<SocketAddr>) {
        self.backend_remote_addr = address.map(|address| address.to_string());
    }

    pub(super) fn mark_request_rewritten(&mut self, elapsed_us: u64) {
        self.request_rewritten_us = Some(elapsed_us);
    }

    pub(super) fn mark_debug_capture_done(&mut self, elapsed_us: u64) {
        self.debug_capture_done_us = Some(elapsed_us);
    }

    pub(super) fn mark_backend_forward_start(&mut self, elapsed_us: u64) {
        self.backend_forward_start_us = Some(elapsed_us);
    }

    pub(super) fn mark_backend_headers_received(&mut self, elapsed_us: u64) {
        self.backend_headers_received_us = Some(elapsed_us);
    }

    pub(super) fn mark_body_first_chunk(&mut self, elapsed_us: u64) {
        if self.backend_body_first_chunk_us.is_none() {
            self.backend_body_first_chunk_us = Some(elapsed_us);
        }
    }

    pub(super) fn mark_body_complete(&mut self, elapsed_us: u64) {
        self.backend_body_complete_us = Some(elapsed_us);
    }

    pub(super) fn mark_stream_complete(&mut self, elapsed_us: u64) {
        self.stream_complete_us = Some(elapsed_us);
    }

    #[allow(dead_code)]
    pub(super) fn to_attempt_record(&self, now_us: u64) -> InspectorAttemptRecord {
        let elapsed_us = now_us.saturating_sub(self.started_us);
        InspectorAttemptRecord {
            attempt: self.attempt,
            backend: self.backend.clone(),
            backend_target: self.backend_target.clone(),
            backend_remote_addr: self.backend_remote_addr.clone(),
            debug_capture_id: self.debug_capture_id.clone(),
            status: 0,
            outcome: "in_progress".to_owned(),
            error_kind: None,
            started_us: self.started_us,
            ended_us: now_us,
            elapsed_us,
            elapsed_ms: Duration::from_micros(elapsed_us).as_millis() as u64,
            upstream_status: None,
            request_rewritten_us: self.request_rewritten_us,
            debug_capture_done_us: self.debug_capture_done_us,
            backend_forward_start_us: self.backend_forward_start_us,
            backend_headers_received_us: self.backend_headers_received_us,
            backend_body_first_chunk_us: self.backend_body_first_chunk_us,
            backend_body_complete_us: self.backend_body_complete_us,
            stream_complete_us: self.stream_complete_us,
        }
    }

    pub(super) fn finish_at_body_complete_or(
        self,
        status: StatusCode,
        upstream_status: Option<u16>,
        outcome: &'static str,
        error_kind: Option<&'static str>,
        fallback_ended_us: u64,
    ) -> InspectorAttemptRecord {
        let ended_us = self.backend_body_complete_us.unwrap_or(fallback_ended_us);
        self.finish(status, upstream_status, outcome, error_kind, ended_us)
    }

    pub(super) fn finish(
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
            elapsed_ms: Duration::from_micros(elapsed_us).as_millis() as u64,
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
