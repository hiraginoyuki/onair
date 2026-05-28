# Roadmap

## Priority 1

- Config hot reload with filesystem watching, validation, and atomic runtime swaps.
- Admin Web UI for viewing active config, backends, models, identities, metrics, and health.
- Health checks per backend with compatibility/capability probes.
- Retry and fallback policies before response streaming starts.
- Weighted and priority routing across multiple compatible backends.

## Priority 2

- Per-identity rate limits, token quotas, and spend caps.
- Model aliasing and richer request/response normalization per provider compatibility profile.
- Admin API for model/backend/client lifecycle changes.
- Persistent audit log with body redaction guarantees.
- OpenTelemetry tracing in addition to metrics.

## Priority 3

- Endpoint-specific compatibility fixtures for embeddings, images, audio, batches, files, and other generic `/v1/*` passthrough paths.
- HTTP upgrade/WebSocket support for APIs that cannot be represented by plain HTTP request forwarding.
- Provider-specific compatibility markers for vLLM, LiteLLM, Ollama, LM Studio, Groq, Together, Fireworks, OpenRouter, Azure OpenAI, and similar APIs.
- Optional Prometheus exposition bridge.
- Request queueing, load shedding, circuit breakers, and adaptive routing.
- Multi-tenant policy packs and config provenance tracking.
