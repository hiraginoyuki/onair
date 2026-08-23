import type { InspectorRecord } from "./types";

const timeFormatter = new Intl.DateTimeFormat(undefined, {
  hour: "numeric",
  minute: "2-digit",
  second: "2-digit",
  fractionalSecondDigits: 3
});

export function formatTime(timestamp: number): string {
  if (!timestamp) return "-";
  const date = new Date(timestamp);
  return Number.isNaN(date.getTime()) ? "-" : timeFormatter.format(date);
}

export function formatDate(timestamp: number): string {
  if (!timestamp) return "-";
  const date = new Date(timestamp);
  return Number.isNaN(date.getTime()) ? "-" : date.toISOString();
}

export function formatMs(microseconds: number | undefined): string {
  if (microseconds === undefined) return "-";
  const milliseconds = microseconds / 1000;
  if (milliseconds < 10) return `${milliseconds.toFixed(2)} ms`;
  if (milliseconds < 100) return `${milliseconds.toFixed(1)} ms`;
  return `${Math.round(milliseconds).toLocaleString()} ms`;
}

export function formatBytes(bytes: number | undefined): string {
  return bytes === undefined ? "-" : `${bytes.toLocaleString()} B`;
}

export function formatOutcome(record: InspectorRecord): string {
  return record.outcome.stage
    ? `${record.outcome.kind}:${record.outcome.stage}`
    : record.outcome.kind;
}

export function statusTone(status: number): string {
  if (status >= 500) return "bad";
  if (status >= 400) return "warn";
  if (status >= 200 && status < 300) return "good";
  return "neutral";
}
