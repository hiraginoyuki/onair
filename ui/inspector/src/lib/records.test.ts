import { describe, expect, it } from "vitest";

import {
  applyEvent,
  freezeRecords,
  isSelectionResponseCurrent,
  markSelectionDetached,
  readySelection,
  reconcileSelection,
  saturatingIncrement,
  shouldAcceptEventSequence,
  shouldFlushCoalesced
} from "./records";
import { versionedRecord } from "./test-fixtures";
import type { SelectionState, StreamEvent, VersionedRecord } from "./types";

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

  it("evicts the oldest map entry while one selected item remains pinned", () => {
    const selected = versionedRecord("one", 1);
    const records = new Map<string, VersionedRecord>();
    applyEvent(records, upsert(selected, 1), 2);
    applyEvent(records, upsert(versionedRecord("two", 1), 2), 2);
    applyEvent(records, upsert(versionedRecord("three", 1), 3), 2);

    expect([...records.keys()]).toEqual(["two", "three"]);
    const selection = markSelectionDetached(readySelection(selected, 1, records), records);
    expect(selection).toMatchObject({ kind: "ready", item: selected, detached: true, epoch: 1 });
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

describe("versioned selection", () => {
  it("keeps lower/equal HTTP detail idempotent and accepts higher SSE detail", () => {
    const revisionThree = versionedRecord("selected", 3, { status: 203 });
    let displayed = new Map([["selected", revisionThree]]);
    let selection = readySelection(revisionThree, 7, displayed);

    selection = reconcileSelection(
      selection,
      "selected",
      versionedRecord("selected", 2, { status: 202 }),
      7,
      displayed
    );
    expect(selection).toMatchObject({ kind: "ready", item: revisionThree });

    const equal = versionedRecord("selected", 3, { status: 500 });
    selection = reconcileSelection(selection, "selected", equal, 7, displayed);
    expect(selection).toMatchObject({ kind: "ready", item: revisionThree });

    const revisionFour = versionedRecord("selected", 4, { status: 204 });
    displayed = new Map([["selected", revisionFour]]);
    selection = reconcileSelection(selection, "selected", revisionFour, 7, displayed);
    expect(selection).toEqual(readySelection(revisionFour, 7, displayed));
  });

  it("treats a newer projection epoch as authoritative even when revision restarts", () => {
    const old = versionedRecord("selected", 19, { status: 219 });
    let selection = readySelection(old, 4, new Map([["selected", old]]));
    const restarted = versionedRecord("selected", 1, { status: 201 });
    const displayed = new Map([["selected", restarted]]);

    selection = reconcileSelection(selection, "selected", restarted, 5, displayed);
    expect(selection).toEqual(readySelection(restarted, 5, displayed));

    selection = reconcileSelection(selection, "selected", old, 4, displayed);
    expect(selection).toEqual(readySelection(restarted, 5, displayed));
  });

  it("suppresses stale HTTP responses by click token and projection epoch", () => {
    const loading: SelectionState = {
      kind: "loading",
      recordId: "selected",
      requestToken: 8,
      epoch: 3
    };
    expect(isSelectionResponseCurrent(8, 8, 3, 3, "selected", loading, false)).toBe(true);
    expect(isSelectionResponseCurrent(7, 8, 3, 3, "selected", loading, false)).toBe(false);
    expect(isSelectionResponseCurrent(8, 8, 2, 3, "selected", loading, false)).toBe(false);
    expect(isSelectionResponseCurrent(8, 8, 3, 3, "different", loading, false)).toBe(false);
    expect(isSelectionResponseCurrent(8, 8, 3, 3, "selected", loading, true)).toBe(false);
  });
});

describe("frozen display projection", () => {
  it("keeps rows and selected detail frozen through ingestion, reset, and snapshot", () => {
    const initial = versionedRecord("selected", 1, { status: 200 });
    let live = new Map([["selected", initial]]);
    const frozen = freezeRecords(live);
    const frozenSelection = readySelection(initial, 1, frozen);
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
    expect(frozenSelection).toEqual(readySelection(initial, 1, frozen));
    expect(live.get("selected")?.record.status).toBe(201);
  });

  it("publishes the latest live map exactly once on resume and reconciles selection", () => {
    const initial = versionedRecord("selected", 7);
    const live = new Map([["selected", versionedRecord("selected", 1)]]);
    const frozen = freezeRecords(new Map([["selected", initial]]));
    let display: ReadonlyMap<string, VersionedRecord> = frozen;
    let publications = 0;
    const publish = (records: ReadonlyMap<string, VersionedRecord>) => {
      publications += 1;
      display = records;
    };

    publish(live);
    const selection = reconcileSelection(
      readySelection(initial, 2, frozen),
      "selected",
      live.get("selected"),
      3,
      display
    );

    expect(publications).toBe(1);
    expect(display).toBe(live);
    expect(selection).toEqual(readySelection(live.get("selected")!, 3, display));
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
