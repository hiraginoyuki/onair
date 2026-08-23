use serde::Serialize;
use serde_json::{Value, json};

use super::{
    InspectorAttemptRecord, InspectorOutcome, InspectorRecordPhase, InspectorRemovalReason,
    InspectorRequestBase, InspectorRequestRecord, InspectorResetReason, InspectorStreamEvent,
    InspectorVersionedRecord,
};
use crate::observe::TimelineSnapshot;

const COMMITTED_CORPUS: &str =
    include_str!("../../../../../ui/inspector/src/lib/fixtures/inspector-wire-contract.json");

#[derive(Serialize)]
struct WireContractCorpus {
    records: CorpusRecords,
    versioned_detail: InspectorVersionedRecord,
    valid_events: Vec<LabeledEvent>,
    malformed: Vec<MalformedCase>,
}

#[derive(Serialize)]
struct CorpusRecords {
    ordinary: InspectorRequestRecord,
    all_optionals: InspectorRequestRecord,
}

#[derive(Serialize)]
struct LabeledEvent {
    label: &'static str,
    event: InspectorStreamEvent,
}

#[derive(Serialize)]
struct MalformedCase {
    label: &'static str,
    value: Value,
}

#[test]
fn committed_wire_corpus_is_serialized_from_rust_dtos() {
    let corpus = wire_contract_corpus();
    let canonical = format!(
        "{}\n",
        serde_json::to_string_pretty(&corpus).expect("wire corpus pretty-serializes")
    );
    let expected = serde_json::to_value(&corpus).expect("wire corpus serializes");
    let committed: Value =
        serde_json::from_str(COMMITTED_CORPUS).expect("committed wire corpus is valid JSON");

    let ordinary = &expected["records"]["ordinary"];
    assert!(ordinary.get("backend_attempts").is_none());
    assert!(ordinary.get("retried_attempts").is_none());
    assert!(ordinary.get("exposed_backend_error").is_none());
    assert!(ordinary.get("client_request_id").is_none());
    assert!(ordinary["timeline"]["auth_done_us"].is_null());

    let all_optionals = &expected["records"]["all_optionals"];
    for pointer in [
        "/client_request_id",
        "/query",
        "/backend_remote_addr",
        "/debug_capture_id",
        "/error_kind",
        "/response_body_bytes",
        "/outcome/stage",
        "/timeline/auth_done_us",
        "/timeline/request_inspected_us",
        "/timeline/route_selected_us",
        "/timeline/request_rewritten_us",
        "/timeline/debug_capture_done_us",
        "/timeline/backend_forward_start_us",
        "/timeline/backend_headers_received_us",
        "/timeline/backend_body_first_chunk_us",
        "/timeline/backend_body_complete_us",
        "/timeline/response_rewritten_us",
        "/timeline/client_response_ready_us",
        "/timeline/stream_complete_us",
        "/backend_attempts/0/backend_remote_addr",
        "/backend_attempts/0/debug_capture_id",
        "/backend_attempts/0/error_kind",
        "/backend_attempts/0/upstream_status",
        "/backend_attempts/0/request_rewritten_us",
        "/backend_attempts/0/debug_capture_done_us",
        "/backend_attempts/0/backend_forward_start_us",
        "/backend_attempts/0/backend_headers_received_us",
        "/backend_attempts/0/backend_body_first_chunk_us",
        "/backend_attempts/0/backend_body_complete_us",
        "/backend_attempts/0/stream_complete_us",
    ] {
        assert!(
            all_optionals
                .pointer(pointer)
                .is_some_and(|value| !value.is_null()),
            "expected optional fixture field at {pointer}"
        );
    }

    assert_eq!(committed, expected);
    assert_eq!(
        COMMITTED_CORPUS, canonical,
        "committed corpus must use the canonical Rust formatting"
    );
}

fn wire_contract_corpus() -> WireContractCorpus {
    let ordinary = ordinary_record();
    let all_optionals = all_optionals_record();
    let versioned_detail = InspectorVersionedRecord {
        record_id: all_optionals.base.record_id.clone(),
        revision: 7,
        record: all_optionals.clone(),
    };

    let valid_events = vec![
        labeled(
            "snapshot_empty",
            InspectorStreamEvent::Snapshot {
                stream_seq: 0,
                records: Vec::new(),
            },
        ),
        labeled(
            "snapshot_records",
            InspectorStreamEvent::Snapshot {
                stream_seq: 1,
                records: vec![
                    InspectorVersionedRecord {
                        record_id: ordinary.base.record_id.clone(),
                        revision: 1,
                        record: ordinary.clone(),
                    },
                    versioned_detail.clone(),
                ],
            },
        ),
        labeled(
            "upsert_initial",
            upsert(2, 2, InspectorRecordPhase::Initial, ordinary.clone()),
        ),
        labeled(
            "upsert_live",
            upsert(3, 8, InspectorRecordPhase::Live, all_optionals.clone()),
        ),
        labeled(
            "upsert_terminal",
            upsert(4, 3, InspectorRecordPhase::Terminal, ordinary.clone()),
        ),
        labeled(
            "remove_retention_evicted",
            InspectorStreamEvent::RecordRemoved {
                stream_seq: 5,
                record_id: ordinary.base.record_id.clone(),
                revision: 4,
                reason: InspectorRemovalReason::RetentionEvicted,
            },
        ),
        labeled(
            "remove_explicit",
            InspectorStreamEvent::RecordRemoved {
                stream_seq: 6,
                record_id: all_optionals.base.record_id.clone(),
                revision: 9,
                reason: InspectorRemovalReason::Explicit,
            },
        ),
        labeled(
            "reset_resume_unavailable",
            InspectorStreamEvent::Reset {
                stream_seq: 7,
                reason: InspectorResetReason::ResumeUnavailable,
            },
        ),
        labeled(
            "reset_lagged",
            InspectorStreamEvent::Reset {
                stream_seq: 8,
                reason: InspectorResetReason::Lagged,
            },
        ),
        labeled(
            "reset_server_restarted",
            InspectorStreamEvent::Reset {
                stream_seq: 9,
                reason: InspectorResetReason::ServerRestarted,
            },
        ),
    ];

    let ordinary_upsert = event_value(upsert(
        10,
        4,
        InspectorRecordPhase::Terminal,
        ordinary.clone(),
    ));
    let optional_upsert = event_value(upsert(
        11,
        9,
        InspectorRecordPhase::Live,
        all_optionals.clone(),
    ));
    let populated_snapshot = event_value(InspectorStreamEvent::Snapshot {
        stream_seq: 12,
        records: vec![InspectorVersionedRecord {
            record_id: ordinary.base.record_id.clone(),
            revision: 4,
            record: ordinary.clone(),
        }],
    });
    let removal = event_value(InspectorStreamEvent::RecordRemoved {
        stream_seq: 13,
        record_id: ordinary.base.record_id.clone(),
        revision: 5,
        reason: InspectorRemovalReason::Explicit,
    });
    let reset = event_value(InspectorStreamEvent::Reset {
        stream_seq: 14,
        reason: InspectorResetReason::Lagged,
    });

    let malformed = vec![
        malformed(
            "invalid_array_backend_attempts_object",
            set_at(
                ordinary_upsert.clone(),
                &["record", "backend_attempts"],
                json!({}),
            ),
        ),
        malformed(
            "invalid_array_retried_attempts_string",
            set_at(
                ordinary_upsert.clone(),
                &["record", "retried_attempts"],
                json!("none"),
            ),
        ),
        malformed(
            "invalid_boolean_stream_integer",
            set_at(ordinary_upsert.clone(), &["record", "stream"], json!(0)),
        ),
        malformed(
            "invalid_boolean_exposure_string",
            set_at(
                ordinary_upsert.clone(),
                &["record", "exposed_backend_error"],
                json!("false"),
            ),
        ),
        malformed(
            "invalid_string_method_integer",
            set_at(ordinary_upsert.clone(), &["record", "method"], json!(42)),
        ),
        malformed(
            "invalid_string_optional_client_request_id_boolean",
            set_at(
                ordinary_upsert.clone(),
                &["record", "client_request_id"],
                json!(true),
            ),
        ),
        malformed(
            "invalid_integer_status_string",
            set_at(ordinary_upsert.clone(), &["record", "status"], json!("200")),
        ),
        malformed(
            "invalid_integer_status_fractional",
            set_at(ordinary_upsert.clone(), &["record", "status"], json!(200.5)),
        ),
        malformed(
            "invalid_integer_timeline_negative",
            set_at(
                ordinary_upsert.clone(),
                &["record", "timeline", "total_us"],
                json!(-1),
            ),
        ),
        malformed(
            "invalid_integer_attempt_status_string",
            set_at(
                optional_upsert,
                &["record", "backend_attempts", "0", "status"],
                json!("502"),
            ),
        ),
        malformed(
            "invalid_revision_zero",
            set_at(ordinary_upsert.clone(), &["revision"], json!(0)),
        ),
        malformed(
            "invalid_revision_string",
            set_at(ordinary_upsert.clone(), &["revision"], json!("4")),
        ),
        malformed(
            "invalid_id_empty",
            set_at(ordinary_upsert.clone(), &["record_id"], json!("")),
        ),
        malformed(
            "invalid_id_mismatch",
            set_at(
                ordinary_upsert.clone(),
                &["record_id"],
                json!("different-record"),
            ),
        ),
        malformed("invalid_envelope_array", json!([])),
        malformed("invalid_envelope_null", Value::Null),
        malformed(
            "invalid_envelope_unknown_kind",
            json!({"kind": "keepalive", "stream_seq": 15}),
        ),
        malformed(
            "invalid_envelope_stream_sequence_fractional",
            set_at(reset.clone(), &["stream_seq"], json!(14.5)),
        ),
        malformed(
            "invalid_envelope_snapshot_records_object",
            set_at(populated_snapshot.clone(), &["records"], json!({})),
        ),
        malformed(
            "invalid_envelope_snapshot_entry_array",
            set_at(populated_snapshot, &["records", "0"], json!([])),
        ),
        malformed(
            "invalid_envelope_upsert_phase",
            set_at(ordinary_upsert, &["phase"], json!("unknown")),
        ),
        malformed(
            "invalid_envelope_removal_reason",
            set_at(removal, &["reason"], json!("unknown")),
        ),
        malformed(
            "invalid_envelope_reset_reason",
            set_at(reset, &["reason"], json!("unknown")),
        ),
    ];

    WireContractCorpus {
        records: CorpusRecords {
            ordinary,
            all_optionals,
        },
        versioned_detail,
        valid_events,
        malformed,
    }
}

fn ordinary_record() -> InspectorRequestRecord {
    InspectorRequestRecord {
        base: InspectorRequestBase {
            record_id: "ordinary-record".to_owned(),
            client_request_id: None,
            started_at_unix_ms: 1_700_000_000_000,
            method: "POST".to_owned(),
            path: "/v1/responses".to_owned(),
            query: None,
            route: "fixture-route".to_owned(),
            identity: "fixture-identity".to_owned(),
            requested_model: "fixture-model".to_owned(),
            public_model: "fixture-model".to_owned(),
            backend_model: "fixture-backend-model".to_owned(),
            backend: "fixture-backend".to_owned(),
            backend_target: "fixture-target".to_owned(),
            backend_remote_addr: None,
            stream: false,
            peer_addr: "not-recorded".to_owned(),
            effective_client_addr: "not-recorded".to_owned(),
            trusted_proxy_addr: "not-recorded".to_owned(),
            forwarded_for: "not-recorded".to_owned(),
            user_agent: "fixture-client".to_owned(),
            request_body_bytes: 123,
            debug_capture_id: None,
            exposed_backend_error: false,
        },
        outcome: InspectorOutcome::Completed,
        status: 200,
        error_kind: None,
        backend_attempts: Vec::new(),
        retried_attempts: Vec::new(),
        response_body_bytes: None,
        input_tokens: 0,
        cached_input_tokens: 0,
        output_tokens: 0,
        completed_at_unix_ms: 1_700_000_000_002,
        timeline: TimelineSnapshot {
            started_unix_ms: 1_700_000_000_000,
            total_us: 1_500,
            proxy_entry_us: 0,
            ..TimelineSnapshot::default()
        },
    }
}

fn all_optionals_record() -> InspectorRequestRecord {
    InspectorRequestRecord {
        base: InspectorRequestBase {
            record_id: "all-optionals-record".to_owned(),
            client_request_id: Some("fixture-client-request".to_owned()),
            started_at_unix_ms: 1_700_000_001_000,
            method: "POST".to_owned(),
            path: "/v1/chat/completions".to_owned(),
            query: Some("mode=fixture".to_owned()),
            route: "fixture-route".to_owned(),
            identity: "fixture-identity".to_owned(),
            requested_model: "fixture-model".to_owned(),
            public_model: "fixture-model".to_owned(),
            backend_model: "fixture-backend-model".to_owned(),
            backend: "fixture-backend".to_owned(),
            backend_target: "fixture-target".to_owned(),
            backend_remote_addr: Some("not-recorded".to_owned()),
            stream: true,
            peer_addr: "not-recorded".to_owned(),
            effective_client_addr: "not-recorded".to_owned(),
            trusted_proxy_addr: "not-recorded".to_owned(),
            forwarded_for: "not-recorded".to_owned(),
            user_agent: "fixture-client".to_owned(),
            request_body_bytes: 234,
            debug_capture_id: Some("fixture-capture".to_owned()),
            exposed_backend_error: true,
        },
        outcome: InspectorOutcome::Preflight {
            stage: "routing".to_owned(),
        },
        status: 503,
        error_kind: Some("fixture_error".to_owned()),
        backend_attempts: vec![all_optionals_attempt()],
        retried_attempts: Vec::new(),
        response_body_bytes: Some(456),
        input_tokens: 11,
        cached_input_tokens: 4,
        output_tokens: 7,
        completed_at_unix_ms: 1_700_000_001_009,
        timeline: TimelineSnapshot {
            started_unix_ms: 1_700_000_001_000,
            total_us: 9_000,
            proxy_entry_us: 0,
            auth_done_us: Some(100),
            request_inspected_us: Some(200),
            route_selected_us: Some(300),
            request_rewritten_us: Some(400),
            debug_capture_done_us: Some(500),
            backend_forward_start_us: Some(600),
            backend_headers_received_us: Some(700),
            backend_body_first_chunk_us: Some(800),
            backend_body_complete_us: Some(850),
            response_rewritten_us: Some(875),
            client_response_ready_us: Some(900),
            stream_complete_us: Some(950),
        },
    }
}

fn all_optionals_attempt() -> InspectorAttemptRecord {
    InspectorAttemptRecord {
        attempt: 1,
        backend: "fixture-backend".to_owned(),
        backend_target: "fixture-target".to_owned(),
        backend_remote_addr: Some("not-recorded".to_owned()),
        debug_capture_id: Some("fixture-attempt-capture".to_owned()),
        status: 502,
        outcome: "upstream_non_success".to_owned(),
        error_kind: Some("fixture_attempt_error".to_owned()),
        started_us: 600,
        ended_us: 850,
        elapsed_us: 250,
        elapsed_ms: 0,
        upstream_status: Some(502),
        request_rewritten_us: Some(400),
        debug_capture_done_us: Some(500),
        backend_forward_start_us: Some(600),
        backend_headers_received_us: Some(700),
        backend_body_first_chunk_us: Some(800),
        backend_body_complete_us: Some(850),
        stream_complete_us: Some(950),
    }
}

fn upsert(
    stream_seq: u64,
    revision: u64,
    phase: InspectorRecordPhase,
    record: InspectorRequestRecord,
) -> InspectorStreamEvent {
    InspectorStreamEvent::RecordUpsert {
        stream_seq,
        record_id: record.base.record_id.clone(),
        revision,
        phase,
        record: Box::new(record),
    }
}

fn labeled(label: &'static str, event: InspectorStreamEvent) -> LabeledEvent {
    LabeledEvent { label, event }
}

fn event_value(event: InspectorStreamEvent) -> Value {
    serde_json::to_value(event).expect("fixture event serializes")
}

fn malformed(label: &'static str, value: Value) -> MalformedCase {
    MalformedCase { label, value }
}

fn set_at(mut value: Value, path: &[&str], replacement: Value) -> Value {
    let (field, parents) = path.split_last().expect("fixture path is non-empty");
    let mut target = &mut value;
    for parent in parents {
        target = match target {
            Value::Object(object) => object
                .get_mut(*parent)
                .unwrap_or_else(|| panic!("missing fixture object field {parent}")),
            Value::Array(array) => {
                let index = parent
                    .parse::<usize>()
                    .unwrap_or_else(|_| panic!("invalid fixture array index {parent}"));
                array
                    .get_mut(index)
                    .unwrap_or_else(|| panic!("missing fixture array index {parent}"))
            }
            _ => panic!("fixture path parent {parent} is not a container"),
        };
    }
    match target {
        Value::Object(object) => {
            object.insert((*field).to_owned(), replacement);
        }
        Value::Array(array) => {
            let index = field
                .parse::<usize>()
                .unwrap_or_else(|_| panic!("invalid fixture array index {field}"));
            array[index] = replacement;
        }
        _ => panic!("fixture path target is not a container"),
    }
    value
}
