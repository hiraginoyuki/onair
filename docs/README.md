# Documentation

These docs expand the operator and contributor context that would make the
top-level README too long. They describe current behavior, not future plans.
Planned work belongs in [../ROADMAP.md](../ROADMAP.md).

## Files

- [configuration.md](configuration.md): config file structure, startup config
  path, hot reload, access rules, client address handling, context metadata,
  and API key guidance.
- [routing.md](routing.md): endpoint/capability markers, compatibility
  routing, route policies, sticky / round-robin / weighted-random
  strategies, fallback attempts, prompt caching, and request conversion
  policy.
- [observability.md](observability.md): metrics, debug capture,
  inspector/operator endpoints, health probes, and logging.
- [security.md](security.md): backend anonymity boundary, sanitized
  errors/headers, debug-capture risk, inspector exposure, and secret hygiene.

## Documentation Rules

- Keep [../README.md](../README.md) as the fast entry point: intent, behavior
  summary, quick start, routing matrix, and links to deeper docs.
- Keep stable operator behavior in tracked docs.
- Keep future work and planning in [../ROADMAP.md](../ROADMAP.md).
- Keep local task notes, handoffs, debug summaries, and decision records in the
  repository-local `.local/` directory.
- When source behavior changes, update the matching docs in the same logical
  change.
