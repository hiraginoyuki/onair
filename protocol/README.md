# LLM Protocol Alpha

> Working name only. This protocol and its Rust reference crates are
> unpublished and may be renamed or redesigned before an external release.

`protocol/` is the normative, language-neutral contract for LLM Protocol Alpha
`0.1.0`. The Rust implementation under `crates/llm-protocol-*` is a reference
implementation, not the source of truth.

## Contents

- [SPEC.md](SPEC.md): scope, terminology, invariants, and conformance rules.
- [profiles/registry.json](profiles/registry.json): frozen initial profile
  identities.
- [schemas/](schemas/): JSON Schema Draft 2020-12 documents.
- [vectors/](vectors/): synthetic conformance vector manifest and fixtures.
- [benchmarks/scenarios.json](benchmarks/scenarios.json): synthetic,
  local-only benchmark scenario manifest. The runner is added in Phase 5.

The cache-report schema covers content-free structural reports. It intentionally
does not model prompt content, raw cache keys, retention values, or default
fingerprints.

All public artifacts in this directory use contract version `0.1.0`.
The reference implementation currently includes frozen OpenAI Chat
Completions, OpenAI Responses, and Anthropic Messages codecs for the selected
typed alpha subset. Each codec decodes protocol envelopes to the shared IR and
encodes it for its own frozen profile, so all six directed dialect pairs cross
the same IR boundary. They do not route production OnAir traffic.

The OnAir parity harness and benchmark execution remain later alpha phases.

## Status

The alpha defines protocol semantics only. It does not route production OnAir
traffic, promise provider cache hits, implement generic HTTP forwarding, or
support uploads and other binary transport.

Fixtures are synthetic. Do not add credentials, private endpoints or
hostnames, debug captures, real prompts, real completions, or live benchmark
observations to this directory.
