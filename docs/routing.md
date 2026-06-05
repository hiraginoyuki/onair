# Routing

onair routes by public model, endpoint family, support markers, and
per-route expose markers. Native endpoint support is preferred when a route
explicitly exposes the requested native endpoint. Compatibility paths require
explicit compatibility markers.

## Terminology

- Public model: the model ID visible to clients.
- Backend model: the model ID sent to the upstream backend after auth/access
  checks pass.
- Backend supports: a `[[backend]].supports` marker saying what an upstream
  can receive.
- Route expose: a `[[route]].expose` marker saying what client API surfaces
  a public model (or model-less path) may accept.
- Compatibility marker: an explicit marker that chooses a request/response
  translation path for a public model.

## Endpoint Matrix

| Client endpoint | Upstream endpoint | Backend supports | Route expose |
| --- | --- | --- | --- |
| `/v1/chat/completions` | `/v1/chat/completions` | `chat` or `chat_completions` | `chat` or `chat_completions` when `expose` is non-empty |
| `/v1/responses` | `/v1/responses` | `responses` | `responses` when `expose` is non-empty |
| `/v1/responses` | `/v1/chat/completions` | `chat` or `chat_completions` | `responses_via_chat_completions` in backend `supports` or route `expose` |
| `/v1/chat/completions` | `/v1/responses` | `responses` | `chat_completions_via_responses` in backend `supports` or route `expose` |

## Backend Supports

`[[backend]].supports` is a backend-wide marker list. It is the only field
that controls what an upstream can receive; there is no `capability` (singular)
alias and the old `[[backend.model]].endpoints` field is gone.

Support markers are matched against `/v1/*` path families and common
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
  a non-empty `tools` array. If a route has a non-empty `expose` list, that
  route must also include a tool marker before tool-bearing requests are
  forwarded.
- `all` as a broad marker for native `/v1/*` paths and optional feature
  markers. Compatibility paths still require an exact compatibility marker on
  the backend or route.

For a backend that should receive any OpenAI-compatible HTTP route it supports,
use:

```toml
[[backend]]
id = "openai"
base_url = "https://api.openai.com"
api_key_env = "OPENAI_API_KEY"
supports = ["all", "streaming"]
```

## Public Routes

`[[route]]` blocks declare one public-facing model (or one model-less path)
and the backends that can serve it. A `[[route]]` is required for every
model-bearing request, for synthetic `/v1/models` output, for public-to-backend
model rewrites, and for any model name referenced by a `[[client]]` model
whitelist or `[access].default_models`. Model-less paths (such as
`/v1/embeddings`) opt in by setting `path = "..."` instead of `public = "..."`.

A `[[route]]` block has the following shape:

- `public = "..."` for model-bearing routes, or `path = "..."` for
  model-less routes. Exactly one of `public` / `path` per block.
- `expose = [...]` lists the client API surfaces this route accepts. It can
  further restrict the route to endpoint keys such as `chat`,
  `chat_completions`, `responses`, `responses_via_chat_completions`,
  `chat_completions_via_responses`, `embeddings`, plus feature markers such
  as `tools`. If omitted or empty, the route is allowed for native endpoints
  and feature markers supported by the backend; compatibility still requires
  an exact compatibility marker.
- `backends = [...]` lists the upstreams that may serve this route. For
  model-bearing routes each entry is `"model@backend"`: the upstream model
  name `model` served by backend `backend`. For model-less routes each entry
  is a bare backend id (no `@`) because there is no model name to bind.
- Optional per-route policy overrides:
  `tool_schema_mode`, `responses_store`, `responses_max_output_tokens`,
  `chat_stream_usage`. These override the backend defaults for that route
  only.
- Optional `context_length` for model-bearing routes: `omitted` or `"none"`
  hides the value, an integer literal sets a fixed `n_ctx`, and `"upstream"`
  forwards the live `n_ctx` from the first backend's
  `/props?model=<backend_model>`. See
  [configuration.md](configuration.md#context-length) for the full behavior.

### `model@backend` syntax

The `model@backend` form is read as "upstream model name `model` served by
backend `backend`". The model comes first because in the routing context the
model is the primary noun: the operator asks "where does GPT-5 go?", and
`gpt-5@openai` reads as "GPT-5 at OpenAI". Model-less routes drop the
`model@` prefix entirely and use bare backend ids.

```toml
# Model-bearing route: public "gpt-4o" maps to upstream "gpt-4o" on backend
# "openai", with chat/responses/tools allowed.
[[route]]
public = "gpt-4o"
expose = ["chat", "responses", "tools"]
backends = ["gpt-4o@openai"]

# Model-bearing route: same public model, but the client /v1/responses is
# translated to upstream /v1/chat/completions (no native "responses" support
# on this backend). The upstream model name is "llama-3".
[[route]]
public = "llama-3-frontend"
expose = ["responses_via_chat_completions"]
backends = ["llama-3@llama"]

# Model-less route: client /v1/embeddings is served by backend "llama" with
# no model rewrite.
[[route]]
path = "/v1/embeddings"
expose = ["embeddings"]
backends = ["llama"]
```

The order of `backends` is priority order when multiple compatible backends
match, and it also seeds the fallback list.

### Strict-require-route

Every public model referenced anywhere in the config (in
`[access].default_models`, in any `[[client]].models`, or by name in a
now-removed `[[backend.model]]`) must have a matching `[[route]]` block with
`public = "<that name>"`, or config load fails with an error. This is the
operator's signal that the exposure decision was not made; add a `[[route]]`
or remove the model from the client.

For each entry in `route.backends`, the validator also checks whether
`backend.supports` overlaps with the union of "native markers" implied by
`route.expose` (or the route's compat-marker combinations). When there is no
overlap the validator emits a `tracing::warn!` and the config still loads;
the operator can see the empty candidate set on `/_onair/operator/config`
and act on the warning. This warning is intentional: removing it or
promoting it to a load-time error would be a behavior change.

### Compat-marker semantics

`route.expose` does NOT implicitly include compat markers. Concretely,
`expose = ["chat"]` does not imply `chat_completions_via_responses` is
available. Compat markers must be explicit in `expose`. The compat-marker
decision logic in `request_mode_for_responses` and
`request_mode_for_chat_completions` is unchanged by the schema refactor;
only the field name it reads from changed.

Quick mental model:

- `[[backend]].supports` says what the upstream backend can do.
- `[[route]].expose` says what this public model (or path) may accept.
- A compatibility marker chooses a translation path for that route.
- Native routing wins when the native endpoint marker is also allowed.
- A public model with no `[[route]]` is a config error, not a silent 404.

Examples:

- `supports = ["responses"]`: upstream can receive `/v1/responses`.
- `expose = ["chat"]`: this public model accepts client Chat Completions
  requests natively.
- `expose = ["chat_completions_via_responses"]`: this public model accepts
  client Chat Completions requests and sends upstream Responses requests.
- To force Chat-to-Responses compatibility, omit `chat` from that route's
  `expose` list.

onair validates the strings in `supports` and `expose` against the
known marker set at load and reload time. See
[configuration.md](configuration.md#capability-and-endpoint-marker-validation)
for the `unknown_capability_policy` / `unknown_endpoint_policy` settings
that control whether a typo fails to load or just emits a warning.

## Compatibility Routes

A route that allows `responses` serves client `/v1/responses` natively. A
route that allows `responses_via_chat_completions` serves client
`/v1/responses` by translating the request to upstream
`/v1/chat/completions` and translating successful responses back to Responses
shape.

A route that allows `chat_completions_via_responses` serves client
`/v1/chat/completions` by translating the request to upstream `/v1/responses`
and translating successful responses back to Chat Completions shape.

Routes with only `expose = ["chat"]` must not serve client `/v1/responses`
through compatibility. Routes with only `expose = ["responses"]` must not
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
in converted tool `parameters`. A `[[route]].tool_schema_mode` value
overrides the backend default for that route. Native `responses` routes
and direct chat requests are not schema-sanitized by this setting.

`[[backend]].responses_store` controls upstream Responses-compatible
forwarding. The default, `preserve`, leaves the client's `store` field
untouched. Use `force_false` for a backend or wrapper that requires explicit
non-storage: onair adds `"store": false` when forwarding native `/v1/responses`
or Chat-to-Responses compatibility requests and only when the client omitted
`store`; explicit client values are preserved. A
`[[route]].responses_store` value overrides the backend default for that
route. This setting does not affect direct chat requests or
Responses-to-Chat compatibility requests.

`[[backend]].responses_max_output_tokens` controls upstream
Responses-compatible forwarding. The default, `preserve`, forwards the
generated or client-supplied `max_output_tokens` field unchanged. Use `drop`
for a backend or wrapper that rejects the Responses field entirely, or use
`rename_to_max_tokens` / `rename_to_max_completion_tokens` only if the backend
explicitly expects one of those alternate names. A
`[[route]].responses_max_output_tokens` value overrides the backend default
for that route. This setting applies to native `/v1/responses`
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
request opted in. A `[[route]].chat_stream_usage` value overrides the backend
default for that route. This setting applies to direct
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
