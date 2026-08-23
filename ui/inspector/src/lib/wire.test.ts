import { describe, expect, it } from "vitest";

import { syntheticRecord, versionedRecord } from "./test-fixtures";
import type { StreamEvent } from "./types";
import { decodeStreamEvent, decodeVersionedRecord } from "./wire";

function decodeOk(input: string | unknown): StreamEvent {
  const result = decodeStreamEvent(input);
  expect(result.ok).toBe(true);
  if (!result.ok) throw new Error(`decode failed: ${result.reason}`);
  return result.event;
}

describe("wire decoder", () => {
  it("decodes all four application event kinds", () => {
    const item = versionedRecord("synthetic-1", 1);
    const events = [
      { kind: "snapshot", stream_seq: 0, records: [item] },
      {
        kind: "record_upsert",
        stream_seq: 1,
        record_id: item.record_id,
        revision: 2,
        phase: "terminal",
        record: item.record
      },
      {
        kind: "record_removed",
        stream_seq: 2,
        record_id: item.record_id,
        revision: 3,
        reason: "retention_evicted"
      },
      { kind: "reset", stream_seq: 3, reason: "server_restarted" }
    ];

    for (const event of events) expect(decodeOk(JSON.stringify(event))).toEqual(event);
  });

  it("defaults omitted arrays and exposure while normalizing timeline nulls", () => {
    const record = syntheticRecord("defaults");
    const wireRecord: Record<string, unknown> = { ...record };
    delete wireRecord.backend_attempts;
    delete wireRecord.retried_attempts;
    delete wireRecord.exposed_backend_error;
    wireRecord.timeline = {
      ...record.timeline,
      auth_done_us: null,
      backend_headers_received_us: 42
    };

    const decoded = decodeOk({
      kind: "record_upsert",
      stream_seq: 1,
      record_id: "defaults",
      revision: 1,
      phase: "initial",
      record: wireRecord
    });
    expect(decoded.kind).toBe("record_upsert");
    if (decoded.kind !== "record_upsert") throw new Error("upsert did not decode");
    expect(decoded.record.backend_attempts).toEqual([]);
    expect(decoded.record.retried_attempts).toEqual([]);
    expect(decoded.record.exposed_backend_error).toBe(false);
    expect(decoded.record.timeline.auth_done_us).toBeUndefined();
    expect(decoded.record.timeline.backend_headers_received_us).toBe(42);
  });

  it("applies the same defaults to snapshot and versioned detail records", () => {
    const record = syntheticRecord("defaults-everywhere");
    const omitted: Record<string, unknown> = { ...record };
    delete omitted.backend_attempts;
    delete omitted.retried_attempts;
    delete omitted.exposed_backend_error;

    const snapshot = decodeOk({
      kind: "snapshot",
      stream_seq: 1,
      records: [{ record_id: record.record_id, revision: 2, record: omitted }]
    });
    if (snapshot.kind !== "snapshot") throw new Error("snapshot did not decode");
    expect(snapshot.records[0].record.backend_attempts).toEqual([]);

    const detail = decodeVersionedRecord({
      record_id: record.record_id,
      revision: 2,
      record: omitted
    });
    expect(detail?.record.retried_attempts).toEqual([]);
    expect(detail?.record.exposed_backend_error).toBe(false);
  });

  it("decodes the reusable versioned detail shape", () => {
    const decoded = decodeVersionedRecord(versionedRecord("detail", 7));
    expect(decoded?.record_id).toBe("detail");
    expect(decoded?.revision).toBe(7);
    expect(decoded?.record.record_id).toBe("detail");
  });

  it.each([
    ["backend_attempts is not an array", { backend_attempts: {} }],
    ["retried_attempts is not an array", { retried_attempts: "none" }],
    ["exposure is not a boolean", { exposed_backend_error: "false" }],
    ["stream is not a boolean", { stream: 0 }],
    ["method is not a string", { method: 42 }],
    ["status is not an integer", { status: "200" }],
    ["status is fractional", { status: 200.5 }],
    ["timeline integer is negative", { timeline: { ...syntheticRecord("x").timeline, total_us: -1 } }]
  ])("rejects an explicitly invalid record when %s", (_name, override) => {
    const record = { ...syntheticRecord("invalid"), ...override };
    expect(
      decodeStreamEvent({
        kind: "record_upsert",
        stream_seq: 1,
        record_id: "invalid",
        revision: 1,
        phase: "live",
        record
      })
    ).toEqual({ ok: false, reason: "invalid_shape" });
  });

  it.each([
    {
      kind: "record_upsert",
      stream_seq: 1,
      record_id: "different",
      revision: 1,
      phase: "live",
      record: syntheticRecord("valid")
    },
    {
      kind: "record_upsert",
      stream_seq: 1,
      record_id: "valid",
      revision: 0,
      phase: "live",
      record: syntheticRecord("valid")
    },
    {
      kind: "record_upsert",
      stream_seq: 1,
      record_id: "valid",
      revision: 1,
      phase: "unknown",
      record: syntheticRecord("valid")
    },
    {
      kind: "record_removed",
      stream_seq: 2,
      record_id: "",
      revision: 2,
      reason: "explicit"
    },
    {
      kind: "record_removed",
      stream_seq: 2,
      record_id: "valid",
      revision: 2,
      reason: "unknown"
    },
    { kind: "reset", stream_seq: 3, reason: "unknown" },
    { kind: "reset", stream_seq: 1.5, reason: "lagged" },
    { kind: "snapshot", stream_seq: 1, records: "not-an-array" }
  ])("rejects an invalid event envelope", (event) => {
    expect(decodeStreamEvent(event)).toEqual({ ok: false, reason: "invalid_shape" });
  });

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
