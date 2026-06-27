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
- Per-route `expose_backend_errors` opt-in for forwarding non-2xx upstream error bodies to the client (status mapped through `map_upstream_status`, body capped at 1 MiB, strict header allowlist of `content-type` + `retry-after`). Default is off and preserves the privacy-target default that converts non-2xx upstream responses to a generic OpenAI error envelope.
- Project versioning policy: Pride Versioning (`PROUD.DEFAULT.SHAME`) with a lenient deprecation-alias pattern for PROUD bumps, CVSS-tiered security disclosures, and explicit SHAME-for-internals rules. Documented at `docs/versioning.md`.
- Streaming debug capture (response side). Per-route/per-backend `stream_capture = true` opt-in records `upstream_response.ndjson` and `client_response.ndjson` per streaming request, with per-event `ts_us`, parsed SSE frames on the client side, and per-side `timings` summaries in `metadata.json`. Async best-effort writer (256-event bounded channel, 500 ms drain budget, `try_send` only on the hot path), never blocks the stream. Default off. Documented at `docs/observability.md#streaming-debug-capture` and `.local/decisions/2026-06-27-streaming-debug-capture.md`. P2 (request-side capture) and P4 (size-cap / retention) deferred — see `.local/plans/streaming-debug-capture-plan.md`.

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

## Public Route Redesign

- **Date:** 2026-06-05 JST
- **Commit:** `cf28f57`

The "what client API surface is exposed for a public model" decision is
lifted out of `[[backend.model]]` and into a top-level `[[route]]` block.
The new shape is route-driven: each `[[route]]` block declares one
public-facing model (or one model-less path) and the backends that can
serve it, so the exposure decision is made once per public model name
instead of being duplicated per backend. `[[backend]].capabilities` is
renamed to `[[backend]].supports` (the `capability` singular alias is
removed), and the old `[[backend.model]]` block with its `endpoints`
field is gone. Model-bearing routes use `public = "..."` with
`backends = ["<model>@<backend>", ...]` (the `model@backend` syntax
reads as "upstream model name `model` served by backend `backend`");
model-less routes use `path = "..."` with bare backend ids in
`backends`. `expose = [...]` lists the client API surfaces the route
accepts. A strict-require-route rule rejects any public model that
lacks a matching `[[route]]` (the operator's signal that the exposure
decision was not made), and the validator emits a `tracing::warn!` when
an entry in `route.backends` has no overlap between `backend.supports`
and the markers implied by `route.expose`.
