import type { SelectionState, VersionedRecord } from "./types";

export function selectionRecordId(selection: SelectionState): string {
  if (selection.kind === "none") return "";
  return selection.kind === "ready" ? selection.item.record_id : selection.recordId;
}

export function selectionItem(selection: SelectionState): VersionedRecord | undefined {
  return selection.kind === "ready" ? selection.item : undefined;
}

export function readySelection(
  item: VersionedRecord,
  epoch: number,
  displayed: ReadonlyMap<string, VersionedRecord>
): SelectionState {
  return {
    kind: "ready",
    item,
    detached: displayed.get(item.record_id)?.revision !== item.revision,
    epoch
  };
}

export function reconcileSelection(
  selection: SelectionState,
  selectedId: string,
  candidate: VersionedRecord | undefined,
  candidateEpoch: number,
  displayed: ReadonlyMap<string, VersionedRecord>
): SelectionState {
  if (!selectedId) return { kind: "none" };
  if (!candidate || candidate.record_id !== selectedId) {
    return markSelectionDetached(selection, displayed);
  }

  if (selection.kind === "ready" && selection.item.record_id === selectedId) {
    if (candidateEpoch < selection.epoch) return markSelectionDetached(selection, displayed);
    if (candidateEpoch === selection.epoch && candidate.revision <= selection.item.revision) {
      return markSelectionDetached(selection, displayed);
    }
  } else if (
    selection.kind !== "none" &&
    selectionRecordId(selection) === selectedId &&
    candidateEpoch < selection.epoch
  ) {
    return selection;
  }

  return readySelection(candidate, candidateEpoch, displayed);
}

export function markSelectionDetached(
  selection: SelectionState,
  displayed: ReadonlyMap<string, VersionedRecord>
): SelectionState {
  if (selection.kind !== "ready") return selection;
  const detached = displayed.get(selection.item.record_id)?.revision !== selection.item.revision;
  return detached === selection.detached ? selection : { ...selection, detached };
}

export function isSelectionResponseCurrent(
  requestToken: number,
  currentToken: number,
  requestEpoch: number,
  currentEpoch: number,
  requestedId: string,
  selection: SelectionState
): boolean {
  return (
    requestToken === currentToken &&
    requestEpoch === currentEpoch &&
    requestedId === selectionRecordId(selection)
  );
}
