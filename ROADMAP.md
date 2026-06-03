# Roadmap

These items are ordered roughly by dependency and operator value, not by implementation difficulty alone.

## Implemented

- Backend-anonymity-focused OpenAI-compatible reverse proxy behavior: public model visibility, backend model rewriting, header allowlisting, generic backend errors, and backend redirect blocking.
- Responses API compatibility for chat-completions backends, including request translation, response translation, streaming text-event translation, and function-tool request normalization.
- Filesystem config hot reload for runtime policy, with invalid reloads preserving the previous config.
- Local read-only operator API and inspector overview cards for sanitized active config, effective model visibility, runtime state, and telemetry exporter status.
- Local request inspector with live request table, identity column, sortable columns, filter help, quick filters, saved local table-view presets, pause/resume live updates, selected-record JSON copy/download actions, column selection, hover-expanded table values, detail view, bounded SSE replay, per-request timing timelines, and backend-attempt waterfalls with expandable per-attempt detail panes.
- Default-local inspector/operator access that uses the effective client address after trusted-proxy header processing.
- Opt-in debug capture with private filesystem permissions for exact request-body troubleshooting.
- Backend health snapshots from proxied traffic and optional active probes.
- Conservative retry/fallback before response commitment for pre-response connect/send/timeout failures.
- Default-off SQLite persistence for the latest retained inspector records, with restart recovery and per-process writer-thread panic logging. Bodies are still excluded from inspector records.
- Round-robin and weighted-random routing strategies for spreading traffic across multiple backends that serve the same public model.
- Background context-size cache for upstream-mode models: onair polls each owning backend's `/props?model=<backend_model>` on a 60 s interval and forwards the live `default_generation_settings.n_ctx` to `/v1/models` and `/props`. The old `[[backend]].context_length` field and `"inherit"` mode are replaced by a single per-model `"upstream"` policy; failures hide the value until the next successful refresh.

## Priority 1

- Capability-aware health probes beyond the generic configured health path.
- Health-aware routing, circuit breakers, and richer retry policies such as status-code-specific fallback.

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
