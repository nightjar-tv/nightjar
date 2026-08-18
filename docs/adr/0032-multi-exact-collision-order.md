# ADR-0032: Multi-exact collision resolution order

- Status: **accepted**
- Date: 2026-08-03
- Amended: 2026-08-03 — ID is a search precondition, not a ladder step;
  distinctive = rejection list; tied-candidate cap; reference-episode
  preference
- Amended: 2026-08-03 — full ladder table (year → counts → episode
  title); method `exact_title_episode_title` at 0.90; TV-only; accepted
- Amended: 2026-08-18 — confirmation endorses without renaming the route;
  agreement moves to its own three-state column
- Depends on: ADR-0026 §2 (floor + existing TV collision pin); ADR-0028
  (manual fix); ADR-0031 §7 coverage sample (soft-key re-run)
- Related: the TMDB show-coverage soft-key sample, 2026-08-03;
  the episode title availability note, 2026-08-03
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

## Decision

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

**TV only.** Movies keep the ADR-0026 year / nearest-year table; there is
no episode-title tier for movies.

**ADR-0026 §2 steps 1–3 unchanged; this ADR adds step 4.** Full order:

| Order | Discriminator | Method (score) | Fires when | Cost |
|---|---|---|---|---|
| 1 | Premiere year | `exact_title_library_year` (0.90) | Library year uniquely matches one candidate's `first_air_date` year | Search fields only |
| 2 | Episode file count | `exact_title_episode_count` (0.90) | Soft count uniquely matches `/tv/{id}` `number_of_episodes` | Detail when year did not pin |
| 3 | Season count | `exact_title_season_count` (0.90) | Exact match on `number_of_seasons` | Same detail fetch |
| 4 | Episode-title tie-break | `exact_title_episode_title` (0.90) | ≥2 still tied; usable local reference episode; fetch that episode's name for each tied candidate; **exactly one** matches | Detail calls bounded by the tied-set cap |
| — | Unpinned / manual fix (ADR-0028) | `exact_title_collision_unpinned` (0.72) | None of 1–4 pinned uniquely | Operator |

If a ladder step selects zero or two-or-more candidates, try the next. If
none pin, stay unmatched at 0.72 / `exact_title_collision_unpinned` (path
`item_key`). Do not lower the floor. Do not silently pick top-1.

### Episode-title step constraints

**When it may fire:** TV multi-exact title ties that survived steps 1–3,
with tied-candidate count **≤ 5**. Above that cap the step **declines
outright** (pathological title must not fan out to 2N detail calls).
"2–3 detail calls on the rare residue" is the expectation for normal
collisions, not a substitute for the cap.

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
**Method string** `exact_title_episode_title` at **0.90** (extend the
ADR-0026 method table; do not retune mid-weights without a new sample).

## Out of scope for the implementing slice

- Changing the 0.80 floor
- Artwork (0027), refresh cadence, TVDB
- Treating episode-title as default matching (multi-exact residue only)

## Alternatives considered

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

## Consequences

- Resolve path: ID present → detail fetch, no search. Else search; on
  multi-exact, run the ladder; else unmatched / manual fix.
- Amend ADR-0026 §2 to point here for ID-vs-ladder shape and the title
  step (keep 0026 as the score / method table; add
  `exact_title_episode_title` at 0.90).
- Implementing slice: episode-title step + named decline reasons (no
  usable reference; over cap; no unique match). Year and count pins
  already exist; ID skip-search already exists as a path — wire the
  precondition so it cannot lose to year.
- Top Gear-class residue after a declined or non-unique title attempt is
  expected when every candidate shares a placeholder episode name;
  year/ID/manual own it.

---

## Amendment 2026-08-18 — one column cannot hold two facts

The ladder's original shape has confirmation *replacing* the route: whenever
episode titles agreed, `match_method` became `exact_title_episode_confirmed`
and the candidate rose to 0.90. That reads naturally — confirmation is the
last rung, so the last rung names the row — and it is wrong in a way only a
measurement shows.

`match_method` answers **what selected this candidate**. Confirmation
sometimes selects (it redirects the pick to a different candidate) and
sometimes only agrees (the year or a count pin already chose, and the titles
concur). Those are different events and the column was recording them the
same way.

**What the single column did, measured on the replay harness:**

| | items |
|---|---:|
| moved off `exact_title_year`, `_episode_count`, `_episode_title`, `_season_count` | ~10,570 |
| `exact_title_episode_title` after | 0 |
| `exact_title_season_count` after | 0 |

Two tokens went to zero. The column could no longer say those routes had
fired at all — the same collapse `series_row` produces at scale, arrived at
from the other direction. A diagnostic that reports one cause for five is not
a coarser diagnostic; it is a wrong one.

The obvious correction — keep the route, drop the agreement — loses the other
half. 770 bindings that confirmation successfully pinned would report
`exact_title_collision_unpinned`, a token whose meaning is *nothing pinned
this*. Both single-column arrangements are false, in opposite directions.

### The decision

**Confirmation redirects → it owns `match_method`.** It chose the candidate,
so `exact_title_episode_confirmed` is the honest answer, and 0.90 stands.

**Confirmation endorses → the route stays and the confidence still rises.**
The defect being fixed is a correct answer dying at 0.72 with its evidence in
hand; raising it does not require renaming what selected it.

**Agreement is recorded in its own column,
`media_items.metadata_confirmed_by_episode_title`** (migration 023),
nullable, three-state:

| value | meaning |
|---|---|
| `NULL` | not evaluated — no candidate episodes fetched, nothing compared |
| `0` | evaluated, no folder title agreed |
| `1` | evaluated, one did |

`NOT NULL DEFAULT 0` was rejected: it collapses *no evidence* into *no
agreement*, which is precisely the distinction `compare_episode_title`'s
one-directional contract exists to protect. This is the fifth place in this
project where that collapse had to be refused explicitly.

The column is **diagnostic and never control flow**. Nothing reads it to
decide what happens next; it exists so a measurement can ask how often
confirmation agreed with the route that won, without the route token having
to carry both facts.

### Consequences

- `MatchCandidate` gains `confirmed_by_episode_title: Option<bool>`, read off
  the branch that fired rather than recomputed — a row can never carry
  `exact_title_episode_confirmed` next to a `0`.
- The value is threaded through `ProviderResult::Hit` and
  `ResolveOutcome::Resolved` as `confirmed: Option<bool>` and written by
  `apply_search_hit`, the same call site that records the route. A writer
  without a caller is how the route column shipped inert the first time; the
  producer is asserted by test before this is measured.
- The method table in ADR-0026 §2 is unchanged. `exact_title_episode_confirmed`
  keeps its 0.90; the endorsement case now reaches 0.90 under its own token.
