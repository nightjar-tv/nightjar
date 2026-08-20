# ADR-0047: What may count as a title match, and what evidence may break a tie

- Status: **proposed** (amended 2026-08-20: the selection half is decided — see
  *Amendment: the count tier is gated*; corrected 2026-08-20 — see *Correction:
  the gate removes the count tier*)
- Date: 2026-08-20
- Supersedes: nothing. It narrows two rules that ADR-0026 and ADR-0032 left
  open, and does not replace either record.
- Depends on: ADR-0026 §2 (the scoring floor and the confidence tiers this
  reads); ADR-0032 (the collision pin, whose discriminator order this
  questions); ADR-0033 §8 (the folder cross-check, which catches some of what
  gets through); ADR-0046 (multi-entity binding — a folder may legitimately
  span a parent and its spin-off, so this must not make that unreachable)
- Gate: Gate 3 — metadata auto-match ≥95% correct
- Related: the matcher oracle, 2026-08-19, warmed 2026-08-20
  (`notes/loop-matcher/06`–`08`); the analysis scripts
  `notes/loop-matcher/scripts/collision_evidence.py` and `prefix_admission.py`,
  which produce every number below

## Context

The matcher oracle now measures its whole population — 73,625 rows, 0 stalled,
0 provider errors, `requests=0`, noise floor 0. Before warming, 39.3% of it was
invisible. **The single largest class the warm exposed is confident wrong
binding by the exact-title collision tier: 1,607 rows that bind at 0.90 and bind
the wrong entity.** Before warming there were 20 wrong bindings in the whole
suite; there are now 2,410.

These rows are not the tier failing to pin — that is the `absent` class and it is
a different record. These are the tier pinning, confidently, and being wrong.

Two rules combine to produce them, and they need deciding together because
fixing either alone leaves most of the population.

### Half one: admission. `name_matches_query` accepts a title extension.

`match_score.rs`:

    // Prefix: "the continental from the world…" after colon fold.
    if nk.starts_with(query_norm)
        && nk.len() > query_norm.len()
        && nk.as_bytes().get(query_norm.len()) == Some(&b' ')
    { return true; }

**The intent is real and should survive.** TMDB's official name is often longer
than the folder's: `The Continental` →
`The Continental: From the World of John Wick`. Without this arm that folder does
not match at all.

**But a franchise spin-off has exactly the same shape**, and the predicate cannot
tell them apart. Measured over the collision tier's wrong bindings, by folding
both names through the shipped chain (134 distinct `(entity, bound)` pairs where
the bound entity's name is cached):

| how the wrong candidate was admitted | pairs | share |
|---|---:|---:|
| **title extension — candidate is the query plus a tail** | **80** | **59.7%** |
| exact fold — a genuine same-title collision | 52 | 38.8% |
| neither | 2 | 1.5% |

So the majority of these wrong bindings are **not** same-title collisions. They
are admissions of a different show whose title begins with the folder's.

And the harm is concentrated where "starts with" discriminates least — the query
is the folder's title, and:

| words in the query | prefix admissions |
|---|---:|
| **1** | **55** |
| 2 | 15 |
| 3–6 | 10 |

**69% of them have a one-word query.** `Cross` admits `Cross My Mind`. `Sugar`
admits `Sugar Highs`. `Silo`, `Luther`, `Archer` — a one-word show title admits
every show whose name starts with that word.

The discarded tails are mostly companion content: `Official Podcast`,
`Webisodes`, `Tales from '85`, `The Interns`, `A Year in the Life`.

### Half two: selection. The count discriminators invert on a partial library.

`pin_collision` takes *"the first discriminator that selects exactly one of
`exact`"*, in the order **episode count → season count → premiere year**. The
uniqueness discipline is right — two matches means no pin. The **signals** are
the problem.

`library.episode_count` is the number of episode *files the folder holds*.
`CandidateShape.episode_count` is the candidate's **total across all seasons**.
A library holding one season of a long-running show — the normal state of anyone
mid-collection — therefore looks nothing like the correct entity and very like a
short one:

| correct entity | its eps / seasons | bound instead | its eps / seasons |
|---|---:|---|---:|
| Archer | 140 / 14 | Archer (1975) | 6 / 1 |
| The Blacklist | 218 / 10 | The Blacklist: Redemption | 8 / 1 |
| The Mentalist | 151 / 7 | Mentalist | 10 / 1 |
| The Good Wife | 156 / 7 | The Good Wife (2019) | 10 / 1 |
| Gilmore Girls | 153 / 7 | …: A Year in the Life | 4 / 1 |
| Rick and Morty | 91 / 9 | …: The Anime | 10 / 1 |

**Of 284 wrong binds where both counts are known, 267 — 94.0% — went to an
entity with *fewer* episodes than the correct one.**

`episode_count_close` compounds it: the tolerance is
`max(ceil(candidate × 0.15), 5)`, so a 6-episode candidate is "close" to any
library of 1–11 files, while the 140-episode correct answer needs 119–161.

`season_count` inverts identically: the folder asserts 1 season, the correct
entity has 7, the spin-off has 1, and `== Some(1)` selects the spin-off uniquely.

**The codebase already knows this.** `CandidateShape.season_episode_counts` exists
with the docstring: *"a count of seasons cannot say whether a candidate could hold
the folder's files, and a total episode count inverts on a folder holding more
episodes than the entity has. Per-season counts answer both."* It was added for
this and `pin_collision` does not consult it — the weakest, most inversion-prone
signal is tried **first**, and because the first unique match wins, it pre-empts
every better one.

## Decision — the question, and the options

**When may a candidate whose title strictly extends the query be treated as a
title match, and what evidence may then choose among the admitted candidates?**

### On admission

- **A. Leave it.** Keeps `The Continental`. Keeps 80 measured wrong bindings.
- **B. Drop the prefix arm.** Removes ~80 wrong bindings and breaks every folder
  whose provider name is legitimately longer. **Now measured** — see the
  `tv.shortfolder` note below: it costs **58 correct rows**. So B trades roughly
  58 correct for 80 wrong, which under the leave bar is defensible; it is still
  dominated by C, which keeps the 58 *and* removes the 80.
- **C. Admit the extension, but never let it *win* against an exact fold.**
  An extension becomes a candidate only when no exact fold survives. Cheap,
  keeps `The Continental`, and removes the 80 wherever an exact fold exists
  alongside — which is the case in every example above.
- **D. Gate the arm on query length** (e.g. ≥2 words, or ≥N characters).
  Addresses 69% of the measured harm with one condition. Arbitrary in a way the
  others are not, and a threshold is exactly the "rule to tune" this record is
  trying not to be.

**Recommended: C**, with B's cost measured before it is ever reconsidered. C is a
precedence rule rather than a threshold, it is expressible in the existing tier
order, and `ADR-0046` still reaches a genuine parent-plus-spin-off folder through
the multi-entity path rather than through a title extension.

### On selection

- **E. Leave the order.** Keeps 1,607.
- **F. Reorder: per-season coverage before total counts.** Use
  `season_episode_counts` — can this candidate hold the seasons the folder
  asserts, at roughly the counts the folder holds? — ahead of
  `episode_count`/`season_count`. Uses evidence already fetched and already
  modelled; no new provider call.
- **G. Require the total-count signals to be *corroborated*** rather than
  sufficient alone.
- **H. Withhold the pin when the library is plainly partial** (one season, few
  files) and the candidates differ in magnitude. Declines instead of guessing:
  turns wrong into absent, which is recoverable.

**Recommended: F, with H as the fallback when F cannot separate.** F is what the
existing field was built for; H matches this project's standing preference that a
comparator may report agreement or silence but not disagreement on weak evidence.

## Consequences

**Measurable.** Every option above moves oracle rows, and the instrument now sees
the whole population with a noise floor of 0 and a `wrong.kind` verdict that
cannot hide in `partial`. The suite to watch is `tv.noyear` (63.9%), `tv.root`
(63.9%) and `tv.scene` (61.6%) — where these 1,607 live — against the
Sonarr-shaped shapes at 99.9–100.0%, which must not move.

**Absent is an acceptable outcome; wrong is not.** Options H and C trade wrong
bindings for unmatched files by design. That is the right direction: a wrong
binding triggers fetches and corrupts watch state, an unmatched file is
recoverable in the fix UI.

### `tv.shortfolder` — the shape that costs option B (added 2026-08-20)

The gap this record named is closed. `tv.shortfolder` renders a folder under the
head of a colon-named show — `Quiet on Set: The Dark Side of Kids TV` in a folder
called `Quiet on Set` — in `tv.noyear`'s filename form with no year, so the
title-extension rule is the only route to the right entity.

**Ambiguity is tested against the provider, not the kept set.** The first cut
checked the head against other oracle entities and passed 29 shows; **eleven of
them are shows TMDB also lists under the bare head** — a folder called `Monarch`
could honestly mean `Monarch`, `Spartacus` `Spartacus`. For those the oracle has
no answer, and asserting one manufactures the bad-oracle rows that put six wrong
answers through the parser sweep. Every exclusion is counted and printed:

    two colon names share this head                  29
    provider also lists a show under the bare head   11
    head is another kept entity's full name           4
    head shorter than 3 characters                    1
    head's search not cached — unverifiable           1

17 shows survive, 113 rows.

**Measured, warm, `requests=0`:** 58 correct (51.3%), 54 absent, 1
`wrong.unknownepisode`. Nine of the ten matched groups bound via `exact_title` —
the extension admitted with no surviving competitor.

So the admission rule **is** load-bearing, and it is worth **58 rows** here, not
the near-total I assumed when this record was first drafted. That is the number to
weigh option B against, and it makes C the dominant choice on evidence rather than
on argument: with no exact fold competing, C keeps all 58.

The shape is small and small in a way worth stating: 113 rows against the 80 wrong
bindings the same rule produces. It can show the rule is load-bearing; it cannot
show it is worth its cost.

**What this record still does not settle.** The oracle is English names only,
season 1 mostly, one drain from an empty database, no NFOs, and no manual-match or
rescan path. `tv.shortfolder` covers only colon-named shows whose head is
unambiguous — a provider name longer without a colon (`The Office US`) is not
generated.


---

## Amendment, 2026-08-20 — the count tier is gated

A severity-weighting decision, taken deliberately rather than derived from the
rate, and recorded here because the rate argues the other way.

### What was measured

With `slots_explained` and `sole_season_coverer` ahead of the counts, the count
discriminators still fire whenever those decline. Gating them off when any
candidate carries a per-season list — *where better evidence was available and
declined, worse evidence does not decide* — produces exactly three transitions,
every one to `absent`:

    correct              -> absent   836
    wrong.unknownepisode -> absent   698
    wrong.entity         -> absent    60

| | keep | gate |
|---|---:|---:|
| correct | 59,101 | 58,265 |
| wrong.entity | 64 | **4** |
| wrong.unknownepisode | 698 | **0** |
| absent | 13,302 | 14,896 |
| **correct%** | **80.1%** | **79.0%** |

`tv.noyear`, `tv.root` and `tv.scene` reach **zero wrong bindings of every
class**.

### Why gate, when the rate falls

**The gated firings are 52.4% precise** — 836 right against 758 wrong. That is a
coin flip selecting an entity at **0.90 confidence**, above the auto-match floor,
and it has been selecting them all along.

`BLOCK1_LEAVE_BAR` settles the direction: a wrong binding triggers fetches and
corrupts watch state; an unmatched file is recoverable through the fix flow.
Trading 758 unrecoverable failures for 836 recoverable ones is the exchange that
bar exists to make. **79.0% honestly measured is worth more than 80.1% where a
percentage point comes from a coin flip.**

The counts are kept where nothing better has run — a candidate whose detail was
fetched without its seasons — which is the path `long_run_episode_count_pins_
over_short_reboot` and `episode_count_pins_supernatural_shape` exercise.

### What was tried first and rejected

Two sharper predicates, both wrong:

1. **Abstain when any candidate has an unasserted season.** Broke five shipped
   tests, including the discriminator working correctly on a long run against a
   short reboot — those tests carry no per-season list, so the rule was silenced
   entirely.
2. **Containment as equality with the folder's total** —
   `slots_explained == library.episode_count`. Broke
   `a_close_slots_race_does_not_pick_a_primary`: 20 files, one candidate
   explaining 20 and another 18, so an 18-against-20 difference became decisive.
   That is proximity's brittleness through a different door.

**The fix was never a sharper predicate.** `slots_explained` already asks the
containment question and already runs first; the change is to stop a worse rule
overriding its declination.

### Reproducibility

The gated tree was measured twice, and the second run differs from the first on
**0 of 73,738 rows**. Noise floor 0 in both. 739 workspace tests pass; the single
failure is a pre-existing flaky HLS seek assertion in `transcode` that fails
identically on `origin/main`.

---

## Correction, 2026-08-20 — the gate removes the count tier, it does not narrow it

**The decision above stands.** It was re-measured on a re-warmed oracle and its
ledger reproduced. What is wrong is one sentence of the reasoning, and it is
wrong in the direction that matters: it describes a path production does not
have.

### The false sentence

> The counts are kept where nothing better has run — a candidate whose detail
> was fetched without its seasons — which is the path
> `long_run_episode_count_pins_over_short_reboot` and
> `episode_count_pins_supernatural_shape` exercise.

A candidate "whose detail was fetched without its seasons" is a `/tv/{id}`
response carrying `number_of_episodes` and no `seasons[]`. TMDB does not return
that. `tv_candidate_shape` fills `season_numbers` and `season_episode_counts`
from the same `seasons[]` array, so a candidate carries both or neither, and it
is the only production constructor of a shape that carries counts. The two named
tests hand-build the shape TMDB withholds. They are the evidence for the claim,
and they are also the only place it holds.

The honest statement is the stronger one: **the gate does not narrow the count
tier, it removes it.** `pin_collision` returns on its first line for every real
payload. Everything below that line — both count discriminators, and the
`exact_title_library_year` pin with them — is reachable from tests only.

The year pin is unreachable a second way as well. `queue.rs` sets the TV search
year to `g.year.or(g.library_year)`, and `g.year` is always `None` for an
episode group, so the search year and `LibrarySeriesShape::year` are one value.
The branch that calls `pin_collision` is entered only when that value is `None`.

Nothing in the decision changes. What binds an entity today is
`exact_title_season_coverage`, `exact_title_slots_explained`, the ADR-0032
episode-title pin, and the year discriminators above this branch — the same four
the amendment already called more precise than the counts.

### The confirming measurement

Re-warmed oracle, 79,382 rows. Both arms stall on the same count, so the two
populations are like-for-like:

| arm | correct | wrong.entity | wrong.unknownepisode | absent |
|---|---:|---:|---:|---:|
| gate in | 58,186 | 10 | 20 | 9,944 |
| gate removed | 59,032 | 70 | 735 | 8,323 |

Removing the gate buys **846 correct for 775 wrong — about 1.1 to 1**. That
reproduces the amendment's own 836-against-758 ledger on a different warm. A
coin flip at 0.90 confidence is still a coin flip, and `BLOCK1_LEAVE_BAR` still
decides which way to take it.

### The trap in that measurement

On a **partially warmed** cache the same comparison read as **11 to 1 in favour
of removing the gate**. That was not noise and not a different metric. The two
arms stalled on different items, and 714 of the items hidden from one arm turned
out to be wrong bindings. The arm that looked better looked better because more
of its failures were unmeasured.

**Two arms that leave different things unmeasured cannot be compared** — not at
any sample size, and not however stable the ratio looks. Equal stall counts are
what make the table above one comparison instead of two separate observations,
which is why the table says so.
