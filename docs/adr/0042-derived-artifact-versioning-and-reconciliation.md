# ADR-0042: Derived artifact versioning and library reconciliation

- Status: proposed
- Date: 2026-08-07
- Depends on: ADR-0023 §6 (derived-artifact identity, `content_id`); ADR-0014
  (availability, `unavailable` vs `error`); ADR-0041 (subtitle classification);
  Rule 4.13 (derived artifacts built on demand)
- Gate: **must land before v1.** The columns have to predate the first install.
- Related: Rule 4.5 (deletions), Rule 4.9 (shape before writers)

## Context

Every derived artifact in the product is produced as a consequence of a file
being new or changed. That is correct for the case it was designed for and
wrong for the case that actually recurs: **the code learns to do something new,
and no existing file ever benefits.**

Measured on the dogfood library, 2026-08-07, 24,999 items:

| Gap | Count | Why |
|---|---|---|
| Unclassified subtitles | 23,621 | probed before ADR-0041's classifier existed |
| Unbuilt keyframe maps | 24,992 | built on `playbackInfo`; almost nothing played |
| Probe errors | 3 | failed once, never retried |
| Subtitle errors | 783 (repaired) | needed a hand-written migration to reset |
| `eligible` with no tracks | 53 | classifier confidently wrong |
| `unknown` codec | 20 | classifier honestly uncertain |

Nothing re-reaches any of those without the file changing. `content_id`
(ADR-0023 §6) answers "is the input still the same"; nothing answers "is this
artifact what the current code would produce."

Twice this month a fix required a bespoke repair migration (the 783 reset in
017, and 006/012 before it). That is the pattern this record exists to stop
repeating.

**The timing argument is why this cannot wait.** Pre-v1 with no installs, a
wrong derived artifact can be fixed by wiping the database. That freedom ends at
launch. Adding a version column now costs one integer per artifact kind and
changes no behaviour. Adding it after launch means every artifact predating it
is unversioned, so the mechanism's first job is handling the absence of itself.

## Decision

**Each derived artifact carries the version of the logic that produced it. A
version mismatch marks the artifact stale, never deletes it. Reconciliation
finds gaps; what it does about them is tiered by cost.**

### 1. `derived_version` per artifact kind

An integer constant in code per artifact kind — subtitle classification,
subtitle extraction, keyframe map, probe — stamped onto the row when the
artifact is produced. Bumped by hand in the same commit that changes the
producing logic.

This is an on-disk shape and is decided here under Rule 4.9. It is deliberately
per-kind rather than global: a classifier fix must not mark keyframe maps stale.

### 2. A mismatch means stale, not deleted

**This is the load-bearing sentence.** A stale artifact keeps serving until a
better one exists. A viewer with subtitles today does not lose them because the
extractor improved; they keep the old `.vtt` until the new one is produced.

Re-derivation from an unchanged source can only produce the same or better
output, so a version bump carries no content risk — only compute cost. Any
implementation that deletes on mismatch has misread this decision.

### 3. Re-derivation policy is tiered by cost, because the costs differ by
three orders of magnitude

| Artifact | Measured cost | On version mismatch |
|---|---|---|
| Subtitle classification | 217 ms (header read) | re-derive in background |
| Keyframe map | 130 ms median (index read) | re-derive in background |
| Subtitle extraction | 5 s median, full source read | mark stale, re-derive on next demand |
| Probe | header read | re-derive in background |

Cheap artifacts sweep automatically after a bump — roughly 90 minutes for the
whole library, bounded and cancellable, and nobody notices. Expensive artifacts
are marked stale and re-derived when something next asks for them, which for
extraction means the next time that title is played. Most titles are never
played, so most of that cost is never paid.

That tiering is also what answers "we do not want to re-derive for people who
have no problem." You cannot know who is affected without re-deriving. Lazy,
non-destructive re-derivation means the affected viewer gets the fix the moment
they hit it and everyone else pays nothing.

### 4. Reconciliation finds four kinds of gap

1. **Absent** — no artifact for an item that should have one.
2. **Version-stale** — artifact exists, `derived_version` below current.
3. **Self-declared uncertain** — the producer recorded that it could not decide:
   `unknown`, `unmatched`, and equivalents.
4. **Retryable failure** — `unavailable` per ADR-0014, as opposed to terminal
   `error`.

It does **not** find confidently-wrong results. An artifact that recorded a
definite answer which happens to be false is indistinguishable from a correct
one — see item 7.

### 5. It fills the cheap gaps and reports the expensive ones

Filling is bounded by the tier in item 3. Reporting means a count per gap kind
that an operator or a later health surface can read. Expensive gaps are never
filled without a deliberate invocation.

### 6. Relationship to Rule 4.13, stated rather than papered over

Rule 4.13 says a derived artifact is built when a consumer needs it, and that a
whole-library pass is an explicitly requested operation. A background sweep after
a version bump is in tension with that.

The resolution: **the sweep is triggered by an upgrade, which is a deliberate
act**, not by scanning, and it is confined to the cheap tier. It is bounded,
cancellable, respects the single bulk-reader gate, and pauses with ADR-0014
availability. An unbounded continuous reconciler would violate 4.13 and is
rejected in the alternatives below.

### 7. `nightjar doctor` handles the confidently-wrong cases

Versioning cannot express "the old output was definite and wrong." That needs a
targeted repair keyed on the bug's signature — the 53 `eligible`-with-no-tracks
rows, the 783 `error` rows. Those are the migration 006/012/017 pattern and they
stay that way.

`doctor` is where such repairs live once the product has installs and a
migration is too blunt. It is invoked, never automatic, and it reports what it
would change before changing it.

### 8. Terminal versus retryable, reused not reinvented

ADR-0014 already distinguishes `unavailable` (the mount was gone; retry) from
`error` (the file is unreadable; do not). Reconciliation applies the same split
rather than inventing a second vocabulary (Rule 4.11). The 783 subtitle rows
existed because a timeout was written as `error`; correct classification at the
point of failure is what makes reconciliation able to recover them without a
human.

## Alternatives considered

**No versioning; target self-declared gaps only.** Genuinely tempting, and it
covers the `unknown` case in front of us today — a classifier that learns
`S_TEXT/WEBVTT` can query for `unknown` and fix exactly those rows. Rejected on
the confidently-wrong case: the 53 `eligible` rows are not `unknown`, they are
definite and false, and nothing in the data distinguishes them. Twice this month
that has required a bespoke repair. Accepting the version column now is cheaper
than the third one, and impossible to add cheaply after launch.

**A single global version rather than per-kind.** One integer, simpler. Rejected:
a subtitle classifier fix would mark every keyframe map stale, and the cheap tier
would sweep the library for no reason.

**Delete stale artifacts and re-derive.** Simplest to reason about. Rejected by
item 2: it takes working subtitles away from a viewer to fix a problem they may
not have.

**A continuous background reconciler.** Would keep the library permanently
converged. Rejected under Rule 4.13 and on the evidence of 2026-08-06, when two
whole-library background queues against a degraded array took the server down.
Reconciliation runs on a deliberate trigger.

**Wipe and re-derive via `doctor` instead of versioning.** Works pre-v1, which is
exactly why it is tempting now. Rejected because it does not scale past the first
install: a fix that requires every household to run a full re-derivation is a
support burden, and most of them do not have the problem.

## Consequences

**Good**

- A fix shipped in week two of v1 reaches existing libraries without a bespoke
  migration and without asking anyone to re-scan.
- No viewer loses a working artifact to an improvement.
- The gaps become countable, which is what a health surface would render later
  and what makes "the library is up to date" a checkable claim rather than an
  assumption.
- Expensive re-derivation is paid only by people who actually reach the affected
  content.

**Bad (accepted)**

- One integer per artifact kind on every row, forever, mostly equal to the
  current constant. That is the cost of being able to ask the question at all.
- Bumping the version is a manual step in the same commit as the logic change,
  and a forgotten bump is silent. A test asserting the constant changed whenever
  a producer's output changes is not writable in general; this is discipline, and
  it should be a line on `SLICE_CLOSEOUT.md`.
- Confidently-wrong results still need targeted repairs. Versioning narrows that
  class, it does not remove it.
- The cheap-tier sweep is in tension with Rule 4.13's letter, resolved by
  upgrade-triggered and bounded rather than by exemption.

**Open, deliberately not decided here**

- The health surface itself — reading these counts is a rendering, and it can
  follow. This record only makes the counts exist.
- Whether `doctor` is a subcommand of the server binary or a separate tool.
- Trickplay and any later derived artifact inherit this mechanism; their tiers
  are decided with them.
- What the first reconciliation run costs on a real library. The tier table
  above uses measured per-item figures (217 ms, 130 ms, 5 s) from
  `nightjar-meta/notes/measurement-provenance-2026-08-06.md`, but the sweep
  itself is unmeasured. It must be measured before the cheap tier is enabled by
  default, not assumed from the per-item numbers.
