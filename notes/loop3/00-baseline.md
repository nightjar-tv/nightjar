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

### `tv.numbered` read 0.0% here, and the cause was the harness

**Corrected after iteration 2. The first reading of this was wrong.**

A shape falling from 99.9% to 0.0% between two `main` commits would be a live
regression on `main`, so it was checked rather than assumed. The same generated
library drained at `6221c59` — the commit before this base — gives the identical
`ready=0 unmatched=5604`, so **the product did not move.** That much held.

The explanation offered here first — that `gen_library.py` was edited and the
shape changed under the number — was wrong. The cause is the **replay harness**.

`tv.numbered` renders `Dept. Q (2025)/Season 01/S01E01.mkv`: a basename carrying
no title at all. The product's rule for that is `nightjar_scanner::stored_title`,
which substitutes the show folder's name for an empty parse, and **both scanner
indexing paths call it**. Its own doc comment says the replay harness needs the
same answer, and names the consequence of it not having it: "a titleless episode
came out with an empty title, an empty title is not a query, and 5,644 generated
rows scored `absent` for a reason that was the harness rather than the product."

5,644 is `tv.numbered`. The harness patch this loop inherited re-derives the
title with `parse_filename` alone. It is the defect the product already fixed,
still sitting in the instrument.

Corrected: `replay.rs` now calls the shipped `stored_title`. `tv.numbered` goes
to **5,644 of 5,644, 100.0%**, and **no other shape moves at all**. The patch
lives at `~/nightjar-wt-loop3-scratch/oracle-harness.patch`; it is deliberately
not committed to the product tree.

This is why the baseline table above is superseded. The corrected one:

| verdict | first reading | corrected harness |
|---|---:|---:|
| correct | 58,186 | **63,830** |
| absent | 20,593 | **14,949** |
| `wrong.kind` | 573 | 573 |
| `wrong.entity` | 10 | 10 |
| `wrong.unknownepisode` | 8 | 8 |
| stalled | 12 | 12 |
| **total** | **79,382** | **79,382** |

Only `tv.numbered` differs — 0 correct / 5,644 absent becomes 5,644 correct /
0 absent. **Every measurement in this loop's later notes uses the corrected
harness**, and iteration 2 was re-run on both arms against it. Its delta is
unchanged: the same 26 rows move, in the same shape, in the same direction.

**The lesson is the one already on the wall.** A zero from an instrument that
derives a field differently from production is not a result, and this is the
second time the same field has done it here.

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
