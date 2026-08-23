import type { InspectorRecord, VersionedRecord } from "./types";

export function syntheticRecord(
  recordId: string,
  overrides: Partial<InspectorRecord> = {}
): InspectorRecord {
  return {
    record_id: recordId,
    started_at_unix_ms: 1_700_000_000_000,
    method: "POST",
    path: "/synthetic",
    route: "synthetic-route",
    identity: "synthetic-operator",
    requested_model: "synthetic-model",
    public_model: "synthetic-model",
    backend_model: "synthetic-backend-model",
    backend: "synthetic-backend",
    backend_target: "synthetic-target",
    stream: false,
    peer_addr: "not-recorded",
    effective_client_addr: "not-recorded",
    trusted_proxy_addr: "not-recorded",
    forwarded_for: "not-recorded",
    user_agent: "synthetic-client",
    request_body_bytes: 0,
    exposed_backend_error: false,
    outcome: { kind: "completed" },
    status: 200,
    backend_attempts: [],
    retried_attempts: [],
    input_tokens: 0,
    cached_input_tokens: 0,
    output_tokens: 0,
    completed_at_unix_ms: 1_700_000_000_001,
    timeline: {
      started_unix_ms: 1_700_000_000_000,
      total_us: 1_000,
      proxy_entry_us: 0
    },
    ...overrides
  };
}
export function versionedRecord(
  recordId: string,
  revision: number,
  overrides: Partial<InspectorRecord> = {}
): VersionedRecord {
  return {
    record_id: recordId,
    revision,
    record: syntheticRecord(recordId, overrides)
  };
}
