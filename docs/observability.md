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

- `GET /_onair/inspector`: information-dense browser UI with a live request
  table, operator overview cards, backend-health cards, detail pane,
  per-request timeline bars, and backend-attempt waterfalls with expandable
  per-attempt detail panes.
- `GET /_onair/inspector/requests`: JSON list of retained records, newest
  first. Use `?limit=<n>` to cap the response; onair clamps the limit to
  `1..=10000` and defaults to `1000`.
- `GET /_onair/inspector/requests/{record_id}`: JSON detail for one retained
  record.
- `GET /_onair/inspector/events`: server-sent events for low-latency live
  updates. Use `?snapshot_limit=<n>` to cap the initial replay; the UI
  defaults this to `1000`.
- The UI updates the URL hash to the selected `record_id`, so a request detail
  view can be bookmarked or shared locally.
- The request table includes sortable columns, identity by default, a column
  chooser, local saved table-view presets, quick filters for errors, fallback,
  and slow requests, pause/resume live updates, copy/download actions for the
  selected request JSON, and a filter input that uses space-separated terms so
  every term must match the retained request metadata it searches.

Read-only operator endpoints use the same `[inspector]` enablement and
effective-client loopback/`allow_remote` gate:

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
