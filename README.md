# onair

onair is an OpenAI-compatible HTTP reverse proxy router for operating one
public API surface over one or more compatible backends.

## Intent

onair lets a proxy operator expose stable OpenAI-style API keys, model names,
and routing policy without exposing the backend provider, backend URL,
backend model ID, or other obvious backend-specific details to clients. The
privacy target is backend anonymity from ordinary API-visible server
behavior: model listing, model IDs, request/response model fields, headers,
and error bodies should not reveal which backend handled a request.

This is not a full traffic-analysis defense. Timing, throughput, token rate,
model quality, and other behavioral fingerprints can still reveal
information about the backing service. The project focuses on hiding simpler
protocol/configuration leaks while preserving compatibility with
OpenAI-style clients.

Planned work lives in [ROADMAP.md](ROADMAP.md). This README describes the
current behavior and points to detailed operator/contributor references under
[docs/](docs/README.md).

## Behavior Summary

- Clients authenticate with OpenAI-style `Authorization: Bearer ...` headers.
- Each authenticated identity sees only its configured public model whitelist.
- Public model names are mapped to backend model names after access checks pass.
- `/v1/*` requests that are not handled by onair itself can be forwarded to a
  compatible backend when backend capabilities allow it.
- `POST /v1/chat/completion` is accepted as a typo-compatible alias and
  forwarded upstream as `/v1/chat/completions`.
- Native Chat Completions and Responses requests use matching native backend
  capabilities when available.
- Explicit compatibility markers can bridge client `/v1/responses` through
  upstream `/v1/chat/completions`, or client `/v1/chat/completions` through
  upstream `/v1/responses`.
- Native routing is preferred when a route declares the requested native
  endpoint. To force a compatibility path for a model route, omit the native
  endpoint marker and include the relevant compatibility marker.
- `stream: true` responses are proxied as server-sent events, with configured
  backend model names rewritten back to public model names in JSON/SSE
  responses.
- OpenAI-compatible `usage.total_tokens` values are preserved. When a backend
  reports prompt/input and completion/output token counts without a total,
  onair adds the corresponding total to chat-completion and Responses
  JSON/SSE responses.
- Backend errors are converted to generic OpenAI-style errors, and response
  headers are allowlisted before returning to the client.
- OpenTelemetry metrics record request counts, status codes, latency, stream
  duration, backend usage, and token counters when an OpenAI-compatible
  `usage` object is present.
- For streaming Chat Completions-compatible upstreams, an opt-in route policy
  can request usage chunks when clients omit that request option.
- A disabled-by-default local inspector can retain recent request metadata
  and render live timing timelines and backend-attempt waterfalls in a
  browser without storing prompt or completion bodies. Optional
  default-off SQLite persistence restores the latest retained records
  after a process restart.

## Quick Start

Start from `onair.example.toml`:

```sh
cp onair.example.toml onair.toml
cargo run -- --config onair.toml
```

Configuration is TOML. The path comes from `--config`, `-c`, or
`ONAIR_CONFIG`. If none is provided, onair reads `onair.toml` from the current
directory.

A minimal native route looks like this:

```toml
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

[[backend.model]]
public = "gpt-4o-mini"
backend = "llama-3.1-8b-instruct"
endpoints = ["chat", "responses"]
```

See [docs/configuration.md](docs/configuration.md) for the full config model,
hot reload behavior, access rules, context-length metadata, client address
logging, and API key guidance.

## Routing Quick Reference

Compatibility routing is explicit. A native endpoint marker does not imply a
compatibility path.

| Client endpoint | Upstream endpoint | Backend capability | Route marker |
| --- | --- | --- | --- |
| `/v1/chat/completions` | `/v1/chat/completions` | `chat` or `chat_completions` | `chat` or `chat_completions` when `endpoints` is non-empty |
| `/v1/responses` | `/v1/responses` | `responses` | `responses` when `endpoints` is non-empty |
| `/v1/responses` | `/v1/chat/completions` | `chat` or `chat_completions` | `responses_via_chat_completions` in backend `capabilities` or route `endpoints` |
| `/v1/chat/completions` | `/v1/responses` | `responses` | `chat_completions_via_responses` in backend `capabilities` or route `endpoints` |

For example, a Responses-native backend can expose a public Chat Completions
route by using the explicit compatibility endpoint marker:

```toml
[[backend]]
id = "responses-wrapper"
base_url = "http://127.0.0.1:8001"
capabilities = ["responses", "streaming", "tools"]

[[backend.model]]
public = "gpt-4o"
backend = "backend-responses-model"
endpoints = ["chat_completions_via_responses", "tools"]
```

See [docs/routing.md](docs/routing.md) for capability markers, native
preference, compatibility conversions, route-level policies, sticky /
round-robin / weighted-random strategies, fallback attempts, prompt caching,
and tool-call constraints.

## Documentation

- [docs/configuration.md](docs/configuration.md): config file structure,
  startup config path, hot reload, access rules, client address handling,
  context metadata, and API key guidance.
- [docs/routing.md](docs/routing.md): endpoint/capability markers,
  compatibility routing, route policies, sticky / round-robin /
  weighted-random strategies, fallback attempts, prompt caching, and
  request conversion policy.
- [docs/observability.md](docs/observability.md): metrics, debug capture,
  inspector/operator endpoints, health probes, and logging.
- [docs/security.md](docs/security.md): backend anonymity boundary, sanitized
  errors/headers, debug-capture risk, inspector exposure, and secret hygiene.
- [ROADMAP.md](ROADMAP.md): implemented milestones and future work.

## Operator Notes

- onair hot-reloads most routing, access, backend, debug-capture, inspector,
  health, and trusted-proxy settings. Listener bind address, body limit, and
  telemetry exporter settings require restart.
- Debug capture is default-off and writes exact request/upstream bodies when
  enabled. Use it only while reproducing a trusted local issue.
- The inspector and operator endpoints are default-off and local-only unless
  `allow_remote = true`; they expose useful operational metadata and should be
  protected accordingly.
- Backend health is currently an operator signal. It does not automatically
  remove unhealthy backends from routing.
