use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::broadcast;
use tracing::error;

use onair_core::config::InspectorConfig;
use onair_core::error::Result;

use super::records::{InspectorOutcome, InspectorRequestRecord};
use crate::observe::inspector_persistence::{InspectorPersistenceWriter, restore_records};
use crate::util::sanitize_for_storage;

pub(super) const MAX_RETENTION_REQUESTS: usize = 100_000;
const EVENT_CHANNEL_CAPACITY: usize = 1024;
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct InspectorStore {
    pub(super) inner: Arc<InspectorStoreInner>,
}

pub(super) struct InspectorStoreInner {
    pub(super) records: Mutex<VecDeque<InspectorRequestRecord>>,
    pub(super) events: broadcast::Sender<InspectorRequestRecord>,
    pub(super) persistence: Option<PersistenceComponents>,
}

pub(super) struct PersistenceComponents {
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
            onair_core::error::ConfigError::new(
                "inspector.persistence.path is required when persistence is enabled",
            )
        })?;
        let retention_requests = config.retention_requests.clamp(1, MAX_RETENTION_REQUESTS);
        let drain_timeout = Duration::from_millis(config.persistence.drain_timeout_ms);
        let (records, writer, handle) = restore_records(path, retention_requests, drain_timeout)?;
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
        if let Some(client_request_id) =
            client_request_id.and_then(|value| sanitize_for_storage(value, 80))
        {
            record_id.push('-');
            record_id.push_str(&client_request_id);
        }
        record_id
    }
}
