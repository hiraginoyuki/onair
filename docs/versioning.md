# Versioning policy

onair follows [Pride Versioning](https://pridever.org/) with a
project-specific layout. The version number is `PROUD.DEFAULT.SHAME`
(e.g. `0.1.0`, `0.1.1`, `1.0.0`). The single source of truth is
the `version` field in `[workspace.package]` in `Cargo.toml`. The
release workflow's `meta` job reads it and propagates the value
to flake.nix, the GitHub release tag, and the auto-generated
release notes.

All 4 workspace crates (`onair`, `onair-core`, `onair-obs`,
`onair-proxy`) share the workspace `version` field via
`version.workspace = true`. They are an inseparable proxy and
version together. Per-crate versioning is out of scope and
would warrant a separate decision.

## Quick reference

| Tier | Use when | Operator experience |
| --- | --- | --- |
| **PROUD** | Breaking change to a user-facing surface | Read release notes. Old configs may need migration. |
| **DEFAULT** | New user-facing capability, backward-compat | Read release notes for new features. Existing configs unchanged. |
| **SHAME** | Internal refactor, fix, doc/test update | No operator action needed. |

## PROUD bumps (breaking)

A bump is **PROUD** when **any** of the following happens:

- **Config schema**: a field is removed, renamed, or its type
  changes; a field's semantic changes such that a
  previously-accepted config now errors out or produces a
  meaningfully different result.
- **Public HTTP surface**: a client-facing endpoint is
  removed, renamed, or its response shape changes; an error
  code is removed or its meaning changes.
- **Wire protocol**: an OpenAI-shape response (chat,
  responses, embeddings) is changed in a way that breaks
  conforming clients.
- **CLI**: a flag is removed, renamed, or its semantic
  changes.
- **Nix flake**: a system is removed from `flake.nix`'s
  `systems` list; the consumer pin shape changes; the
  `hashes` attrset semantics change.
- **Backward-incompatible behavior**: an existing config
  value that was previously valid produces a different
  result, even if the config still parses. (E.g. a default
  that was `1.0` becomes `0.5` — the config still parses
  but the outcome is different.)

### PROUD-bump deprecation pattern (lenient)

Every PROUD bump that touches a user-facing surface carries
a **deprecation alias** for at least one DEFAULT release
cycle before removal. The alias is the actual migration
mechanism; the policy is enforced at every PROUD bump.

The recipe for each surface:

- **Config fields**: `#[serde(rename = "new_name", alias = "old_name")]`.
  The old name still parses. The config validator emits
  `tracing::warn!` at config load with the format
  `"field X is deprecated, use Y instead; will be removed in A.B.C"`.
  The decision record schedules the removal.
- **HTTP endpoints**: keep the old route registered to the
  same handler (Axum allows this). The handler emits a
  `tracing::warn!` on first hit. Old clients keep working.
- **CLI flags**: clap's `#[arg(alias = "old_name")]`. The flag
  still works; the parser emits a deprecation warning.
- **Capability markers**: keep the old marker in
  `KNOWN_MARKERS` (the allowlist is enlarged; the routing
  logic accepts both old and new). The validator's
  compat-marker-pair check accepts both.

The deprecation carries forward through subsequent DEFAULT
releases until the **next PROUD bump**, which removes the
alias. This is the standard deprecation pattern. The cost
is ~1-2 lines of code per migrated surface plus 1 line of
deprecation warning plus 1 page of `docs/` update. For
the onair project's current size, the per-PROUD cost is
on the order of 10-20 lines of code plus 1 page of
documentation. Acceptable.

The decision record for the PROUD bump schedules the
removal and points at the migration path. The commit
message summarizes the breaking change for the release
notes (see "Changelog / release notes" below).

## DEFAULT bumps (features, non-breaking)

A bump is **DEFAULT** when:

- A new optional config field is added (with a default that
  preserves the old behavior — existing configs are
  unchanged).
- A new HTTP endpoint is added.
- A new CLI flag is added.
- A new metric, label, or inspector card is added.
- A new capability marker is added to `KNOWN_MARKERS` (the
  allowlist is enlarged; the routing logic accepts both old
  and new).
- Performance improvement with no observable behavior
  change.
- Deprecation warnings added (no removal) — this is the
  transition state before a future PROUD bump.
- Internal refactors that improve code quality without
  changing user-visible behavior (e.g. the `BTreeSet`
  standardization for capability sets was DEFAULT because
  it added an alias path).

## SHAME bumps (fixes, internal)

A bump is **SHAME** when:

- A bug is fixed.
- A backward-compatible security fix (the most common case;
  see "Security disclosures" below).
- A documentation fix.
- A test fix.
- A CI / workflow fix.
- An internal refactor that does not change user-visible
  behavior (e.g. the `parking_lot` migration, the inspector
  god-module split, the C18 v1_proxy header borrow
  refactor, the `replace: true` release body fix).

## Pre-PROUD-1.0 semantics

While `PROUD == 0` (i.e. versions like `0.x.y`), the proxy
is **pre-stable**. DEFAULT bumps **may break** — operators
are expected to read release notes on every DEFAULT release.

The first `PROUD == 1` bump (i.e. `1.0.0`) is reserved for a
deliberate "we are ready to commit to API stability" signal.
It should be a considered release — not just a milestone
marker. The plan for the `1.0.0` release includes a
final "what's stable" audit of every user-facing surface
(likely its own decision record) and a pass to either add
deprecation aliases or mark the surface as final.

## Pre-release and build metadata

- Pre-release tags: `-rc1`, `-rc2`, etc. (e.g. `0.2.0-rc1`).
- Build metadata: `+local-tag` (e.g. `0.2.0+local`).
- The current release workflow only cuts releases on
  `v*.*.*` tags. Pre-release and build-metadata handling
  would need a small workflow tweak to be useful; tracked
  as a known gap. Operators who want an `-rc1` or `+local`
  cycle would need to coordinate with the release workflow
  maintainer.

## Security disclosures

Three tiers, by CVSS-like severity. The CVSS score is a
guideline, not a rule; the operator making the release
decides the tier based on the actual operator-facing impact.

- **SHAME-bump (most common)**: backward-compatible security
  fix. Operators can upgrade without any config change. (E.g.
  a vulnerability in a dependency that's patched in the new
  release.)
- **DEFAULT-bump (uncommon)**: requires operator action to
  apply the fix. E.g. a new required field, a new validation
  step, a deprecated cipher/algorithm that needs to be
  rotated away from. Triggered by CVSS ≥ 7.
- **PROUD-bump (rare)**: critical CVE (CVSS ≥ 9) that
  requires breaking changes in the config schema or wire
  protocol to fully remediate. The deprecation-alias
  machinery applies here. Out of scope for most bugs; rare.

Security disclosures are tracked via
`.local/decisions/<date>-<short-name>.md` with the same
structure as other decisions (intent, why, validation, known
gaps). The release notes should call out security-relevant
commits.

## Decision record hygiene

Every PROUD bump should have a corresponding decision record
under `.local/decisions/` documenting:

- The breaking change (what was changed, where, why)
- The migration path (what operators need to do)
- The operator-facing impact (which configs / scripts /
  workflows break)
- The deprecation-alias schedule (when will the old name be
  removed)
- The validation (how it was tested)
- The known gaps (what wasn't tested)

DEFAULT and SHAME bumps are not required to have decision
records, but the commit message should summarize the change
for the release notes.

The decision record lifecycle:

1. The PROUD bump commit references the decision record in
   its message.
2. The decision record is updated to mark the deprecation
   as active.
3. The next PROUD bump removes the alias and the decision
   record is updated to mark the deprecation as completed.

## Changelog / release notes

- The `release.yml` workflow uses
  `softprops/action-gh-release` with
  `generate_release_notes: true` and `replace: true`
  (the `unstable` tag is force-pushed on every run, so
  `replace: true` prevents the auto-generated body from
  accumulating).
- For PROUD releases, the release notes should have a
  "Breaking Changes" section as the first thing. The
  release workflow does not currently have a hook for
  this; the operator can edit the auto-generated notes
  before publishing, or rely on the decision record being
  linked from a release post. Tracked as a known gap.
- For DEFAULT releases, the release notes should mention
  the new user-facing capability in plain language.
  Auto-generation usually handles this well.
- For SHAME releases, no special action is needed. Auto-
  generated notes are fine.

## Moving tags

Already in place (added in the
`2026-06-08-vxy-moving-tag` decision record). They are:

- `vX.Y.Z` (immutable) — a specific release
- `vX.Y` (moving) — the latest `vX.Y.x` patch release;
  stays put across a minor bump
- `vX` (moving) — the latest `vX.*.*`
- `latest` (moving) — the newest release across all majors
- `unstable` (moving, force-pushed) — the rolling release,
  force-pushed to main HEAD on every release run

Operators pin to the most specific tag that fits their
risk profile: `?ref=v0.1.0` for a frozen version,
`?ref=v0.1` for the latest patch (recommended for most
operators), `?ref=v0` for the latest minor, `?ref=latest`
for the bleeding edge. The README pinning matrix has the
full table.

## Project-history cross-reference

The policy is validated against the project's own recent
history (76 commits since `89e1f83`):

- **Would be PROUD** (with the lenient policy, the rename
  would carry a deprecation alias for one cycle):
  - `cf28f57` route redesign (renamed `capabilities` →
    `supports`, removed `[[backend.model]]`, added
    `[[route]]`). Did NOT carry aliases — old configs
    that used `capabilities` would error out on the new
    version. With this policy, a future similar change
    would add `#[serde(alias = "capabilities")]` for one
    DEFAULT cycle, with a `tracing::warn!` pointing at the
    migration guide.

- **Were DEFAULT** (new user-facing capability with
  backward-compat default):
  - `8af1ef8` `extra_body` config field.
  - `6a898de` `expose_backend_errors` config field.
  - The `apply_data_prologue` SSE refactor (B4 partial in
    the audit).
  - The `vX.Y` moving tag addition.
  - The Windows MSVC build matrix addition.

- **Were SHAME** (internal / fix):
  - The `parking_lot` migration (audit B-slice).
  - The inspector god-module split (`da48d51`).
  - The C18 v1_proxy header borrow refactor.
  - The `replace: true` release body fix.
  - The fmt fixes (`8b07761`, `c76c55f`).
  - The `b6d4c10` onair-obs `parking_lot` migration.

The user can use the policy to retroactively label past
bumps and as the spec for future ones. The next time a
schema change is needed, the deprecation-alias recipe
should be applied.

## Known gaps and follow-ups

- Pre-release and build-metadata handling: the release
  workflow only cuts on `v*.*.*` tags. A small workflow
  tweak to support `-rc1` and `+local` would be useful;
  out of scope here.
- The route redesign `cf28f57` is a PROUD-level change
  that did not carry deprecation aliases. Retroactively
  adding them is out of scope (the past is the past; the
  policy applies going forward). The decision record for
  this policy notes this as a known gap.
- The "Breaking Changes" section in PROUD release notes
  is currently a manual edit; automating it (e.g. via
  a workflow that detects PROUD-level commits and
  prepends a section) is a possible future improvement.
- Operator workflow for "should I bump to 1.0.0?" — the
  policy defers this to a deliberate, considered
  decision. Tracked for future work; out of scope here.

## Secret hygiene

This file intentionally omits secrets, tokens, credentials,
private URLs, private hostnames, raw request bodies,
upstream response bodies, and long logs. Keep it that way
when updating.

## See also

- `.local/decisions/2026-06-08-versioning-policy.md` — the
  decision record for this policy.
- `.local/decisions/2026-06-06-inline-flake-release-metadata.md`
  — the release pipeline design, including the moving tags
  that the policy's "Moving tags" section references.
- `.local/decisions/2026-06-08-vxy-moving-tag.md` — the
  decision to push a `vX.Y` minor-version moving tag.
- `.local/decisions/2026-06-08-release-body-replace-not-append.md`
  — the `replace: true` change that keeps the release body
  bounded.
- [Pride Versioning](https://pridever.org/) — the upstream
  versioning scheme this policy adapts.
