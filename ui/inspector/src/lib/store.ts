import type { Column, ColumnKey, InspectorRecord, StreamEvent } from "./types";

export const CLIENT_LIMIT = 1000;
export const PENDING_LIMIT = 256;
export const ROW_HEIGHT = 40;

export const columns: Column[] = [
  { key: "time", label: "time", defaultWidth: 164, minWidth: 148, maxWidth: 240, align: "left" },
  { key: "status", label: "status", defaultWidth: 40, minWidth: 40, maxWidth: 104, align: "right" },
  { key: "total", label: "total ms", defaultWidth: 108, minWidth: 88, maxWidth: 148, align: "right" },
  { key: "route", label: "route", defaultWidth: 128, minWidth: 88, maxWidth: 240, align: "left" },
  { key: "model", label: "model", defaultWidth: 188, minWidth: 132, maxWidth: 340, align: "left" },
  { key: "backend", label: "backend", defaultWidth: 148, minWidth: 112, maxWidth: 280, align: "left" },
  { key: "outcome", label: "outcome", defaultWidth: 180, minWidth: 132, maxWidth: 300, align: "left" },
  { key: "exposed", label: "exposed", defaultWidth: 80, minWidth: 80, maxWidth: 108, align: "right" }
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
  if (paused && (event.kind === "record_upsert" || event.kind === "record_removed")) {
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
    const current = records.get(event.record_id);
    if (current && event.revision > current.revision) records.delete(event.record_id);
    return { reset: false, droppedPending: false };
  }
  if (event.kind === "record_upsert") {
    const current = records.get(event.record_id);
    if (!current || event.revision > current.revision) {
      records.set(event.record_id, { revision: event.revision, record: event.record });
      while (records.size > CLIENT_LIMIT) records.delete(records.keys().next().value as string);
    }
  }
  return { reset: false, droppedPending: false };
}

export function columnWidth(key: ColumnKey, widths: Partial<Record<ColumnKey, number>>): number {
  const column = columns.find((candidate) => candidate.key === key)!;
  const width = widths[key];
  return width && Number.isFinite(width)
    ? Math.max(column.minWidth, Math.min(column.maxWidth, width))
    : column.defaultWidth;
}
