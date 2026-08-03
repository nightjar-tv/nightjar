# ADR-0032: Multi-exact collision resolution order

- Status: **proposed** (matching-policy question — accept before coding the
  episode-title tie-break)
- Date: 2026-08-03
- Amended: 2026-08-03 — ID is a search precondition, not a ladder step;
  distinctive = rejection list; tied-candidate cap; reference-episode
  preference
- Depends on: ADR-0026 §2 (floor + existing TV collision pin); ADR-0028
  (manual fix); ADR-0031 §7 coverage sample (soft-key re-run)
- Related: `notes/tmdb-show-coverage-sample-2026-08-03-softkey.md`;
  `notes/episode-title-availability-2026-08-03.md`
- Numbering: next free after 0031. **0027 remains artwork.**

## Context

After show soft-key normalisation (case, `&`/`and`, dashes, regional
parens), the 50-show TMDB coverage sample is still **47/50** auto-match.
The three residues — Will & Grace, Top Gear, Shameless — are the same
class: multi-exact title collision. The scorer correctly stays at 0.72
(`exact_title_collision_unpinned`) rather than guessing. That is not a
defect.

ADR-0026 §2 already pins TV multi-exact ties with, in order: premiere
year → episode count → season count. Coverage residues show year alone is
not enough when candidates share a premiere year or the library year is
absent; count pins do not fire on these three. Episode titles on disk are
abundant (~21.8k distinctive after-token titles in dogfood) and are the
natural next discriminator — **but only when they actually differ across
tied candidates**.

Top Gear is the honesty check: local S01E01 is often `Episode 1`, and
TMDB's 1977 and 2002 entries likely share that placeholder. Episode title
does not break the tie; year (or an explicit ID) does. So episode title is
complementary to year and ID, not a replacement — and this ADR must admit
that its own title tier will not fix the case that motivates looking past
year alone.

Manual fix (ADR-0028) remains the last resort after the ladder fails.

## Decision needed

Two shapes, not one table:

1. **When is search skipped entirely?**
2. **When search returns a multi-exact title tie, what discriminators run,
   in what order, and when does a step decline?**

### Precondition (outside the ladder): explicit provider ID

An explicit TMDB tv id (NFO / user override) is a **user assertion of
identity**, not inference. It is **not** a collision discriminator.

**If an ID is present: do not search. Fetch that show.** There is no tied
set and no year/title ladder. Putting ID after year would let a wrong
folder year out-vote an ID the operator typed to fix that exact problem,
and would break the ecosystem escape hatch ("when matching is wrong, add
the ID and it stops arguing").

ID among a search result set ("filter the tied candidates to this id") is
the wrong shape and is rejected here.

### Collision ladder (search already ran; ≥2 exact-title hits)

| Order | Discriminator | Fires when | Cost |
|---|---|---|---|
| 1 | Premiere year (existing ADR-0026 pin) | Library year uniquely matches one candidate's `first_air_date` year | Search fields only |
| 2 | Episode-title tie-break | ≥2 candidates still tied; a usable local reference episode exists (see below); fetch that episode's name for each tied candidate; **exactly one** matches | Detail calls bounded by the tied-set cap |
| — | Manual fix (ADR-0028) | Still unpinned | Operator |

If a ladder step selects zero or two-or-more candidates, try the next. If
none pin, stay unmatched at 0.72 / `exact_title_collision_unpinned` (path
`item_key`). Do not lower the floor. Do not silently pick top-1.

Existing ADR-0026 episode-count / season-count pins remain available as
already-shipped discriminators between year and title unless a later amend
retires them; this ADR does not reopen that table — it adds the title step
and relocates ID out of the ladder.

### Episode-title step constraints (if accepted)

**When it may fire:** multi-exact title ties that survived year (and any
still-active count pins), with tied-candidate count **≤ 5**. Above that
cap the step **declines outright** (pathological title must not fan out
to 2N detail calls). "2–3 detail calls on the rare residue" is the
expectation for normal collisions, not a substitute for the cap.

**Reference episode:** prefer a local episode whose after-token title is
**usable** under the rejection list below. Prefer mid-season (or any
non-pilot) usable episode over S01E01 / 1x01. Use S01E01 only if it is
usable — pilots are the most likely to be placeholder-titled or shared
across original/reboot pairs. If no usable reference episode exists, the
step declines.

**Usable vs rejected (explicit list, not a cleverness test):**

Reject (do not use as reference; if the only candidates are rejected, the
step declines rather than guesses):

- `Episode N` / `Episode NN` (any spacing/zero-padding)
- `Ep N` / `Ep NN`
- Show title repeated (folded soft-key equality with the show soft key)
- Empty / junk-only after the episode token

Everything else is usable, including numeric titles (Chernobyl `1-23-45`,
9-1-1 `7.1`, Promised Neverland codes, …). The measure's "bare-number"
bucket over-counted those as placeholders; do **not** put bare numbers on
the rejection list. When uncertain whether a title matches a reject form,
**decline** — failing to fire is cheap; a wrong pin is not.

**Compare** folded titles (same soft-key discipline as show matching).
**Method string** names the step that pinned (extend the ADR-0026 method
table; do not retune mid-weights without a new sample).

## Out of scope until accepted

- Implementing the episode-title fetch tier
- Changing the 0.80 floor
- Artwork (0027), refresh cadence, TVDB

## Alternatives (for the decision)

**ID as ladder step 3 (filter tied search hits).** Rejected: ID is
assertion, not inference; must skip search.

**Episode title before year.** Rejected unless evidence shows year
mis-pins more often than title helps; year is free and already shipping.

**Always fetch S01E01 for every show search.** Rejected: budget and
latency; collisions are rare. S01E01 as the default *reference* for the
title step is also rejected (see reference-episode rule above).

**Collapse Top Gear-class ties with a looser floor.** Rejected: wrong
series match is worse than unmatched (ADR-0026).

**Unbounded fan-out on large tied sets.** Rejected: hard cap (5).

## Consequences (once accepted)

- Resolve path: ID present → detail fetch, no search. Else search; on
  multi-exact, run the ladder; else unmatched / manual fix.
- Amend ADR-0026 §2 to point here for ID-vs-ladder shape and the title
  step (keep 0026 as the score / method table).
- Implementing slice: episode-title step + named decline reasons (no
  usable reference; over cap; no unique match). Year already exists;
  ID skip-search already exists as a path — wire the precondition so it
  cannot lose to year.
- Top Gear-class residue after a declined or non-unique title attempt is
  expected when every candidate shares a placeholder episode name;
  year/ID/manual own it.

## Status

**Proposed.** Do not code the episode-title tier until this order is
accepted (or explicitly amended) in a follow-up commit that flips Status
to accepted.
