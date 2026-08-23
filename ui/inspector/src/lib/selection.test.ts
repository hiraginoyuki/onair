import { describe, expect, it, vi } from "vitest";

import { versionedRecord } from "./test-fixtures";
import type { SelectionState, VersionedRecord } from "./types";
import {
  fetchVersionedRecord,
  isSelectionResponseCurrent,
  markSelectionDetached,
  readySelection,
  reconcileSelection
} from "./selection";

describe("versioned selection", () => {
  it("marks a selected record detached after bounded-table eviction", () => {
    const selected = versionedRecord("one", 1);
    const displayed = new Map<string, VersionedRecord>([
      ["two", versionedRecord("two", 1)],
      ["three", versionedRecord("three", 1)]
    ]);

    const selection = markSelectionDetached(readySelection(selected, 1, displayed), displayed);
    expect(selection).toMatchObject({
      kind: "ready",
      item: selected,
      detached: true,
      epoch: 1
    });
  });

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

  it("reconciles a pinned selection when a frozen projection resumes", () => {
    const initial = versionedRecord("selected", 7);
    const live = new Map([["selected", versionedRecord("selected", 1)]]);
    const selection = reconcileSelection(
      readySelection(initial, 2, new Map([["selected", initial]])),
      "selected",
      live.get("selected"),
      3,
      live
    );

    expect(selection).toEqual(readySelection(live.get("selected")!, 3, live));
  });
});

describe("versioned detail lookup", () => {
  it("uses the versioned route and decoder", async () => {
    const item = versionedRecord("detail/id", 7);
    const fetcher = vi.fn(async () =>
      new Response(JSON.stringify(item), {
        status: 200,
        headers: { "content-type": "application/json" }
      })
    );

    await expect(fetchVersionedRecord("detail/id", fetcher)).resolves.toEqual(item);
    expect(fetcher).toHaveBeenCalledWith(
      "/_onair/inspector-next/requests/detail%2Fid",
      { cache: "no-store" }
    );
  });

  it("rejects unavailable, malformed, and mismatched detail responses", async () => {
    const unavailable = vi.fn(async () => new Response("", { status: 404 }));
    await expect(fetchVersionedRecord("missing", unavailable)).rejects.toThrow(
      "record missing is not retained"
    );

    const malformed = vi.fn(async () =>
      new Response(JSON.stringify({ record_id: "invalid", revision: 0, record: {} }), {
        status: 200
      })
    );
    await expect(fetchVersionedRecord("invalid", malformed)).rejects.toThrow(
      "record invalid has an invalid shape"
    );

    const mismatched = vi.fn(async () =>
      new Response(JSON.stringify(versionedRecord("other", 1)), { status: 200 })
    );
    await expect(fetchVersionedRecord("requested", mismatched)).rejects.toThrow(
      "record requested has an invalid shape"
    );
  });
});
