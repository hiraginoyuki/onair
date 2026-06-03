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
capabilities = ["chat", "responses", "streaming", "tools"]
timeout_ms = 120000

[[backend.model]]
public = "gpt-4o-mini"
backend = "llama-3.1-8b-instruct"
context_length = "upstream"
endpoints = ["chat", "responses"]
```

onair interprets the file as routing and visibility policy:

- `[server]`, `[telemetry]`, `[debug_capture]`, `[inspector]`, and `[health]`
  configure process-level behavior.
- `[access].default_models` grants public models to every configured client.
- Each `[[client]]` adds one authenticated identity and may extend that
  identity's model whitelist.
- Each `[[backend]]` defines one upstream OpenAI-compatible service plus
  capability markers that decide which `/v1/*` request families it can
  receive.
- Each `[[backend.model]]` maps one public model name to the backend model name
  that upstream should receive.
- `[routing]` chooses how to select among multiple compatible backend routes
  and how many fallback attempts to allow before response commitment.

See [routing.md](routing.md) for capability and endpoint marker semantics.
See [observability.md](observability.md) for telemetry, debug capture,
inspector, and health details.

## Hot Reload

onair watches the config file's parent directory and reloads the config when
the file changes. Save bursts and atomic replacement writes are debounced
before loading. Reloads are validated before they are applied, and invalid TOML
or invalid model/client/backend rules keep the previous config active.

Reloaded immediately:

- `[access]`, `[[client]]`, `[[backend]]`, `[[backend.model]]`,
  `[routing]`, `[debug_capture]`, `[inspector]`, `[health]`,
  `[server].trusted_proxy_cidrs`, backend auth, model mappings, capabilities,
  timeouts, context metadata, client-address trust policy, debug capture
  settings, inspector runtime settings, and health probe settings.

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

- Omitting `[[backend.model]].context_length` or setting it to `"none"` hides
  the value, which is the default OpenAI-compatible behavior. No metadata is
  exposed for that public model in `/v1/models` or `/props`.
- `[[backend.model]].context_length = <integer>` returns a fixed value for
  that public model. `/v1/models` and `/v1/models/{model}` expose
  `meta.n_ctx` and `meta.n_ctx_train`, both equal to the configured integer.
- `[[backend.model]].context_length = "upstream"` forwards the live context
  size from the backend that owns this route. onair issues a background
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
  exact field. Move the value to a `[[backend.model]].context_length` entry
  (either an integer or `"upstream"`).
- The old `"inherit"` mode is no longer accepted. Replace it with
  `"upstream"` to forward the live value, or with the desired integer if
  the value is known at config time.

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
