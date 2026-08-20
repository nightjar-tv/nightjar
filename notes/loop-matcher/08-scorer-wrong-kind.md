# Iteration 8 — the scorer can now say the worst thing it sees

Instrument work, taken first despite being the cheapest, for the reason the
ordering was decided on: **the loop is judged by this tool, and the most severe
class in the suite was invisible to it.** A slice aimed at cross-kind bindings
would have measured flat.

Patch: `~/nightjar-wt-matcher-scratch/scorer-wrongkind.patch` (185 lines against
`score_binding.py`). The spike is not in the product repo, so the change lives
there and is recorded here.

## What was wrong

For a tv entity the scorer looked for an episode link, then a show link, and filed
"neither" as `partial` (`score_binding.py:107`). **A movie link is neither.** So an
episode file bound to a film — the file has left the TV library entirely — was
reported under the verdict that reads as *incomplete but not wrong*.

The movie arm had the same hole in the other direction: a film bound to a show or
an episode also fell to `partial`.

## The fix

`wrong.kind`, checked only after every same-kind path has failed, in both
directions. Ordered first among the wrong classes in `ORDER`, because the file is
in the wrong *library* and no re-match inside the right one can fix it.

**Verified to change nothing else.** Re-scored the same run directories — no
re-drain, so the bindings are identical — and diffed row by row on
`(shape, batch, path)`:

    verdicts changed by the scorer edit: 573
    {('partial', 'wrong.kind'): 573}

Exactly the intended reclassification and not one other row. `partial` is now 0
across the suite; every one of those rows was a cross-kind binding.

## The bigger defect the fix uncovered

**The by-shape table hardcoded its columns**, and two verdicts had none:
`wrong.kind`, and `wrong.unknownepisode` — which appears only once the cache is
warm and accounts for **1,737 rows**.

Their rows were counted in `meas` and displayed nowhere, so a shape line simply
did not add up, and nothing said so. I read and quoted those tables for a whole
session without checking that a row summed:

    tv.noyear   meas 5644 = correct 3609 + wrong 41 + absent 1391 = 5041

603 rows missing from that line, in every report I gave. The verdict *totals* were
always right; the per-shape breakdown was not.

Fixed by deriving the columns from `ORDER` — so a verdict added later gets a column
without anyone remembering — and by **checking every row against its own total**,
printing a `!! rows that do not reconcile` block when it fails. It now prints
nothing, because every row reconciles.

That check is the point. A hardcoded subset fails silently; a derived set with an
assertion fails loudly.

### And it corrected something I had reported as zero

With the columns visible, the "99.9%" shapes are not wrong-free:

| shape | correct | wrong.unk | absent |
|---|---:|---:|---:|
| tv.flat | 5,640 | **4** | 0 |
| tv.numbered | 5,640 | **4** | 0 |
| tv.sonarr.plain | 5,640 | **4** | 0 |
| tv.partial | 1,649 | **1** | 0 |
| tv.single | 697 | **1** | 0 |

Small, but I had reported those shapes as `wrong 0`. They are not.

## The wrong-pairs listing

Widening it to every wrong class broke it: `wrong.unknownepisode` has
`bound = None` by construction, so it printed 1,737 lines of `-> tmdb:None` and
pushed the nameable pairs off the end. **Name what can be named, count what
cannot, hide neither** — it now lists `wrong.entity` and `wrong.kind`, marks the
latter `A FILM,`, and prints a count of the rows that carry no nameable entity.

The film ids are not resolved to titles. `cache-inventory.json` was built from the
pre-warm cache and those films were fetched during warming; re-running
`inventory.py` would hand `pick_entities.py` a bigger cache and move the
2,410-entity set, breaking every existing join. Not worth a prettier label —
`crosskind.py` lists them with their file paths.

## The suite, as the instrument now reports it

    items 73625   stalled 0   provider errors 0   http requests 0

| verdict | rows |
|---|---:|
| correct | 55,377 |
| absent | 15,838 |
| wrong.unknownepisode | 1,737 |
| **wrong.kind** | **573** |
| wrong.entity | 100 |
| partial | **0** |

Sums to 73,625. Every by-shape row reconciles.

| shape | correct% | wrong.KIND | wrong.ent | wrong.unk | absent |
|---|---:|---:|---:|---:|---:|
| tv.sonarr, tv.flat.titled, tv.twoseason, movie.sonarr | 100.0% | 0 | 0 | 0 | 0 |
| tv.flat, tv.numbered, tv.sonarr.plain | 99.9% | 0 | 0 | 4 | 0 |
| tv.partial, tv.single | 99.9% | 0 | 0 | 1 | 0 |
| movie.scene / yearfile / yearfolder | 99.9% | 0 | 1 | 0 | 1 |
| tv.root | 63.9% | 0 | 41 | 593 | 1,401 |
| tv.noyear | 63.9% | 0 | 41 | 603 | 1,391 |
| tv.scene | 61.6% | 0 | 14 | 527 | 1,624 |
| movie.noyear | 58.8% | 0 | 1 | 0 | 705 |
| tv.handmade | 0.0% | 0 | 0 | 0 | 5,644 |
| **tv.episodetitle** | 0.0% | **573** | 0 | 0 | 5,070 |

## Next, in the order decided

1. **The collision tier's 1,607 confident wrong binds** —
   `exact_title_episode_count` (989) and `exact_title_season_count` (604) choosing
   the wrong same-titled entity at 0.90 confidence. Largest severity item; needs
   an ADR, because it is a question about what evidence may decide a binding.
2. **The title-prefix family** — `Suits → Suits LA`, `Gilmore Girls → A Year in
   the Life`, `Stranger Things → Tales from '85`, `Sherlock → Sherlock & Daughter`,
   `Avatar → Avatar (2024)`. The stored title is a strict *prefix* of the bound
   entity's. A distinct mechanism from a same-title collision and the cleanest
   remaining fix.

Both are now measurable, and `wrong.kind` means a change that moves a file
between libraries can no longer hide in `partial`.
