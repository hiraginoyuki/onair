# onair

onair is an OpenAI-compatible HTTP reverse proxy router for operating one public API surface over one or more compatible backends.

## Intent

onair lets a proxy operator expose stable OpenAI-style API keys, model names, and routing policy without exposing the backend provider, backend URL, backend model ID, or other obvious backend-specific details to clients. The privacy target is backend anonymity from ordinary API-visible server behavior: model listing, model IDs, request/response model fields, headers, and error bodies should not reveal which backend handled a request.

This is not a full traffic-analysis defense. Timing, throughput, token rate, model quality, and other behavioral fingerprints can still reveal information about the backing service. The project focuses on hiding simpler protocol/configuration leaks while preserving compatibility with OpenAI-style clients.

Planned work lives in [ROADMAP.md](ROADMAP.md). This README describes the current behavior and how to operate the software.

## Behavior Summary

- Clients authenticate with OpenAI-style `Authorization: Bearer ...` headers.
- Each authenticated identity sees only its configured public model whitelist.
- Public model names are mapped to backend model names after access checks pass.
- `/v1/*` requests that are not handled by onair itself can be forwarded to a compatible backend when backend capabilities allow it.
- `POST /v1/chat/completion` is accepted as a typo-compatible alias and forwarded upstream as `/v1/chat/completions`.
- `POST /v1/responses` uses native Responses backends when a matching backend/model route declares `responses`; chat-completions-only routes can serve Responses requests through a compatibility layer that translates common fields such as `input`, `instructions`, `max_output_tokens`, and function tools to `/v1/chat/completions`, then translates successful chat-completion responses back to Responses shape.
- `stream: true` responses are proxied as server-sent events, with configured backend model names rewritten back to public model names in JSON/SSE responses.
- OpenAI-compatible `usage.total_tokens` values are preserved; when a backend reports prompt/input and completion/output token counts without a total, onair adds the corresponding total to chat-completion and Responses JSON/SSE responses.
- Backend errors are converted to generic OpenAI-style errors, and response headers are allowlisted before returning to the client.
- OpenTelemetry metrics record request counts, status codes, latency, stream duration, backend usage, and token counters when an OpenAI-compatible `usage` object is present.
- A disabled-by-default local inspector can retain recent request metadata and render live timing timelines and backend-attempt waterfalls in a browser without storing prompt or completion bodies.

## Operation

Start from `onair.example.toml`:

```sh
cp onair.example.toml onair.toml
cargo run -- --config onair.toml
```

Configuration is TOML. The path comes from `--config`, `-c`, or `ONAIR_CONFIG`; if none is provided, onair reads `onair.toml` from the current directory.

The core sections are:

```toml
[server]
bind = "127.0.0.1:8080"
request_body_limit_bytes = 2097152
# Only trust Forwarded/X-Forwarded-For/X-Real-IP from these immediate peer CIDRs.
trusted_proxy_cidrs = []

[telemetry]
service_name = "onair"
exporter = "none" # or "otlp"
otlp_endpoint = "http://127.0.0.1:4317"
export_interval_ms = 30000

[debug_capture]
# Dangerous: captures exact prompt/request bodies to local files. Enable only while reproducing.
enabled = false
directory = "onair-debug-captures"

[inspector]
# Disabled by default. Serves /_onair/inspector when enabled.
enabled = false
retention_requests = 10000
allow_remote = false

[health]
# Disabled by default because probes send requests to configured backends.
active = false
interval_ms = 30000
timeout_ms = 2000
path = "/v1/models"

[routing]
# "priority" chooses the first matching backend. "sticky" hashes identity/path/model/prompt_cache_key
# across all matching backends, improving prompt-cache locality when several backends serve a model.
# fallback_attempts tries extra compatible backends after a pre-response connect/send/timeout failure.
strategy = "priority"
fallback_attempts = 1

[access]
default_models = ["gpt-4o-mini"]

[[client]]
id = "dev"
api_key_env = "ONAIR_DEV_API_KEY"
models = ["gpt-4o"]

[[backend]]
id = "local-vllm"
base_url = "http://127.0.0.1:8000"
api_key_env = "LOCAL_VLLM_API_KEY"
context_length = 131072
capabilities = ["chat", "responses", "streaming", "tools"]
timeout_ms = 120000

[[backend.model]]
public = "gpt-4o-mini"
backend = "llama-3.1-8b-instruct"
context_length = "inherit"
endpoints = ["chat", "responses"]
```

onair interprets the file as routing and visibility policy:

- `[server]`, `[telemetry]`, `[debug_capture]`, `[inspector]`, and `[health]` configure process-level behavior.
- `[access].default_models` grants public models to every configured client.
- Each `[[client]]` adds one authenticated identity and may extend that identity's model whitelist.
- Each `[[backend]]` defines one upstream OpenAI-compatible service plus capability markers that decide which `/v1/*` request families it can receive.
- Each `[[backend.model]]` maps one public model name to the backend model name that upstream should receive.
- `[routing]` chooses how to select among multiple compatible backend routes and how many fallback attempts to allow before response commitment.

### Hot reload

onair watches the config file's parent directory and reloads the config when the file changes. Save bursts and atomic replacement writes are debounced before loading. Reloads are validated before they are applied, and invalid TOML or invalid model/client/backend rules keep the previous config active.

Reloaded immediately:

- `[access]`, `[[client]]`, `[[backend]]`, `[[backend.model]]`, `[routing]`, `[debug_capture]`, `[inspector]`, `[health]`, `[server].trusted_proxy_cidrs`, backend auth, model mappings, capabilities, timeouts, context metadata, client-address trust policy, debug capture settings, inspector settings, and health probe settings.

Restart required:

- `[server].bind`, `[server].request_body_limit_bytes`, and `[telemetry]` exporter settings.

### Access rules

- Every `[[client]]` must have `api_key` or `api_key_env`.
- A client's effective model whitelist is `[access].default_models` union `[[client]].models`.
- `/v1/models` only lists the authenticated client's effective whitelist intersected with configured backend routes.
- `/v1/models/{model}` only returns model objects for authenticated clients that can access that configured public model.
- Requests for models outside the effective whitelist return `404`, intentionally indistinguishable from a missing model.
- Requests for whitelisted models with no compatible backend route also return `404`.
- For model-bearing requests, onair detects `model` in JSON bodies, URL-encoded forms, multipart form fields, and query strings.
- Public model IDs are rewritten to backend model IDs in JSON bodies, URL-encoded forms, multipart form fields, and query strings before forwarding.

### Client address logging

onair logs the immediate socket peer address for proxied requests. If onair is behind a trusted reverse proxy, set `[server].trusted_proxy_cidrs` to the proxy source CIDRs to allow `Forwarded`, `X-Forwarded-For`, or `X-Real-IP` to populate `effective_client_addr` in logs. Forwarded headers are ignored by default and are also ignored when the immediate peer is not trusted.

For appended `Forwarded` or `X-Forwarded-For` chains, onair uses the closest valid IP/socket hop instead of the leftmost value so client-supplied spoofed entries are not treated as authoritative. Configure the trusted proxy to overwrite forwarded headers if logs should show the original external client rather than the client seen by that proxy.
Repeated `Forwarded` or `X-Forwarded-For` header lines are treated as one chain and resolved the same way: the closest valid hop wins.

### Backend capabilities

`[[backend]].capabilities` is a marker list; `capability` is accepted as a TOML alias. Capability markers are matched against `/v1/*` path families and common aliases:

- `chat` or `chat_completions` for `/v1/chat/completions`.
- `responses` for native `/v1/responses`; `chat`/`chat_completions` backends can also serve `/v1/responses` through onair's Responses-to-Chat compatibility layer when the matching backend/model route does not declare native `responses`.
- `embeddings` for `/v1/embeddings`.
- `images` or `image` for `/v1/images/*`.
- `audio` for `/v1/audio/*`.
- `files` or `file` for `/v1/files/*`.
- `batches`, `fine_tuning`, `assistants`, `threads`, `vector_stores`, `uploads`, and similar first path segments.
- `streaming` for `stream: true` requests.
- `tools`, `tool_calls`, `function_calling`, or `functions` for requests with a non-empty `tools` array. If a model route has a non-empty `endpoints` list, that route must also include a tool marker before tool-bearing requests are forwarded.
- `all` as a broad marker for any `/v1/*` path and optional feature marker.

`[[backend.model]]` entries are optional for backends that only serve model-less endpoints. They are required for model-bearing requests, synthetic `/v1/models` output, and public-to-backend model rewrites.

`[[backend.model]].endpoints` can further restrict a model route to endpoint keys such as `chat`, `chat_completions`, `responses`, `audio`, or `embeddings`, plus feature markers such as `tools`. If omitted or empty, the model route is allowed for any endpoint and feature supported by the backend. A route that allows `responses` serves client `/v1/responses` natively. A route that allows `chat` but not `responses` can serve client `/v1/responses` through the compatibility layer when the backend itself is chat-capable. Backend order is priority order when multiple compatible routes match, and also for model-less requests.

`[[backend]].tool_schema_mode` controls only the Responses-to-Chat compatibility conversion for function-tool schemas. The default, `preserve`, forwards the schema shape the client sent after wrapping Responses function tools into Chat Completions format. Use `llamacpp_compat` only for a llama.cpp-style chat backend/template that rejects common JSON Schema fragments: it recursively removes `default`, collapses simple nullable `type = ["...", "null"]`, and collapses simple nullable `anyOf`/`oneOf` pairs in converted tool `parameters`. A `[[backend.model]].tool_schema_mode` value overrides the backend default for that model route. Native `responses` routes and direct chat requests are not schema-sanitized by this setting.

Set `[routing].strategy = "sticky"` when multiple backends serve the same public model and you want cache-heavy traffic to keep landing on the same backend. The sticky key is derived from identity, path, public model, and `prompt_cache_key` when provided. The router still forwards `prompt_cache_key` and `prompt_cache_retention` unchanged.

`[routing].fallback_attempts` adds a limited number of extra backend tries after a pre-response connect/send/timeout failure. The selected backend is still tried first, and the fallback only happens before any upstream response headers or client-visible body bytes are committed. Non-success HTTP responses are not retried by default, and streaming responses are never retried once response bytes begin flowing to the client. The default `fallback_attempts = 1` gives one recovery attempt after the preferred backend fails early.

Set `fallback_attempts = 0` to preserve strict single-backend behavior. Even conservative pre-response fallback can duplicate upstream work if a backend times out after accepting a request, so use low values and check backend billing/side-effect semantics before increasing it.

### Context length

- `[[backend]].context_length` sets a backend-level default context length for inheritance.
- `[[backend.model]].context_length = "inherit"` copies the backend-level value into llama.cpp-style public metadata.
- `[[backend.model]].context_length = <integer>` returns a specific value for that public model.
- `[[backend.model]].context_length = "none"` or omitting the field entirely hides the value, which is the default OpenAI-compatible behavior.
- If you use `"inherit"` without a backend-level `context_length`, config loading fails.
- When visible, `/v1/models` and `/v1/models/{model}` expose the value as `meta.n_ctx` and `meta.n_ctx_train`, matching llama.cpp's OpenAI-compatible model object shape.
- `/props?model=<public-model>` and `/v1/props?model=<public-model>` expose the runtime context as `default_generation_settings.n_ctx`, matching llama.cpp's props endpoint shape.

For a backend that should receive any OpenAI-compatible HTTP route it supports, use:

```toml
[[backend]]
id = "openai"
base_url = "https://api.openai.com"
api_key_env = "OPENAI_API_KEY"
capabilities = ["all", "streaming"]
```

### Backend secrecy

- `[[backend]].base_url` must be an absolute `http` or `https` URL without embedded credentials, query strings, or fragments. Use `api_key` or `api_key_env` for backend credentials.
- `/v1/models` and `/v1/models/{model}` are synthesized from public config; backend model IDs are not listed.
- Model-bearing requests are rewritten to backend model IDs only after access checks pass.
- Successful JSON and SSE responses rewrite backend model IDs back to public model IDs when a model mapping is known.
- Non-success backend responses are converted to generic OpenAI-style errors; backend error bodies are discarded.
- Response headers use an allowlist. onair keeps useful API headers such as `content-type` and `content-disposition`, sets its own cache policy, and echoes only a client-supplied `x-request-id`.
- Backend anonymity covers protocol-visible signs. onair does not try to hide timing, throughput, token rate, model quality, or other behavioral fingerprints.

### Prompt caching

- Prompt caching is backend-defined and works best when the backend is OpenAI or another provider with compatible cache behavior.
- onair preserves `prompt_cache_key` and `prompt_cache_retention`, and only rewrites configured public model IDs to backend model IDs.
- `prompt_cache_key` also participates in sticky routing when `[routing].strategy = "sticky"`.
- Static prompt prefixes, tools, schemas, images, and their ordering must remain stable for backend cache hits.
- Extended cache retention such as `prompt_cache_retention = "24h"` may have data-retention implications and should only be enabled for backends/models where you intend that behavior.

## API keys

Clients authenticate with OpenAI-style bearer tokens:

```http
Authorization: Bearer <generated-onair-client-key>
```

The recommended onair key shape is:

```text
sk-ona-<fixed-ed25519-style-prefix><43 base64url characters>
```

The fixed `AAAAC3NzaC1lZDI1NTE5AAAAI` prefix mimics the start of the SSH `ssh-ed25519` public-key wire blob: length-prefixed algorithm name plus the length prefix for a 32-byte public key. The 43-character suffix should encode 32 random bytes using unpadded base64url, matching the random portion length of an Ed25519 public key blob. That gives `2^256` possible suffix values, approximately `1.16e77`, before accounting for any generator mistakes or operational leakage.

Example generator:

```sh
python3 - <<'PY'
import base64, secrets
prefix = "sk-ona-" + "AAAAC3NzaC1lZDI1NTE5AAAAI"
suffix = base64.urlsafe_b64encode(secrets.token_bytes(32)).decode().rstrip("=")
print(prefix + suffix)
PY
```

## Metrics

Set `[telemetry].exporter = "otlp"` to install an OpenTelemetry meter provider and export metrics to an OTLP/gRPC collector. The default endpoint comes from the OpenTelemetry OTLP exporter unless `otlp_endpoint` is set.

Metric instruments:

- `onair.requests`: counter labeled by `route`, `identity`, `model`, `backend`, `stream`, and `status_code`.
- `onair.backend.requests`: counter labeled by `route`, `identity`, `model`, `backend`, and `stream`.
- `onair.request.duration`: histogram in seconds with the same labels as `onair.requests`.
- `onair.stream.duration`: histogram in seconds for streaming response lifetime.
- `onair.tokens`: counter labeled by `direction=input|cached_input|output` when backend responses include OpenAI-compatible usage data such as `prompt_tokens`, `completion_tokens`, `input_tokens`, `output_tokens`, or `cached_tokens`. Client-facing responses preserve or synthesize `usage.total_tokens` from the prompt/input and completion/output counts when possible.

Prompt and response bodies are not logged by onair.

## Debug Capture

`[debug_capture]` is a default-off troubleshooting path for cases where exact request bytes are needed. It is not controlled by `RUST_LOG`, and enabling it writes prompt, request, and selected upstream error-response bodies to disk.

```toml
[debug_capture]
enabled = true
mode = "failures"
directory = "onair-debug-captures"
```

Set `mode = "failures"` to capture only attempts that fail with an upstream non-success response, upstream send error, timeout, body-read error, or stream error. Set `mode = "all"` to capture every successfully routed upstream attempt, including successful responses. The default mode is `all` for backward-compatible troubleshooting behavior when debug capture is enabled.

For each captured upstream attempt, onair creates one private capture directory containing:

- `inbound.body`: the exact body received from the client before model rewriting.
- `upstream.body`: the exact body sent to the backend after model rewriting.
- `upstream_error.body`: the upstream non-success response body, only when the backend returns a non-success status and a body is available. This diagnostic file is capped at 1 MiB and marked as truncated in metadata when the cap is reached.
- `metadata.json`: route, identity, public/backend model IDs, path/query metadata, debug-capture mode, body sizes, status/outcome, and capture file names.

When a capture is written, proxy failure/retry logs and inspector records include `debug_capture_id` so a generic client-facing upstream error can be correlated with the local capture directory.

Security guidance:

- Enable debug capture only while reproducing a trusted local issue, then disable it and delete the directory.
- Captures can include prompts, tool inputs, uploaded file bytes, personal data, credentials sent in bodies, and sensitive query parameters.
- The default `onair-debug-captures` directory is ignored by this repository, but custom directories are not automatically protected from commits, backups, or sharing.
- `debug_capture.directory` may be relative to the current working directory or absolute, but it must not contain `..` path components.
- onair logs a `warn` event for every captured request so accidental enablement is visible.

## Inspector

`[inspector]` is a default-off, in-process request inspector for timing and routing diagnostics. It keeps recent request records in memory and serves a local Web UI at `/_onair/inspector` when enabled.

```toml
[inspector]
enabled = true
retention_requests = 10000
allow_remote = false
```

Endpoints:

- `GET /_onair/inspector`: information-dense browser UI with a live request table, operator overview cards, backend-health cards, detail pane, per-request timeline bars, and backend-attempt waterfalls with expandable per-attempt detail panes.
- `GET /_onair/inspector/requests`: JSON list of retained records, newest first. Use `?limit=<n>` to cap the response; onair clamps the limit to `1..=10000` and defaults to `1000`.
- `GET /_onair/inspector/requests/{record_id}`: JSON detail for one retained record.
- `GET /_onair/inspector/events`: server-sent events for low-latency live updates. Use `?snapshot_limit=<n>` to cap the initial replay; the UI defaults this to `1000`.
- The UI updates the URL hash to the selected `record_id`, so a request detail view can be bookmarked or shared locally.
- The request table includes sortable columns, identity by default, a column chooser, local saved table-view presets, quick filters for errors/fallback/slow requests, pause/resume live updates, copy/download actions for the selected request JSON, and a filter input that uses space-separated terms so every term must match the retained request metadata it searches.

Read-only operator endpoints use the same `[inspector]` enablement and effective-client loopback/`allow_remote` gate:

- `GET /_onair/operator/runtime`: process uptime, current time, retained inspector record count, route object counts, and telemetry exporter status.
- `GET /_onair/operator/config`: sanitized active config. Client and backend API keys are never returned; backends expose only whether an API key is configured.
- `GET /_onair/operator/models`: effective public model visibility per client plus configured backend routes for each public model.
- `GET /_onair/operator/health`: backend health observed from proxied traffic and optional active probes, including split traffic/probe counters, consecutive failures, last status, last error kind, last source, and last observed latency.

Each retained record includes route, identity, public/backend model IDs, final backend ID/target, backend remote socket when available, immediate/effective client address, trusted proxy details, user agent, body sizes, response status, OpenAI-compatible usage counters when present, backend attempt records, retried pre-response attempts when fallback occurred, and a timeline snapshot. Timeline fields use a wall-clock `started_at_unix_ms` plus monotonic microsecond offsets for proxy/auth/routing/rewrite/backend/response milestones.

`backend_attempts` is the structured source for the UI waterfall. It includes every upstream attempt, including the final successful or failed attempt. Each attempt records the selected backend, configured backend target, backend remote socket when available, client-facing status, upstream status when headers were received, outcome/error kind, elapsed timing, debug capture ID when applicable, and monotonic offsets for request rewrite, debug capture, backend send start, upstream headers, first body chunk, body completion, and stream completion when those phases occurred. The waterfall keeps attempt rows compact by default and provides expand/collapse controls for dense per-attempt metadata and phase timings. `retried_attempts` is retained as a compatibility/summary field and only contains attempts that were abandoned before trying a fallback backend.

Security guidance:

- The inspector does not store prompt or completion bodies. It can still expose sensitive metadata such as model names, client IDs, source addresses, user agents, query strings, request sizes, token counts, and debug capture IDs.
- Operator endpoints can expose backend IDs, backend URLs, backend model IDs, model visibility policy, and local filesystem paths such as `debug_capture.directory`.
- With `allow_remote = false`, inspector endpoints are only served when the effective client address resolves to loopback after trusted-proxy header processing. This is the default and is appropriate for `bind = "127.0.0.1:8080"` plus a browser on the same host, while still rejecting forwarded remote clients that arrive through a trusted proxy.
- Set `allow_remote = true` only if the onair bind address is protected by another access-control layer, such as SSH tunneling, a private VPN, or a trusted reverse proxy with its own authentication.
- Inspector data is memory-only and disappears on process restart. Increase `retention_requests` only as much as needed; config loading rejects values above `100000` to avoid accidental unbounded memory growth.
- Inspector responses use `Cache-Control: no-store`; avoid putting them behind shared caches or public reverse proxies.
- Backend health always reflects requests that onair has already routed. If `[health].active = true`, onair also probes each configured backend at `[health].path` using the backend API key when configured.
- Active health probes are disabled by default because they send HTTP requests to every configured backend. Use a low-cost path such as `/v1/models`, and keep in mind that probes can be visible to upstream providers.
- onair does not follow backend redirects for normal proxy requests or health probes; redirect responses are treated as non-success.
- Backend health is currently an operator signal; it does not automatically remove unhealthy backends from routing.

## Logging

onair logs sanitized failures at `warn` and successful proxy/model responses at `debug`. Use higher verbosity when debugging routing without exposing request or response bodies:

```sh
RUST_LOG=onair=debug,tower_http=info cargo run -- --config onair.toml
```

Successful proxy logs include route, backend ID, configured backend target, backend remote address when available, immediate/effective client address, trusted proxy address when used, user agent, requested/public/backend model IDs, attempt index/count, request body size, response status, response size for buffered responses, and stream duration for streaming responses. Retry logs at `warn` show the failed backend and next fallback backend for pre-response send failures. Timeline snapshot logs at `debug` include a wall-clock start timestamp plus monotonic microsecond offsets for auth, request inspection, route selection, request rewriting, backend forward start, upstream headers received, first/complete backend body read, response rewriting, response readiness, and stream completion where applicable. They intentionally do not include prompt or completion bodies.
