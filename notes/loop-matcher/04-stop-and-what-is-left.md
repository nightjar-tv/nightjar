# Iteration 4 — stopped. Everything left hits a stop condition.

Three iterations, three keeps, no reverts. I stopped here rather than at 15
because every remaining mechanism is either **unmeasurable by the four
instruments** or **needs an ADR decision** — both named stop conditions. What
follows is the evidence for that claim, mechanism by mechanism, so the next
reader does not have to re-derive it.

## The candidate I examined and did not make

The drain and the manual retry disagree about movie year precedence, and the
drain contradicts the rule `clean.rs` documents.

    clean.rs:7   /// Prefer folder `Title (Year)` over probe year …
    fix.rs:111   clean_movie_title(&item.title, year_from_path(&item.path).or(item.year))
    queue.rs:713 clean_movie_title(&it.title, it.year.or(folder_year))

Three ways, two answers. `fix.rs` matches the docstring; the drain — the path
that matters — takes the filename year first. That is exactly the
`p.year.or(folder_year)` the brief names as M3.

**I did not change it, because I can show first that it would measure flat.** The
oracle prints the query the drain actually built:

    movie.yearfolder:  match Blade Runner → tmdb:Some(78) method=exact_title_year
    movie.yearfolder:  match Wonder Woman → tmdb:Some(297762) method=exact_title_year

The query is `Blade Runner`, not `Blade Runner 2049`. **The title is already
truncated before any year is consulted**, so no year precedence can help: `Blade
Runner 2049` is not an exact-title match for `Blade Runner`, and every year
lands on tmdb:78 (Blade Runner, 1982). Changing 2049 to 2017 changes which
`Blade Runner` is nearest, not which film is found.

So M3's remaining cause is the **parser truncating a title at a trailing
four-digit number**, in `nightjar-core` — the code #149 rewrote 2,249 lines of.
Its measurable population is **8 rows across 2 entities** (Blade Runner 2049,
Wonder Woman 1984), and touching it puts the 74,624-name sweep live. Eight rows
does not buy that blast radius, and the judge rule would revert a flat result
anyway. Running a change I have already shown cannot move the instrument is
theatre, not measurement.

**The precedence inconsistency is still a real defect and should be fixed** — by
someone who can measure it, or as a correctness change justified on the
docstring rather than on a number.

## M1 and M2 — cannot be measured

Established in iteration zero and unchanged. The replay's `NIGHTJAR_REPARSE`
re-derives the title with `parse_filename` alone and omits the scanner's
`title_from_folder`, so the oracle cannot see the scanner's title derivation at
all. `tv.numbered` sits at 5,644 measured / 0.0% correct for that reason and not
because the product fails there.

`tv.handmade` remains **100% stalled — 5,644 rows never measured.**

I did not edit the oracle. Per the hard limit, a wrong instrument is reported.

## M5 — a genuine information limit, and a new signal is an ADR decision

`exact_title_collision_unpinned` is the dominant remaining reason: **1,305 items**
across `movie.noyear` (705), `tv.root` (457) and `tv.noyear` (143).

The candidates were read out of the cached search responses the matcher actually
saw (`notes/loop-matcher/scripts/collisions.py`). They are **real films and shows
sharing an exact title**, not a fold agreeing with too much:

    The BFG              2 exact:  2016, 1990
    The Stepford Wives   2 exact:  2004, 1975
    Doubt                9 exact:  2008, 2013, 1951, 2003, 2009, 1982, 2021, …
    The Visitor         16 exact:  1979 … 2025
    CODA                17 exact:  1970 … 2026

Distribution over `movie.noyear`'s 704 collision groups: 187 have 2 exact
candidates, and the tail runs to 20.

**There is no year on disk in any of these shapes** — `movie.noyear` is
`Name/Name.1080p.BluRay.mkv`, `tv.noyear` has a yearless folder, `tv.root` has no
folder. A human could not break these ties from the filename either. Breaking
them needs a signal the matcher does not currently use — the probe's `duration_ms`
against TMDB `runtime`, or provider popularity ordering. **Choosing such a
tie-break is an ADR decision**, and it changes what a binding *means*, so it is
not a loop's call.

*Caveat on the numbers above:* `collisions.py` normalises titles with its own
rule, not the shipped fold. The candidate lists it prints are read from the cache
and are trustworthy; the exact **counts** may differ from what the shipped chain
computes, and a reimplemented `norm_key` has misreported this project before. It
reported 21 groups with a single exact candidate, which the shipped code cannot
produce — a sole exact hit reports `exact_title` or `exact_title_zero_seasons`,
never `collision_unpinned`. That discrepancy is my script, not a defect, and it
is why the counts are labelled approximate.

## tv.scene — the largest remaining measurable mechanism, and it is an ADR change

`tv.scene` renders one folder per episode, named for the release:

    Scrubs.S01E01.1080p.WEB-DL.x264-GRP/Scrubs.S01E01.1080p.WEB-DL.x264-GRP.mkv

`is_season_directory` does not recognise that as a season directory, so
`show_folder_relpath` calls **each episode's release folder a show folder.** Every
show fragments into ten single-episode groups:

| shape | files | groups |
|---|---:|---:|
| tv.flat | 5,644 | **1,202** |
| **tv.scene** | 5,644 | **9,035** |

A single-episode group has one season and one episode, so `pin_collision`,
`sole_season_coverer` and `primary_by_slots_explained` have no coverage evidence
to work with — which is why `tv.scene` keeps 342 `negative_cache` groups, 51
unpinned collisions, 395 absents, 8 wrong binds and 1,812 stalls while `tv.flat`
now has none of the first four. The fragmentation also multiplies provider calls:
`errors=1780` against `tv.flat`'s `184`.

This is the same family as M4 and M6 — the show-folder rule naming the wrong
directory — and it is the largest measurable mechanism left. **But the fix
changes what ADR-0033 Q2 defines a show folder to be**, which every consumer
depends on agreeing about. That is an ADR decision, so the loop stops here and
hands it over.

## Stop conditions hit

- **A change needs an ADR decision** — M5's tie-break signal; `tv.scene`'s
  show-folder definition.
- **A mechanism cannot be measured** — M1, M2, and `tv.handmade` entirely.

Not hit: two consecutive reverts (there were none), a schema migration, an
unexplained dogfood regression, 15 iterations.
