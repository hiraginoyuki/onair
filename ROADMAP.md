# Roadmap

These items are ordered roughly by dependency and operator value, not by implementation difficulty alone.

## Priority 1

- Read-only operator API for active config, backend health, effective model visibility, and telemetry snapshots.
- Health checks per backend with compatibility and capability probes.
- Retry and fallback policies before response streaming starts.
- Weighted backend selection for compatible routes.

## Priority 2

- Provider compatibility profiles with richer request/response normalization per backend family.
- Endpoint-specific compatibility fixtures for embeddings, images, audio, batches, files, assistants, threads, and other generic `/v1/*` passthrough paths.
- HTTP upgrade/WebSocket support for APIs that cannot be represented by plain HTTP request forwarding.
- OpenTelemetry tracing in addition to metrics.

## Priority 3

- Per-identity rate limits, token quotas, and spend caps.
- Admin API and Web UI for model/backend/client lifecycle changes.
- Persistent audit log with body redaction guarantees.
- Request queueing, load shedding, circuit breakers, and adaptive routing.
- Multi-tenant policy packs and config provenance tracking.
- Optional Prometheus exposition bridge.
