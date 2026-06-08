# Configuration

Configuration is TOML. Start from the example:

```sh
cp onair.example.toml onair.toml
cargo run -- --config onair.toml
```

The config path comes from `--config`, `-c`, or `ONAIR_CONFIG`. If none is
provided, onair reads `onair.toml` from the current directory.

## Core Sections

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

[inspector.persistence]
# Disabled by default. Restores the latest retained inspector records after restart.
enabled = false
# path = ".local/inspector.sqlite"

[health]
# Disabled by default because probes send requests to configured backends.
active = false
interval_ms = 30000
timeout_ms = 2000
path = "/v1/models"

[routing]
# "priority" chooses the first matching backend. "sticky" hashes identity/path/model/prompt_cache_key
# across all matching backends, improving prompt-cache locality when several backends serve a model.
# "round_robin" cycles the primary backend per model across compatible backends. "weighted_random" uses
# [[backend]].weight to bias primary selection. See docs/routing.md for the full strategy matrix.
# fallback_attempts tries extra compatible backends after a pre-response connect/send/timeout failure.
strategy = "priority"
fallback_attempts = 1

[access]
default_models = ["gpt-4o-mini"]

[[client]]
id = "dev"
api_key_env = "ONAIR_DEV_API_KEY"

[[backend]]
id = "local-vllm"
base_url = "http://127.0.0.1:8000"
api_key_env = "LOCAL_VLLM_API_KEY"
supports = ["chat", "responses", "streaming", "tools"]
timeout_ms = 120000

[[route]]
public = "gpt-4o-mini"
expose = ["chat", "responses"]
backends = ["llama-3.1-8b-instruct@local-vllm"]
context_length = "upstream"
```

onair interprets the file as routing and visibility policy:

- `[server]`, `[telemetry]`, `[debug_capture]`, `[inspector]`, and `[health]`
  configure process-level behavior.
- `[access].default_models` grants public models to every configured client.
- Each `[[client]]` adds one authenticated identity and may extend that
  identity's model whitelist.
- Each `[[backend]]` defines one upstream OpenAI-compatible service plus
  `supports` markers that decide which `/v1/*` request families it can
  receive.
- Each `[[route]]` declares one public-facing model (or one model-less path)
  and the backends that can serve it. Model-bearing routes use
  `public = "..."` and `backends = ["model@backend", ...]`; model-less
  routes use `path = "..."` and bare backend ids in `backends`. `expose`
  lists the client API surfaces this route accepts.
- `[routing]` chooses how to select among multiple compatible backend routes
  and how many fallback attempts to allow before response commitment.

See [routing.md](routing.md) for support and expose marker semantics.
See [observability.md](observability.md) for telemetry, debug capture,
inspector, and health details.

## Hot Reload

onair watches the config file's parent directory and reloads the config when
the file changes. Save bursts and atomic replacement writes are debounced
before loading. Reloads are validated before they are applied, and invalid TOML
or invalid model/client/backend rules keep the previous config active.

Reloaded immediately:

- `[access]`, `[[client]]`, `[[backend]]`, `[[route]]`, `[routing]`,
  `[debug_capture]`, `[inspector]`, `[health]`,
  `[server].trusted_proxy_cidrs`, backend auth, route declarations, backend
  `supports`, timeouts, context metadata, client-address trust policy,
  debug capture settings, inspector runtime settings, and health probe
  settings.

Restart required:

- `[server].bind`, `[server].request_body_limit_bytes`, `[telemetry]`
  exporter settings, and `[inspector.persistence]` settings.

## Access Rules

- Every `[[client]]` must have `api_key` or `api_key_env`.
- A client's effective model whitelist is `[access].default_models` union
  `[[client]].models`.
- `/v1/models` only lists the authenticated client's effective whitelist
  intersected with configured backend routes.
- `/v1/models/{model}` only returns model objects for authenticated clients
  that can access that configured public model.
- Requests for models outside the effective whitelist return `404`,
  intentionally indistinguishable from a missing model.
- Requests for whitelisted models with no compatible backend route also return
  `404`.
- For model-bearing requests, onair detects `model` in JSON bodies,
  URL-encoded forms, multipart form fields, and query strings.
- Public model IDs are rewritten to backend model IDs in JSON bodies,
  URL-encoded forms, multipart form fields, and query strings before
  forwarding.

## Client Address Logging

onair logs the immediate socket peer address for proxied requests. If onair is
behind a trusted reverse proxy, set `[server].trusted_proxy_cidrs` to the proxy
source CIDRs to allow `Forwarded`, `X-Forwarded-For`, or `X-Real-IP` to
populate `effective_client_addr` in logs. Forwarded headers are ignored by
default and are also ignored when the immediate peer is not trusted.

For appended `Forwarded` or `X-Forwarded-For` chains, onair uses the closest
valid IP/socket hop instead of the leftmost value so client-supplied spoofed
entries are not treated as authoritative. Configure the trusted proxy to
overwrite forwarded headers if logs should show the original external client
rather than the client seen by that proxy.

Repeated `Forwarded` or `X-Forwarded-For` header lines are treated as one
chain and resolved the same way: the closest valid hop wins.

## Context Length

- Omitting `[[route]].context_length` or setting it to `"none"` hides
  the value, which is the default OpenAI-compatible behavior. No metadata is
  exposed for that public model in `/v1/models` or `/props`.
- `[[route]].context_length = <integer>` returns a fixed value for
  that public model. `/v1/models` and `/v1/models/{model}` expose
  `meta.n_ctx` and `meta.n_ctx_train`, both equal to the configured integer.
- `[[route]].context_length = "upstream"` forwards the live context
  size from the first backend in `route.backends` (the one whose
  `/props?model=<backend_model>` will be polled). onair issues a background
  `GET <backend.base_url>/props?model=<backend_model>` to the backend on a
  60 s interval and caches the `default_generation_settings.n_ctx` value.
  `/v1/models` and `/v1/models/{model}` expose the value as `meta.n_ctx`
  only; `meta.n_ctx_train` is omitted because llama.cpp's `/props` does
  not return a corresponding field. `/props?model=<public-model>` exposes
  the same value as `default_generation_settings.n_ctx`.
- If the upstream `/props` request fails (timeout, connect error, non-2xx,
  malformed body, missing `n_ctx`), the model is hidden from `/v1/models`
  and `/props` until the next successful refresh, and the operator API
  reports `context_length_source: "upstream"` with a null
  `context_length_last_fetch_unix_ms`. No retry is attempted within a
  single refresh tick; the next tick retries on its own schedule.
- `[[backend]].context_length` is not a recognized field. Old configs that
  use it fail to load with a serde `unknown field` error pointing to the
  exact field. Move the value to a `[[route]].context_length` entry
  (either an integer or `"upstream"`).
- The old `"inherit"` mode is no longer accepted. Replace it with
  `"upstream"` to forward the live value, or with the desired integer if
  the value is known at config time.

## `[[route]]` Schema

`[[route]]` blocks are the operator's "what client API surface is exposed
for this public model" declaration. A `[[route]]` is required for every
public model referenced by `[access].default_models` or any
`[[client]].models`, for synthetic `/v1/models` output, and for any
model-bearing request. Model-less paths (such as `/v1/embeddings`) use
`path = "..."` instead of `public = "..."`.

Fields:

- `public = "<model-name>"` (string, optional): the public model ID for
  this route. Exactly one of `public` or `path` per block.
- `path = "<request-path>"` (string, optional): the model-less request path
  for this route. Must start with `/`. Exactly one of `public` or `path`
  per block.
- `expose = [...]` (string set, default `[]`): the client API surfaces
  this route accepts. See [routing.md](routing.md#backend-supports) for
  the marker vocabulary. Empty means "any native endpoint or feature
  marker the backend supports"; compatibility paths always require an
  explicit compat marker.
- `backends = [...]` (string list, default `[]`): the upstreams that may
  serve this route. For model-bearing routes each entry is
  `"<model>@<backend>"` (the upstream model name `<model>` served by
  backend `<backend>`); for model-less routes each entry is a bare
  backend id. The list order is priority order for primary selection and
  also seeds the fallback list. The referenced backend must exist in
  `[[backend]]`; otherwise config load fails.
- `context_length` (optional, model-bearing routes only): omitted or
  `"none"` hides the value (the default); an integer literal sets a
  fixed `n_ctx`; `"upstream"` forwards the live value from the first
  backend's `/props?model=<backend_model>`. See
  [Context Length](#context-length) above.
- Per-route policy overrides (each defaults to the backend value):
  `tool_schema_mode`, `responses_store`, `responses_max_output_tokens`,
  `chat_stream_usage`. See [routing.md](routing.md#route-policies) for
  the full semantics.
- `extra_body = { ... }` (inline table, default `{}`): arbitrary fields
  merged into the upstream request body. See
  [Upstream request body overrides](#upstream-request-body-overrides)
  below for the merge rules and protected-key list.

The previous `[[backend.model]]` block and its `endpoints` field are
removed. Any public model name that was previously listed in
`[[backend.model]]` must be re-declared under `[[route]]`, or config
load fails. The `capability` (singular) TOML alias for the backend
marker field is also removed; use `supports = [...]`.

## Upstream request body overrides

`extra_body` lets operators inject arbitrary fields into the upstream
request body sent for a `[[backend]]` (as a default) or a `[[route]]`
(as an override). It exists to surface upstream-specific knobs
without onair having to ship a hardcoded field for each one — the
canonical use case is reasoning toggles like
`reasoning_split = true` on providers that emit inline `<think>` text
in `delta.content` by default.

```toml
[[backend]]
id = "minimax"
base_url = "https://api.minimax.io/v1"
supports = ["chat", "responses", "streaming"]
# Backend-level default: every route bound to this backend inherits
# this unless overridden.
extra_body = { chat_template_kwargs = { enable_thinking = true } }

[[route]]
public = "minimax-m3"
expose = ["chat", "responses"]
backends = ["minimax-m3@minimax"]
# Route-level override: wins over the backend default on key
# conflict; non-conflicting keys from both sides are preserved.
extra_body = { reasoning_split = true, temperature = 0.7 }
```

Merge rules:

- The route's `extra_body` is shallow-merged on top of the bound
  backend's `extra_body`. Route wins on key conflict; non-conflicting
  keys from both sides survive.
- A route that binds to multiple backends takes the **first**
  binding's backend defaults. Operators who need different
  per-binding overrides should split the route.
- The merge is applied **after** onair's own rewrite (model swap,
  `responses_store`, `chat_stream_usage`, etc.), so onair's
  transformations always win.

### Protected keys

The following keys are onair-managed and cannot be overridden by
`extra_body`. Any protected key in `extra_body` is dropped with a
`tracing::warn!` carrying the route label and the offending key name:

- `model` — onair always rewrites this from the public name to the
  bound backend's model id.
- `stream` — onair may force this for SSE-aware stream-usage
  injection.
- `messages`, `input` — onair rewrites between them in compat paths.
- `tools`, `tool_choice` — onair may rewrite schemas per
  `tool_schema_mode`.
- `store` — onair sets this per `responses_store` policy.
- `max_output_tokens`, `max_tokens`, `max_completion_tokens` — onair
  may rename per `responses_max_output_tokens` policy.
- `stream_options` — onair may add `include_usage` per
  `chat_stream_usage` policy.

The protected list is the union of fields onair actually rewrites.
`n`, `logprobs`, `top_logprobs`, `previous_response_id` are *rejected*
in compat paths (see `responses_compat.rs`) but are not in the
protected list because they are not "managed" by onair in the same
sense — `extra_body` is allowed to set them, and onair's existing
rejection logic still applies.

### Format and value types

`extra_body` accepts the same TOML value types as a normal inline
table: strings, integers, floats, booleans, arrays, and nested
tables. Datetimes are accepted but are stringified before they reach
the upstream. NaN and infinity floats are mapped to JSON null on
serialize.

### Hot reload

`extra_body` is hot-reloadable. Changing the value and saving the
config file causes the new map to take effect on the next request
without restarting onair.

### Worked example: `reasoning_split`

A common pattern with providers that emit inline `<think>` text in
`delta.content` is to set a per-request toggle that switches the
response to a structured `delta.reasoning_content` field instead.
The onair-side fix is one line:

```toml
[[route]]
public = "minimax-m3"
backends = ["minimax-m3@minimax"]
extra_body = { reasoning_split = true }
```

If the upstream does not recognize this field, the warn-and-drop
policy on protected keys does not apply (it is not a protected key)
and the field is forwarded as-is. The upstream will either use it
or ignore it; either way onair does not interpret the value.

## Exposing backend errors

By default, onair converts every non-2xx upstream response into a
generic OpenAI-style error envelope and discards the upstream body.
This is the privacy-target default; see
[security.md](security.md#backend-secrecy). Operators who want the
client to see the upstream's actual error body can opt in per
backend and override per route:

```toml
[[backend]]
id = "minimax"
base_url = "https://api.minimax.io/v1"
supports = ["chat", "responses", "streaming"]
# Opt every route bound to this backend in.
expose_backend_errors = true

[[route]]
public = "minimax-m3"
expose = ["chat", "responses"]
backends = ["minimax-m3@minimax"]
# Route override: wins over the backend's default. Use `false` to
# opt a single route back out of a backend that has it on.
expose_backend_errors = false
```

Behavior with the field on:

- The upstream status is mapped through `map_upstream_status`
  (4xx and 429/408 keep their value; other 5xx collapse to
  `502 Bad Gateway`) and returned on the wire.
- The upstream body is forwarded verbatim, capped at 1 MiB. If the
  body is larger than the cap, the request falls back to the
  generic sanitized envelope; the truncation is still recorded in
  debug capture.
- Response headers use a strict allowlist: `content-type`
  (forwarded verbatim from the upstream, defaulting to
  `application/json` when the upstream omits it) and `retry-after`
  (only if the upstream set one). `x-request-id` is always
  echoed by the inbound value via `PropagateRequestIdLayer`; the
  upstream's own `x-request-id` is never forwarded, to avoid
  leaking backend-internal request ids.
- The client never sees `server`, `set-cookie`, `www-authenticate`,
  or `x-ratelimit-*` headers.
- Health-snapshot failures record the **original** upstream
  status, so the operator's health view reflects the true cause.
  Metrics record the **mapped** status the client sees, matching
  the sanitized path.
- The inspector records `exposed_backend_error: true` and the
  request card shows an `exposed` marker in the status column. A
  quick filter ("exposed") is also available.

Behavior with the field off (the default): unchanged from the
pre-existing privacy-target default — non-2xx responses are
replaced with the sanitized OpenAI error envelope.

The field is hot-reloadable. Per-route `Some(value)` wins over the
backend's default; `None` inherits. When a route binds to multiple
backends, the first binding's backend contributes the default,
matching the `extra_body` "first binding wins" rule. Operators
who need different per-binding overrides should split the route.

### Strict-require-route

Every public model referenced anywhere in the config (in
`[access].default_models`, in any `[[client]].models`, or by name in a
now-removed `[[backend.model]]`) must have a matching `[[route]]` block
with `public = "<that name>"`, or config load fails. This is the
operator's signal that the exposure decision was not made; add a
`[[route]]` or remove the model from the client. For example, a client
with `models = ["gpt-4o"]` and no `[[route]] public = "gpt-4o"` fails to
load with:

```text
invalid config: client 'dev' references public model 'gpt-4o' which has
no [[route]] declaration; add a [[route]] block or remove the model
from the client
```

For each entry in `route.backends`, the validator also checks whether
`backend.supports` overlaps with the union of "native markers" implied
by `route.expose` (or the route's compat-marker combinations). When there
is no overlap the validator emits a `tracing::warn!` and the config
still loads; the operator can see the empty candidate set on
`/_onair/operator/config` and act on the warning.

## Capability And Endpoint Marker Validation

`[[backend]].supports` and `[[route]].expose` accept a set of
marker strings that the router matches against `/v1/*` path families,
streaming, tool use, and the two explicit compat paths
(`responses_via_chat_completions`, `chat_completions_via_responses`).
Because these are `BTreeSet<String>`, a typo would otherwise be loaded
silently and only surface as a request-time 404.

onair validates every string against a small known set and applies the
policy set under `[routing]`:

```toml
[routing]
unknown_capability_policy = "warn"   # default
unknown_endpoint_policy  = "warn"   # default
```

- `warn` (default): the unknown marker is reported at `WARN` level on
  load and on every hot reload. The config still loads and runs.
- `error`: the config is rejected. Initial load exits with a non-zero
  status; hot reloads keep the previous config active and log the
  rejection at `WARN`.

The known-marker allowlist covers the structural typo-prone set
(`streaming`, `tools` family, `chat` / `chat_completions` / `completions`,
`responses` / `response`, the two compat markers, `all`) and the path
families enumerated in [routing.md](routing.md) (`embeddings`, `images` /
`image`, `audio`, `files` / `file`, `models` / `model`, `batches`,
`fine_tuning`, `assistants`, `threads`, `vector_stores`, `uploads`, and
their singular forms). Custom path families are not on the list; set
`unknown_*_policy = "warn"` to tolerate them, or extend the
`routing::KNOWN_MARKERS` list to add them.

Error messages name the location, the offending marker, and the full
known list, for example:

```text
invalid config: route 'public=gpt-4o' endpoint 'responses_via_chat_completion'
is not a recognized marker; allowed: all, streaming, chat, chat_completions,
...
```

Both policies are reloaded immediately when the config file changes.

## API Keys

Clients authenticate with OpenAI-style bearer tokens:

```http
Authorization: Bearer <generated-onair-client-key>
```

The recommended onair key shape is:

```text
sk-ona-<fixed-ed25519-style-prefix><43 base64url characters>
```

The fixed `AAAAC3NzaC1lZDI1NTE5AAAAI` prefix mimics the start of the SSH
`ssh-ed25519` public-key wire blob: length-prefixed algorithm name plus the
length prefix for a 32-byte public key. The 43-character suffix should encode
32 random bytes using unpadded base64url, matching the random portion length
of an Ed25519 public key blob. That gives `2^256` possible suffix values,
approximately `1.16e77`, before accounting for any generator mistakes or
operational leakage.

Example generator:

```sh
python3 - <<'PY'
import base64, secrets
prefix = "sk-ona-" + "AAAAC3NzaC1lZDI1NTE5AAAAI"
suffix = base64.urlsafe_b64encode(secrets.token_bytes(32)).decode().rstrip("=")
print(prefix + suffix)
PY
```
