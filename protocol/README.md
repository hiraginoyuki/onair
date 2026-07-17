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
  local-only benchmark scenario manifest.
- [preview.html](preview.html): static local documentation preview/index.

The cache-report schema covers content-free structural reports. It intentionally
does not model prompt content, raw cache keys, retention values, or default
fingerprints.

Active envelope vectors freeze both sides of the IR boundary: complete source
IR, complete target wire envelopes, and complete target IR obtained by
re-decoding canonical output, plus ordered diagnostics and cache reports.
Cache-analysis vectors carry a complete typed cache-plan
application and resulting request IR. The shared test-only conformance runner
rejects duplicate or orphaned vectors and executes every active manifest entry.
An ignored maintenance test can regenerate expectations only when explicitly
enabled with `LLM_PROTOCOL_BLESS=1`; generated changes still require review
because the committed vectors, not Rust serialization, are normative.

The manifest also declares a machine-checked coverage matrix. All six directed
profile pairs must cover requests, buffered responses, protocol errors,
streams, and cache reports, for thirty cells total. Each cell is claimed once
with an explicit support classification. `supported` cells may still be
adapted or lossy; unsupported cells must produce no target envelope.

A second machine-checked matrix covers all nine typed content-part variants and
all thirteen stream-event variants. Every feature has separate source-decode
and cross-profile disposition evidence. Ordered target semantics are checked by
re-decoding the canonical target envelope; opaque content and events can only
claim explicit non-portability across profiles.

All public artifacts in this directory use contract version `0.1.0`.
The reference implementation currently includes frozen OpenAI Chat
Completions, OpenAI Responses, and Anthropic Messages codecs for the selected
typed alpha subset. Each codec decodes protocol envelopes to the shared IR and
encodes it for its own frozen profile, so all six directed dialect pairs cross
the same IR boundary. They do not route production OnAir traffic.

`llm-protocol-onair-parity` is a test-only workspace crate. It compares a
selected buffered request/response subset against public current `onair-core`
compatibility entry points using synthetic inputs; it does not route production
traffic through the alpha codecs. Each selected parity assertion projects the
raw target JSON through an independent test-only semantic DTO and compares both
implementations with a literal expected value; alpha target decoders are not the
parity oracle. The projection normalizes only named representation differences,
including system/developer instruction placement, provider-generated response
metadata, and provider-specific cache-usage detail. It intentionally leaves
streaming proxy normalizer parity out of scope because alpha stream conformance
already tests the codec lifecycle under arbitrary byte partitioning.

The same crate exposes `llm-protocol-benchmark`. It reads this tracked
synthetic manifest and defaults to dry-run without network activity. Live mode
requires `--live`, `--confirm-live`, a local-only JSON configuration beneath
`.local/` when it is stored inside this checkout (an external local path is
also accepted), hard call/input/output caps that cover all selected
scenario/profile work, and a local-only output path beneath `.local/`. It
generates synthetic protocol requests and writes only redacted outcome, status,
latency, profile identity, and observed-or-inconclusive cache token totals. It
never treats cache observations as portability evidence.

## Status

The alpha defines protocol semantics only. It does not route production OnAir
traffic, promise provider cache hits, implement generic HTTP forwarding, or
support uploads and other binary transport.

Fixtures are synthetic. Do not add credentials, private endpoints or
hostnames, debug captures, real prompts, real completions, or live benchmark
observations to this directory.
