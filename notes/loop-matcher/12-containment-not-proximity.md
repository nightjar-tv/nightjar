# Iteration 12 — containment, not proximity: the last piece of the collision tier

## The residual, restated

`episode_count_close(le, ce)` asks *is the candidate's episode count near the
folder's file count*. For The Firm the folder holds **10 files of a 22-episode
season 1**; the wrong candidate's season 1 holds about **10**. `close(10, 22)` is
false and `close(10, 10)` is true, so **exactly one** candidate matches — the wrong
one — and `try_pin` pins it.

Proximity rewards a candidate for being the same *size* as an incomplete folder.
That is the same inversion as comparing totals, one level down: restricting the
count to the folder's seasons removed the cross-season form and left the
within-season form untouched.

**There is no count evidence that separates these two.** Both can hold 10 files.
The right outcome is to decline, and the way to reach it is a predicate that both
candidates satisfy.

## Why not `candidate_eps >= folder_files`

Because that is **the containment test refuted on 2026-08-18**, and
`slots_explained`'s docstring records why: compared on totals it inverts on a
folder holding *more* files than the entity has — `Firefly`'s 14 files against an
11-episode entity failed it while `Firefly Lane` passed.

The safe form is per-season with min-capping, which is exactly `slots_explained`:
a folder season with more files than the entity's season contributes the entity's
count and never disqualifies it.

## The change

The episode-count discriminator matches when the candidate **explains the whole
folder** — `slots_explained(shape, library) == library.episode_count` — instead of
when its count is *near* the folder's.

**With no per-season list, proximity on a comparable total is kept.** That path is
what the three shipped count tests exercise (`long_run_…_over_short_reboot`,
`episode_count_pins_supernatural_shape`,
`season_count_pins_when_episode_count_ambiguous`) — 181 files against candidates of
181 and 12, 311 against 327 and short. Containment on totals would break the
Firefly case those tests do not cover, and refusing outright would silence the rule
where it is right, which is the mistake half two's first cut made.

## Prediction

| | predicted |
|---|---|
| The Firm's 20 rows | **decline → absent**, because both candidates explain all 10 files and `try_pin` sees two |
| `exact_title_episode_count` wrong (616 → after half two, fewer) | **falls further** |
| its correct | **falls too** — containment matches more candidates, so it declines more often |
| net wrong | **down**; net correct **down slightly or flat** |
| overall correct% | **80.1% → 79.5–80.5%** — flat is the expected shape |
| the three shipped count tests | **pass**, via the no-per-season-list fallback |
| tv.shortfolder | **74, unchanged** |

**This one is not expected to raise the rate.** It trades a coin-flip pin for an
absent, so the number that says whether it worked is *wrong down with correct
roughly held* — and if correct falls materially further than wrong, it is not worth
keeping and I will say so.
