export type Outcome = { kind: string; stage?: string };

export type Timeline = {
  started_unix_ms: number;
  total_us: number;
  proxy_entry_us: number;
  auth_done_us?: number;
  request_inspected_us?: number;
  route_selected_us?: number;
  request_rewritten_us?: number;
  debug_capture_done_us?: number;
  backend_forward_start_us?: number;
  backend_headers_received_us?: number;
  backend_body_first_chunk_us?: number;
  backend_body_complete_us?: number;
  response_rewritten_us?: number;
  client_response_ready_us?: number;
  stream_complete_us?: number;
};

export type InspectorAttempt = {
  attempt: number;
  backend: string;
  backend_target: string;
  backend_remote_addr?: string;
  debug_capture_id?: string;
  status: number;
  outcome: string;
  error_kind?: string;
  started_us: number;
  ended_us: number;
  elapsed_us: number;
  elapsed_ms: number;
  upstream_status?: number;
  request_rewritten_us?: number;
  debug_capture_done_us?: number;
  backend_forward_start_us?: number;
  backend_headers_received_us?: number;
  backend_body_first_chunk_us?: number;
  backend_body_complete_us?: number;
  stream_complete_us?: number;
};

export type InspectorRecord = {
  record_id: string;
  client_request_id?: string;
  started_at_unix_ms: number;
  method: string;
  path: string;
  query?: string;
  route: string;
  identity: string;
  requested_model: string;
  public_model: string;
  backend_model: string;
  backend: string;
  backend_target: string;
  backend_remote_addr?: string;
  stream: boolean;
  peer_addr: string;
  effective_client_addr: string;
  trusted_proxy_addr: string;
  forwarded_for: string;
  user_agent: string;
  request_body_bytes: number;
  debug_capture_id?: string;
  exposed_backend_error: boolean;
  outcome: Outcome;
  status: number;
  error_kind?: string;
  backend_attempts: InspectorAttempt[];
  retried_attempts: InspectorAttempt[];
  response_body_bytes?: number;
  input_tokens: number;
  cached_input_tokens: number;
  output_tokens: number;
  completed_at_unix_ms: number;
  timeline: Timeline;
};

export type StreamEvent =
  | {
      kind: "snapshot";
      stream_seq: number;
      records: { record_id: string; revision: number; record: InspectorRecord }[];
    }
  | {
      kind: "record_upsert";
      stream_seq: number;
      record_id: string;
      revision: number;
      phase: "initial" | "live" | "terminal";
      record: InspectorRecord;
    }
  | {
      kind: "record_removed";
      stream_seq: number;
      record_id: string;
      revision: number;
      reason: "retention_evicted" | "explicit";
    }
  | { kind: "reset"; stream_seq: number; reason: "resume_unavailable" | "lagged" | "server_restarted" }
  | { kind: "keepalive"; stream_seq: number };

export type ColumnKey =
  | "time"
  | "status"
  | "total"
  | "route"
  | "model"
  | "backend"
  | "outcome"
  | "exposed";

export type Column = {
  key: ColumnKey;
  label: string;
  defaultWidth: number;
  minWidth: number;
  maxWidth: number;
  align: "left" | "right";
};
