import type { SelectionState, VersionedRecord } from "./types";
import { decodeVersionedRecord } from "./wire";

export type FetchLike = (
  input: RequestInfo | URL,
  init?: RequestInit
) => Promise<Response>;

export function selectionRecordId(selection: SelectionState): string {
  if (selection.kind === "none") return "";
  return selection.kind === "ready" ? selection.item.record_id : selection.recordId;
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

/**
 * Reconciles one authoritative candidate without comparing revisions across
 * projection epochs. Equal revisions are deliberately idempotent.
 */
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
  selection: SelectionState,
  paused: boolean
): boolean {
  return (
    !paused &&
    requestToken === currentToken &&
    requestEpoch === currentEpoch &&
    requestedId === selectionRecordId(selection)
  );
}

export async function fetchVersionedRecord(
  recordId: string,
  fetcher: FetchLike = fetch
): Promise<VersionedRecord> {
  const response = await fetcher(
    `/_onair/inspector-next/requests/${encodeURIComponent(recordId)}`,
    { cache: "no-store" }
  );
  if (!response.ok) throw new Error(`record ${recordId} is not retained`);
  const fetched = decodeVersionedRecord(await response.json());
  if (!fetched || fetched.record_id !== recordId) {
    throw new Error(`record ${recordId} has an invalid shape`);
  }
  return fetched;
}
