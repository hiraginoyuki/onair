import type {
  InspectorAttempt,
  InspectorRecord,
  StreamEvent,
  VersionedRecord
} from "./types";

const EVENT_KINDS = ["snapshot", "record_upsert", "record_removed", "reset"] as const;
const RECORD_PHASES = ["initial", "live", "terminal"] as const;
const REMOVAL_REASONS = ["retention_evicted", "explicit"] as const;
const RESET_REASONS = ["resume_unavailable", "lagged", "server_restarted"] as const;

export type DecodeResult =
  | { ok: true; event: StreamEvent }
  | { ok: false; reason: "invalid_json" | "invalid_shape" };

export function decodeStreamEvent(input: string | unknown): DecodeResult {
  let parsed: unknown = input;
  if (typeof input === "string") {
    try {
      parsed = JSON.parse(input) as unknown;
    } catch {
      return { ok: false, reason: "invalid_json" };
    }
  }

  const normalized = normalizeStreamEvent(parsed);
  if (!isObject(normalized)) return { ok: false, reason: "invalid_shape" };
  if (
    typeof normalized.kind !== "string" ||
    !EVENT_KINDS.includes(normalized.kind as (typeof EVENT_KINDS)[number]) ||
    !isSafeInteger(normalized.stream_seq)
  ) {
    return { ok: false, reason: "invalid_shape" };
  }

  if (normalized.kind === "snapshot") {
    if (!Array.isArray(normalized.records) || !normalized.records.every(isVersionedRecord)) {
      return { ok: false, reason: "invalid_shape" };
    }
    return { ok: true, event: normalized as StreamEvent };
  }

  if (normalized.kind === "record_upsert") {
    if (
      !isRecordId(normalized.record_id) ||
      !isRevision(normalized.revision) ||
      !RECORD_PHASES.includes(normalized.phase as (typeof RECORD_PHASES)[number]) ||
      !isInspectorRecord(normalized.record) ||
      normalized.record.record_id !== normalized.record_id
    ) {
      return { ok: false, reason: "invalid_shape" };
    }
    return { ok: true, event: normalized as StreamEvent };
  }

  if (normalized.kind === "record_removed") {
    if (
      !isRecordId(normalized.record_id) ||
      !isRevision(normalized.revision) ||
      !REMOVAL_REASONS.includes(normalized.reason as (typeof REMOVAL_REASONS)[number])
    ) {
      return { ok: false, reason: "invalid_shape" };
    }
    return { ok: true, event: normalized as StreamEvent };
  }

  if (!RESET_REASONS.includes(normalized.reason as (typeof RESET_REASONS)[number])) {
    return { ok: false, reason: "invalid_shape" };
  }
  return { ok: true, event: normalized as StreamEvent };
}

export function decodeVersionedRecord(input: unknown): VersionedRecord | undefined {
  if (!isObject(input)) return undefined;
  const normalized = {
    ...input,
    record: normalizeInspectorRecord(input.record)
  };
  return isVersionedRecord(normalized) ? normalized : undefined;
}

function normalizeStreamEvent(event: unknown): unknown {
  if (!isObject(event)) return event;
  if (event.kind === "snapshot" && Array.isArray(event.records)) {
    return {
      ...event,
      records: event.records.map((entry) => {
        if (!isObject(entry)) return entry;
        return { ...entry, record: normalizeInspectorRecord(entry.record) };
      })
    };
  }
  if (event.kind === "record_upsert") {
    return { ...event, record: normalizeInspectorRecord(event.record) };
  }
  return event;
}

function normalizeInspectorRecord(record: unknown): unknown {
  if (!isObject(record)) return record;
  const timeline = isObject(record.timeline)
    ? Object.fromEntries(
        Object.entries(record.timeline).map(([key, value]) => [key, value ?? undefined])
      )
    : record.timeline;
  return {
    ...record,
    backend_attempts: record.backend_attempts === undefined ? [] : record.backend_attempts,
    retried_attempts: record.retried_attempts === undefined ? [] : record.retried_attempts,
    exposed_backend_error:
      record.exposed_backend_error === undefined ? false : record.exposed_backend_error,
    timeline
  };
}

function isVersionedRecord(value: unknown): value is VersionedRecord {
  if (!isObject(value)) return false;
  return Boolean(
    isRecordId(value.record_id) &&
      isRevision(value.revision) &&
      isInspectorRecord(value.record) &&
      value.record.record_id === value.record_id
  );
}

function isInspectorRecord(record: unknown): record is InspectorRecord {
  if (!isObject(record)) return false;
  const timeline = record.timeline;
  const outcome = record.outcome;
  return Boolean(
    isRecordId(record.record_id) &&
      isSafeInteger(record.started_at_unix_ms) &&
      typeof record.method === "string" &&
      typeof record.path === "string" &&
      typeof record.route === "string" &&
      typeof record.identity === "string" &&
      typeof record.requested_model === "string" &&
      typeof record.public_model === "string" &&
      typeof record.backend_model === "string" &&
      typeof record.backend === "string" &&
      typeof record.backend_target === "string" &&
      typeof record.stream === "boolean" &&
      typeof record.peer_addr === "string" &&
      typeof record.effective_client_addr === "string" &&
      typeof record.trusted_proxy_addr === "string" &&
      typeof record.forwarded_for === "string" &&
      typeof record.user_agent === "string" &&
      isSafeInteger(record.request_body_bytes) &&
      typeof record.exposed_backend_error === "boolean" &&
      isSafeInteger(record.status) &&
      Array.isArray(record.backend_attempts) &&
      record.backend_attempts.every(isInspectorAttempt) &&
      Array.isArray(record.retried_attempts) &&
      record.retried_attempts.every(isInspectorAttempt) &&
      isSafeInteger(record.input_tokens) &&
      isSafeInteger(record.cached_input_tokens) &&
      isSafeInteger(record.output_tokens) &&
      isSafeInteger(record.completed_at_unix_ms) &&
      isOptionalString(record.client_request_id) &&
      isOptionalString(record.query) &&
      isOptionalString(record.backend_remote_addr) &&
      isOptionalString(record.debug_capture_id) &&
      isOptionalString(record.error_kind) &&
      isOptionalSafeInteger(record.response_body_bytes) &&
      isObject(timeline) &&
      isSafeInteger(timeline.started_unix_ms) &&
      isSafeInteger(timeline.total_us) &&
      isSafeInteger(timeline.proxy_entry_us) &&
      isOptionalSafeInteger(timeline.auth_done_us) &&
      isOptionalSafeInteger(timeline.request_inspected_us) &&
      isOptionalSafeInteger(timeline.route_selected_us) &&
      isOptionalSafeInteger(timeline.request_rewritten_us) &&
      isOptionalSafeInteger(timeline.debug_capture_done_us) &&
      isOptionalSafeInteger(timeline.backend_forward_start_us) &&
      isOptionalSafeInteger(timeline.backend_headers_received_us) &&
      isOptionalSafeInteger(timeline.backend_body_first_chunk_us) &&
      isOptionalSafeInteger(timeline.backend_body_complete_us) &&
      isOptionalSafeInteger(timeline.response_rewritten_us) &&
      isOptionalSafeInteger(timeline.client_response_ready_us) &&
      isOptionalSafeInteger(timeline.stream_complete_us) &&
      isObject(outcome) &&
      typeof outcome.kind === "string" &&
      isOptionalString(outcome.stage)
  );
}

function isInspectorAttempt(attempt: unknown): attempt is InspectorAttempt {
  if (!isObject(attempt)) return false;
  return Boolean(
    isSafeInteger(attempt.attempt) &&
      typeof attempt.backend === "string" &&
      typeof attempt.backend_target === "string" &&
      isSafeInteger(attempt.status) &&
      typeof attempt.outcome === "string" &&
      isSafeInteger(attempt.started_us) &&
      isSafeInteger(attempt.ended_us) &&
      isSafeInteger(attempt.elapsed_us) &&
      isSafeInteger(attempt.elapsed_ms) &&
      isOptionalString(attempt.backend_remote_addr) &&
      isOptionalString(attempt.debug_capture_id) &&
      isOptionalString(attempt.error_kind) &&
      isOptionalSafeInteger(attempt.upstream_status) &&
      isOptionalSafeInteger(attempt.request_rewritten_us) &&
      isOptionalSafeInteger(attempt.debug_capture_done_us) &&
      isOptionalSafeInteger(attempt.backend_forward_start_us) &&
      isOptionalSafeInteger(attempt.backend_headers_received_us) &&
      isOptionalSafeInteger(attempt.backend_body_first_chunk_us) &&
      isOptionalSafeInteger(attempt.backend_body_complete_us) &&
      isOptionalSafeInteger(attempt.stream_complete_us)
  );
}

function isObject(value: unknown): value is Record<string, unknown> {
  return Boolean(value && typeof value === "object" && !Array.isArray(value));
}

function isSafeInteger(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

function isRevision(value: unknown): value is number {
  return isSafeInteger(value) && value > 0;
}

function isRecordId(value: unknown): value is string {
  return typeof value === "string" && value.length > 0;
}

function isOptionalString(value: unknown): boolean {
  return value === undefined || typeof value === "string";
}

function isOptionalSafeInteger(value: unknown): boolean {
  return value === undefined || isSafeInteger(value);
}
