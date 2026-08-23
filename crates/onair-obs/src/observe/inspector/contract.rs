use serde::{Deserialize, Serialize};

use super::records::InspectorRequestRecord;

/// Wire contract for the next inspector event stream.
///
/// The legacy stream carries bare records and has no resume identity. These
/// envelopes are kept separate so the new transport can be introduced without
/// changing the existing operator route in place.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InspectorStreamEvent {
    Snapshot {
        stream_seq: u64,
        records: Vec<InspectorVersionedRecord>,
    },
    RecordUpsert {
        stream_seq: u64,
        record_id: String,
        revision: u64,
        phase: InspectorRecordPhase,
        record: Box<InspectorRequestRecord>,
    },
    RecordRemoved {
        stream_seq: u64,
        record_id: String,
        revision: u64,
        reason: InspectorRemovalReason,
    },
    Reset {
        stream_seq: u64,
        reason: InspectorResetReason,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InspectorVersionedRecord {
    pub record_id: String,
    pub revision: u64,
    pub record: InspectorRequestRecord,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InspectorRecordPhase {
    Initial,
    Live,
    Terminal,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InspectorRemovalReason {
    RetentionEvicted,
    Explicit,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InspectorResetReason {
    ResumeUnavailable,
    Lagged,
    ServerRestarted,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::observe::inspector::tests::test_record;

    #[test]
    fn serializes_synthetic_record_upsert_contract() {
        let event = InspectorStreamEvent::RecordUpsert {
            stream_seq: 42,
            record_id: "synthetic-1".to_owned(),
            revision: 3,
            phase: InspectorRecordPhase::Terminal,
            record: Box::new(test_record("synthetic-1")),
        };

        let value = serde_json::to_value(event).expect("event serializes");
        assert_eq!(value["kind"], "record_upsert");
        assert_eq!(value["stream_seq"], 42);
        assert_eq!(value["record_id"], "synthetic-1");
        assert_eq!(value["revision"], 3);
        assert_eq!(value["phase"], "terminal");
        assert_eq!(value["record"]["record_id"], "synthetic-1");
    }

    #[test]
    fn round_trips_all_application_event_shapes() {
        let events = [
            InspectorStreamEvent::Snapshot {
                stream_seq: 10,
                records: vec![InspectorVersionedRecord {
                    record_id: "synthetic-1".to_owned(),
                    revision: 1,
                    record: test_record("synthetic-1"),
                }],
            },
            InspectorStreamEvent::RecordUpsert {
                stream_seq: 11,
                record_id: "synthetic-1".to_owned(),
                revision: 2,
                phase: InspectorRecordPhase::Live,
                record: Box::new(test_record("synthetic-1")),
            },
            InspectorStreamEvent::RecordRemoved {
                stream_seq: 12,
                record_id: "synthetic-1".to_owned(),
                revision: 3,
                reason: InspectorRemovalReason::RetentionEvicted,
            },
            InspectorStreamEvent::Reset {
                stream_seq: 13,
                reason: InspectorResetReason::ResumeUnavailable,
            },
        ];

        for event in events {
            let encoded = serde_json::to_string(&event).expect("event serializes");
            let decoded: InspectorStreamEvent =
                serde_json::from_str(&encoded).expect("event deserializes");
            assert_eq!(
                serde_json::to_value(decoded).unwrap(),
                serde_json::to_value(event).unwrap()
            );
        }
    }

    #[test]
    fn uses_stable_machine_readable_names() {
        let examples = [
            (
                InspectorStreamEvent::Reset {
                    stream_seq: 1,
                    reason: InspectorResetReason::ServerRestarted,
                },
                json!({
                    "kind": "reset",
                    "stream_seq": 1,
                    "reason": "server_restarted"
                }),
            ),
            (
                InspectorStreamEvent::RecordRemoved {
                    stream_seq: 2,
                    record_id: "synthetic-1".to_owned(),
                    revision: 4,
                    reason: InspectorRemovalReason::Explicit,
                },
                json!({
                    "kind": "record_removed",
                    "stream_seq": 2,
                    "record_id": "synthetic-1",
                    "revision": 4,
                    "reason": "explicit"
                }),
            ),
        ];

        for (event, expected) in examples {
            assert_eq!(serde_json::to_value(event).unwrap(), expected);
        }
    }
}
