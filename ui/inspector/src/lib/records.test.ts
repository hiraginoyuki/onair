import { describe, expect, it } from "vitest";

import {
  applyEvent,
  freezeRecords,
  saturatingIncrement,
  shouldAcceptEventSequence,
  shouldFlushCoalesced
} from "./records";
import { versionedRecord } from "./test-fixtures";
import type { StreamEvent, VersionedRecord } from "./types";

function snapshot(streamSeq: number, records: VersionedRecord[]): StreamEvent {
  return { kind: "snapshot", stream_seq: streamSeq, records };
}

function upsert(item: VersionedRecord, streamSeq: number): StreamEvent {
  return {
    kind: "record_upsert",
    stream_seq: streamSeq,
    record_id: item.record_id,
    revision: item.revision,
    phase: "live",
    record: item.record
  };
}

describe("canonical record reducer", () => {
  it("accepts a lower reset sequence as a new server revision domain", () => {
    expect(
      shouldAcceptEventSequence(90, true, {
        kind: "reset",
        stream_seq: 1,
        reason: "resume_unavailable"
      })
    ).toBe(true);
    expect(shouldAcceptEventSequence(1, false, snapshot(1, []))).toBe(true);
    expect(shouldAcceptEventSequence(90, true, snapshot(1, []))).toBe(false);
  });

  it("replaces state with empty and oversized snapshots in wire order", () => {
    const records = new Map([["old", versionedRecord("old", 1)]]);
    expect(applyEvent(records, snapshot(0, []))).toEqual({
      accepted: true,
      changed: true,
      reset: true
    });
    expect(records.size).toBe(0);

    const thousand = Array.from({ length: 1_000 }, (_, index) =>
      versionedRecord(`record-${index}`, 1)
    );
    applyEvent(records, snapshot(1, thousand));
    expect(records.size).toBe(1_000);
    expect([...records.keys()].at(0)).toBe("record-0");
    expect([...records.keys()].at(-1)).toBe("record-999");

    applyEvent(records, snapshot(2, [versionedRecord("evicted", 1), ...thousand]));
    expect(records.size).toBe(1_000);
    expect(records.has("evicted")).toBe(false);
    expect([...records.keys()].at(0)).toBe("record-0");
    expect([...records.keys()].at(-1)).toBe("record-999");
  });

  it("treats snapshot order changes as a canonical change", () => {
    const one = versionedRecord("one", 1);
    const two = versionedRecord("two", 1);
    const records = new Map([
      [one.record_id, one],
      [two.record_id, two]
    ]);

    expect(applyEvent(records, snapshot(2, [two, one])).changed).toBe(true);
    expect([...records.keys()]).toEqual(["two", "one"]);
  });

  it("accepts only newer upserts and removals", () => {
    const records = new Map<string, VersionedRecord>();
    const revisionTwo = versionedRecord("selected", 2, { status: 202 });
    expect(applyEvent(records, upsert(revisionTwo, 1)).accepted).toBe(true);
    const canonicalRevisionTwo = records.get("selected");

    const duplicate = versionedRecord("selected", 2, { status: 500 });
    expect(applyEvent(records, upsert(duplicate, 2))).toEqual({
      accepted: false,
      changed: false,
      reset: false
    });
    expect(records.get("selected")).toBe(canonicalRevisionTwo);

    expect(applyEvent(records, upsert(versionedRecord("selected", 1), 3)).accepted).toBe(false);
    const revisionThree = versionedRecord("selected", 3, { status: 204 });
    expect(applyEvent(records, upsert(revisionThree, 4)).accepted).toBe(true);
    expect(records.get("selected")?.record).toBe(revisionThree.record);

    expect(
      applyEvent(records, {
        kind: "record_removed",
        stream_seq: 5,
        record_id: "selected",
        revision: 3,
        reason: "explicit"
      }).accepted
    ).toBe(false);
    expect(
      applyEvent(records, {
        kind: "record_removed",
        stream_seq: 6,
        record_id: "selected",
        revision: 4,
        reason: "retention_evicted"
      }).accepted
    ).toBe(true);
    expect(records.has("selected")).toBe(false);
  });

  it("ignores a duplicate batched update after its snapshot revision", () => {
    const canonical = versionedRecord("batched", 4, { status: 204 });
    const records = new Map<string, VersionedRecord>();
    applyEvent(records, snapshot(4, [canonical]));
    const before = records.get("batched");

    expect(applyEvent(records, upsert(versionedRecord("batched", 4, { status: 500 }), 4)).accepted).toBe(false);
    expect(records.get("batched")).toBe(before);
  });

  it("evicts the oldest map entry at the client bound", () => {
    const records = new Map<string, VersionedRecord>();
    applyEvent(records, upsert(versionedRecord("one", 1), 1), 2);
    applyEvent(records, upsert(versionedRecord("two", 1), 2), 2);
    applyEvent(records, upsert(versionedRecord("three", 1), 3), 2);

    expect([...records.keys()]).toEqual(["two", "three"]);
  });

  it("clears on reset and accepts the following authoritative snapshot", () => {
    const records = new Map([["before", versionedRecord("before", 2)]]);
    expect(
      applyEvent(records, { kind: "reset", stream_seq: 3, reason: "resume_unavailable" })
    ).toEqual({ accepted: true, changed: true, reset: true });
    expect(records.size).toBe(0);

    applyEvent(records, snapshot(3, [versionedRecord("after", 4)]));
    expect([...records.keys()]).toEqual(["after"]);
  });
});

describe("frozen display projection", () => {
  it("keeps rows frozen through ingestion, reset, and snapshot", () => {
    const initial = versionedRecord("selected", 1, { status: 200 });
    let live = new Map([["selected", initial]]);
    const frozen = freezeRecords(live);
    let pausedUpdateCount = 0;

    for (const event of [
      upsert(versionedRecord("selected", 2, { status: 202 }), 2),
      upsert(versionedRecord("selected", 2, { status: 500 }), 3),
      upsert(versionedRecord("new", 1), 4),
      { kind: "reset", stream_seq: 5, reason: "lagged" } as StreamEvent,
      snapshot(5, [versionedRecord("selected", 1, { status: 201 })])
    ]) {
      const transition = applyEvent(live, event);
      if (transition.accepted) pausedUpdateCount = saturatingIncrement(pausedUpdateCount);
    }
    live = new Map(live);

    expect(pausedUpdateCount).toBe(4);
    expect(frozen.get("selected")?.revision).toBe(1);
    expect(frozen.get("selected")?.record.status).toBe(200);
    expect(live.get("selected")?.record.status).toBe(201);
  });

  it("saturates the paused update counter", () => {
    expect(saturatingIncrement(Number.MAX_SAFE_INTEGER)).toBe(Number.MAX_SAFE_INTEGER);
    expect(saturatingIncrement(4, 5)).toBe(5);
    expect(saturatingIncrement(5, 5)).toBe(5);
  });
});

it("flushes coalesced publications before another distinct ID exceeds the bound", () => {
  const queued = new Map<string, unknown>([
    ["one", true],
    ["two", true]
  ]);
  expect(shouldFlushCoalesced(queued, "one", 2)).toBe(false);
  expect(shouldFlushCoalesced(queued, "three", 2)).toBe(true);
});
