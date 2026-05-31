use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::broadcast;

use crate::config::InspectorConfig;
use crate::error::{Error, Result};
use crate::observe::TimelineSnapshot;

const DEFAULT_RETENTION_REQUESTS: usize = 10_000;
const MAX_RETENTION_REQUESTS: usize = 100_000;
const EVENT_CHANNEL_CAPACITY: usize = 1024;
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

pub(crate) const UI_HTML: &str = include_str!("inspector.html");

#[derive(Clone)]
pub(crate) struct InspectorStore {
    inner: Arc<InspectorStoreInner>,
}

struct InspectorStoreInner {
    records: Mutex<VecDeque<InspectorRequestRecord>>,
    events: broadcast::Sender<InspectorRequestRecord>,
}

impl InspectorStore {
    pub(crate) fn new() -> Self {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            inner: Arc::new(InspectorStoreInner {
                records: Mutex::new(VecDeque::new()),
                events,
            }),
        }
    }

    pub(crate) fn record(
        &self,
        enabled: bool,
        retention_requests: usize,
        record: InspectorRequestRecord,
    ) {
        if !enabled {
            return;
        }

        let retention_requests = retention_requests.clamp(1, MAX_RETENTION_REQUESTS);
        {
            let mut records = self
                .inner
                .records
                .lock()
                .expect("inspector store lock poisoned");
            records.push_back(record.clone());
            while records.len() > retention_requests {
                records.pop_front();
            }
        }

        let _ = self.inner.events.send(record);
    }

    pub(crate) fn records_limited(&self, limit: usize) -> Vec<InspectorRequestRecord> {
        let records = self
            .inner
            .records
            .lock()
            .expect("inspector store lock poisoned");
        let skip = records.len().saturating_sub(limit.max(1));
        records.iter().skip(skip).cloned().collect()
    }

    pub(crate) fn retained_len(&self) -> usize {
        self.inner
            .records
            .lock()
            .expect("inspector store lock poisoned")
            .len()
    }

    pub(crate) fn get(&self, record_id: &str) -> Option<InspectorRequestRecord> {
        self.inner
            .records
            .lock()
            .expect("inspector store lock poisoned")
            .iter()
            .rev()
            .find(|record| record.base.record_id == record_id)
            .cloned()
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<InspectorRequestRecord> {
        self.inner.events.subscribe()
    }

    pub(crate) fn next_record_id(
        started_at_unix_ms: u64,
        client_request_id: Option<&str>,
    ) -> String {
        let sequence = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut record_id = format!("{started_at_unix_ms}-{}-{sequence}", std::process::id());
        if let Some(client_request_id) = client_request_id.and_then(safe_segment) {
            record_id.push('-');
            record_id.push_str(&client_request_id);
        }
        record_id
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct InspectorRequestRecord {
    #[serde(flatten)]
    pub(crate) base: InspectorRequestBase,
    pub(crate) outcome: InspectorOutcome,
    pub(crate) status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error_kind: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) backend_attempts: Vec<InspectorAttemptRecord>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) retried_attempts: Vec<InspectorAttemptRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) response_body_bytes: Option<usize>,
    pub(crate) input_tokens: u64,
    pub(crate) cached_input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) completed_at_unix_ms: u64,
    pub(crate) timeline: TimelineSnapshot,
}

impl InspectorRequestRecord {
    pub(crate) fn new(init: InspectorRequestRecordInit) -> Self {
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
            completed_at_unix_ms: unix_millis(),
            timeline: init.timeline,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct InspectorRequestRecordInit {
    pub(crate) base: InspectorRequestBase,
    pub(crate) outcome: InspectorOutcome,
    pub(crate) status: u16,
    pub(crate) error_kind: Option<String>,
    pub(crate) backend_attempts: Vec<InspectorAttemptRecord>,
    pub(crate) retried_attempts: Vec<InspectorAttemptRecord>,
    pub(crate) response_body_bytes: Option<usize>,
    pub(crate) tokens: InspectorTokenCounts,
    pub(crate) timeline: TimelineSnapshot,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct InspectorAttemptRecord {
    pub(crate) attempt: usize,
    pub(crate) backend: String,
    pub(crate) backend_target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) backend_remote_addr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) debug_capture_id: Option<String>,
    pub(crate) status: u16,
    pub(crate) outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error_kind: Option<String>,
    pub(crate) started_us: u64,
    pub(crate) ended_us: u64,
    pub(crate) elapsed_us: u64,
    pub(crate) elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) upstream_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) request_rewritten_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) debug_capture_done_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) backend_forward_start_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) backend_headers_received_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) backend_body_first_chunk_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) backend_body_complete_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stream_complete_us: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct InspectorRequestBase {
    pub(crate) record_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) client_request_id: Option<String>,
    pub(crate) started_at_unix_ms: u64,
    pub(crate) method: String,
    pub(crate) path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) query: Option<String>,
    pub(crate) route: String,
    pub(crate) identity: String,
    pub(crate) requested_model: String,
    pub(crate) public_model: String,
    pub(crate) backend_model: String,
    pub(crate) backend: String,
    pub(crate) backend_target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) backend_remote_addr: Option<String>,
    pub(crate) stream: bool,
    pub(crate) peer_addr: String,
    pub(crate) effective_client_addr: String,
    pub(crate) trusted_proxy_addr: String,
    pub(crate) forwarded_for: String,
    pub(crate) user_agent: String,
    pub(crate) request_body_bytes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) debug_capture_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum InspectorOutcome {
    Completed,
    Preflight { stage: &'static str },
    UpstreamTimeout,
    UpstreamRequestFailed,
    UpstreamNonSuccess,
    UpstreamBodyReadFailed,
    UpstreamStreamFailed,
    StreamIncomplete,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct InspectorTokenCounts {
    pub(crate) input: u64,
    pub(crate) cached_input: u64,
    pub(crate) output: u64,
}

pub(crate) fn validate_config(config: &InspectorConfig) -> Result<()> {
    if config.retention_requests == 0 {
        return Err(Error::Config(
            "inspector.retention_requests must be greater than zero".to_owned(),
        ));
    }
    if config.retention_requests > MAX_RETENTION_REQUESTS {
        return Err(Error::Config(format!(
            "inspector.retention_requests must be at most {MAX_RETENTION_REQUESTS}"
        )));
    }
    Ok(())
}

pub(crate) fn default_retention_requests() -> usize {
    DEFAULT_RETENTION_REQUESTS
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn safe_segment(value: &str) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_record(record_id: &str) -> InspectorRequestRecord {
        InspectorRequestRecord::new(InspectorRequestRecordInit {
            base: InspectorRequestBase {
                record_id: record_id.to_owned(),
                client_request_id: None,
                started_at_unix_ms: 1,
                method: "POST".to_owned(),
                path: "/v1/responses".to_owned(),
                query: None,
                route: "responses".to_owned(),
                identity: "dev".to_owned(),
                requested_model: "public".to_owned(),
                public_model: "public".to_owned(),
                backend_model: "backend".to_owned(),
                backend: "backend-a".to_owned(),
                backend_target: "127.0.0.1:8000".to_owned(),
                backend_remote_addr: None,
                stream: false,
                peer_addr: "127.0.0.1:50000".to_owned(),
                effective_client_addr: "127.0.0.1:50000".to_owned(),
                trusted_proxy_addr: "none".to_owned(),
                forwarded_for: "none".to_owned(),
                user_agent: "none".to_owned(),
                request_body_bytes: 12,
                debug_capture_id: None,
            },
            outcome: InspectorOutcome::Completed,
            status: 200,
            error_kind: None,
            backend_attempts: Vec::new(),
            retried_attempts: Vec::new(),
            response_body_bytes: Some(32),
            tokens: InspectorTokenCounts::default(),
            timeline: TimelineSnapshot::default(),
        })
    }

    #[test]
    fn disabled_store_does_not_record() {
        let store = InspectorStore::new();
        store.record(false, 10, test_record("one"));
        assert!(store.records_limited(usize::MAX).is_empty());
    }

    #[test]
    fn store_retains_latest_records() {
        let store = InspectorStore::new();
        store.record(true, 2, test_record("one"));
        store.record(true, 2, test_record("two"));
        store.record(true, 2, test_record("three"));

        let records = store.records_limited(usize::MAX);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].base.record_id, "two");
        assert_eq!(records[1].base.record_id, "three");
        assert!(store.get("one").is_none());
        assert!(store.get("three").is_some());
    }

    #[test]
    fn request_ids_include_safe_client_request_id() {
        let record_id = InspectorStore::next_record_id(42, Some("req/test value"));
        assert!(record_id.contains("42-"));
        assert!(record_id.ends_with("req_test_value"));
    }

    #[test]
    fn embedded_ui_renders_attempt_waterfall() {
        assert!(UI_HTML.contains("attempt waterfall"));
        assert!(UI_HTML.contains("backend_attempts"));
        assert!(UI_HTML.contains("waterfall-row"));
        assert!(UI_HTML.contains("Filter uses space-separated terms"));
        assert!(UI_HTML.contains("columnOptions"));
        assert!(UI_HTML.contains("quickFilters"));
        assert!(UI_HTML.contains("sortStorageKey"));
        assert!(UI_HTML.contains("presetStorageKey"));
        assert!(UI_HTML.contains("Authenticated onair client identity."));
        assert!(UI_HTML.contains("copy record json"));
        assert!(UI_HTML.contains("Save and restore local table views"));
        assert!(UI_HTML.contains("save view"));
        assert!(UI_HTML.contains("data-full"));
        assert!(UI_HTML.contains("pause"));
    }
}
