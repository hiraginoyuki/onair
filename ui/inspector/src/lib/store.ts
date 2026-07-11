import type { Column, ColumnKey, InspectorRecord, StreamEvent } from "./types";

export const CLIENT_LIMIT = 1000;
export const PENDING_LIMIT = 256;
export const ROW_HEIGHT = 34;
export const columns: Column[] = [
  { key: "time", label: "time", defaultWidth: 156, minWidth: 140, maxWidth: 220, align: "left" },
  { key: "status", label: "status", defaultWidth: 68, minWidth: 52, maxWidth: 96, align: "right" },
  { key: "total", label: "total ms", defaultWidth: 104, minWidth: 88, maxWidth: 140, align: "right" },
  { key: "route", label: "route", defaultWidth: 116, minWidth: 84, maxWidth: 220, align: "left" },
  { key: "model", label: "model", defaultWidth: 180, minWidth: 120, maxWidth: 320, align: "left" },
  { key: "backend", label: "backend", defaultWidth: 132, minWidth: 100, maxWidth: 260, align: "left" },
  { key: "outcome", label: "outcome", defaultWidth: 168, minWidth: 120, maxWidth: 280, align: "left" }
];

export function eventSequence(event: StreamEvent): number {
  return event.stream_seq;
}

export function applyEvent(
  records: Map<string, { revision: number; record: InspectorRecord }>,
  event: StreamEvent,
  paused: boolean,
  pending: StreamEvent[]
): { reset: boolean; droppedPending: boolean } {
  if (paused && event.kind === "record_upsert") {
    const droppedPending = pending.length >= PENDING_LIMIT;
    if (droppedPending) pending.shift();
    pending.push(event);
    return { reset: false, droppedPending };
  }
  if (event.kind === "snapshot") {
    records.clear();
    for (const entry of event.records.slice(-CLIENT_LIMIT)) records.set(entry.record_id, entry);
    return { reset: true, droppedPending: false };
  }
  if (event.kind === "reset") {
    records.clear();
    return { reset: true, droppedPending: false };
  }
  if (event.kind === "record_removed") {
    records.delete(event.record_id);
    return { reset: false, droppedPending: false };
  }
  if (event.kind === "record_upsert") {
    const current = records.get(event.record_id);
    if (!current || event.revision >= current.revision) {
      records.set(event.record_id, { revision: event.revision, record: event.record });
      while (records.size > CLIENT_LIMIT) records.delete(records.keys().next().value as string);
    }
  }
  return { reset: false, droppedPending: false };
}

export function columnWidth(key: ColumnKey, widths: Partial<globalThis.Record<ColumnKey, number>>): number {
  const column = columns.find((candidate) => candidate.key === key)!;
  return widths[key] ?? column.defaultWidth;
}
