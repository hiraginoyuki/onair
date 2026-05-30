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

    pub(crate) fn records(&self) -> Vec<InspectorRequestRecord> {
        self.inner
            .records
            .lock()
            .expect("inspector store lock poisoned")
            .iter()
            .cloned()
            .collect()
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) response_body_bytes: Option<usize>,
    pub(crate) input_tokens: u64,
    pub(crate) cached_input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) completed_at_unix_ms: u64,
    pub(crate) timeline: TimelineSnapshot,
}

impl InspectorRequestRecord {
    pub(crate) fn new(
        base: InspectorRequestBase,
        outcome: InspectorOutcome,
        status: u16,
        error_kind: Option<String>,
        response_body_bytes: Option<usize>,
        tokens: InspectorTokenCounts,
        timeline: TimelineSnapshot,
    ) -> Self {
        Self {
            base,
            outcome,
            status,
            error_kind,
            response_body_bytes,
            input_tokens: tokens.input,
            cached_input_tokens: tokens.cached_input,
            output_tokens: tokens.output,
            completed_at_unix_ms: unix_millis(),
            timeline,
        }
    }
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
        InspectorRequestRecord::new(
            InspectorRequestBase {
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
            InspectorOutcome::Completed,
            200,
            None,
            Some(32),
            InspectorTokenCounts::default(),
            TimelineSnapshot::default(),
        )
    }

    #[test]
    fn disabled_store_does_not_record() {
        let store = InspectorStore::new();
        store.record(false, 10, test_record("one"));
        assert!(store.records().is_empty());
    }

    #[test]
    fn store_retains_latest_records() {
        let store = InspectorStore::new();
        store.record(true, 2, test_record("one"));
        store.record(true, 2, test_record("two"));
        store.record(true, 2, test_record("three"));

        let records = store.records();
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
}
