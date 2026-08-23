import { describe, expect, it } from "vitest";

import {
  isSelectionResponseCurrent,
  readySelection,
  reconcileSelection
} from "./records";
import { versionedRecord } from "./test-fixtures";
import type { SelectionState } from "./types";

describe("versioned selection", () => {
  it("keeps lower/equal detail idempotent and accepts a higher revision", () => {
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

    selection = reconcileSelection(
      selection,
      "selected",
      versionedRecord("selected", 3, { status: 500 }),
      7,
      displayed
    );
    expect(selection).toMatchObject({ kind: "ready", item: revisionThree });

    const revisionFour = versionedRecord("selected", 4, { status: 204 });
    displayed = new Map([["selected", revisionFour]]);
    selection = reconcileSelection(selection, "selected", revisionFour, 7, displayed);
    expect(selection).toEqual(readySelection(revisionFour, 7, displayed));
  });

  it("treats a newer projection epoch as authoritative when revisions restart", () => {
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
    expect(isSelectionResponseCurrent(8, 8, 3, 3, "selected", loading)).toBe(true);
    expect(isSelectionResponseCurrent(7, 8, 3, 3, "selected", loading)).toBe(false);
    expect(isSelectionResponseCurrent(8, 8, 2, 3, "selected", loading)).toBe(false);
    expect(isSelectionResponseCurrent(8, 8, 3, 3, "different", loading)).toBe(false);
  });
});
