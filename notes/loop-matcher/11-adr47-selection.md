# Iteration 11 — ADR-0047 half two: coverage first, and magnitudes only when comparable

## The evidence that decides the design

Method against outcome, over the fully-warm post-half-one suite:

| discriminator | correct | wrong | precision |
|---|---:|---:|---:|
| `exact_title_year` | 30,806 | **0** | 100% |
| `exact_title` (sole candidate) | 10,288 | **0** | 100% |
| `exact_title_slots_explained` | 74 | **0** | 100% |
| `exact_title_season_coverage` | 30 | **0** | 100% |
| **`exact_title_episode_count`** | 882 | **616** | 59% |
| **`exact_title_season_count`** | 358 | **440** | **45%** |

**Every coverage-based discriminator is perfect. Both count-based ones are near
random, and `season_count` is worse than a coin flip.** And `pin_collision` tries
the two near-random ones **first**, so they pre-empt the perfect ones on every
folder they happen to select uniquely.

That is the whole defect in one table.

## Why the counts invert, stated once

`library.episode_count` is *files in the folder*. `CandidateShape.episode_count`
is *the candidate's total across all seasons*. These are different quantities.
A folder holding one season of a long-running show — the normal state of anyone
mid-collection — is small, so it matches a small candidate:

    Archer         140 eps / 14 seasons  ->  bound Archer (1975)   6 eps / 1 season
    The Blacklist  218 / 10              ->  bound Redemption      8 / 1

267 of 284, 94.0%, went to an entity with fewer episodes than the correct one.
`season_count` inverts the same way: the folder asserts 1 season, the correct
entity has 7, the spin-off has 1, and `== Some(1)` selects the spin-off uniquely.

## The change

**F — coverage before magnitudes.** `sole_season_coverer` and
`primary_by_slots_explained` move ahead of `pin_collision`. Both already exist,
both are already tested, and both are 100% precise on this population. Their
ordering after the counts was deliberate and conservative — *"only after
`pin_collision` declines, so no folder that pins today is re-attributed"* — and
that conservatism is exactly what gave the inverting discriminators priority.

**H — a magnitude comparison needs comparable magnitudes.** The count
discriminators abstain when the folder asserts *fewer seasons than a surviving
candidate has*, because then the folder is partial with respect to that candidate
and its file count is not the candidate's episode count. Abstaining is not
declining to answer: coverage, slots, episode titles and the year all still run.

Not a threshold. The condition is a statement about whether two numbers measure
the same thing.

## Prediction, written before running

| | predicted |
|---|---|
| **the 42 rows half one lost** | **recovered, most or all** — the counts abstain for them and coverage or slots decide instead |
| wrong from `episode_count` + `season_count` (1,056) | **falls sharply** — the inversion needs a magnitude mismatch, which is exactly the abstain condition |
| correct from those methods (1,240) | **falls too**, by less — a complete library stays comparable |
| `exact_title_year`, `exact_title` (41,094 correct, 0 wrong) | **untouched** — different branches |
| tv.shortfolder | **58, unchanged** |
| overall correct% | **79.1% → 79–82%**, and the honest test is *wrong down with correct roughly held*, not the rate |

The rate is the wrong thing to watch here. Trading a near-random pin for an
absent is a win under the leave bar even when the rate is flat, and the number
that says so is total wrong.

If the 42 do not come back, my model of this half is wrong and the ordering was
not the mechanism.
