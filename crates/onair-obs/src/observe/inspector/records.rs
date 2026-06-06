use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::observe::TimelineSnapshot;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InspectorRequestRecord {
    #[serde(flatten)]
    pub base: InspectorRequestBase,
    pub outcome: InspectorOutcome,
    pub status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backend_attempts: Vec<InspectorAttemptRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retried_attempts: Vec<InspectorAttemptRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_body_bytes: Option<usize>,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub completed_at_unix_ms: u64,
    pub timeline: TimelineSnapshot,
}

impl InspectorRequestRecord {
    pub fn new(init: InspectorRequestRecordInit) -> Self {
        Self {
            base: init.base,
            outcome: init.outcome,
            status: init.status,
            error_kind: init.error_kind,
            backend_attempts: init.backend_attempts,
            retried_attempts: init.retried_attempts,
            response_body_bytes: init.response_body_bytes,
            input_tokens: init.tokens.input,
            cached_input_tokens: init.tokens.cached_input,
            output_tokens: init.tokens.output,
            // 0 is the sentinel for "clock unavailable" (SystemTime
            // before the UNIX epoch or a u128→u64 conversion overflow
            // would land here). Monotonic records (started_at_unix_ms +
            // total_us / 1000) never collapse to 0, so a literal 0 in
            // a completed record is unambiguously the clock-failure
            // case and never a real time.
            completed_at_unix_ms: unix_millis(),
            timeline: init.timeline,
        }
    }
}

#[derive(Debug, Clone)]
pub struct InspectorRequestRecordInit {
    pub base: InspectorRequestBase,
    pub outcome: InspectorOutcome,
    pub status: u16,
    pub error_kind: Option<String>,
    pub backend_attempts: Vec<InspectorAttemptRecord>,
    pub retried_attempts: Vec<InspectorAttemptRecord>,
    pub response_body_bytes: Option<usize>,
    pub tokens: InspectorTokenCounts,
    pub timeline: TimelineSnapshot,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InspectorAttemptRecord {
    pub attempt: usize,
    pub backend: String,
    pub backend_target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_remote_addr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debug_capture_id: Option<String>,
    pub status: u16,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    pub started_us: u64,
    pub ended_us: u64,
    pub elapsed_us: u64,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_rewritten_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debug_capture_done_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_forward_start_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_headers_received_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_body_first_chunk_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_body_complete_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_complete_us: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InspectorRequestBase {
    pub record_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_request_id: Option<String>,
    pub started_at_unix_ms: u64,
    pub method: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    pub route: String,
    pub identity: String,
    pub requested_model: String,
    pub public_model: String,
    pub backend_model: String,
    pub backend: String,
    pub backend_target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_remote_addr: Option<String>,
    pub stream: bool,
    pub peer_addr: String,
    pub effective_client_addr: String,
    pub trusted_proxy_addr: String,
    pub forwarded_for: String,
    pub user_agent: String,
    pub request_body_bytes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debug_capture_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InspectorOutcome {
    InFlight,
    Completed,
    Preflight { stage: String },
    UpstreamTimeout,
    UpstreamRequestFailed,
    UpstreamNonSuccess,
    UpstreamBodyReadFailed,
    UpstreamStreamFailed,
    StreamIncomplete,
    Interrupted,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct InspectorTokenCounts {
    pub input: u64,
    pub cached_input: u64,
    pub output: u64,
}

// 0 is the sentinel for "the system clock is unavailable or the
// u128→u64 conversion overflowed". Real timestamps from
// SystemTime::now() since the UNIX epoch are strictly positive in
// any year past 1970, and monotonic stamp math in
// mark_record_interrupted never collapses to 0, so a literal 0 in
// an inspector record is unambiguously a clock-failure indicator.
pub(super) fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(0)
}

pub(super) fn safe_segment(value: &str) -> Option<String> {
    let segment = value
        .chars()
        .filter_map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                Some(character)
            } else if character.is_ascii() && !character.is_ascii_control() {
                Some('_')
            } else {
                None
            }
        })
        .take(80)
        .collect::<String>();
    (!segment.is_empty()).then_some(segment)
}
