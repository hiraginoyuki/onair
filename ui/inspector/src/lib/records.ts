import { CLIENT_LIMIT } from "./store";
import type { SelectionState, StreamEvent, VersionedRecord } from "./types";

export type RecordMap = Map<string, VersionedRecord>;

export type RecordTransition = {
  accepted: boolean;
  changed: boolean;
  reset: boolean;
};

export function eventSequence(event: StreamEvent): number {
  return event.stream_seq;
}

export function shouldAcceptEventSequence(
  lastSequence: number,
  streamReady: boolean,
  event: StreamEvent
): boolean {
  if (event.kind === "reset") {
    // A restarted server can legitimately reset its in-memory sequence. The
    // reset is the boundary that makes the smaller domain authoritative.
    return streamReady || event.stream_seq !== lastSequence;
  }
  if (event.kind === "snapshot") return !streamReady || event.stream_seq >= lastSequence;
  return event.stream_seq > lastSequence;
}

/** Applies one authoritative/replayed event to the bounded canonical map. */
export function applyEvent(
  records: RecordMap,
  event: StreamEvent,
  limit = CLIENT_LIMIT
): RecordTransition {
  const boundedLimit = Math.max(1, limit);
  if (event.kind === "snapshot") {
    const next = new Map<string, VersionedRecord>();
    for (const entry of event.records.slice(-boundedLimit)) next.set(entry.record_id, entry);
    const changed = !sameRevisions(records, next);
    records.clear();
    for (const [recordId, entry] of next) records.set(recordId, entry);
    return { accepted: true, changed, reset: true };
  }

  if (event.kind === "reset") {
    const changed = records.size > 0;
    records.clear();
    return { accepted: true, changed, reset: true };
  }

  if (event.kind === "record_removed") {
    const current = records.get(event.record_id);
    if (!current || event.revision <= current.revision) {
      return { accepted: false, changed: false, reset: false };
    }
    records.delete(event.record_id);
    return { accepted: true, changed: true, reset: false };
  }

  const current = records.get(event.record_id);
  if (current && event.revision <= current.revision) {
    return { accepted: false, changed: false, reset: false };
  }
  records.set(event.record_id, {
    record_id: event.record_id,
    revision: event.revision,
    record: event.record
  });
  while (records.size > boundedLimit) {
    const oldest = records.keys().next().value;
    if (oldest === undefined) break;
    records.delete(oldest);
  }
  return { accepted: true, changed: true, reset: false };
}

export function freezeRecords(
  records: ReadonlyMap<string, VersionedRecord>
): ReadonlyMap<string, VersionedRecord> {
  return new Map(records);
}

export function saturatingIncrement(
  value: number,
  maximum = Number.MAX_SAFE_INTEGER
): number {
  return value >= maximum ? maximum : value + 1;
}

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

export function shouldFlushCoalesced(
  queued: ReadonlyMap<string, unknown>,
  recordId: string,
  limit = CLIENT_LIMIT
): boolean {
  return !queued.has(recordId) && queued.size >= Math.max(1, limit);
}

function sameRevisions(
  left: ReadonlyMap<string, VersionedRecord>,
  right: ReadonlyMap<string, VersionedRecord>
): boolean {
  if (left.size !== right.size) return false;
  const leftEntries = left.entries();
  const rightEntries = right.entries();
  while (true) {
    const leftEntry = leftEntries.next();
    const rightEntry = rightEntries.next();
    if (leftEntry.done || rightEntry.done) return leftEntry.done === rightEntry.done;
    if (
      leftEntry.value[0] !== rightEntry.value[0] ||
      leftEntry.value[1].revision !== rightEntry.value[1].revision
    ) {
      return false;
    }
  }
}
