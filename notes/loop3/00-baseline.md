# Iteration 0 — the baseline, measured rather than inherited

Base: `origin/main` at `2f6efb7`. Branch `loop/matcher-residual`, worktree
`~/Documents/GitHub/nightjar-wt-loop3`, target `~/nightjar-wt-loop3-target`.

Nothing was changed here. Every instrument was re-run at the base so later
iterations have a table to join against.

## The oracle

`measure_warmed.sh` against the warmed cache, `EXPECT_ROWS=79382`.

    population: 2410 entities, 79382 generated rows
    cache entries: 20350
    runs 39   provider errors 2   http requests 0
    NOISE FLOOR: 0 of 79382 rows

| verdict | rows |
|---|---:|
| correct | 58,186 |
| absent | 20,593 |
| `wrong.kind` | 573 |
| `wrong.entity` | 10 |
| `wrong.unknownepisode` | 8 |
| stalled | 12 |
| **total** | **79,382** |

**Three of the handed-over baseline's cells were wrong**, and the handed-over
table did not reconcile: it summed to 79,950 against a stated population of
79,382.

| cell | handed over | measured here |
|---|---:|---:|
| correct | 58,186 | 58,186 |
| `wrong.kind` | 573 | 573 |
| `wrong.entity` | 10 | 10 |
| absent | 9,944 | **20,593** |
| stalled | **11,217** | **12** |
| `wrong.unknownepisode` | 20 | **8** |

The stall cell is the one that changes what is possible. 11,217 stalled would
have meant `tv.mixedroot` — 5,644 rows, added after the last scored run — was
undrained and unusable, and item 2 names it as the only instrument that can see
F5. It is drained, it is warm, and it stalls 12 rows of 5,644.

## Per shape, at the base

| shape | meas | correct | wrong.kind | wrong.ent | wrong.unk | absent | stall | correct% |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| tv.episodetitle | 5,643 | 0 | **573** | 0 | 0 | 5,070 | 0 | 0.0% |
| tv.mixedroot | 5,632 | 5,565 | 0 | **6** | **8** | 53 | 12 | 98.8% |
| movie.noyear | 1,712 | 1,006 | 0 | 1 | 0 | **705** | 0 | 58.8% |
| movie.scene / yearfile / yearfolder | 1,712 each | 1,710 each | 0 | 1 each | 0 | 1 each | 0 | 99.9% |
| movie.sonarr | 1,712 | 1,712 | 0 | 0 | 0 | 0 | 0 | 100.0% |
| tv.flat, tv.flat.titled, tv.sonarr, tv.sonarr.plain | 5,644 each | 5,644 each | 0 | 0 | 0 | 0 | 0 | 100.0% |
| tv.twoseason | 6,278 | 6,278 | 0 | 0 | 0 | 0 | 0 | 100.0% |
| tv.partial | 1,650 | 1,650 | 0 | 0 | 0 | 0 | 0 | 100.0% |
| tv.single | 698 | 698 | 0 | 0 | 0 | 0 | 0 | 100.0% |
| tv.noyear | 5,644 | 4,579 | 0 | 0 | 0 | 1,065 | 0 | 81.1% |
| tv.root | 5,644 | 4,579 | 0 | 0 | 0 | 1,065 | 0 | 81.1% |
| tv.scene | 5,644 | 4,349 | 0 | 0 | 0 | 1,295 | 0 | 77.1% |
| tv.shortfolder | 113 | 64 | 0 | 0 | 0 | 49 | 0 | 56.6% |
| tv.handmade | 5,644 | 0 | 0 | 0 | 0 | 5,644 | 0 | 0.0% |
| tv.numbered | 5,644 | 0 | 0 | 0 | 0 | 5,644 | 0 | 0.0% |

### `tv.numbered` reads 0.0% here and read 99.9% at the last recorded run

Checked rather than assumed, because a shape falling from 99.9% to 0.0% between
two `main` commits would be a live regression on `main` and would outrank
everything in the brief.

It is not. The same generated library, drained at `6221c59` — the commit before
this base — gives `ready=0 unmatched=5604`, the identical answer:

    tv.numbered-b0   DONE groups=693 ready=0 unmatched=5604 errors=0 requests=0
    tv.numbered-b1   DONE groups=5   ready=0 unmatched=40   errors=0 requests=0

So the product did not move. `gen_library.py` was edited at 16:47 and `out/lib`
regenerated at 18:14 on 2026-08-20, both after the last scored `main` run at
14:51, and `tv.numbered` now renders `Dept. Q (2025)/Season 01/S01E01.mkv` —
a basename carrying no title at all. **The shape changed under the number.**

Consequence for this loop: `tv.numbered`'s 5,644 rows are not a rate. They are
still usable differentially — both revisions give the same answer, so a move
there would still be a move — but its absolute 0.0% says nothing about the
matcher, and it is a large part of why `absent` here is 20,593 rather than the
9,944 handed over.

**Every `wrong` row outside `movie.*` and `tv.episodetitle` is in
`tv.mixedroot`** — 6 `wrong.entity` and 8 `wrong.unknownepisode`. That is the
population item 2 is about.

## The parser sweep

`./run.sh 2f6efb7 2f6efb7 ~/Documents/GitHub/nightjar-wt-loop3`

    generated 74624 names
    base (2f6efb7): 74624 parsed
    head (2f6efb7): 74624 parsed
    HEAD right, BASE wrong 0
    BASE right, HEAD wrong 0
    gains by field: title 0  season 0  episode 0  year 0

Recorded at this base, as asked. The sweep is differential only — it prints no
absolute bucket table — so the known-good at this base is the null run, and it
also proves the harness is wired to this worktree rather than to the default
`REPO` it ships with, which points at a different checkout.

## The parser corpus

    844 cases, 738 applicable, pass 524, fail 214 = 71.0%

## What was set up

The oracle needs three files the product tree does not carry — `replay.rs`,
`oracle_query.rs`, and the response-cache branch in `tmdb/mod.rs`. They are
kept as a patch in scratch and applied to throwaway measurement worktrees, never
to the branch. The branch is clean.
