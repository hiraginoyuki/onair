use std::sync::Arc;

use onair_core::config::InspectorPersistenceConfig;

use super::live::LiveRecord;
use super::records::{
    InspectorOutcome, InspectorRequestBase, InspectorRequestRecord, InspectorRequestRecordInit,
    InspectorTokenCounts,
};
use super::store::InspectorStore;
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
fn v2_page_resume_starts_with_snapshot() {
    let store = InspectorStore::new();
    store.record(true, 10, test_record("current"));

    let (_, initial) = store.subscribe_v2_from_page(Some(1), 3);
    assert!(matches!(
        initial.as_slice(),
        [InspectorStreamEvent::Snapshot { stream_seq: 1, records }]
            if records.len() == 1 && records[0].record_id == "current"
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
