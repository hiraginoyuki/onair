# Routing

onair routes by public model, endpoint family, capability markers, and
per-model endpoint markers. Native endpoint support is preferred when a route
explicitly supports the requested native endpoint. Compatibility paths require
explicit compatibility markers.

## Terminology

- Public model: the model ID visible to clients.
- Backend model: the model ID sent to the upstream backend after auth/access
  checks pass.
- Backend capability: a `[[backend]].capabilities` marker saying what an
  upstream can receive.
- Model endpoint: a `[[backend.model]].endpoints` marker saying what a public
  model may expose.
- Compatibility marker: an explicit marker that chooses a request/response
  translation path for a public model.

## Endpoint Matrix

| Client endpoint | Upstream endpoint | Backend capability | Route marker |
| --- | --- | --- | --- |
| `/v1/chat/completions` | `/v1/chat/completions` | `chat` or `chat_completions` | `chat` or `chat_completions` when `endpoints` is non-empty |
| `/v1/responses` | `/v1/responses` | `responses` | `responses` when `endpoints` is non-empty |
| `/v1/responses` | `/v1/chat/completions` | `chat` or `chat_completions` | `responses_via_chat_completions` in backend `capabilities` or route `endpoints` |
| `/v1/chat/completions` | `/v1/responses` | `responses` | `chat_completions_via_responses` in backend `capabilities` or route `endpoints` |

## Backend Capabilities

`[[backend]].capabilities` is a backend-wide marker list. `capability` is
accepted as a TOML alias.

Capability markers are matched against `/v1/*` path families and common
aliases:

- `chat` or `chat_completions` for native `/v1/chat/completions`.
- `responses` for native `/v1/responses`.
- `responses_via_chat_completions` for client `/v1/responses` routed through
  upstream `/v1/chat/completions`. The selected backend must still be
  Chat Completions-capable.
- `chat_completions_via_responses` for client `/v1/chat/completions` routed
  through upstream `/v1/responses`. The selected backend must still be
  Responses-capable.
- `embeddings` for `/v1/embeddings`.
- `images` or `image` for `/v1/images/*`.
- `audio` for `/v1/audio/*`.
- `files` or `file` for `/v1/files/*`.
- `batches`, `fine_tuning`, `assistants`, `threads`, `vector_stores`,
  `uploads`, and similar first path segments.
- `streaming` for `stream: true` requests.
- `tools`, `tool_calls`, `function_calling`, or `functions` for requests with
  a non-empty `tools` array. If a model route has a non-empty `endpoints`
  list, that route must also include a tool marker before tool-bearing
  requests are forwarded.
- `all` as a broad marker for native `/v1/*` paths and optional feature
  markers. Compatibility paths still require an exact compatibility marker on
  the backend or model route.

For a backend that should receive any OpenAI-compatible HTTP route it supports,
use:

```toml
[[backend]]
id = "openai"
base_url = "https://api.openai.com"
api_key_env = "OPENAI_API_KEY"
capabilities = ["all", "streaming"]
```

## Model Endpoints

`[[backend.model]]` entries are optional for backends that only serve
model-less endpoints. They are required for model-bearing requests, synthetic
`/v1/models` output, and public-to-backend model rewrites.

`[[backend.model]].endpoints` can further restrict a model route to endpoint
keys such as `chat`, `chat_completions`, `responses`,
`responses_via_chat_completions`, `chat_completions_via_responses`, `audio`,
or `embeddings`, plus feature markers such as `tools`.

If omitted or empty, the model route is allowed for native endpoints and
feature markers supported by the backend; compatibility still requires an
exact compatibility marker at backend level.

Backend order is priority order when multiple compatible routes match, and
also for model-less requests.

Quick mental model:

- `[[backend]].capabilities` says what the upstream backend can do.
- `[[backend.model]].endpoints` says what this public model may expose.
- A compatibility marker chooses a translation path for that public model.
- Native routing wins when the native endpoint marker is also allowed.

Examples:

- `capabilities = ["responses"]`: upstream can receive `/v1/responses`.
- `endpoints = ["chat"]`: this public model accepts client Chat Completions
  requests natively.
- `endpoints = ["chat_completions_via_responses"]`: this public model accepts
  client Chat Completions requests and sends upstream Responses requests.
- To force Chat-to-Responses compatibility, omit `chat` from that model route.

## Compatibility Routes

A route that allows `responses` serves client `/v1/responses` natively. A
route that allows `responses_via_chat_completions` serves client
`/v1/responses` by translating the request to upstream
`/v1/chat/completions` and translating successful responses back to Responses
shape.

A route that allows `chat_completions_via_responses` serves client
`/v1/chat/completions` by translating the request to upstream `/v1/responses`
and translating successful responses back to Chat Completions shape.

Routes with only `endpoints = ["chat"]` must not serve client `/v1/responses`
through compatibility. Routes with only `endpoints = ["responses"]` must not
serve client `/v1/chat/completions` through compatibility.

## Route Policies

`[[backend]].weight` biases backend selection under
`[routing].strategy = "weighted_random"`. The default is `1`, and
`weight = 0` is rejected at config load. Weights are only consulted when
the strategy is `weighted_random`; `priority`, `sticky`, and
`round_robin` treat every backend as equally eligible. Weights are
integers; higher weight means higher probability of selection, and equal
weights produce uniform random selection.

`[[backend]].tool_schema_mode` controls only the Responses-to-Chat
compatibility conversion for function-tool schemas. The default, `preserve`,
forwards the schema shape the client sent after wrapping Responses function
tools into Chat Completions format. Use `llamacpp_compat` only for a
llama.cpp-style chat backend/template that rejects common JSON Schema
fragments: it recursively removes `default`, collapses simple nullable
`type = ["...", "null"]`, and collapses simple nullable `anyOf`/`oneOf` pairs
in converted tool `parameters`. A `[[backend.model]].tool_schema_mode` value
overrides the backend default for that model route. Native `responses` routes
and direct chat requests are not schema-sanitized by this setting.

`[[backend]].responses_store` controls upstream Responses-compatible
forwarding. The default, `preserve`, leaves the client's `store` field
untouched. Use `force_false` for a backend or wrapper that requires explicit
non-storage: onair adds `"store": false` when forwarding native `/v1/responses`
or Chat-to-Responses compatibility requests and only when the client omitted
`store`; explicit client values are preserved. A
`[[backend.model]].responses_store` value overrides the backend default for
that model route. This setting does not affect direct chat requests or
Responses-to-Chat compatibility requests.

`[[backend]].responses_max_output_tokens` controls upstream
Responses-compatible forwarding. The default, `preserve`, forwards the
generated or client-supplied `max_output_tokens` field unchanged. Use `drop`
for a backend or wrapper that rejects the Responses field entirely, or use
`rename_to_max_tokens` / `rename_to_max_completion_tokens` only if the backend
explicitly expects one of those alternate names. A
`[[backend.model]].responses_max_output_tokens` value overrides the backend
default for that model route. This setting applies to native `/v1/responses`
and Chat-to-Responses compatibility requests; it does not affect direct chat
requests or Responses-to-Chat compatibility requests.

`[[backend]].chat_stream_usage` controls only upstream Chat Completions JSON
requests. The default, `preserve`, leaves the client's `stream_options`
untouched. Use `insert` for a backend that honors Chat Completions
`stream_options.include_usage`: when the forwarded request is `stream: true`,
onair adds `"stream_options": {"include_usage": true}` only if the client
omitted `stream_options.include_usage`; existing client values, including
explicit `false`, are preserved. Inserted usage is consumed for onair token
metrics and debug diagnostics, but it is filtered out of client streaming
responses unless the client explicitly requested
`stream_options.include_usage = true`. Use `force_true` only when operator
telemetry should override client request fidelity: onair enables
`include_usage`, preserves other `stream_options` object fields, and replaces a
non-object `stream_options` value with an object containing only
`include_usage`; client responses still hide usage unless the original client
request opted in. A `[[backend.model]].chat_stream_usage` value overrides the
backend default for that model route. This setting applies to direct
`/v1/chat/completions` forwarding and to the Responses-to-Chat compatibility
path because that path forwards upstream as Chat Completions; it does not
apply to native `/v1/responses` or Chat-to-Responses compatibility requests.

onair also preflights client `/v1/responses` tool history before forwarding it.
Every top-level `function_call` item in `input` must have a matching
`function_call_output`; otherwise onair returns a local `400` with a clear
input error instead of sending malformed history to the backend and surfacing a
generic upstream rejection.

## Sticky Routing And Fallback

Set `[routing].strategy = "sticky"` when multiple backends serve the same
public model and you want cache-heavy traffic to keep landing on the same
backend. The sticky key is derived from identity, path, public model, and
`prompt_cache_key` when provided. The router still forwards
`prompt_cache_key` and `prompt_cache_retention` unchanged.

### Round-Robin Strategy

Set `[routing].strategy = "round_robin"` to cycle the primary backend
across the compatible candidates for each request. The router keeps an
in-process counter per public model (or per request path for model-less
endpoints such as `/v1/embeddings`), so a busy model does not advance
another model's pointer. Counters are not shared between onair
instances; each process cycles its candidates independently. Counter
entries are created lazily and persist for the process lifetime; they
are not freed when a model is removed from config.

The rotated ordering means the fallback list also rotates. The same
set of compatible backends remains reachable as fallbacks, but their
order changes per request.

### Weighted-Random Strategy

Set `[routing].strategy = "weighted_random"` to pick a primary backend
per request using each candidate's `[[backend]].weight` value. Weights
are summed, and a uniformly random integer in `[0, total)` selects the
primary. Higher `weight` raises the probability of selection. `weight =
0` is rejected at config load; equal weights produce uniform random
selection. The rotated ordering means the fallback list also rotates in
the same way as the other strategies.

### Fallback

`[routing].fallback_attempts` adds a limited number of extra backend tries
after a pre-response connect/send/timeout failure.

The selected backend is still tried first, and the fallback only happens
before any upstream response headers or client-visible body bytes are
committed.

Non-success HTTP responses are not retried by default, and streaming responses
are never retried once response bytes begin flowing to the client. The default
`fallback_attempts = 1` gives one recovery attempt after the preferred backend
fails early.

Set `fallback_attempts = 0` to preserve strict single-backend behavior. Even
conservative pre-response fallback can duplicate upstream work if a backend
times out after accepting a request, so use low values and check backend
billing/side-effect semantics before increasing it.

## Prompt Caching

- Prompt caching is backend-defined and works best when the backend is OpenAI
  or another provider with compatible cache behavior.
- onair preserves `prompt_cache_key` and `prompt_cache_retention`, and only
  rewrites configured public model IDs to backend model IDs.
- `prompt_cache_key` also participates in sticky routing when
  `[routing].strategy = "sticky"`.
- Static prompt prefixes, tools, schemas, images, and their ordering must
  remain stable for backend cache hits.
- Extended cache retention such as `prompt_cache_retention = "24h"` may have
  data-retention implications and should only be enabled for backends/models
  where you intend that behavior.
