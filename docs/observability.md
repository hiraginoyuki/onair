# Observability

onair exposes metrics, logs, optional exact-body debug capture, a local
inspector UI, read-only operator endpoints, and optional active backend health
probes.

Prompt and response bodies are not logged by onair. Debug capture is the
intentional exception and is default-off.

## Metrics

Set `[telemetry].exporter = "otlp"` to install an OpenTelemetry meter provider
and export metrics to an OTLP/gRPC collector. The default endpoint comes from
the OpenTelemetry OTLP exporter unless `otlp_endpoint` is set.

Metric instruments:

- `onair.requests`: counter labeled by `route`, `identity`, `model`,
  `backend`, `stream`, and `status_code`.
- `onair.backend.requests`: counter labeled by `route`, `identity`, `model`,
  `backend`, and `stream`.
- `onair.request.duration`: histogram in seconds with the same labels as
  `onair.requests`.
- `onair.stream.duration`: histogram in seconds for streaming response
  lifetime.
- `onair.tokens`: counter labeled by `direction=input|cached_input|output`
  when backend responses include OpenAI-compatible usage data such as
  `prompt_tokens`, `completion_tokens`, `input_tokens`, `output_tokens`, or
  `cached_tokens`.
- Client-facing responses preserve or synthesize `usage.total_tokens` from the
  prompt/input and completion/output counts when possible.
- For streaming Chat Completions-compatible upstreams that require clients to
  opt into terminal usage chunks, set `chat_stream_usage = "insert"` on the
  relevant backend or model route. onair records tokens from the injected
  upstream usage chunks and filters those chunks from client responses that did
  not opt in.
- Native Responses usage is parsed from Responses events/objects instead of
  Chat-style `stream_options.include_usage`.

## Debug Capture

`[debug_capture]` is a default-off troubleshooting path for cases where exact
request bytes are needed. It is not controlled by `RUST_LOG`, and enabling it
writes prompt, request, and selected upstream error-response bodies to disk.

```toml
[debug_capture]
enabled = true
mode = "failures"
directory = "onair-debug-captures"
```

Set `mode = "failures"` to capture only attempts that fail with an upstream
non-success response, upstream send error, timeout, body-read error, or stream
error.

Set `mode = "all"` to capture every successfully routed upstream attempt,
including successful responses. The default mode is `all` for
backward-compatible troubleshooting behavior when debug capture is enabled.

For each captured upstream attempt, onair creates one private capture directory
containing:

- `inbound.body`: the exact body received from the client before model
  rewriting.
- `upstream.body`: the exact body sent to the backend after model rewriting.
- `upstream_error.body`: the upstream non-success response body, only when the
  backend returns a non-success status and a body is available. This
  diagnostic file is capped at 1 MiB and marked as truncated in metadata when
  the cap is reached.
- `metadata.json`: route, identity, public/backend model IDs, path/query
  metadata, debug-capture mode, body sizes, status/outcome, and capture file
  names.

For streamed responses, `metadata.json` also includes a body-free
`stream_usage` summary with the number of observed `usage` objects, the union
of their field names, observed SSE event/type/object names, and the
event/type/object names that carried usage metadata. This is intended for
checking whether an upstream emitted usage metadata without storing SSE
response text, prompt text, completion text, or tool arguments.

When a capture is written, proxy failure/retry logs and inspector records
include `debug_capture_id` so a generic client-facing upstream error can be
correlated with the local capture directory.

Security guidance:

- Enable debug capture only while reproducing a trusted local issue, then
  disable it and delete the directory.
- Captures can include prompts, tool inputs, uploaded file bytes, personal
  data, credentials sent in bodies, and sensitive query parameters.
- The default `onair-debug-captures` directory is ignored by this repository,
  but custom directories are not automatically protected from commits, backups,
  or sharing.
- `debug_capture.directory` may be relative to the current working directory or
  absolute, but it must not contain `..` path components.
- onair logs a `warn` event for every captured request so accidental enablement
  is visible.

See [security.md](security.md) for the privacy boundary around debug capture.

## Streaming Debug Capture

`stream_capture` is an opt-in layer on top of `[debug_capture]` that records
every event on every side of the proxy for a streaming response with per-event
microsecond timestamps. It is the diagnostic for "what did the upstream
actually stream back, and what did onair emit to the client, and when" — the
question that pre-stream `inbound.body` / `upstream.body` snapshots cannot
answer.

```toml
[[backend]]
id = "minimax-io"
base_url = "https://api.minimax.io"
api_key = "..."
supports = ["chat", "responses_via_chat_completions"]

# Per-backend default; routes can override.
stream_capture = false

[[route]]
public = "gpt-5.5"
backends = ["minimax-io"]

# Per-route opt-in for streaming capture.
stream_capture = true

[debug_capture]
enabled = true
mode = "failures"
directory = "onair-debug-captures"
```

Resolution: the route's `stream_capture` (if `Some`) wins; otherwise the bound
backend's value is inherited, matching the `expose_backend_errors` and
`extra_body` precedence rules. Default is `false` for both, matching the
privacy-target posture.

When `stream_capture = true` and the request is streamed, onair writes two
NDJSON files per capture directory in addition to the existing files:

- `<capture-dir>/<request-id>/upstream_response.ndjson` — one JSON object per
  line, recording:
  - `kind: "header"` with `name` (e.g. `:status`, `content-type`) and `value`
  - `kind: "body_chunk"` with `bytes` (raw byte count) and `data` (UTF-8 lossy
    of the chunk; replacement characters for invalid UTF-8)
  - `kind: "done"` once the upstream stream completes
  - `kind: "error"` if a chunk read fails (with `error_kind` from
    `upstream_error_kind`)
- `<capture-dir>/<request-id>/client_response.ndjson` — same shape, but
  recorded after onair's `SseStrategy` normalization so the events reflect
  what onair actually sent to the client. SSE frames are parsed: each
  `kind: "sse"` event carries the original `event:` name and the joined
  `data:` lines. Partial frames at chunk boundaries are buffered and
  emitted on the next chunk.

The hot path uses `try_send` against a bounded `mpsc` channel (256 events
in flight); overflow increments `dropped_events` and sets `truncated = true`
in the per-side summary. The writer task drains with a 500 ms default
budget on completion; a future P4 size-cap policy will add retention.

`metadata.json` adds:

- `files.upstream_response = "upstream_response.ndjson"` (or absent)
- `files.client_response = "client_response.ndjson"` (or absent)
- `timings.upstream_response` and `timings.client_response`, each carrying:
  - `started_at_unix_us`, `first_event_at_unix_us`, `completed_at_unix_us`
  - `total_duration_us`
  - `event_count`, `dropped_events`, `truncated`

Both `timings` fields are absent for non-streaming responses or when
`stream_capture` is disabled, so existing readers that ignore unknown fields
keep working.

Reading the captures during an incident:

```sh
# Find the capture directory for a failing request
ls -lt onair-debug-captures/ | head

# Walk upstream-side events chronologically; ts_us is monotonic per file
jq -c '.' onair-debug-captures/1782449307393-17484-1/upstream_response.ndjson

# Same for the client side
jq -c '.' onair-debug-captures/1782449307393-17484-1/client_response.ndjson

# Inspect timings + truncation flag
jq .metadata.timings onair-debug-captures/1782449307393-17484-1/metadata.json
```

The motivating incident (Codex → onair → `MiniMax-M3`, three captures from
PID 17484 returning upstream 400 `invalid_prompt`) had no NDJSON files
because `stream_capture` was off; with the feature on, the next incident of
that shape records every upstream chunk and every client-emitted SSE event
with timestamps, so the operator can replay the wire without rerunning the
request. See `.local/decisions/2026-06-27-streaming-debug-capture.md` for
the durable rationale.

Not in P1 (deferred):

- `inbound_stream.ndjson` / `outbound_stream.ndjson` for streamed uploads
  (rare today, common tomorrow). See
  `.local/plans/streaming-debug-capture-plan.md` for P2.
- Replay tooling that visualizes the timeline (P3).
- Size-cap and retention policy (P4).

## Inspector

`[inspector]` is a default-off, in-process request inspector for timing and
routing diagnostics. It keeps recent request records in memory and serves a
local Web UI at `/_onair/inspector` when enabled. Optional SQLite persistence
can restore the latest retained records after a process restart.

```toml
[inspector]
enabled = true
retention_requests = 10000
allow_remote = false
# Optional additional effective-client CIDRs, for example a narrow Tailscale range.
allowed_client_cidrs = []

[inspector.persistence]
enabled = false
# path = ".local/inspector.sqlite"
```

Persistence is default-off and uses the same `retention_requests` limit as the
in-memory inspector view. It is intended for restart recovery, not as a
long-lived audit log or historical query store. When enabled, onair stores the
full inspector record JSON plus indexed metadata columns in SQLite; prompt,
completion, and debug-capture bodies are still not included in inspector
records.

Endpoints:

- `GET /_onair/inspector`: primary Svelte inspector UI.
- `GET /_onair/inspector-next`: alias for the exact same Svelte artifact during
  the rollout soak. Both UI responses have the same bytes, local/CIDR access
  gate, Content Security Policy, `Cache-Control: no-store`, and
  `X-Content-Type-Options: nosniff` headers.
- `GET /_onair/inspector-next/events`: versioned server-sent-event transport.
  A new page starts with one authoritative `snapshot` envelope. Subsequent
  events are complete-record `record_upsert` replacements,
  `record_removed` tombstones, or `reset` boundaries. Entries carry positive
  per-record revisions and events carry stream sequences. Browser-managed
  same-`EventSource` reconnects may use `Last-Event-ID`; when replay is not
  available, the stream sends `reset` followed by an authoritative snapshot.
  The 15-second keepalive is an SSE transport comment, not a named application
  event.
  Use `?snapshot_limit=<n>` to cap snapshot records; the UI requests `1000`.
- `GET /_onair/inspector-next/requests/{record_id}`: versioned JSON detail as
  `{ "record_id", "revision", "record" }`. The UI uses this route for deep
  links and for a selected record outside its bounded table window.
- `GET /_onair/inspector/requests`: retained compatibility JSON list, newest
  first. Use `?limit=<n>` to cap the response; onair clamps the limit to
  `1..=10000` and defaults to `1000`.
- `GET /_onair/inspector/requests/{record_id}`: retained compatibility detail
  with the legacy bare-record JSON shape.
- `GET /_onair/inspector/events`: retained compatibility SSE stream with one
  bare record per initial `snapshot` event and bare `request` updates.

The Svelte UI presents a bounded, virtualized live table with fixed 40 px rows
and time, HTTP status, total duration, route, model, backend, outcome, and
exposed-error columns. Columns are sortable; variable-width columns can be
resized by pointer or keyboard, persist their widths locally, and can be reset.
The filter searches retained request metadata. Horizontal scrolling stays
inside the table pane, and the table/detail layout stacks at narrow widths.

Pause freezes the displayed table and selected detail while canonical stream
ingestion continues. Resume publishes the latest canonical projection without
making replay a correctness dependency. Connection, reconnect, recovery,
reset, warning, frozen-view, and detached-selection states are shown explicitly.

Selecting a row updates the URL hash, so a request can be bookmarked locally.
The detail pane shows the selected versioned revision, request/routing/client
metadata, sizes and token usage, timeline bars, expandable backend-attempt
waterfalls, retried-attempt summaries, and record-only raw JSON. Copy and
download actions export only the selected record, not its version wrapper.

Read-only operator endpoints use the same `[inspector]` enablement and
effective-client loopback/CIDR/`allow_remote` gate:

- `GET /_onair/operator/runtime`: process uptime, current time, retained
  inspector record count, route object counts, and telemetry exporter status.
- `GET /_onair/operator/config`: sanitized active config. Client and backend
  API keys are never returned; backends expose only whether an API key is
  configured.
- `GET /_onair/operator/models`: effective public model visibility per client
  plus configured backend routes for each public model.
- `GET /_onair/operator/health`: backend health observed from proxied traffic
  and optional active probes, including split traffic/probe counters,
  consecutive failures, last status, last error kind, last source, and last
  observed latency.

The Svelte inspector polls `/_onair/operator/runtime` for the retained-record
count. The other operator endpoints remain independently available JSON APIs;
they are not embedded in the current table/detail surface.

Each retained record includes route, identity, public/backend model IDs, final
backend ID/target, backend remote socket when available,
immediate/effective client address, trusted proxy details, user agent, body
sizes, response status, OpenAI-compatible usage counters when present, backend
attempt records, retried pre-response attempts when fallback occurred, and a
timeline snapshot.

Timeline fields use a wall-clock `started_at_unix_ms` plus monotonic
microsecond offsets for proxy/auth/routing/rewrite/backend/response
milestones.

`backend_attempts` is the structured source for the UI waterfall. It includes
every upstream attempt, including the final successful or failed attempt.

Each attempt records the selected backend, configured backend target, backend
remote socket when available, client-facing status, upstream status when
headers were received, outcome/error kind, elapsed timing, debug capture ID
when applicable, and monotonic offsets for request rewrite, debug capture,
backend send start, upstream headers, first body chunk, body completion, and
stream completion when those phases occurred.

The waterfall keeps attempt rows compact by default and provides
expand/collapse controls for dense per-attempt metadata and phase timings.
`retried_attempts` is retained as a compatibility/summary field and only
contains attempts that were abandoned before trying a fallback backend.

## Health Probes

Backend health always reflects requests that onair has already routed. If
`[health].active = true`, onair also probes each configured backend at
`[health].path` using the backend API key when configured.

Active health probes are disabled by default because they send HTTP requests
to every configured backend. Use a low-cost path such as `/v1/models`, and
keep in mind that probes can be visible to upstream providers.

onair does not follow backend redirects for normal proxy requests or health
probes; redirect responses are treated as non-success.

Backend health is currently an operator signal; it does not automatically
remove unhealthy backends from routing.

## Context-Size Refresh

For each `[[backend.model]]` whose `context_length` policy is `"upstream"`,
onair issues a background `GET <backend.base_url>/props?model=<backend_model>`
to the owning backend on a 60 s interval and caches the
`default_generation_settings.n_ctx` value. The refresh task runs once
immediately after config load, then sleeps between ticks. Each request uses
the backend's configured API key (if any) and a 5 s per-fetch timeout.
Failures are recorded as a non-2xx status, JSON parse error, or
`reqwest::Error` class (`timeout`, `connect`, `request`, `unknown`); the
model is hidden from `/v1/models` and `/props` until the next successful
refresh. The operator endpoint `/_onair/operator/models` reports the
`context_length_source` and `context_length_last_fetch_unix_ms` for each
public model so operators can see whether the cached value is fresh.

## Logging

onair logs sanitized failures at `warn` and successful proxy/model responses at
`debug`. Use higher verbosity when debugging routing without exposing request
or response bodies:

```sh
RUST_LOG=onair=debug,tower_http=info cargo run -- --config onair.toml
```

Successful proxy logs include route, backend ID, configured backend target,
backend remote address when available, immediate/effective client address,
trusted proxy address when used, user agent, requested/public/backend model
IDs, attempt index/count, request body size, response status, response size for
buffered responses, and stream duration for streaming responses.

Streaming completion logs also include token totals plus body-free usage
diagnostics (`stream_usage_object_count`, `stream_usage_keys`,
`stream_event_names`, and `stream_usage_event_names`) at `debug` verbosity.

Retry logs at `warn` show the failed backend and next fallback backend for
pre-response send failures.

Timeline snapshot logs at `debug` include a wall-clock start timestamp plus
monotonic microsecond offsets for auth, request inspection, route selection,
request rewriting, backend forward start, upstream headers received,
first/complete backend body read, response rewriting, response readiness, and
stream completion where applicable. They intentionally do not include prompt or
completion bodies.
