use std::sync::Arc;

use axum::http::StatusCode;

use crate::proxy_state::ProxyState;
use onair_core::openai::UsageTotals;
use onair_obs::metrics::MetricLabels;
use onair_obs::observe::{
    ClientInfo, InspectorAttemptRecord, InspectorOutcome, InspectorRequestBase,
    InspectorRequestRecord, InspectorRequestRecordInit, InspectorStore, InspectorTokenCounts,
    RequestTimeline,
};

#[allow(unused_imports)]
use super::attempt::InspectorAttemptBuilder;
use super::{ModelLogFields, ProxyContext};

pub(super) struct RequestObservationBase {
    pub(super) inspector_enabled: bool,
    pub(super) inspector_retention_requests: usize,
    pub(super) record_id: String,
    pub(super) client_request_id: Option<String>,
    pub(super) started_at_unix_ms: u64,
    pub(super) method: String,
    pub(super) path: String,
    pub(super) query: Option<String>,
    pub(super) client_info: ClientInfo,
    pub(super) request_body_bytes: usize,
}

pub(super) struct PreflightInspectorRecord<'a> {
    pub(super) state: &'a Arc<ProxyState>,
    pub(super) observation: &'a RequestObservationBase,
    pub(super) timeline: &'a RequestTimeline,
    pub(super) route: &'a str,
    pub(super) identity: &'a str,
    pub(super) model: Option<&'a str>,
    pub(super) stream: bool,
    pub(super) status: StatusCode,
    pub(super) stage: &'static str,
}

pub(super) fn routed_inspector_base(
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
        exposed_backend_error: false,
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
        exposed_backend_error: false,
    }
}

pub(super) fn record_preflight_inspector(record: PreflightInspectorRecord<'_>) {
    if !record.observation.inspector_enabled {
        return;
    }

    record.state.inspector.upsert_final(
        record.observation.inspector_enabled,
        record.observation.inspector_retention_requests,
        InspectorRequestRecord::new(InspectorRequestRecordInit {
            base: preflight_inspector_base(&record),
            outcome: InspectorOutcome::Preflight {
                stage: record.stage.to_owned(),
            },
            status: record.status.as_u16(),
            error_kind: None,
            backend_attempts: Vec::new(),
            retried_attempts: Vec::new(),
            response_body_bytes: None,
            tokens: InspectorTokenCounts::default(),
            timeline: record.timeline.snapshot(),
        }),
    );
}

pub(super) fn record_context_inspector(
    context: ProxyContext<'_>,
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
    let final_record = InspectorRequestRecord::new(InspectorRequestRecordInit {
        base,
        outcome,
        status: status.as_u16(),
        error_kind: error_kind.map(str::to_owned),
        backend_attempts: context.backend_attempts.clone(),
        retried_attempts: context.retried_attempts.clone(),
        response_body_bytes,
        tokens,
        timeline: context.timeline.snapshot(),
    });
    context.live_finalize(final_record);
}

pub(super) struct InspectorRecord<'a> {
    pub(super) base: InspectorRequestBase,
    pub(super) timeline: &'a RequestTimeline,
    pub(super) outcome: InspectorOutcome,
    pub(super) status: StatusCode,
    pub(super) error_kind: Option<&'static str>,
    pub(super) backend_attempts: Vec<InspectorAttemptRecord>,
    pub(super) retried_attempts: Vec<InspectorAttemptRecord>,
    pub(super) response_body_bytes: Option<usize>,
    pub(super) tokens: InspectorTokenCounts,
}

pub(super) fn record_inspector_request(
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
        InspectorRequestRecord::new(InspectorRequestRecordInit {
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

pub(super) fn inspector_tokens(usage: UsageTotals) -> InspectorTokenCounts {
    InspectorTokenCounts {
        input: usage.input,
        cached_input: usage.cached_input,
        output: usage.output,
    }
}

pub(super) fn initial_live_record(
    observation: &RequestObservationBase,
    timeline: &RequestTimeline,
    route: &str,
) -> InspectorRequestRecord {
    let base = InspectorRequestBase {
        record_id: observation.record_id.clone(),
        client_request_id: observation.client_request_id.clone(),
        started_at_unix_ms: observation.started_at_unix_ms,
        method: observation.method.clone(),
        path: observation.path.clone(),
        query: observation.query.clone(),
        route: route.to_owned(),
        identity: "unknown".to_owned(),
        requested_model: "unknown".to_owned(),
        public_model: "unknown".to_owned(),
        backend_model: "unknown".to_owned(),
        backend: "unknown".to_owned(),
        backend_target: "unknown".to_owned(),
        backend_remote_addr: None,
        stream: false,
        peer_addr: observation.client_info.peer_addr().to_owned(),
        effective_client_addr: observation.client_info.effective_client_addr().to_owned(),
        trusted_proxy_addr: observation.client_info.trusted_proxy_addr().to_owned(),
        forwarded_for: observation.client_info.forwarded_for().to_owned(),
        user_agent: observation.client_info.user_agent().to_owned(),
        request_body_bytes: observation.request_body_bytes,
        debug_capture_id: None,
        exposed_backend_error: false,
    };
    InspectorRequestRecord::new(InspectorRequestRecordInit {
        base,
        outcome: InspectorOutcome::InFlight,
        status: 0,
        error_kind: None,
        backend_attempts: Vec::new(),
        retried_attempts: Vec::new(),
        response_body_bytes: None,
        tokens: InspectorTokenCounts::default(),
        timeline: timeline.snapshot(),
    })
}

#[allow(dead_code)]
pub(super) fn build_live_record_from_context(context: &ProxyContext) -> InspectorRequestRecord {
    let mut base = context.inspector_base.clone();
    base.backend_remote_addr = context
        .backend_remote_addr
        .map(|address| address.to_string());
    base.debug_capture_id = context
        .debug_capture
        .as_ref()
        .map(|capture| capture.id().to_owned());
    let now_us = context.timeline.elapsed_us();
    let mut backend_attempts = context.backend_attempts.clone();
    if let Some(current) = &context.current_attempt {
        backend_attempts.push(current.to_attempt_record(now_us));
    }
    InspectorRequestRecord::new(InspectorRequestRecordInit {
        base,
        outcome: InspectorOutcome::InFlight,
        status: 0,
        error_kind: None,
        backend_attempts,
        retried_attempts: context.retried_attempts.clone(),
        response_body_bytes: None,
        tokens: InspectorTokenCounts::default(),
        timeline: context.timeline.snapshot(),
    })
}
