# ADR-0032: Multi-exact collision resolution order

- Status: **proposed** (matching-policy question — accept before coding the
  episode-title tie-break)
- Date: 2026-08-03
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
complementary to year and ID, not a replacement.

Manual fix (ADR-0028) remains the last resort. Explicit provider-ID
override (NFO / user-supplied TMDB id) is already a higher-trust path than
search scoring when present — this ADR must place it in the same ordered
ladder so implementers do not invent a second policy.

## Decision needed

**When two or more `/search/tv` hits share a normalised title, what is the
ordered list of discriminators, and what happens if none pin uniquely?**

Proposed order (accept or amend before any fetch-tier code):

| Order | Discriminator | Fires when | Cost |
|---|---|---|---|
| 1 | Premiere year (existing ADR-0026 pin) | Library year uniquely matches one candidate's `first_air_date` year | Search fields only |
| 2 | Episode-title tie-break | ≥2 candidates still tied; local library has a **distinctive** after-token title on a reference episode (prefer S01E01 / 1x01 when present); fetch that episode's name for each tied candidate; **exactly one** matches | 2–3 detail calls on the rare multi-exact residue only — not a default path |
| 3 | Explicit provider ID | NFO or user override already names a TMDB tv id among the tied set | No extra search |
| 4 | Manual fix (ADR-0028) | Still unpinned | Operator |

If a step selects zero or two-or-more candidates, try the next. If none pin,
stay unmatched at 0.72 / `exact_title_collision_unpinned` (path `item_key`).
Do not lower the floor. Do not silently pick top-1.

**Episode-title step constraints (if this order is accepted):**

- Fire only on multi-exact title ties that survived year.
- Skip when the local reference title is a placeholder (`Episode N`, `Ep N`,
  show title repeated). Numeric titles (Chernobyl `1-23-45`, 9-1-1 `7.1`,
  …) are **distinctive signal**, not placeholders — confirmed by eyeballing
  all 54 "bare-number" measure hits.
- Compare folded titles (same soft-key discipline as show matching).
- Method string names the step that pinned (extend the ADR-0026 method
  table; do not retune mid-weights without a new sample).

## Out of scope until accepted

- Implementing the episode-title fetch tier
- Changing the 0.80 floor
- Artwork (0027), refresh cadence, TVDB

## Alternatives (for the decision)

**Episode title before year.** Rejected unless evidence shows year
mis-pins more often than title helps; year is free and already shipping.

**Always fetch S01E01 for every show search.** Rejected: budget and
latency; collisions are rare.

**Collapse Top Gear-class ties with a looser floor.** Rejected: wrong
series match is worse than unmatched (ADR-0026).

## Consequences (once accepted)

- Amend ADR-0026 §2 collision-pin table to match this order (or keep 0026
  as the score table and point here for the extended ladder).
- Implementing slice: episode-title step only; year already exists; ID and
  manual fix already exist as paths — wire them into the same ordered
  refuse/pin story with named methods.
- Top Gear-class residue after title attempt is expected when placeholders
  match; year/ID/manual own it.

## Status

**Proposed.** Do not code the episode-title tier until this order is
accepted (or explicitly amended) in a follow-up commit that flips Status
to accepted.
