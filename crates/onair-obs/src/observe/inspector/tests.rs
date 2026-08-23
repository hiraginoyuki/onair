use std::collections::HashMap;
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, mpsc};
use std::time::{Duration, Instant};

use onair_core::config::InspectorPersistenceConfig;

use super::live::LiveRecord;
use super::records::{
    InspectorOutcome, InspectorRequestBase, InspectorRequestRecord, InspectorRequestRecordInit,
    InspectorTokenCounts,
};
use super::store::{InspectorStore, ProjectionTestPoint};
use super::ui::ui_html;
use crate::observe::TimelineSnapshot;
use crate::observe::inspector::{
    InspectorRecordPhase, InspectorRemovalReason, InspectorResetReason, InspectorStreamEvent,
};
use crate::observe::inspector_persistence::stored_count;

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

pub(crate) fn test_record(record_id: &str) -> InspectorRequestRecord {
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
            exposed_backend_error: false,
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
fn versioned_lookup_returns_each_retained_revision() {
    let store = InspectorStore::new();
    store.upsert_initial(true, 10, in_flight_record("versioned"));
    assert_eq!(store.get_versioned("versioned").unwrap().revision, 1);

    store.upsert(true, 10, in_flight_record("versioned"));
    assert_eq!(store.get_versioned("versioned").unwrap().revision, 2);

    store.record(true, 10, test_record("versioned"));
    let retained = store
        .get_versioned("versioned")
        .expect("versioned record is retained");
    assert_eq!(retained.record_id, "versioned");
    assert_eq!(retained.revision, 3);
    assert_eq!(retained.record.base.record_id, "versioned");
    assert_eq!(store.get("versioned").unwrap().base.record_id, "versioned");

    let InspectorStreamEvent::Snapshot { records, .. } = store.snapshot_event(10) else {
        panic!("snapshot event expected");
    };
    assert_eq!(records.len(), 1);
    assert_eq!(
        serde_json::to_value(&records[0]).unwrap(),
        serde_json::to_value(&retained).unwrap()
    );
    assert!(store.get_versioned("missing").is_none());
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
fn v2_stream_emits_retention_removal_before_replacement() {
    let store = InspectorStore::new();
    let (mut receiver, _) = store.subscribe_v2(None, 10);

    store.record(true, 1, test_record("oldest"));
    let _ = receiver.blocking_recv().expect("initial terminal update");

    store.record(true, 1, test_record("newest"));
    let removed = receiver.blocking_recv().expect("retention removal");
    assert!(matches!(
        removed,
        InspectorStreamEvent::RecordRemoved {
            stream_seq: 2,
            record_id,
            revision: 2,
            reason: InspectorRemovalReason::RetentionEvicted,
        } if record_id == "oldest"
    ));

    assert!(matches!(
        receiver.blocking_recv().expect("replacement terminal update"),
        InspectorStreamEvent::RecordUpsert {
            stream_seq: 3,
            record_id,
            revision: 1,
            phase: InspectorRecordPhase::Terminal,
            ..
        } if record_id == "newest"
    ));
}

#[test]
fn v2_snapshot_and_queued_update_share_one_authoritative_cut() {
    use tokio::sync::broadcast::error::TryRecvError;

    let store = Arc::new(InspectorStore::new());
    store.upsert_initial(true, 10, in_flight_record("same"));
    let (mut receiver, _) = store.subscribe_v2(None, 10);

    let snapshot_locked = Arc::new(Barrier::new(2));
    let release_snapshot = Arc::new(Barrier::new(2));
    let writer_before_lock = Arc::new(Barrier::new(2));
    let writer_mutated = Arc::new(AtomicBool::new(false));
    store.set_projection_test_hook(Some(Arc::new({
        let snapshot_locked = Arc::clone(&snapshot_locked);
        let release_snapshot = Arc::clone(&release_snapshot);
        let writer_before_lock = Arc::clone(&writer_before_lock);
        let writer_mutated = Arc::clone(&writer_mutated);
        move |point| match point {
            ProjectionTestPoint::SnapshotRecordsLocked => {
                snapshot_locked.wait();
                release_snapshot.wait();
            }
            ProjectionTestPoint::WriterBeforeRecordsLock { record_id } if record_id == "same" => {
                writer_before_lock.wait();
            }
            ProjectionTestPoint::WriterRecordsMutated { record_id } if record_id == "same" => {
                writer_mutated.store(true, Ordering::Release);
            }
            _ => {}
        }
    })));

    let snapshot_store = Arc::clone(&store);
    let snapshot_thread = std::thread::spawn(move || snapshot_store.snapshot_event(10));
    snapshot_locked.wait();

    let writer_store = Arc::clone(&store);
    let writer_thread = std::thread::spawn(move || {
        let mut update = in_flight_record("same");
        update.timeline.auth_done_us = Some(456);
        writer_store.upsert(true, 10, update);
    });
    writer_before_lock.wait();

    // The writer is inside the real mutation helper and is about to acquire
    // the retained-record lock. The snapshot still owns that lock, so the
    // writer cannot mutate records or publish a sequence across this cut.
    assert!(!writer_mutated.load(Ordering::Acquire));
    assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));

    release_snapshot.wait();
    let snapshot = snapshot_thread.join().expect("snapshot thread completes");
    writer_thread.join().expect("writer thread completes");
    store.set_projection_test_hook(None);

    let InspectorStreamEvent::Snapshot {
        stream_seq,
        records,
    } = snapshot
    else {
        panic!("snapshot event expected");
    };
    assert_eq!(stream_seq, 1);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].revision, 1);

    let queued = receiver.blocking_recv().expect("queued writer update");
    assert!(matches!(
        queued,
        InspectorStreamEvent::RecordUpsert {
            stream_seq: 2,
            revision: 2,
            ..
        }
    ));

    let mut projected: HashMap<String, super::InspectorVersionedRecord> = records
        .into_iter()
        .map(|record| (record.record_id.clone(), record))
        .collect();
    let InspectorStreamEvent::RecordUpsert {
        record_id,
        revision,
        record,
        ..
    } = queued
    else {
        panic!("record upsert expected");
    };
    projected.insert(
        record_id.clone(),
        super::InspectorVersionedRecord {
            record_id,
            revision,
            record: *record,
        },
    );

    let InspectorStreamEvent::Snapshot {
        records: canonical, ..
    } = store.snapshot_event(10)
    else {
        panic!("canonical snapshot expected");
    };
    let mut projected: Vec<_> = projected.into_values().collect();
    projected.sort_by(|left, right| left.record_id.cmp(&right.record_id));
    assert_eq!(
        serde_json::to_value(projected).unwrap(),
        serde_json::to_value(canonical).unwrap()
    );
}

#[test]
fn concurrent_retention_one_writers_publish_without_resurrection_or_deadlock() {
    let store = Arc::new(InspectorStore::new());
    let (mut receiver, _) = store.subscribe_v2(None, 1);

    let first_mutated = Arc::new(Barrier::new(2));
    let release_first = Arc::new(Barrier::new(2));
    let second_before_lock = Arc::new(Barrier::new(2));
    let second_mutated = Arc::new(AtomicBool::new(false));
    store.set_projection_test_hook(Some(Arc::new({
        let first_mutated = Arc::clone(&first_mutated);
        let release_first = Arc::clone(&release_first);
        let second_before_lock = Arc::clone(&second_before_lock);
        let second_mutated = Arc::clone(&second_mutated);
        move |point| match point {
            ProjectionTestPoint::WriterRecordsMutated { record_id } if record_id == "first" => {
                first_mutated.wait();
                release_first.wait();
            }
            ProjectionTestPoint::WriterBeforeRecordsLock { record_id } if record_id == "second" => {
                second_before_lock.wait();
            }
            ProjectionTestPoint::WriterRecordsMutated { record_id } if record_id == "second" => {
                second_mutated.store(true, Ordering::Release);
            }
            _ => {}
        }
    })));

    let (done_tx, done_rx) = mpsc::channel();
    let first_store = Arc::clone(&store);
    let first_done = done_tx.clone();
    let first = std::thread::spawn(move || {
        first_store.record(true, 1, test_record("first"));
        first_done.send("first").expect("report first completion");
    });
    first_mutated.wait();

    let second_store = Arc::clone(&store);
    let second = std::thread::spawn(move || {
        second_store.record(true, 1, test_record("second"));
        done_tx.send("second").expect("report second completion");
    });
    second_before_lock.wait();

    // The first writer holds both ownership of its retained mutation and the
    // right to publish it. The second writer has entered the same helper but
    // cannot evict or publish until the first writer completes.
    assert!(!second_mutated.load(Ordering::Acquire));
    release_first.wait();

    let mut completed = vec![
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first writer completes without deadlock"),
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("second writer completes without deadlock"),
    ];
    completed.sort_unstable();
    assert_eq!(completed, ["first", "second"]);
    first.join().expect("first writer thread joins");
    second.join().expect("second writer thread joins");
    store.set_projection_test_hook(None);

    assert!(matches!(
        receiver.blocking_recv().expect("first upsert"),
        InspectorStreamEvent::RecordUpsert {
            stream_seq: 1,
            record_id,
            revision: 1,
            ..
        } if record_id == "first"
    ));
    assert!(matches!(
        receiver.blocking_recv().expect("first removal"),
        InspectorStreamEvent::RecordRemoved {
            stream_seq: 2,
            record_id,
            revision: 2,
            reason: InspectorRemovalReason::RetentionEvicted,
        } if record_id == "first"
    ));
    assert!(matches!(
        receiver.blocking_recv().expect("second replacement"),
        InspectorStreamEvent::RecordUpsert {
            stream_seq: 3,
            record_id,
            revision: 1,
            ..
        } if record_id == "second"
    ));

    let canonical = store.records_limited(usize::MAX);
    assert_eq!(canonical.len(), 1);
    assert_eq!(canonical[0].base.record_id, "second");
}

#[test]
#[ignore = "synthetic measurement harness; run explicitly with --ignored --nocapture"]
fn measure_snapshot_and_replay_behavior() {
    let snapshot_corpora = [100, 1_000]
        .into_iter()
        .map(measure_snapshot_corpus)
        .collect::<Vec<_>>();
    let report = serde_json::json!({
        "schema": "onair-inspector-snapshot-replay-measurement-v1",
        "samples": {
            "warmups": 10,
            "recorded": 50,
        },
        "snapshot_corpora": snapshot_corpora,
        "directed_replay": directed_replay_measurement(),
        "production_observations": {
            "reconnect_frequency": "unavailable_without_production_telemetry",
            "replay_hit_rate": "unavailable_without_production_telemetry",
            "directed_cases_are_production_frequency": false,
        },
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("measurement report serializes")
    );
}

fn measure_snapshot_corpus(record_count: usize) -> serde_json::Value {
    let store = InspectorStore::new();
    for index in 0..record_count {
        store.record(true, record_count, deterministic_measurement_record(index));
    }

    let construction = measure_percentiles(|| store.snapshot_event(record_count));
    let snapshot = store.snapshot_event(record_count);
    let serialized_snapshot = serde_json::to_vec(&snapshot).expect("snapshot serializes");
    let serialization = measure_percentiles(|| {
        serde_json::to_vec(&snapshot).expect("snapshot serializes during measurement")
    });
    let (replay_events, replay_serialized_bytes, configured_capacity_events) =
        store.v2_replay_measurement();

    serde_json::json!({
        "record_count": record_count,
        "serialized_snapshot_bytes": serialized_snapshot.len(),
        "snapshot_construction_us": construction,
        "snapshot_serialization_us": serialization,
        "replay_buffer": {
            "configured_capacity_events": configured_capacity_events,
            "retained_events": replay_events,
            "serialized_event_bytes": replay_serialized_bytes,
        },
    })
}

fn measure_percentiles<T>(mut operation: impl FnMut() -> T) -> serde_json::Value {
    for _ in 0..10 {
        black_box(operation());
    }
    let mut samples = Vec::with_capacity(50);
    for _ in 0..50 {
        let started = Instant::now();
        black_box(operation());
        samples.push(started.elapsed().as_secs_f64() * 1_000_000.0);
    }
    samples.sort_by(f64::total_cmp);
    serde_json::json!({
        "p50": percentile(&samples, 50),
        "p95": percentile(&samples, 95),
    })
}

fn percentile(samples: &[f64], percentile: usize) -> f64 {
    let rank = (samples.len() * percentile).div_ceil(100);
    samples[rank.saturating_sub(1).min(samples.len().saturating_sub(1))]
}

fn deterministic_measurement_record(index: usize) -> InspectorRequestRecord {
    let mut record = test_record(&format!("measurement-{index:04}"));
    let started_at_unix_ms = 1_700_000_000_000_u64.saturating_add(index as u64);
    record.base.started_at_unix_ms = started_at_unix_ms;
    record.completed_at_unix_ms = started_at_unix_ms.saturating_add(1);
    record.timeline = TimelineSnapshot {
        started_unix_ms: started_at_unix_ms,
        total_us: 1_000_u64.saturating_add(index as u64),
        proxy_entry_us: 0,
        ..TimelineSnapshot::default()
    };
    record
}

fn directed_replay_measurement() -> serde_json::Value {
    let store = InspectorStore::new();
    for revision in 0..=4_096 {
        let mut record = deterministic_measurement_record(0);
        record.timeline.total_us = revision;
        store.upsert(true, 1, record);
    }

    let current_sequence = stream_sequence(&store.snapshot_event(1));
    let (_, hit) = store.subscribe_v2(Some(current_sequence.saturating_sub(1)), 1);
    let (_, current) = store.subscribe_v2(Some(current_sequence), 1);
    let (_, miss) = store.subscribe_v2(Some(0), 1);
    let sequence_after_miss = miss.last().map(stream_sequence).unwrap_or(current_sequence);
    let future_cursor = sequence_after_miss.saturating_add(1);
    let (_, future) = store.subscribe_v2(Some(future_cursor), 1);

    serde_json::json!({
        "hit": directed_replay_case(current_sequence.saturating_sub(1), &hit),
        "current": directed_replay_case(current_sequence, &current),
        "miss": directed_replay_case(0, &miss),
        "future": directed_replay_case(future_cursor, &future),
    })
}

fn directed_replay_case(cursor: u64, events: &[InspectorStreamEvent]) -> serde_json::Value {
    serde_json::json!({
        "cursor": cursor,
        "event_count": events.len(),
        "event_kinds": events.iter().map(stream_event_kind).collect::<Vec<_>>(),
        "authoritative_snapshot": events
            .iter()
            .any(|event| matches!(event, InspectorStreamEvent::Snapshot { .. })),
    })
}

fn stream_event_kind(event: &InspectorStreamEvent) -> &'static str {
    match event {
        InspectorStreamEvent::Snapshot { .. } => "snapshot",
        InspectorStreamEvent::RecordUpsert { .. } => "record_upsert",
        InspectorStreamEvent::RecordRemoved { .. } => "record_removed",
        InspectorStreamEvent::Reset { .. } => "reset",
    }
}

fn stream_sequence(event: &InspectorStreamEvent) -> u64 {
    match event {
        InspectorStreamEvent::Snapshot { stream_seq, .. }
        | InspectorStreamEvent::RecordUpsert { stream_seq, .. }
        | InspectorStreamEvent::RecordRemoved { stream_seq, .. }
        | InspectorStreamEvent::Reset { stream_seq, .. } => *stream_seq,
    }
}

#[test]
fn v2_stream_tracks_phases_revisions_and_replay() {
    let store = InspectorStore::new();
    let (mut receiver, initial) = store.subscribe_v2(None, 10);
    assert!(matches!(
        initial.as_slice(),
        [InspectorStreamEvent::Snapshot { stream_seq: 0, .. }]
    ));

    store.upsert_initial(true, 10, in_flight_record("stream-1"));
    let initial_update = receiver.blocking_recv().expect("initial v2 update");
    assert!(matches!(
        initial_update,
        InspectorStreamEvent::RecordUpsert {
            stream_seq: 1,
            revision: 1,
            phase: InspectorRecordPhase::Initial,
            ..
        }
    ));

    let mut live = in_flight_record("stream-1");
    live.timeline.auth_done_us = Some(123);
    store.upsert(true, 10, live);
    let live_update = receiver.blocking_recv().expect("live v2 update");
    assert!(matches!(
        live_update,
        InspectorStreamEvent::RecordUpsert {
            stream_seq: 2,
            revision: 2,
            phase: InspectorRecordPhase::Live,
            ..
        }
    ));

    store.record(true, 10, test_record("stream-1"));
    let terminal_update = receiver.blocking_recv().expect("terminal v2 update");
    assert!(matches!(
        terminal_update,
        InspectorStreamEvent::RecordUpsert {
            stream_seq: 3,
            revision: 3,
            phase: InspectorRecordPhase::Terminal,
            ..
        }
    ));

    let (_, replay) = store.subscribe_v2(Some(1), 10);
    assert_eq!(replay.len(), 2);
    assert!(matches!(
        replay[0],
        InspectorStreamEvent::RecordUpsert {
            stream_seq: 2,
            revision: 2,
            ..
        }
    ));
    assert!(matches!(
        replay[1],
        InspectorStreamEvent::RecordUpsert {
            stream_seq: 3,
            revision: 3,
            ..
        }
    ));
}

#[test]
fn v2_unavailable_resume_returns_reset_and_snapshot() {
    let store = InspectorStore::new();
    for index in 0..=4096 {
        store.record(true, 10, test_record(&format!("replay-{index}")));
    }

    let (_, initial) = store.subscribe_v2(Some(0), 3);
    assert!(matches!(
        initial.as_slice(),
        [
            InspectorStreamEvent::Reset {
                reason: InspectorResetReason::ResumeUnavailable,
                ..
            },
            InspectorStreamEvent::Snapshot { records, .. }
        ] if records.len() == 3
    ));
}

#[test]
fn v2_future_resume_returns_reset_and_snapshot() {
    let store = InspectorStore::new();
    store.record(true, 10, test_record("current"));

    let (_, initial) = store.subscribe_v2(Some(99), 3);
    assert!(matches!(
        initial.as_slice(),
        [
            InspectorStreamEvent::Reset {
                reason: InspectorResetReason::ResumeUnavailable,
                ..
            },
            InspectorStreamEvent::Snapshot { records, .. }
        ] if records.len() == 1
    ));
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
    let config = onair_core::config::InspectorConfig {
        enabled: true,
        retention_requests: 3,
        allow_remote: false,
        allowed_client_cidrs: vec![],
        persistence: InspectorPersistenceConfig {
            enabled: true,
            path: Some(path.clone()),
            ..InspectorPersistenceConfig::default()
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
    let config = onair_core::config::InspectorConfig {
        enabled: true,
        retention_requests: 3,
        allow_remote: false,
        allowed_client_cidrs: vec![],
        persistence: InspectorPersistenceConfig {
            enabled: true,
            path: Some(path.clone()),
            ..InspectorPersistenceConfig::default()
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
    let config = onair_core::config::InspectorConfig {
        enabled: true,
        retention_requests: 2,
        allow_remote: false,
        allowed_client_cidrs: vec![],
        persistence: InspectorPersistenceConfig {
            enabled: true,
            path: Some(path.clone()),
            ..InspectorPersistenceConfig::default()
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
    let config = onair_core::config::InspectorConfig {
        enabled: true,
        retention_requests: 32,
        allow_remote: false,
        allowed_client_cidrs: vec![],
        persistence: InspectorPersistenceConfig {
            enabled: true,
            path: Some(path.clone()),
            ..InspectorPersistenceConfig::default()
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
    let config = onair_core::config::InspectorConfig {
        enabled: true,
        retention_requests: 8,
        allow_remote: false,
        allowed_client_cidrs: vec![],
        persistence: InspectorPersistenceConfig {
            enabled: true,
            path: Some(path.clone()),
            ..InspectorPersistenceConfig::default()
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
    let store = Arc::new(InspectorStore::new());
    let cloned = Arc::clone(&store);

    let result = std::thread::spawn(move || {
        let _guard = cloned.inner.records.lock();
        panic!("simulated panic while holding the inspector store lock");
    })
    .join();

    assert!(result.is_err(), "expected the spawned thread to panic");

    // The store must still be usable: insert and look up a record.
    store.record(true, 4, test_record("after-panic"));
    let stored = store.get("after-panic").expect("record survives panic");
    assert_eq!(stored.base.record_id, "after-panic");
}

#[tokio::test]
async fn broadcast_lagged_is_observed_when_slow_subscriber_falls_behind() {
    use tokio::sync::broadcast::error::TryRecvError;

    let store = InspectorStore::new();
    let mut receiver = store.subscribe();
    // EVENT_CHANNEL_CAPACITY is 1024. Push that many more events
    // than the receiver can buffer so the receiver is forced to
    // report Lagged on the next recv.
    for index in 0..(1024 + 16) {
        store.record(true, 2048, test_record(&format!("flood-{index}")));
    }
    let result = receiver.try_recv();
    assert!(
        matches!(result, Err(TryRecvError::Lagged(_))),
        "expected Lagged after a flood, got {result:?}"
    );
}

#[tokio::test]
async fn broadcast_closed_is_observed_after_store_drop() {
    use tokio::sync::broadcast::error::TryRecvError;

    let store = InspectorStore::new();
    let mut receiver = store.subscribe();
    drop(store);
    let result = receiver.try_recv();
    assert!(
        matches!(result, Err(TryRecvError::Closed)),
        "expected Closed after the store is dropped, got {result:?}"
    );
}
