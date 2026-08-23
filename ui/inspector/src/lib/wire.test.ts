import { describe, expect, it } from "vitest";

import wireContractCorpus from "./fixtures/inspector-wire-contract.json";
import type { StreamEvent } from "./types";
import { decodeStreamEvent, decodeVersionedRecord } from "./wire";

function decodeOk(input: string | unknown): StreamEvent {
  const result = decodeStreamEvent(input);
  expect(result.ok).toBe(true);
  if (!result.ok) throw new Error(`decode failed: ${result.reason}`);
  return result.event;
}

describe("Rust wire contract corpus", () => {
  it("decodes every producer event shape", () => {
    expect(wireContractCorpus.valid_events.map(({ label }) => label)).toEqual([
      "snapshot_empty",
      "snapshot_records",
      "upsert_initial",
      "upsert_live",
      "upsert_terminal",
      "remove_retention_evicted",
      "remove_explicit",
      "reset_resume_unavailable",
      "reset_lagged",
      "reset_server_restarted"
    ]);

    for (const { label, event } of wireContractCorpus.valid_events) {
      const result = decodeStreamEvent(event);
      expect(result, label).toMatchObject({ ok: true });
    }
  });

  it("asserts raw Rust omissions before applying frontend defaults", () => {
    const raw = wireContractCorpus.records.ordinary as Record<string, unknown>;
    expect(Object.hasOwn(raw, "backend_attempts")).toBe(false);
    expect(Object.hasOwn(raw, "retried_attempts")).toBe(false);
    expect(Object.hasOwn(raw, "exposed_backend_error")).toBe(false);
    expect(Object.hasOwn(raw, "client_request_id")).toBe(false);
    expect((raw.timeline as Record<string, unknown>).auth_done_us).toBeNull();

    const snapshotFixture = wireContractCorpus.valid_events.find(
      ({ label }) => label === "snapshot_records"
    );
    if (!snapshotFixture) throw new Error("Rust snapshot fixture is missing");
    const snapshot = decodeOk(snapshotFixture.event);
    if (snapshot.kind !== "snapshot") throw new Error("snapshot did not decode");
    const ordinary = snapshot.records.find(({ record_id }) => record_id === "ordinary-record");
    expect(ordinary?.record.backend_attempts).toEqual([]);
    expect(ordinary?.record.retried_attempts).toEqual([]);
    expect(ordinary?.record.exposed_backend_error).toBe(false);
    expect(ordinary?.record.timeline.auth_done_us).toBeUndefined();
  });

  it("decodes the producer's versioned detail with every optional field and an attempt", () => {
    const detail = decodeVersionedRecord(wireContractCorpus.versioned_detail);
    expect(detail?.record_id).toBe("all-optionals-record");
    expect(detail?.revision).toBe(7);
    expect(detail?.record.client_request_id).toBe("fixture-client-request");
    expect(detail?.record.query).toBe("mode=fixture");
    expect(detail?.record.backend_remote_addr).toBe("not-recorded");
    expect(detail?.record.debug_capture_id).toBe("fixture-capture");
    expect(detail?.record.error_kind).toBe("fixture_error");
    expect(detail?.record.response_body_bytes).toBe(456);
    expect(detail?.record.exposed_backend_error).toBe(true);
    expect(detail?.record.backend_attempts).toHaveLength(1);
    expect(detail?.record.backend_attempts[0]).toMatchObject({
      attempt: 1,
      backend_remote_addr: "not-recorded",
      debug_capture_id: "fixture-attempt-capture",
      error_kind: "fixture_attempt_error",
      upstream_status: 502,
      stream_complete_us: 950
    });
    expect(detail?.record.retried_attempts).toEqual([]);
    expect(detail?.record.timeline).toMatchObject({
      auth_done_us: 100,
      request_inspected_us: 200,
      route_selected_us: 300,
      request_rewritten_us: 400,
      debug_capture_done_us: 500,
      backend_forward_start_us: 600,
      backend_headers_received_us: 700,
      backend_body_first_chunk_us: 800,
      backend_body_complete_us: 850,
      response_rewritten_us: 875,
      client_response_ready_us: 900,
      stream_complete_us: 950
    });
  });

  it.each(wireContractCorpus.malformed)("rejects $label", ({ value }) => {
    expect(decodeStreamEvent(value)).toEqual({ ok: false, reason: "invalid_shape" });
  });
});

describe("wire decoder transport boundary", () => {
  it("distinguishes invalid JSON from an invalid decoded shape", () => {
    expect(decodeStreamEvent("not-json")).toEqual({ ok: false, reason: "invalid_json" });
    expect(decodeStreamEvent("null")).toEqual({ ok: false, reason: "invalid_shape" });
  });

  it("never decodes transport comments or removed application keepalives", () => {
    expect(decodeStreamEvent(": keepalive\n\n")).toEqual({
      ok: false,
      reason: "invalid_json"
    });
    expect(decodeStreamEvent({ kind: "keepalive", stream_seq: 1 })).toEqual({
      ok: false,
      reason: "invalid_shape"
    });
  });
});
