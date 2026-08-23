import { CLIENT_LIMIT } from "./store";
import type { StreamEvent, VersionedRecord } from "./types";

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
