use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::broadcast;
use tracing::error;

use onair_core::config::InspectorConfig;
use onair_core::error::Result;
use onair_core::sanitize::{DISPLAY_SEGMENT_MAX_CHARS, sanitize_for_storage};

use super::records::{InspectorOutcome, InspectorRequestRecord};
use super::{
    InspectorRecordPhase, InspectorResetReason, InspectorSnapshotEntry, InspectorStreamEvent,
};
use crate::observe::inspector_persistence::{InspectorPersistenceWriter, restore_records};

pub(super) const MAX_RETENTION_REQUESTS: usize = 100_000;
const EVENT_CHANNEL_CAPACITY: usize = 1024;
const V2_REPLAY_CAPACITY: usize = 4096;
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct InspectorStore {
    pub(super) inner: Arc<InspectorStoreInner>,
}

pub(super) struct InspectorStoreInner {
    pub(super) records: Mutex<RetainedRecords>,
    pub(super) events: broadcast::Sender<InspectorRequestRecord>,
    pub(super) v2_events: broadcast::Sender<InspectorStreamEvent>,
    pub(super) v2_state: Mutex<V2State>,
    pub(super) persistence: Option<PersistenceComponents>,
}

pub(super) struct RetainedRecords {
    records_by_id: HashMap<String, RetainedRecord>,
    retention_order: VecDeque<String>,
}

struct RetainedRecord {
    record: InspectorRequestRecord,
    revision: u64,
}

struct EvictedRecord {
    record_id: String,
    revision: u64,
}

struct UpsertResult {
    revision: u64,
    evicted: Vec<EvictedRecord>,
}

pub(super) struct V2State {
    next_sequence: u64,
    replay: VecDeque<InspectorStreamEvent>,
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
            for retained in records.records_by_id.values_mut() {
                if matches!(retained.record.outcome, InspectorOutcome::InFlight) {
                    mark_record_interrupted(&mut retained.record);
                }
            }
            records
                .retention_order
                .iter()
                .filter_map(|record_id| records.records_by_id.get(record_id))
                .filter(|retained| matches!(retained.record.outcome, InspectorOutcome::Interrupted))
                .map(|retained| retained.record.clone())
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

// Add or replace a record while preserving insertion order and FIFO eviction.
// Replacing a record is O(1); only insertion and eviction touch the order
// deque.
fn upsert_with_fifo_eviction(
    records: &mut RetainedRecords,
    record: InspectorRequestRecord,
    retention_requests: usize,
) -> UpsertResult {
    let record_id = record.base.record_id.clone();
    if let Some(existing) = records.records_by_id.get_mut(&record_id) {
        existing.record = record;
        existing.revision = existing.revision.saturating_add(1);
        return UpsertResult {
            revision: existing.revision,
            evicted: Vec::new(),
        };
    }
    let mut evicted = Vec::new();
    while records.retention_order.len() >= retention_requests {
        let Some(evicted_id) = records.retention_order.pop_front() else {
            break;
        };
        if let Some(retained) = records.records_by_id.remove(&evicted_id) {
            evicted.push(EvictedRecord {
                record_id: evicted_id,
                // A removal is a state transition after the last retained
                // revision, so duplicate or stale deliveries can be ignored.
                revision: retained.revision.saturating_add(1),
            });
        }
    }
    records.retention_order.push_back(record_id.clone());
    records.records_by_id.insert(
        record_id,
        RetainedRecord {
            record,
            revision: 1,
        },
    );
    UpsertResult {
        revision: 1,
        evicted,
    }
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
        let (v2_events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let mut retained = RetainedRecords {
            records_by_id: HashMap::with_capacity(records.len()),
            retention_order: VecDeque::with_capacity(records.len()),
        };
        for record in records {
            upsert_with_fifo_eviction(&mut retained, record, MAX_RETENTION_REQUESTS);
        }
        Self {
            inner: Arc::new(InspectorStoreInner {
                records: Mutex::new(retained),
                events,
                v2_events,
                v2_state: Mutex::new(V2State {
                    next_sequence: 0,
                    replay: VecDeque::with_capacity(V2_REPLAY_CAPACITY),
                }),
                persistence,
            }),
        }
    }

    pub fn record(&self, enabled: bool, retention_requests: usize, record: InspectorRequestRecord) {
        if !enabled {
            return;
        }

        let retention_requests = retention_requests.clamp(1, MAX_RETENTION_REQUESTS);
        let result = {
            let mut records = self.inner.records.lock();
            upsert_with_fifo_eviction(&mut records, record.clone(), retention_requests)
        };
        let UpsertResult { revision, evicted } = result;
        self.publish_v2_removed(evicted);
        self.publish_v2(record.clone(), InspectorRecordPhase::Terminal, revision);

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
        let result = {
            let mut records = self.inner.records.lock();
            upsert_with_fifo_eviction(&mut records, record.clone(), retention_requests)
        };
        let UpsertResult { revision, evicted } = result;
        self.publish_v2_removed(evicted);
        self.publish_v2(record.clone(), InspectorRecordPhase::Live, revision);

        let _ = self.inner.events.send(record);
    }

    pub fn upsert_initial(
        &self,
        enabled: bool,
        retention_requests: usize,
        record: InspectorRequestRecord,
    ) {
        if !enabled {
            return;
        }

        let retention_requests = retention_requests.clamp(1, MAX_RETENTION_REQUESTS);
        let result = {
            let mut records = self.inner.records.lock();
            upsert_with_fifo_eviction(&mut records, record.clone(), retention_requests)
        };
        let UpsertResult { revision, evicted } = result;
        self.publish_v2_removed(evicted);
        self.publish_v2(record.clone(), InspectorRecordPhase::Initial, revision);
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
        let result = {
            let mut records = self.inner.records.lock();
            upsert_with_fifo_eviction(&mut records, record.clone(), retention_requests)
        };
        let UpsertResult { revision, evicted } = result;
        self.publish_v2_removed(evicted);
        self.publish_v2(record.clone(), InspectorRecordPhase::Terminal, revision);

        if let Some(persistence) = &self.inner.persistence {
            persistence
                .writer
                .record(record.clone(), retention_requests);
        }

        let _ = self.inner.events.send(record);
    }

    pub fn records_limited(&self, limit: usize) -> Vec<InspectorRequestRecord> {
        let records = self.inner.records.lock();
        let skip = records.retention_order.len().saturating_sub(limit.max(1));
        records
            .retention_order
            .iter()
            .skip(skip)
            .filter_map(|record_id| records.records_by_id.get(record_id))
            .map(|retained| retained.record.clone())
            .collect()
    }

    pub fn retained_len(&self) -> usize {
        self.inner.records.lock().retention_order.len()
    }

    pub fn get(&self, record_id: &str) -> Option<InspectorRequestRecord> {
        self.inner
            .records
            .lock()
            .records_by_id
            .get(record_id)
            .map(|retained| retained.record.clone())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<InspectorRequestRecord> {
        self.inner.events.subscribe()
    }

    pub fn subscribe_v2(
        &self,
        last_sequence: Option<u64>,
        snapshot_limit: usize,
    ) -> (
        broadcast::Receiver<InspectorStreamEvent>,
        Vec<InspectorStreamEvent>,
    ) {
        let receiver = self.inner.v2_events.subscribe();
        let initial = match last_sequence {
            Some(last_sequence) => self.replay_since(last_sequence).unwrap_or_else(|| {
                vec![
                    self.next_control_event(InspectorResetReason::ResumeUnavailable),
                    self.snapshot_event(snapshot_limit),
                ]
            }),
            None => vec![self.snapshot_event(snapshot_limit)],
        };
        (receiver, initial)
    }

    pub fn subscribe_v2_from_page(
        &self,
        last_sequence: Option<u64>,
        snapshot_limit: usize,
    ) -> (
        broadcast::Receiver<InspectorStreamEvent>,
        Vec<InspectorStreamEvent>,
    ) {
        let receiver = self.inner.v2_events.subscribe();
        let initial = match last_sequence {
            Some(last_sequence) if self.replay_since(last_sequence).is_none() => vec![
                self.next_control_event(InspectorResetReason::ResumeUnavailable),
                self.snapshot_event(snapshot_limit),
            ],
            // A page reload has no client-side record map to apply replay to.
            // Always send the authoritative snapshot even when its saved
            // sequence is current.
            _ => vec![self.snapshot_event(snapshot_limit)],
        };
        (receiver, initial)
    }

    pub fn reset_event(&self) -> InspectorStreamEvent {
        self.next_control_event(InspectorResetReason::Lagged)
    }

    pub fn snapshot_event(&self, limit: usize) -> InspectorStreamEvent {
        self.snapshot_event_inner(limit)
    }

    fn replay_since(&self, last_sequence: u64) -> Option<Vec<InspectorStreamEvent>> {
        let state = self.inner.v2_state.lock();
        let current = state.next_sequence;
        if last_sequence > current {
            // A client sequence from a previous process cannot be resumed
            // after the in-memory stream counter has restarted.
            return None;
        }
        if last_sequence == current {
            return Some(Vec::new());
        }
        let oldest = state.replay.front().and_then(event_sequence);
        if oldest.is_none_or(|oldest| last_sequence.saturating_add(1) < oldest) {
            return None;
        }
        Some(
            state
                .replay
                .iter()
                .filter(|event| {
                    event_sequence(event).is_some_and(|sequence| sequence > last_sequence)
                })
                .cloned()
                .collect(),
        )
    }

    fn snapshot_event_inner(&self, limit: usize) -> InspectorStreamEvent {
        let records = self.inner.records.lock();
        let skip = records.retention_order.len().saturating_sub(limit.max(1));
        let records = records
            .retention_order
            .iter()
            .skip(skip)
            .filter_map(|record_id| records.records_by_id.get(record_id))
            .map(|retained| InspectorSnapshotEntry {
                record_id: retained.record.base.record_id.clone(),
                revision: retained.revision,
                record: retained.record.clone(),
            })
            .collect();
        let stream_seq = self.inner.v2_state.lock().next_sequence;
        InspectorStreamEvent::Snapshot {
            stream_seq,
            records,
        }
    }

    fn next_control_event(&self, reason: InspectorResetReason) -> InspectorStreamEvent {
        let mut state = self.inner.v2_state.lock();
        state.next_sequence = state.next_sequence.saturating_add(1);
        InspectorStreamEvent::Reset {
            stream_seq: state.next_sequence,
            reason,
        }
    }

    fn publish_v2(
        &self,
        record: InspectorRequestRecord,
        phase: InspectorRecordPhase,
        revision: u64,
    ) {
        let mut state = self.inner.v2_state.lock();
        state.next_sequence = state.next_sequence.saturating_add(1);
        let event = InspectorStreamEvent::RecordUpsert {
            stream_seq: state.next_sequence,
            record_id: record.base.record_id.clone(),
            revision,
            phase,
            record: Box::new(record),
        };
        if state.replay.len() >= V2_REPLAY_CAPACITY {
            state.replay.pop_front();
        }
        state.replay.push_back(event.clone());
        let _ = self.inner.v2_events.send(event);
    }

    fn publish_v2_removed(&self, evicted: Vec<EvictedRecord>) {
        for evicted in evicted {
            let mut state = self.inner.v2_state.lock();
            state.next_sequence = state.next_sequence.saturating_add(1);
            let event = InspectorStreamEvent::RecordRemoved {
                stream_seq: state.next_sequence,
                record_id: evicted.record_id,
                revision: evicted.revision,
                reason: super::InspectorRemovalReason::RetentionEvicted,
            };
            if state.replay.len() >= V2_REPLAY_CAPACITY {
                state.replay.pop_front();
            }
            state.replay.push_back(event.clone());
            let _ = self.inner.v2_events.send(event);
        }
    }

    pub fn next_record_id(started_at_unix_ms: u64, client_request_id: Option<&str>) -> String {
        let sequence = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut record_id = format!("{started_at_unix_ms}-{}-{sequence}", std::process::id());
        if let Some(client_request_id) = client_request_id
            .and_then(|value| sanitize_for_storage(value, DISPLAY_SEGMENT_MAX_CHARS))
        {
            record_id.push('-');
            record_id.push_str(&client_request_id);
        }
        record_id
    }
}

fn event_sequence(event: &InspectorStreamEvent) -> Option<u64> {
    Some(match event {
        InspectorStreamEvent::Snapshot { stream_seq, .. }
        | InspectorStreamEvent::RecordUpsert { stream_seq, .. }
        | InspectorStreamEvent::RecordRemoved { stream_seq, .. }
        | InspectorStreamEvent::Reset { stream_seq, .. }
        | InspectorStreamEvent::Keepalive { stream_seq } => *stream_seq,
    })
}
