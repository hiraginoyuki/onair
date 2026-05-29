# onair

onair is an OpenAI-compatible HTTP reverse proxy router. It accepts OpenAI-style bearer tokens, exposes only the models each authenticated identity is allowed to see, routes requests to compatible backends, and avoids leaking backend-specific details in client-facing errors or response headers.

## Day-one scope

- `GET /v1/models` returns an identity-filtered OpenAI-style model list.
- `GET /v1/models/{model}` returns a synthetic model object for allowed public model IDs.
- Any HTTP method under `/v1/*` can be forwarded to a compatible backend, including OpenAI-compatible endpoints that are not explicitly modeled in onair yet.
- `POST /v1/chat/completion` is accepted as a typo-compatible alias and forwarded upstream as `/v1/chat/completions`.
- `stream: true` responses are proxied as server-sent events; JSON/SSE model fields are normalized so backend model names are rewritten to public model names.
- Model access is whitelist-style. If a known client requests a model outside its whitelist, onair returns `404` with an OpenAI-style `model_not_found` error rather than an authorization error.
- Backend error bodies and backend response headers are not forwarded. Client responses keep only sanitized headers needed for API compatibility plus a client-supplied `x-request-id` echo.
- OpenTelemetry metrics cover request counts, status codes, latency, stream duration, backend usage, and token counters when an OpenAI-compatible `usage` object is present.

## Configuration

Start from `onair.example.toml`:

```sh
cp onair.example.toml onair.toml
cargo run -- --config onair.toml
```

Configuration is TOML. The core sections are:

```toml
[server]
bind = "127.0.0.1:8080"
request_body_limit_bytes = 2097152

[telemetry]
service_name = "onair"
exporter = "none" # or "otlp"
otlp_endpoint = "http://127.0.0.1:4317"
export_interval_ms = 30000

[routing]
# "priority" chooses the first matching backend. "sticky" hashes identity/path/model/prompt_cache_key
# across all matching backends, improving prompt-cache locality when several backends serve a model.
strategy = "priority"

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
capabilities = ["chat", "responses", "streaming"]
timeout_ms = 120000

[[backend.model]]
public = "gpt-4o-mini"
backend = "llama-3.1-8b-instruct"
context_length = "inherit"
endpoints = ["chat", "responses"]
```

### Access rules

- Every `[[client]]` must have `api_key` or `api_key_env`.
- A client's effective model whitelist is `[access].default_models` union `[[client]].models`.
- `/v1/models` only lists the authenticated client's effective whitelist intersected with configured backend routes.
- `/v1/models/{model}` only returns model objects for authenticated clients that can access that configured public model.
- Requests for models outside the effective whitelist return `404`, intentionally indistinguishable from a missing model.
- Requests for whitelisted models with no compatible backend route also return `404`.
- For model-bearing requests, onair detects `model` in JSON bodies, URL-encoded forms, multipart form fields, and query strings.
- Public model IDs are rewritten to backend model IDs in JSON bodies, URL-encoded forms, multipart form fields, and query strings before forwarding.

### Backend capabilities

`[[backend]].capabilities` is a marker list; `capability` is accepted as a TOML alias. Capability markers are matched against `/v1/*` path families and common aliases:

- `chat` or `chat_completions` for `/v1/chat/completions`.
- `responses` for `/v1/responses`.
- `embeddings` for `/v1/embeddings`.
- `images` or `image` for `/v1/images/*`.
- `audio` for `/v1/audio/*`.
- `files` or `file` for `/v1/files/*`.
- `batches`, `fine_tuning`, `assistants`, `threads`, `vector_stores`, `uploads`, and similar first path segments.
- `streaming` for `stream: true` requests.
- `all` as a broad marker for any `/v1/*` path.

`[[backend.model]]` entries are optional for backends that only serve model-less endpoints. They are required for model-bearing requests, synthetic `/v1/models` output, and public-to-backend model rewrites.

`[[backend.model]].endpoints` can further restrict a model route to endpoint keys such as `chat`, `chat_completions`, `responses`, `audio`, or `embeddings`. If omitted or empty, the model route is allowed for any endpoint supported by the backend. Backend order is priority order when multiple compatible routes match, and also for model-less requests.

Set `[routing].strategy = "sticky"` when multiple backends serve the same public model and you want cache-heavy traffic to keep landing on the same backend. The sticky key is derived from identity, path, public model, and `prompt_cache_key` when provided. The router still forwards `prompt_cache_key` and `prompt_cache_retention` unchanged.

### Context length

- `[[backend]].context_length` sets a backend-level default context length for inheritance.
- `[[backend.model]].context_length = "inherit"` copies the backend-level value into the public model listing.
- `[[backend.model]].context_length = <integer>` returns a specific value for that public model.
- `[[backend.model]].context_length = "none"` or omitting the field entirely hides the value, which is the default OpenAI-compatible behavior.
- If you use `"inherit"` without a backend-level `context_length`, config loading fails.

For a backend that should receive any OpenAI-compatible HTTP route it supports, use:

```toml
[[backend]]
id = "openai"
base_url = "https://api.openai.com"
api_key_env = "OPENAI_API_KEY"
capabilities = ["all", "streaming"]
```

### Backend secrecy

- `/v1/models` and `/v1/models/{model}` are synthesized from public config; backend model IDs are not listed.
- Model-bearing requests are rewritten to backend model IDs only after access checks pass.
- Successful JSON and SSE responses rewrite backend model IDs back to public model IDs when a model mapping is known.
- Non-success backend responses are converted to generic OpenAI-style errors; backend error bodies are discarded.
- Response headers use an allowlist. onair keeps useful API headers such as `content-type` and `content-disposition`, sets its own cache policy, and echoes only a client-supplied `x-request-id`.
- onair does not try to hide timing, throughput, token rate, or other behavioral fingerprints.

### Prompt caching

- Prompt caching is backend-defined and works best when the backend is OpenAI or another provider with compatible cache behavior.
- onair preserves `prompt_cache_key` and `prompt_cache_retention`, and only rewrites configured public model IDs to backend model IDs.
- `prompt_cache_key` also participates in sticky routing when `[routing].strategy = "sticky"`.
- Static prompt prefixes, tools, schemas, images, and their ordering must remain stable for backend cache hits.
- Extended cache retention such as `prompt_cache_retention = "24h"` may have data-retention implications and should only be enabled for backends/models where you intend that behavior.

## API keys

Clients authenticate with OpenAI-style bearer tokens:

```http
Authorization: Bearer sk-ona-AAAAC3NzaC1lZDI1NTE5AAAAI...
```

The recommended onair key shape is:

```text
sk-ona-AAAAC3NzaC1lZDI1NTE5AAAAI<43 base64url characters>
```

The fixed `AAAAC3NzaC1lZDI1NTE5AAAAI` prefix mimics the start of the SSH `ssh-ed25519` public-key wire blob: length-prefixed algorithm name plus the length prefix for a 32-byte public key. The 43-character suffix should encode 32 random bytes using unpadded base64url, matching the random portion length of an Ed25519 public key blob. That gives `2^256` possible suffix values, approximately `1.16e77`, before accounting for any generator mistakes or operational leakage.

Example generator:

```sh
python3 - <<'PY'
import base64, secrets
prefix = "sk-ona-AAAAC3NzaC1lZDI1NTE5AAAAI"
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
- `onair.tokens`: counter labeled by `direction=input|cached_input|output` when backend responses include OpenAI-compatible usage data such as `prompt_tokens`, `completion_tokens`, `input_tokens`, `output_tokens`, or `cached_tokens`.

Prompt and response bodies are not logged by onair.
