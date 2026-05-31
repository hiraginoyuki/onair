# Roadmap

These items are ordered roughly by dependency and operator value, not by implementation difficulty alone.

## Implemented

- Backend-anonymity-focused OpenAI-compatible reverse proxy behavior: public model visibility, backend model rewriting, header allowlisting, generic backend errors, and backend redirect blocking.
- Filesystem config hot reload for runtime policy, with invalid reloads preserving the previous config.
- Local read-only operator API and inspector overview cards for sanitized active config, effective model visibility, runtime state, and telemetry exporter status.
- Local request inspector with live request table, identity column, sortable columns, filter help, quick filters, column selection, hover-expanded table values, detail view, bounded SSE replay, per-request timing timelines, and backend-attempt waterfalls.
- Default-local inspector/operator access that uses the effective client address after trusted-proxy header processing.
- Opt-in debug capture with private filesystem permissions for exact request-body troubleshooting.
- Backend health snapshots from proxied traffic and optional active probes.
- Conservative retry/fallback before response commitment for pre-response connect/send/timeout failures.

## Priority 1

- Inspector usability from live testing: pause/resume live updates, copy/export of selected request JSON, saved table presets, and denser per-attempt detail controls.
- Capability-aware health probes beyond the generic configured health path.
- Health-aware routing, circuit breakers, and richer retry policies such as status-code-specific fallback.
- Weighted backend selection for compatible routes.

## Priority 2

- OpenTelemetry tracing in addition to metrics, with export shaped around existing request timelines and backend attempts.
- Optional trace export for local inspection tools after the inspector data model settles.
- Provider compatibility profiles with richer request/response normalization per backend family.
- Endpoint-specific compatibility fixtures for embeddings, images, audio, batches, files, assistants, threads, and other generic `/v1/*` passthrough paths.
- HTTP upgrade/WebSocket support for APIs that cannot be represented by plain HTTP request forwarding.

## Priority 3

- Per-identity rate limits, token quotas, and spend caps.
- Admin API and Web UI for model/backend/client lifecycle changes beyond the local request inspector.
- Persistent inspector/audit log with body redaction guarantees.
- Request queueing, load shedding, and adaptive routing.
- Multi-tenant policy packs and config provenance tracking.
- Optional Prometheus exposition bridge.
