# Roadmap

These items are ordered roughly by dependency and operator value, not by implementation difficulty alone.

## Implemented

- Local read-only operator API and inspector overview cards for sanitized active config, effective model visibility, runtime state, and telemetry exporter status.
- Local request inspector with live request table, detail view, bounded SSE replay, and per-request timing timelines.
- Backend health snapshots from proxied traffic and optional active probes.
- Conservative retry/fallback before response commitment for pre-response connect/send/timeout failures.

## Priority 1

- Capability-aware health probes beyond the generic configured health path.
- Richer operator telemetry snapshots beyond exporter status.
- Health-aware routing, circuit breakers, and richer retry policies such as status-code-specific fallback.
- Weighted backend selection for compatible routes.

## Priority 2

- Provider compatibility profiles with richer request/response normalization per backend family.
- Endpoint-specific compatibility fixtures for embeddings, images, audio, batches, files, assistants, threads, and other generic `/v1/*` passthrough paths.
- HTTP upgrade/WebSocket support for APIs that cannot be represented by plain HTTP request forwarding.
- OpenTelemetry tracing in addition to metrics.

## Priority 3

- Per-identity rate limits, token quotas, and spend caps.
- Admin API and Web UI for model/backend/client lifecycle changes beyond the local request inspector.
- Persistent inspector/audit log with body redaction guarantees.
- Request queueing, load shedding, and adaptive routing.
- Multi-tenant policy packs and config provenance tracking.
- Optional Prometheus exposition bridge.
