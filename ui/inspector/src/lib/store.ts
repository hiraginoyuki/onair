import type { Column, ColumnKey } from "./types";

export const CLIENT_LIMIT = 1000;
export const ROW_HEIGHT = 40;

export const columns: Column[] = [
  { key: "time", label: "time", defaultWidth: 164, minWidth: 148, maxWidth: 240, align: "left" },
  { key: "status", label: "HTTP", defaultWidth: 40, minWidth: 40, maxWidth: 40, align: "right" },
  { key: "total", label: "total ms", defaultWidth: 108, minWidth: 88, maxWidth: 148, align: "right" },
  { key: "route", label: "route", defaultWidth: 128, minWidth: 88, maxWidth: 240, align: "left" },
  { key: "model", label: "model", defaultWidth: 188, minWidth: 132, maxWidth: 340, align: "left" },
  { key: "backend", label: "backend", defaultWidth: 148, minWidth: 112, maxWidth: 280, align: "left" },
  { key: "outcome", label: "outcome", defaultWidth: 180, minWidth: 132, maxWidth: 300, align: "left" },
  { key: "exposed", label: "exposed", defaultWidth: 64, minWidth: 64, maxWidth: 64, align: "right" }
];

export function columnWidth(key: ColumnKey, widths: Partial<Record<ColumnKey, number>>): number {
  const column = columns.find((candidate) => candidate.key === key)!;
  const width = widths[key];
  return width && Number.isFinite(width)
    ? Math.max(column.minWidth, Math.min(column.maxWidth, width))
    : column.defaultWidth;
}
