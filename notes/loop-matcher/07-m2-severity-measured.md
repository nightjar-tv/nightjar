# Iteration 7 — M2's severity, measured. The guard was one leading number.

The caveat that has been on every report since the start is gone, and the last
unmeasured mechanism now has a figure.

## Added as a shape, not as an edit to the entity set

`tv.episodetitle` renders `Show/Season 1/Closure.mkv` — the episode title alone.
It is `tv.handmade` with the `NN - ` prefix removed **and nothing else changed**:
same folder with no year, same season directory, same episode titles from the same
cached payloads.

The 2,410-entity set is untouched. `gen_library.py` reads `out/entities.json` and
was re-run alone, so `pick_entities.py` never saw the warmed cache — which is what
would have grown the entity set and broken every existing join.

    shapes 18, runs 36, files 73625, entities 2410

Every pre-existing shape's row count is unchanged.

## Two defects found in the process, both mine

**The shape.** A season can hold two episodes with the same title — `Furious` has
two called *"Some Days It Don't Come Easy"* — and with the number dropped they
render the same filename. One row of 5,644 violated
`UNIQUE(library_id, path)` and **aborted the entire 5,604-row batch**. The later
duplicate is now dropped rather than renamed: renaming would put a
disambiguating string into the name, and not having one is what the shape is
about. Cost: one row, reported rather than silently deduped. 5,643 not 5,644.

**The script, and this one is worse.** `warm_cache.sh` reported
**"converged: a whole round made no request"** while `tv.episodetitle-b0` panicked
in every round. A batch that never runs makes no request; so does a batch with
nothing left to fetch; and the round total cannot tell them apart. 5,604 rows
never drained and the summary called the warm complete.

Fixed: a failed batch now aborts the round, and the convergence message reads
*"and every batch ran"*. This is the same trap as `requests=N` counting attempts —
**a zero that means "the code never ran", read as "the work is done"** — for the
third time this session, in a third place.

## The measurement

**5,042 live requests** for this shape, converged in one round, every batch ran.
Cumulative warming across the session: **11,972 requests**, cache 8,185 → 20,157,
delta exactly 11,972. Shared cache untouched.

    runs 36   provider errors 0   http requests 0
    population 2410 entities, 73625 rows   noise floor 0

**Nothing pre-existing moved.** All 67,982 earlier rows carry the identical
verdict; zero changed, zero missing. The 5,643 new rows are all one shape. So
every comparison made earlier in this loop still holds.

### The controlled pair, on 5,643 shared slots

| | absent | film-bound |
|---|---:|---:|
| `tv.handmade` (`01 - Closure.mkv`) | **5,643** | **0** |
| `tv.episodetitle` (`Closure.mkv`) | 5,070 | **573** |

One variable. **Removing the leading episode number turns 573 unmatched files into
bindings against films — 10.2%.**

So the assumption the whole project has carried is correct and is now a number.
The oracle's README said *"the leading number is the only reason that query fails
to fold equal to a real film"*. It was right, and the cost of not having it is
10.2% of files leaving the TV library.

## The scorer cannot say this, and I did not teach it to

`tv.episodetitle` scores **0 correct, 0 wrong, 573 partial, 5,070 absent**.

Those 573 are the film bindings. For a tv entity the scorer looks for an episode
link, then a show link, and files "neither" as `partial`
(`score_binding.py:107`). **A movie link is neither.** So the most severe outcome
in the suite is filed under the verdict that reads as *incomplete but not wrong*.

It is not incomplete. `Elementary/Season 1/The Leviathan.mkv → tmdb:movie:333485`
has left the TV library altogether — worse than a wrong show, which at least keeps
the item in the right place and the right library.

Counted from the run databases instead
(`notes/loop-matcher/scripts/crosskind.py`), which reads what the scorer cannot
express:

| shape | episode files bound to a film |
|---|---:|
| **tv.episodetitle** | **573** |
| all 17 others | **0** |

Cleanly isolated to the new shape. Examples:

    Elementary/Season 1/The Leviathan.mkv          -> tmdb:movie:333485
    Archer/Season 1/Skorpio.mkv                    -> tmdb:movie:913072
    Scrubs/Season 1/My First Day.mkv               -> tmdb:movie:532495
    Avatar: The Last Airbender/S1/The Boy in the Iceberg.mkv -> tmdb:movie:1435531

**The scorer wants a `wrong.kind` verdict and I did not add one.** The hard limits
say an instrument being wrong is a finding, not something to edit around, and this
one *understates* severity — exactly the direction where editing it would flatter
nothing but would still be me changing the scorer to suit my result. Reported,
with the number computed independently.

## Where the matcher stands, whole population, nothing hidden

| shape | correct% |
|---|---:|
| movie.sonarr, tv.flat.titled, tv.sonarr, tv.twoseason | **100.0%** |
| tv.flat, tv.numbered, tv.sonarr.plain, tv.partial, tv.single | 99.9% |
| movie.scene / yearfile / yearfolder | 99.9% |
| tv.root | 63.9% |
| tv.noyear | 63.9% |
| tv.scene | 61.6% |
| movie.noyear | 58.8% |
| tv.handmade | 0.0% (all absent) |
| **tv.episodetitle** | **0.0% — 573 of them bound to films** |

Sonarr-shaped layouts are essentially perfect. Everything else runs 59–64%, and
the two title-only shapes bind nothing correctly at all.

**The caveat is retired.** Every rate above is over the whole population: 73,625
rows, 0 stalled, 0 provider errors, 0 requests, noise floor 0.
