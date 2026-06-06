use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use tokio::sync::watch;

use onair_core::config::DebugCaptureConfig;
use onair_obs::metrics::{MetricLabels, RequestTimer};
use onair_obs::observe::debug_capture::{CaptureRequest, RequestCapture, capture_request};
use onair_obs::observe::{
    ClientInfo, InspectorAttemptRecord, InspectorRequestBase, InspectorRequestRecord, LiveRecord,
    RequestTimeline, TimelineEvent,
};

use crate::proxy_state::ProxyState;
use crate::routing::SelectedRoute;

use super::attempt::InspectorAttemptBuilder;
use super::inspector::build_live_record_from_context;
use super::upstream::backend_target;

pub struct ProxyRequest {
    pub(super) method: Method,
    pub(super) uri: Uri,
    pub(super) body: Bytes,
    pub(super) content_type: Option<String>,
    pub(super) stream: bool,
}

pub struct ProxyContext {
    pub(super) state: Arc<ProxyState>,
    pub(super) client_headers: HeaderMap,
    pub(super) debug_capture_config: DebugCaptureConfig,
    pub(super) debug_capture: Option<RequestCapture>,
    pub(super) pending_debug_capture: Option<PendingDebugCapture>,
    pub(super) inspector_base: InspectorRequestBase,
    pub(super) inspector_enabled: bool,
    pub(super) inspector_retention_requests: usize,
    pub(super) live_record: LiveRecord,
    pub(super) client_info: ClientInfo,
    pub(super) shutdown: watch::Receiver<bool>,
    pub(super) backend_target: String,
    pub(super) backend_remote_addr: Option<SocketAddr>,
    pub(super) route: SelectedRoute,
    pub(super) labels: MetricLabels,
    pub(super) model_log_fields: ModelLogFields,
    pub(super) requested_model: Option<String>,
    pub(super) client_stream_usage_requested: bool,
    pub(super) request_body_bytes: usize,
    pub(super) request_timer: RequestTimer,
    pub(super) timeline: RequestTimeline,
    pub(super) attempt: usize,
    pub(super) max_attempts: usize,
    pub(super) backend_attempts: Vec<InspectorAttemptRecord>,
    pub(super) retried_attempts: Vec<InspectorAttemptRecord>,
    pub(super) current_attempt: Option<InspectorAttemptBuilder>,
}

impl ProxyContext {
    pub fn apply_route(&mut self, route: SelectedRoute) {
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
        let base = self.inspector_base.clone();
        self.live_record.update(|record| record.base = base);
    }

    pub fn live_upsert(&self) {
        let snapshot = build_live_record_from_context(self);
        self.live_record.update(|record| *record = snapshot);
    }

    pub fn live_finalize(self, final_record: InspectorRequestRecord) {
        self.live_record.finalize(final_record);
    }

    pub fn record_retried_attempt(&mut self, attempt: InspectorAttemptRecord) {
        self.backend_attempts.push(attempt.clone());
        self.retried_attempts.push(attempt);
        self.live_upsert();
    }

    pub fn record_final_attempt(
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
            self.live_upsert();
        }
    }
}

#[derive(Debug, Clone)]
pub struct PendingDebugCapture {
    pub(super) method: Method,
    pub(super) client_path: String,
    pub(super) client_query: Option<String>,
    pub(super) upstream_path: String,
    pub(super) upstream_query: Option<String>,
    pub(super) content_type: Option<String>,
    pub(super) request_id: Option<String>,
    pub(super) labels: MetricLabels,
    pub(super) requested_model: String,
    pub(super) public_model: String,
    pub(super) backend_model: String,
    pub(super) inbound_body: Bytes,
    pub(super) upstream_body: Vec<u8>,
}

impl PendingDebugCapture {
    pub fn capture(&self, config: &DebugCaptureConfig) -> Option<RequestCapture> {
        capture_request(
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

pub fn ensure_failure_debug_capture(
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
            attempt_record.mark_debug_capture_done(timeline.mark(TimelineEvent::DebugCaptureDone));
        }
    } else if !had_capture && debug_capture.is_some() {
        timeline.mark(TimelineEvent::DebugCaptureDone);
    }
}

pub fn refresh_live_record(_record: &mut InspectorRequestRecord) {}

#[derive(Debug, Clone)]
pub struct ModelLogFields {
    pub(super) requested: String,
    pub(super) public: String,
    pub(super) backend: String,
}

impl ModelLogFields {
    pub fn from_route(requested_model: Option<&str>, route: &SelectedRoute) -> Self {
        Self {
            requested: requested_model.unwrap_or("none").to_owned(),
            public: route.public_model.as_deref().unwrap_or("none").to_owned(),
            backend: route.backend_model.as_deref().unwrap_or("none").to_owned(),
        }
    }
}
