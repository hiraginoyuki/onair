use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::error;

use crate::config::InspectorConfig;
use crate::error::{Error, Result};
use crate::observe::TimelineSnapshot;

use super::inspector_persistence::{InspectorPersistenceWriter, restore_records};

const DEFAULT_RETENTION_REQUESTS: usize = 10_000;
const MAX_RETENTION_REQUESTS: usize = 100_000;
const EVENT_CHANNEL_CAPACITY: usize = 1024;
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

const UI_TEMPLATE: &str = include_str!("inspector.html");
const UI_CSS: &str = include_str!("inspector.css");
const UI_JS: &str = include_str!("inspector.js");

pub(crate) static UI_HTML: LazyLock<String> = LazyLock::new(|| {
    UI_TEMPLATE
        .replace("__ONAIR_INSPECTOR_CSS__", UI_CSS)
        .replace("__ONAIR_INSPECTOR_JS__", UI_JS)
});

pub(crate) fn ui_html() -> &'static str {
    UI_HTML.as_str()
}

#[derive(Clone)]
pub(crate) struct InspectorStore {
    inner: Arc<InspectorStoreInner>,
}

struct InspectorStoreInner {
    records: Mutex<VecDeque<InspectorRequestRecord>>,
    events: broadcast::Sender<InspectorRequestRecord>,
    persistence: Option<PersistenceComponents>,
}

struct PersistenceComponents {
    writer: InspectorPersistenceWriter,
    handle: Option<JoinHandle<()>>,
}

impl Drop for InspectorStoreInner {
    fn drop(&mut self) {
        let Some(components) = self.persistence.take() else {
            return;
        };
        let interrupted: Vec<InspectorRequestRecord> = {
            let mut records = self.records.lock().expect("inspector store lock poisoned");
            for record in records.iter_mut() {
                if matches!(record.outcome, InspectorOutcome::InFlight) {
                    mark_record_interrupted(record);
                }
            }
            records
                .iter()
                .filter(|record| matches!(record.outcome, InspectorOutcome::Interrupted))
                .cloned()
                .collect()
        };
        for record in &interrupted {
            components
                .writer
                .record(record.clone(), MAX_RETENTION_REQUESTS);
        }
        components.writer.request_shutdown();
        if let Some(handle) = components.handle
            && let Err(panic) = handle.join()
        {
            let message = panic_message(&panic);
            error!(
                panic = %message,
                "inspector persistence writer thread panicked; persistence is now disabled for this process"
            );
        }
    }
}

fn mark_record_interrupted(record: &mut InspectorRequestRecord) {
    // `completed_at_unix_ms` is computed from the recorded timeline
    // rather than the wall clock at interruption time: the record
    // already carries `started_at_unix_ms`, and adding the elapsed
    // timeline keeps the wall-clock duration internally consistent
    // with how completed records are stamped. If the request was
    // interrupted before any timeline mark landed, `total_us` is 0
    // and the value collapses to `started_at_unix_ms`; in that case
    // the value reads as "finished at start" but is still
    // monotonically non-decreasing against any sibling record.
    let completed_at_unix_ms = record
        .base
        .started_at_unix_ms
        .saturating_add(record.timeline.total_us / 1000);
    record.outcome = InspectorOutcome::Interrupted;
    record.status = 503;
    record.error_kind = Some("interrupted".to_owned());
    record.completed_at_unix_ms = completed_at_unix_ms;
}

fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = panic.downcast_ref::<&'static str>() {
        (*message).to_owned()
    } else if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_owned()
    }
}

fn upsert_in_place(
    records: &mut VecDeque<InspectorRequestRecord>,
    record: InspectorRequestRecord,
    retention_requests: usize,
) {
    if let Some(slot) = records
        .iter_mut()
        .find(|slot| slot.base.record_id == record.base.record_id)
    {
        *slot = record;
        return;
    }
    records.push_back(record);
    while records.len() > retention_requests {
        records.pop_front();
    }
}

impl InspectorStore {
    pub(crate) fn new() -> Self {
        Self::from_parts(Vec::new(), None)
    }

    pub(crate) fn from_config(config: &InspectorConfig) -> Result<Self> {
        if !config.persistence.enabled {
            return Ok(Self::new());
        }

        let path = config.persistence.path.as_ref().ok_or_else(|| {
            Error::Config(
                "inspector.persistence.path is required when persistence is enabled".to_owned(),
            )
        })?;
        let retention_requests = config.retention_requests.clamp(1, MAX_RETENTION_REQUESTS);
        let (records, writer, handle) = restore_records(path, retention_requests)?;
        Ok(Self::from_parts(
            records,
            Some(PersistenceComponents {
                writer,
                handle: Some(handle),
            }),
        ))
    }

    fn from_parts(
        records: Vec<InspectorRequestRecord>,
        persistence: Option<PersistenceComponents>,
    ) -> Self {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            inner: Arc::new(InspectorStoreInner {
                records: Mutex::new(records.into()),
                events,
                persistence,
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
            upsert_in_place(&mut records, record.clone(), retention_requests);
        }

        if let Some(persistence) = &self.inner.persistence {
            persistence
                .writer
                .record(record.clone(), retention_requests);
        }

        let _ = self.inner.events.send(record);
    }

    pub(crate) fn upsert(
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
            upsert_in_place(&mut records, record.clone(), retention_requests);
        }

        let _ = self.inner.events.send(record);
    }

    pub(crate) fn upsert_final(
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
            upsert_in_place(&mut records, record.clone(), retention_requests);
        }

        if let Some(persistence) = &self.inner.persistence {
            persistence
                .writer
                .record(record.clone(), retention_requests);
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

pub(crate) struct LiveRecord {
    store: InspectorStore,
    enabled: bool,
    retention_requests: usize,
    record: Mutex<InspectorRequestRecord>,
}

impl LiveRecord {
    pub(crate) fn new(
        store: InspectorStore,
        enabled: bool,
        retention_requests: usize,
        initial: InspectorRequestRecord,
    ) -> Self {
        Self {
            store,
            enabled,
            retention_requests,
            record: Mutex::new(initial),
        }
    }

    pub(crate) fn publish_initial(&self) {
        if !self.enabled {
            return;
        }
        let record = self
            .record
            .lock()
            .expect("live inspector record lock poisoned")
            .clone();
        self.store
            .upsert(self.enabled, self.retention_requests, record);
    }

    pub(crate) fn update<F>(&self, mutate: F)
    where
        F: FnOnce(&mut InspectorRequestRecord),
    {
        if !self.enabled {
            return;
        }
        let snapshot = {
            let mut record = self
                .record
                .lock()
                .expect("live inspector record lock poisoned");
            mutate(&mut record);
            record.clone()
        };
        self.store
            .upsert(self.enabled, self.retention_requests, snapshot);
    }

    #[allow(dead_code)]
    pub(crate) fn snapshot(&self) -> InspectorRequestRecord {
        self.record
            .lock()
            .expect("live inspector record lock poisoned")
            .clone()
    }

    pub(crate) fn finalize(self, mut final_record: InspectorRequestRecord) {
        if !self.enabled {
            return;
        }
        let started = self
            .record
            .lock()
            .expect("live inspector record lock poisoned")
            .base
            .started_at_unix_ms;
        final_record.base.started_at_unix_ms = started;
        final_record.timeline.started_unix_ms = started;
        self.store
            .upsert_final(self.enabled, self.retention_requests, final_record);
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct InspectorRequestRecord {
    #[serde(flatten)]
    pub(crate) base: InspectorRequestBase,
    pub(crate) outcome: InspectorOutcome,
    pub(crate) status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) backend_attempts: Vec<InspectorAttemptRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
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

#[derive(Debug, Clone, Deserialize, Serialize)]
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

#[derive(Debug, Clone, Deserialize, Serialize)]
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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum InspectorOutcome {
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
    if config.persistence.enabled {
        let Some(path) = config.persistence.path.as_ref() else {
            return Err(Error::Config(
                "inspector.persistence.path is required when persistence is enabled".to_owned(),
            ));
        };
        if path.as_os_str().is_empty() {
            return Err(Error::Config(
                "inspector.persistence.path must not be empty".to_owned(),
            ));
        }
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
    use super::super::inspector_persistence::stored_count;
    use super::*;
    use crate::config::InspectorPersistenceConfig;

    fn temp_database_path(label: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "onair-inspector-store-{label}-{}-{nonce}.sqlite",
            std::process::id()
        ))
    }

    fn wait_for_stored_count(path: &std::path::Path, minimum: usize) {
        for _ in 0..50 {
            if stored_count(path).unwrap_or_default() >= minimum {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("inspector persistence did not reach {minimum} records");
    }

    fn test_record(record_id: &str) -> InspectorRequestRecord {
        let started_at_unix_ms = match record_id {
            "one" => 1,
            "two" => 2,
            "three" => 3,
            _ => 1,
        };
        InspectorRequestRecord::new(InspectorRequestRecordInit {
            base: InspectorRequestBase {
                record_id: record_id.to_owned(),
                client_request_id: None,
                started_at_unix_ms,
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
    fn live_record_new_does_not_publish_initial_record() {
        let store = InspectorStore::new();
        let mut initial = test_record("deferred-1");
        initial.outcome = InspectorOutcome::InFlight;
        initial.status = 0;
        let _live = LiveRecord::new(store.clone(), true, 8, initial);
        assert!(store.get("deferred-1").is_none());
    }

    #[test]
    fn live_record_publish_initial_pushes_after_creation() {
        let store = InspectorStore::new();
        let mut initial = test_record("publish-1");
        initial.outcome = InspectorOutcome::InFlight;
        initial.status = 0;
        let live = LiveRecord::new(store.clone(), true, 8, initial);
        live.publish_initial();
        let stored = store.get("publish-1").expect("published record");
        assert!(matches!(stored.outcome, InspectorOutcome::InFlight));
        assert_eq!(stored.status, 0);
    }

    #[test]
    fn live_record_partial_field_updates_replace_in_place() {
        let store = InspectorStore::new();
        let mut initial = test_record("live-1");
        initial.outcome = InspectorOutcome::InFlight;
        initial.status = 0;
        let live = LiveRecord::new(store.clone(), true, 8, initial);
        live.update(|record| {
            record.timeline.auth_done_us = Some(1234);
            record.timeline.request_inspected_us = Some(5678);
        });
        let stored = store.get("live-1").expect("updated record");
        assert!(matches!(stored.outcome, InspectorOutcome::InFlight));
        assert_eq!(stored.timeline.auth_done_us, Some(1234));
        assert_eq!(stored.timeline.request_inspected_us, Some(5678));
    }

    #[test]
    fn live_record_upsert_replaces_in_place_preserving_position() {
        let store = InspectorStore::new();
        for id in ["first", "second", "third"] {
            store.record(true, 8, test_record(id));
        }
        let live = LiveRecord::new(store.clone(), true, 8, test_record("second").clone());
        live.update(|record| {
            record.timeline.backend_forward_start_us = Some(9999);
        });
        let records = store.records_limited(usize::MAX);
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].base.record_id, "first");
        assert_eq!(records[1].base.record_id, "second");
        assert_eq!(records[1].timeline.backend_forward_start_us, Some(9999));
        assert_eq!(records[2].base.record_id, "third");
    }

    #[test]
    fn persistent_store_restores_latest_records() {
        let path = temp_database_path("restore");
        let config = InspectorConfig {
            enabled: true,
            retention_requests: 2,
            allow_remote: false,
            persistence: InspectorPersistenceConfig {
                enabled: true,
                path: Some(path.clone()),
            },
        };
        let store = InspectorStore::from_config(&config).unwrap();
        store.record(true, 2, test_record("one"));
        store.record(true, 2, test_record("two"));
        store.record(true, 2, test_record("three"));
        wait_for_stored_count(&path, 2);
        drop(store);

        let restored = InspectorStore::from_config(&config).unwrap();
        let records = restored.records_limited(usize::MAX);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].base.record_id, "two");
        assert_eq!(records[1].base.record_id, "three");
        assert!(restored.get("one").is_none());
        assert!(restored.get("three").is_some());
    }

    #[test]
    fn persistent_store_drop_drains_pending_records() {
        let path = temp_database_path("drain");
        let config = InspectorConfig {
            enabled: true,
            retention_requests: 32,
            allow_remote: false,
            persistence: InspectorPersistenceConfig {
                enabled: true,
                path: Some(path.clone()),
            },
        };
        let store = InspectorStore::from_config(&config).unwrap();
        for index in 0..16 {
            store.record(true, 32, test_record(&format!("drain-{index}")));
        }
        drop(store);
        assert_eq!(stored_count(&path).unwrap_or_default(), 16);
    }

    #[test]
    fn request_ids_include_safe_client_request_id() {
        let record_id = InspectorStore::next_record_id(42, Some("req/test value"));
        assert!(record_id.contains("42-"));
        assert!(record_id.ends_with("req_test_value"));
    }

    #[test]
    fn in_flight_outcome_serializes_as_in_flight_kind() {
        let value = serde_json::to_value(InspectorOutcome::InFlight).unwrap();
        assert_eq!(value, serde_json::json!({"kind": "in_flight"}));
    }

    #[test]
    fn interrupted_outcome_serializes_as_interrupted_kind() {
        let value = serde_json::to_value(InspectorOutcome::Interrupted).unwrap();
        assert_eq!(value, serde_json::json!({"kind": "interrupted"}));
    }

    #[test]
    fn store_drop_marks_inflight_records_as_interrupted() {
        let path = temp_database_path("interrupted");
        let config = InspectorConfig {
            enabled: true,
            retention_requests: 8,
            allow_remote: false,
            persistence: InspectorPersistenceConfig {
                enabled: true,
                path: Some(path.clone()),
            },
        };
        let store = InspectorStore::from_config(&config).unwrap();
        store.record(true, 8, test_record("completed-1"));
        let mut in_flight = test_record("inflight-1");
        in_flight.outcome = InspectorOutcome::InFlight;
        in_flight.status = 0;
        in_flight.error_kind = None;
        store.upsert(true, 8, in_flight);
        drop(store);
        wait_for_stored_count(&path, 2);

        let restored = InspectorStore::from_config(&config).unwrap();
        let record = restored.get("inflight-1").expect("interrupted record");
        assert!(matches!(record.outcome, InspectorOutcome::Interrupted));
        assert_eq!(record.status, 503);
        assert_eq!(record.error_kind.as_deref(), Some("interrupted"));
        let completed = restored.get("completed-1").expect("completed record");
        assert!(matches!(completed.outcome, InspectorOutcome::Completed));
    }

    #[test]
    fn embedded_ui_renders_attempt_waterfall() {
        let html = ui_html();
        assert!(!html.contains("__ONAIR_INSPECTOR_"));
        assert!(html.contains("attempt waterfall"));
        assert!(html.contains("backend_attempts"));
        assert!(html.contains("waterfall-row"));
        assert!(html.contains("Filter uses space-separated terms"));
        assert!(html.contains("columnOptions"));
        assert!(html.contains("quickFilters"));
        assert!(html.contains("sortStorageKey"));
        assert!(html.contains("presetStorageKey"));
        assert!(html.contains("Authenticated onair client identity."));
        assert!(html.contains("copy record json"));
        assert!(html.contains("Save and restore local table views"));
        assert!(html.contains("save view"));
        assert!(html.contains("data-full"));
        assert!(html.contains("expand all"));
        assert!(html.contains("waterfall-row-body"));
        assert!(html.contains("pause"));
    }
}
