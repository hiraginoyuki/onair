export type Outcome = { kind: string; stage?: string };

export type Timeline = {
  total_us?: number;
  started_unix_ms?: number;
};

export type InspectorRecord = {
  record_id: string;
  started_at_unix_ms: number;
  status: number;
  outcome: Outcome;
  route: string;
  identity: string;
  requested_model: string;
  public_model: string;
  backend_model: string;
  backend: string;
  stream: boolean;
  user_agent: string;
  error_kind?: string;
  timeline?: Timeline;
};

export type StreamEvent =
  | { kind: "snapshot"; stream_seq: number; records: { record_id: string; revision: number; record: InspectorRecord }[] }
  | { kind: "record_upsert"; stream_seq: number; record_id: string; revision: number; phase: string; record: InspectorRecord }
  | { kind: "record_removed"; stream_seq: number; record_id: string; revision: number; reason: string }
  | { kind: "reset"; stream_seq: number; reason: string }
  | { kind: "keepalive"; stream_seq: number };

export type ColumnKey = "time" | "status" | "total" | "route" | "model" | "backend" | "outcome";

export type Column = {
  key: ColumnKey;
  label: string;
  defaultWidth: number;
  minWidth: number;
  maxWidth: number;
  align: "left" | "right";
};
