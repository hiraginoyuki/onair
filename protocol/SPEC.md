# LLM Protocol Alpha Specification

Version: `0.1.0`
Status: unpublished alpha

## 1. Scope

LLM Protocol Alpha defines a language-neutral intermediate representation (IR)
and profile-scoped wire contracts for translating among selected LLM vendor
protocols. Its normative artifacts are this specification, the JSON Schemas
under `schemas/`, the profile registry, and the synthetic vector manifest.

The protocol owns:

- typed requests, buffered responses, protocol errors, and stream lifecycle
  events;
- profile identity, vendor codec boundaries, and conversion fidelity reports;
- protocol-owned header semantics and exact same-profile replay of unmodified
  JSON/SSE envelopes;
- cache intent representation and later cache-plan analysis.

The protocol does not own proxy routing, authentication, authorization,
retries, failover, generic HTTP forwarding, storage, prompt logging, tool
execution, uploads, retrieval, binary transport, or provider cache-hit
prediction.

## 2. Versioning And Normative Artifacts

The protocol specification, schemas, registry, and vector manifest are all
versioned as `0.1.0`. They are the stable alpha contract. Rust package APIs,
package names, and internal organization are provisional and must use
`publish = false`.

A conforming artifact declares the same `protocol_version`. Additive
clarifications may use a new contract revision only when existing vectors
remain unchanged. A behavioral change to an existing profile requires a new
profile identity or feature profile and new vectors. A profile named `latest`
is invalid.

## 3. Profiles

A profile is the exact wire contract selected for one envelope. It contains:

1. provider;
2. API family;
3. endpoint;
4. vendor version selector when the vendor exposes a real selector;
5. enabled feature set; and
6. library-owned contract revision.

Initial profiles are OpenAI Chat Completions, OpenAI Responses, and Anthropic
Messages. OpenAI profiles use a pinned library contract revision because they
do not have a wire-level version selector in this alpha. Anthropic Messages
uses the selected `anthropic-version` value.

Encoding an envelope under a different profile is a conversion, even when both
profiles use JSON. A codec must not silently reinterpret an old profile using
new provider behavior.

## 4. Intermediate Representation

The IR represents:

- conversation roles and ordered text, image, and document references;
- tool definitions, calls, and results;
- generation controls and JSON Schema output intent;
- request streaming intent;
- reasoning, citations, refusals, usage, finish reasons, and typed errors;
- cache intent;
- request, response, and stream lifecycle state;
- opaque extensions and provider-owned continuation handles.

Assets are references only: a URL, data reference, media metadata, or provider
file reference. Multipart upload, binary transfer, file lifecycle management,
and retrieval are outside this protocol.

The IR describes output-schema intent but does not validate generated model
output against that schema.

Reference codec APIs carry requests, responses, errors, and complete normalized
streams in one shared vendor-neutral payload. The IR schema uses `kind:
"stream"` with an ordered stream-event array for that payload. A source codec
decodes its wire envelope to the shared payload, and the target codec encodes
it. Implementations must not define pair-specific translator APIs as the
conformance boundary.

## 5. Opaque And Provider-Owned Data

An opaque extension has a namespace, issuing profile, source location, and
opaque JSON, text, or bytes payload. Provider-specific or forward-compatible
unknown fields and SSE events must be represented as opaque extensions rather
than discarded.

Unknown ordered content parts are represented as an `opaque` content part
containing such an extension. This preserves their order within a message
without assigning them a portable semantic type.

A provider-owned continuation handle is an opaque extension with an issuing
profile. It can replay only under that exact profile. Cross-profile conversion
must report a non-portability diagnostic. It must not synthesize an equivalent
target continuation handle.

Unknown SSE frames/events may be preserved only for exact same-profile replay.
Cross-profile encoding must report `forward_compatible_unknown` or another
non-portability diagnostic.

An `anthropic-beta` protocol header is opaque material unless an explicit
frozen feature profile defines its semantics. It is retained for exact
same-profile replay. Canonical re-encoding and cross-profile conversion must
not reproduce or infer beta semantics from that header.

## 6. Conversion Fidelity

Every conversion produces a value, when supported, plus a fidelity class and
machine-readable diagnostics:

| Fidelity | Meaning |
| --- | --- |
| `exact` | The target represents the selected semantics without adaptation. |
| `adapted` | The target is intentionally changed by an explicit, recorded adaptation. |
| `lossy` | A known semantic loss occurred and is reported. |
| `unsupported` | No target value is produced for the selected source semantics. |

Callers choose an acceptance policy. The default protocol behavior is not to
accept loss silently. Diagnostics include a stable code, severity, optional
source location, and non-sensitive explanatory text.

## 7. HTTP Envelopes And Replay

An envelope has an issuing profile, HTTP status, raw protocol-owned header
lines, best-effort generic adapter header metadata, and raw JSON or SSE body
bytes.

The initial protocol-owned header names are:

- `content-type`
- `retry-after`
- `anthropic-version`
- `anthropic-beta`

Raw protocol header lines are retained exactly, including their original
casing and whitespace excluding the line terminator. Generic HTTP headers are
not conformance requirements and are not guaranteed for byte-exact replay.

After decode, an unmodified envelope may replay its original protocol body
bytes and protocol-header lines exactly under the exact issuing profile. A
semantic edit consumes the unmodified decoded value and creates a distinct
modified value. A modified value must be canonically encoded by its codec and
cannot reuse retained raw body bytes or raw protocol header lines.

This guarantee does not extend to generic HTTP byte replay, transport framing,
or cross-profile encoding.

## 8. Cache Intent

OpenAI-style request cache keys and retention remain distinct from Anthropic
ordered cache breakpoints. The IR records their source semantics without
claiming they are equivalent.

A cache analyzer constructs a canonical ordered cache-segment plan from
instructions, messages, content parts, tool definitions, output schema,
assets, and explicit directives. Each public segment descriptor contains only
its structural kind and location. The semantic material used for comparison is
not exposed by the plan or its reports.

Source-to-target reports compare the source request plan with the plan obtained
by decoding the canonical target envelope under its target profile. That target
re-decode is authoritative for what the target represents; an encoder may not
infer preservation from the source request alone. Every source descriptor has
exactly one terminal report entry, and every target descriptor occurs exactly
once as a target. Source entries retain source-plan order, followed by target-
only introductions in target-plan order.

Matching is deterministic and consumes each target segment at most once. For
each source segment in source-plan order, implementations first preserve an
identical segment at the same plan ordinal, then select the first unmatched
target with identical descriptor and semantic value as moved, then the first
unmatched target with the same descriptor as changed, and then the first
unmatched target with the same semantic value as moved. If none exists, the
source is dropped. Cross-provider source directives bypass matching and are
non-portable. Remaining target segments are introduced. Semantic equality is
equality of the canonical typed IR value; it does not expose content in the
report.

The stable classifications have these shapes:

- `preserved` uses source and target descriptors with reason `unchanged`;
- `moved` uses source and target descriptors with reason `order_changed`;
- `changed` uses source and target descriptors with reason
  `semantic_value_changed`;
- `dropped` uses a source descriptor and null target with reason
  `no_target_representation`;
- `introduced` uses null source and a target descriptor with reason
  `no_source_representation` or `target_directive_must_be_explicit`; and
- `non_portable` uses a source descriptor, an optional target descriptor, and
  reason `provider_semantics_differ` or
  `target_directive_must_be_explicit`.

These classifications describe ordered cache-relevant request structure, not
provider cache-key equivalence or cache-hit behavior. The arbitrary IR-to-IR
comparison API additionally reports the common stable prefix and remains
explicitly experimental: its comparison semantics may change as codec
consumers establish the required distinctions.

Ordinary conversion must never synthesize a target cache directive. An
analyzer may recommend a target-specific plan; applying it is an explicit
`adapted` operation with recorded changes. The protocol does not predict
provider cache hits or make cross-provider cache-hit guarantees.

When a typed request is otherwise representable but its source cache intent
uses a different provider family, conversion returns the usable target request
without a synthesized target directive. Its fidelity is `lossy`, and the
content-free cache report marks the source directives `non_portable` with
reason `provider_semantics_differ`. Cross-provider directives are not matched
as equivalent segments. Ordinary conversion must yield no target cache
directive; any target-only directive would be reported as introduced and is a
conformance failure unless it came from a separate explicit plan application.

Cache reports must not include prompt content or a fingerprint by default.
Deployment-local correlation may use caller-supplied HMAC-SHA-256 keys. Plain
SHA-256 is permitted only for explicitly synthetic public fixtures. HMAC
correlation binds each segment's descriptor and semantic material; different
caller keys must produce isolated correlation values.

## 9. Streaming

The normalized stream lifecycle includes:

- request and message start;
- output-part start and end;
- text and reasoning deltas;
- refusal and citation parts;
- tool-call deltas;
- usage;
- terminal state; and
- streamed error.

Full streaming support means the selected typed alpha subset and exact
same-profile replay of retained unknown frames. It does not promise emulation
of every future, beta, signed, encrypted, or undocumented provider event.
Signed, encrypted, or vendor-only payloads may remain opaque for
same-profile replay and must not be manufactured as portable equivalents.

The reference core includes generic incremental SSE framing. It accepts
arbitrary byte partitions, recognizes CRLF and LF line endings, dispatches
events at blank lines, and preserves ordered parsed fields for codec use. It
does not normalize provider event names or decode provider JSON; codecs own
those semantics. For a complete byte stream, resulting frames and terminal
framing behavior must be invariant across byte chunk partitions.

## 10. Conformance

A conforming implementation must:

1. validate declared profiles and vectors against the `0.1.0` schemas;
2. preserve profile identity and raw replay eligibility;
3. report fidelity and diagnostics for every conversion;
4. preserve opaque data for exact same-profile replay or report its
   non-portability;
5. never claim unmodified raw replay after a semantic edit; and
6. keep committed vectors synthetic.

Every active envelope vector contains one complete wire envelope and two
independent expectations. `expect.decode` freezes the source fidelity, the
complete ordered diagnostic sequence, and the decoded profile-scoped IR.
`expect.encode` freezes target fidelity, complete ordered diagnostics, the
complete target envelope when encoding is supported, the complete target IR
obtained by decoding that canonical envelope, and the complete content-free
cache report when one is produced. Unsupported encoding omits both the target
envelope and target re-decode rather than supplying placeholders.

Diagnostic expectations contain code, severity, and source location. Human
messages are deliberately not conformance fields and may improve without a
contract revision. Diagnostic arrays are ordered and may contain repeated
codes at distinct or repeated locations; implementations must not compare them
as sets.

Cache-analysis vectors contain a complete typed operation rather than an
informal intent fragment. Their expected analysis includes fidelity,
diagnostics, resulting request IR, and the full cache report. IR and envelope
expectations are validated against their normative schemas. The committed
expected JSON is authoritative; Rust serialization and the fixture-maintenance
tool are not independent protocol contracts.

The vector manifest must not contain duplicate identifiers or paths. Every
committed vector document must occur exactly once in the manifest, and every
active entry must execute through the same conformance runner. A document that
only validates against JSON Schema but is not executed is not an active
conformance vector.

The manifest declares the frozen coverage-matrix profiles, payload classes,
and `all_distinct_pairs` direction rule. Alpha `0.1.0` has three profiles and
five classes (`request`, `response`, `error`, `stream`, and `cache_report`), so
the matrix contains thirty directed cells. Active vectors claim cells with an
explicit `supported` or `unsupported` classification. Every declared cell must
be claimed exactly once; undeclared, duplicate, or missing cells fail
conformance.

`supported` means that encoding produces a target envelope. Its fidelity may
be `Exact`, `Adapted`, or `Lossy`; support does not imply semantic identity.
`unsupported` requires `Unsupported` fidelity and no target envelope. A
`cache_report` claim requires request IR and a complete content-free report.
Same-profile replay vectors and generic pair smoke tests do not satisfy a
directed matrix cell.

The manifest also declares every typed content-part and stream-event variant.
Each declared feature has exactly one `source_decode` claim and one
`cross_profile` claim. A source-decode claim is valid only when the complete
decoded source IR contains that feature. A cross-profile claim is checked
against the ordered feature occurrences in the source IR and in the target IR
obtained by decoding the canonical target envelope.

Cross-profile feature dispositions are `preserved`, `adapted`, `lossy`,
`unsupported`, or `non_portable`. Preserved occurrences are identical and in
the same order. Adapted occurrences remain typed but differ in a recorded way.
Lossy occurrences are changed or absent and require Lossy conversion fidelity.
Unsupported features produce no target envelope. `non_portable` applies only
to opaque content or stream events: target IR must not contain the opaque
material, and the conversion must report its non-portability. Complete source
and target IR expectations remain normative even when an individual claim
selects one feature from those ordered documents.

The full alpha gate additionally tests declared codec paths, arbitrary SSE byte
partitions, all six directed dialect conversions for the typed subset, cache
reports, and selected OnAir parity. Live provider benchmarks are manual,
local-only, dry-run by default, cap-controlled in live mode, and never gate CI
or releases.
