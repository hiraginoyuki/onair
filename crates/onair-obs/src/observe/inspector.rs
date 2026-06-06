use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::error;

use super::TimelineSnapshot;
use onair_core::config::InspectorConfig;
use onair_core::error::{Error, Result};

use super::inspector_persistence::{InspectorPersistenceWriter, restore_records};

const MAX_RETENTION_REQUESTS: usize = 100_000;
const EVENT_CHANNEL_CAPACITY: usize = 1024;
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

const UI_TEMPLATE: &str = include_str!("inspector.html");
const UI_CSS: &str = include_str!("inspector.css");
const UI_JS: &str = include_str!("inspector.js");

pub static UI_HTML: LazyLock<String> = LazyLock::new(|| {
    UI_TEMPLATE
        .replace("__ONAIR_INSPECTOR_CSS__", UI_CSS)
        .replace("__ONAIR_INSPECTOR_JS__", UI_JS)
});

pub fn ui_html() -> &'static str {
    UI_HTML.as_str()
}

#[derive(Clone)]
pub struct InspectorStore {
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
            let mut records = self.records.lock();
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

// Add `record` to the deque, enforcing the retention cap with FIFO eviction.
//
// If a record with the same `record_id` already exists, replace it in
// place to preserve its deque position (stable table ordering in the UI
// and reverse-scan `get()` semantics). Otherwise the record is treated
// as a new push: when the deque is already at the retention limit, the
// oldest record is popped from the front *before* the new record is
// pushed to the back. Eviction intentionally drops the record from the
// deque only; the persistence layer is notified separately by the
// caller and never sees the evicted record.
fn upsert_with_fifo_eviction(
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
    if records.len() >= retention_requests {
        records.pop_front();
    }
    records.push_back(record);
}

impl Default for InspectorStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InspectorStore {
    pub fn new() -> Self {
        Self::from_parts(Vec::new(), None)
    }

    pub fn from_config(config: &InspectorConfig) -> Result<Self> {
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

    pub fn record(&self, enabled: bool, retention_requests: usize, record: InspectorRequestRecord) {
        if !enabled {
            return;
        }

        let retention_requests = retention_requests.clamp(1, MAX_RETENTION_REQUESTS);
        {
            let mut records = self.inner.records.lock();
            upsert_with_fifo_eviction(&mut records, record.clone(), retention_requests);
        }

        if let Some(persistence) = &self.inner.persistence {
            persistence
                .writer
                .record(record.clone(), retention_requests);
        }

        let _ = self.inner.events.send(record);
    }

    pub fn upsert(&self, enabled: bool, retention_requests: usize, record: InspectorRequestRecord) {
        if !enabled {
            return;
        }

        let retention_requests = retention_requests.clamp(1, MAX_RETENTION_REQUESTS);
        {
            let mut records = self.inner.records.lock();
            upsert_with_fifo_eviction(&mut records, record.clone(), retention_requests);
        }

        let _ = self.inner.events.send(record);
    }

    pub fn upsert_final(
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
            let mut records = self.inner.records.lock();
            upsert_with_fifo_eviction(&mut records, record.clone(), retention_requests);
        }

        if let Some(persistence) = &self.inner.persistence {
            persistence
                .writer
                .record(record.clone(), retention_requests);
        }

        let _ = self.inner.events.send(record);
    }

    pub fn records_limited(&self, limit: usize) -> Vec<InspectorRequestRecord> {
        let records = self.inner.records.lock();
        let skip = records.len().saturating_sub(limit.max(1));
        records.iter().skip(skip).cloned().collect()
    }

    pub fn retained_len(&self) -> usize {
        self.inner.records.lock().len()
    }

    pub fn get(&self, record_id: &str) -> Option<InspectorRequestRecord> {
        self.inner
            .records
            .lock()
            .iter()
            .rev()
            .find(|record| record.base.record_id == record_id)
            .cloned()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<InspectorRequestRecord> {
        self.inner.events.subscribe()
    }

    pub fn next_record_id(started_at_unix_ms: u64, client_request_id: Option<&str>) -> String {
        let sequence = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut record_id = format!("{started_at_unix_ms}-{}-{sequence}", std::process::id());
        if let Some(client_request_id) = client_request_id.and_then(safe_segment) {
            record_id.push('-');
            record_id.push_str(&client_request_id);
        }
        record_id
    }
}

pub struct LiveRecord {
    store: InspectorStore,
    enabled: bool,
    retention_requests: usize,
    record: Mutex<InspectorRequestRecord>,
}

impl LiveRecord {
    pub fn new(
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

    pub fn publish_initial(&self) {
        if !self.enabled {
            return;
        }
        let record = self.record.lock().clone();
        self.store
            .upsert(self.enabled, self.retention_requests, record);
    }

    pub fn update<F>(&self, mutate: F)
    where
        F: FnOnce(&mut InspectorRequestRecord),
    {
        if !self.enabled {
            return;
        }
        let snapshot = {
            let mut record = self.record.lock();
            mutate(&mut record);
            record.clone()
        };
        self.store
            .upsert(self.enabled, self.retention_requests, snapshot);
    }

    #[allow(dead_code)]
    pub fn snapshot(&self) -> InspectorRequestRecord {
        self.record.lock().clone()
    }

    pub fn finalize(self, mut final_record: InspectorRequestRecord) {
        if !self.enabled {
            return;
        }
        let started = self.record.lock().base.started_at_unix_ms;
        final_record.base.started_at_unix_ms = started;
        final_record.timeline.started_unix_ms = started;
        self.store
            .upsert_final(self.enabled, self.retention_requests, final_record);
    }
}

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
    use onair_core::config::InspectorPersistenceConfig;

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

    fn in_flight_count(store: &InspectorStore) -> usize {
        store
            .records_limited(usize::MAX)
            .iter()
            .filter(|record| matches!(record.outcome, InspectorOutcome::InFlight))
            .count()
    }

    fn in_flight_record(record_id: &str) -> InspectorRequestRecord {
        let mut record = test_record(record_id);
        record.outcome = InspectorOutcome::InFlight;
        record.status = 0;
        record.error_kind = None;
        record
    }

    #[test]
    fn retention_boundary_evicts_oldest_record() {
        // Push retention+1 records at a small retention cap and assert
        // the FIFO eviction drops exactly the oldest record.
        let store = InspectorStore::new();
        let retention = 3;
        for index in 0..=retention {
            store.record(true, retention, test_record(&format!("rec-{index}")));
        }

        let records = store.records_limited(usize::MAX);
        assert_eq!(records.len(), retention);
        assert_eq!(records[0].base.record_id, "rec-1");
        assert_eq!(records[1].base.record_id, "rec-2");
        assert_eq!(records[2].base.record_id, "rec-3");
        assert!(store.get("rec-0").is_none());
        assert!(store.get("rec-1").is_some());
    }

    #[test]
    fn retention_boundary_evicts_under_bulk_load() {
        // Stress test: pushing more than the retention cap in one go
        // must keep the deque bounded at the retention limit and
        // retain only the most recent records.
        let store = InspectorStore::new();
        let retention = 4;
        let total = retention + 2;
        for index in 0..total {
            store.record(true, retention, test_record(&format!("bulk-{index}")));
        }

        let records = store.records_limited(usize::MAX);
        assert_eq!(records.len(), retention);
        assert_eq!(records[0].base.record_id, "bulk-2");
        assert_eq!(records[retention - 1].base.record_id, "bulk-5");
        for index in 0..(total - retention) {
            assert!(store.get(&format!("bulk-{index}")).is_none());
        }
    }

    #[test]
    fn eviction_decrements_in_flight_count() {
        // Three in-flight records fill the deque. A fourth push (a
        // final record) evicts the oldest, which is in-flight, so the
        // in-flight count goes from 3 to 2.
        let store = InspectorStore::new();
        for id in ["a", "b", "c"] {
            store.upsert(true, 3, in_flight_record(id));
        }
        assert_eq!(in_flight_count(&store), 3);

        store.record(true, 3, test_record("d"));

        assert_eq!(in_flight_count(&store), 2);
        let records = store.records_limited(usize::MAX);
        assert_eq!(records[0].base.record_id, "b");
        assert!(matches!(records[0].outcome, InspectorOutcome::InFlight));
        assert_eq!(records[2].base.record_id, "d");
        assert!(matches!(records[2].outcome, InspectorOutcome::Completed));
    }

    #[test]
    fn eviction_of_final_record_does_not_change_in_flight_count() {
        // Three final records fill the deque. A fourth push (a final
        // record) evicts the oldest, which is also final, so the
        // in-flight count stays at 0.
        let store = InspectorStore::new();
        for id in ["a", "b", "c"] {
            store.record(true, 3, test_record(id));
        }
        assert_eq!(in_flight_count(&store), 0);

        store.record(true, 3, test_record("d"));

        assert_eq!(in_flight_count(&store), 0);
        let records = store.records_limited(usize::MAX);
        assert_eq!(records[0].base.record_id, "b");
    }

    #[test]
    fn upsert_replace_at_limit_does_not_evict() {
        // Three records fill the deque. Replacing an existing record
        // by id via `upsert` keeps the deque at the retention limit
        // and does not trigger eviction.
        let store = InspectorStore::new();
        for id in ["a", "b", "c"] {
            store.record(true, 3, test_record(id));
        }

        let mut update = test_record("b");
        update.timeline.auth_done_us = Some(1234);
        store.upsert(true, 3, update);

        let records = store.records_limited(usize::MAX);
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].base.record_id, "a");
        assert_eq!(records[1].base.record_id, "b");
        assert_eq!(records[1].timeline.auth_done_us, Some(1234));
        assert_eq!(records[2].base.record_id, "c");
    }

    #[test]
    fn upsert_final_replace_at_limit_does_not_evict() {
        // Three in-flight records fill the deque. Calling
        // `upsert_final` for one of them replaces it in place with
        // the terminal record; the other two stay in flight and the
        // deque is still at the retention limit.
        let store = InspectorStore::new();
        store.upsert(true, 3, in_flight_record("a"));
        store.upsert(true, 3, in_flight_record("b"));
        store.upsert(true, 3, in_flight_record("c"));

        store.upsert_final(true, 3, test_record("a"));

        let records = store.records_limited(usize::MAX);
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].base.record_id, "a");
        assert!(matches!(records[0].outcome, InspectorOutcome::Completed));
        assert_eq!(records[1].base.record_id, "b");
        assert!(matches!(records[1].outcome, InspectorOutcome::InFlight));
        assert_eq!(records[2].base.record_id, "c");
        assert!(matches!(records[2].outcome, InspectorOutcome::InFlight));
        assert_eq!(in_flight_count(&store), 2);
    }

    #[test]
    fn persistence_drain_only_walks_current_deque_state() {
        // Push more records than the retention cap. The first
        // records are evicted from the deque. The persistence writer
        // is enqueued per-push and prunes its own SQLite file to the
        // retention cap on each insert, so the post-drop drain only
        // needs to mark the records still in the deque as
        // `Interrupted`. Evicted records must not appear in the
        // restored file.
        let path = temp_database_path("evict-drain");
        let config = InspectorConfig {
            enabled: true,
            retention_requests: 3,
            allow_remote: false,
            persistence: InspectorPersistenceConfig {
                enabled: true,
                path: Some(path.clone()),
            },
        };
        let store = InspectorStore::from_config(&config).unwrap();
        for index in 0..5 {
            store.record(true, 3, test_record(&format!("drain-{index}")));
        }
        wait_for_stored_count(&path, 3);
        drop(store);

        let restored = InspectorStore::from_config(&config).unwrap();
        let records = restored.records_limited(usize::MAX);
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].base.record_id, "drain-2");
        assert_eq!(records[1].base.record_id, "drain-3");
        assert_eq!(records[2].base.record_id, "drain-4");
        assert!(restored.get("drain-0").is_none());
        assert!(restored.get("drain-1").is_none());
    }

    #[test]
    fn evicted_in_flight_record_is_not_marked_interrupted_on_drop() {
        // With retention 3, push 4 in-flight records. The first one
        // is evicted by the fourth push and is no longer in the
        // deque when `Drop` runs, so it is never marked
        // `Interrupted`. The 3 remaining in-flight records are
        // marked `Interrupted` and persisted.
        let path = temp_database_path("evict-interrupt");
        let config = InspectorConfig {
            enabled: true,
            retention_requests: 3,
            allow_remote: false,
            persistence: InspectorPersistenceConfig {
                enabled: true,
                path: Some(path.clone()),
            },
        };
        let store = InspectorStore::from_config(&config).unwrap();
        for index in 0..4 {
            store.upsert(true, 3, in_flight_record(&format!("evict-{index}")));
        }
        drop(store);
        wait_for_stored_count(&path, 3);

        let restored = InspectorStore::from_config(&config).unwrap();
        let records = restored.records_limited(usize::MAX);
        assert_eq!(records.len(), 3);
        for record in &records {
            assert!(matches!(record.outcome, InspectorOutcome::Interrupted));
            assert_eq!(record.status, 503);
        }
        assert!(restored.get("evict-0").is_none());
        assert!(restored.get("evict-1").is_some());
        assert!(restored.get("evict-2").is_some());
        assert!(restored.get("evict-3").is_some());
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

    #[test]
    fn panic_in_lock_holder_does_not_poison_next_accessor() {
        // parking_lot::Mutex has no poison semantics: a panic in a
        // holder leaves the lock acquirable. This test holds the
        // deque lock, panics, and confirms the next accessor can
        // still mutate the store.
        let store = InspectorStore::new();
        let store_arc = std::sync::Arc::new(store);
        let cloned = std::sync::Arc::clone(&store_arc);

        let result = std::thread::spawn(move || {
            let _guard = cloned.inner.records.lock();
            panic!("simulated panic while holding the inspector store lock");
        })
        .join();

        assert!(result.is_err(), "expected the spawned thread to panic");

        // The store must still be usable: insert and look up a record.
        store_arc.record(true, 4, test_record("after-panic"));
        let stored = store_arc.get("after-panic").expect("record survives panic");
        assert_eq!(stored.base.record_id, "after-panic");
    }
}
