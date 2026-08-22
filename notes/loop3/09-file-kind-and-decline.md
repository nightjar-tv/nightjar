# Iteration 7 — the file decides its own kind, and a decline keeps one

Three findings from a review of this branch, fixed and re-measured here. The
review deliberately did not write the fixes; a different session did. **This
slice does not merge.**

The first two findings are one defect.

## The premise was false

`stored_kind` asserted that **a file under a season directory is not a film**,
with `Specials/` carved out. That is false wherever a library files a film under
`Season N/`, and the dogfood library does exactly that:

    Futurama/Season 5/Futurama Bender's Big Score (2007).avi
    Futurama/Season 5/Futurama Bender's Game (2008).avi
    Futurama/Season 5/Futurama Into the Wild Green Yonder (2009).avi
    Futurama/Season 5/Futurama The Beast with a Billion Backs (2008).avi

Four standalone direct-to-DVD features, each with its own TMDB movie record.
They are misfiled — they belong in a specials directory — and **the matcher
still has to cope, because real libraries are misfiled.**

And it is worse than a wrong search. `episode_group_key` ignores the cleaned
title when the show folder is non-empty (`queue.rs:943`), so once these are
episodes they join the group bound to the Futurama series and **cannot reach
their movie records by any route**.

**The fix reads the file, not the folder.** A basename asserting its own year
and carrying no episode marker is a film wherever it sits. `MediaKind::Movie`
from `parse_filename` already *is* the statement that no season/episode token
was read — the movie arm is the only one that returns it, and it returns
`season: None, episode: None` with it — so the year is the one bit the rule
needs beyond the kind.

### Measured before it was written

**None of the 573 `wrong.kind` rows carries a four-digit run of any kind in its
basename**, let alone a year. All 573 are `tv.episodetitle` — `Dept. Q/Season
1/Episode 1.mkv`. The rule buys the four Futurama films and gives up none of the
573. **The trade is zero**, and it was checked before a line was written.

What it does give up is an episode whose *title* contains a year and which
carries no episode number — `Season 1/Christmas 1999.mkv`. None exists in the
dogfood library or in any generated shape. That is the honest price, and it is a
file the folder rule was guessing about anyway.

## `Specials/` was one spelling of the exception, not the exception

The same predicate accepted `Season 0`, `Season 00`, `S00` and `s0` as
**numbered** — the precise semantics the carve-out exists to exclude. Season zero
*is* the specials season: TMDB numbers it 0, and Plex, Kodi and Jellyfin all
write it as a directory.

`season_directory_number` now answers both questions from one parse, so season
zero stops being numbered without stopping being a season directory and
`show_folder_relpath`'s walk is unmoved.

**It is not made redundant by the file-level rule, and that was checked by
reading rather than assumed.** The two cover different files: the file-level rule
keeps a specials film that carries its own year, wherever it is filed; this one
keeps a specials film that carries **no** year — `Polar Special.mkv` — when the
library spells its specials folder the Plex way. Neither dissolves the other.

**Zero reach in anything measurable.** Neither the dogfood database nor the
capture holds a single `Season 0` or `S00` directory. Latent, and the predicate
serves every library.

## Declining an episode number must not flip the kind

At six digits the parser declined the whole token and fell through to the movie
arm:

| name | before | after |
|---|---|---|
| `Show.Name.S01E123456…mkv` | kind **Movie**, title `Show Name S01E123456` | kind **Episode**, title `Show Name`, season 1, episode `None` |
| `Show.S2016E20160225.mkv` | kind **Movie**, title `Show S`, year 2016 | kind **Episode**, title `Show`, season 2016, episode `None` |

Declining is the safe direction for the **episode field**. It was not safe for
the other three: it threw away the title cut and flipped the kind — **one wrong
field traded for three**, and a wrong kind is the class the scorer ranks first in
`ORDER` as the worst in the suite.

**What it returns now**, and all four are asserted in the test: season — the
season it read; episode — `None`; kind — `Episode`; title — the cut at the token.
Exactly a season pack, because that is what the name now amounts to.

The bare `NNxNNNNNN` keeps declining outright. It has no `S` and no `E` to vouch
for it and it is the shape of a resolution — the same reasoning that lets the
marked spelling carry a four-digit season.

**The old test asserted `season` and `episode` and never looked at `kind` or
`title`.** That is what let this through, and it is the third time on this
project a guard shipped tested only for what it rejects.

## The instrument work, in the same slice

**A `movie.seasondir` shape.** A film with its own year and its own movie record,
under `Season N/`. Paired with `movie.yearfile`, one difference apart. 1,712
rows; the population is **82,806**, not 81,094 — re-score, do not compare across.
The entity set did not move: `entities.json` is byte-identical at 2,410 and every
pre-existing shape's `capture.jsonl` is byte-identical too.

**No warming needed.** Measured through the shipped chain, a film under `Season
05/` builds the same query as `movie.yearfile` — the parent folder does not reach
a movie's query.

**But the shape stalls on the arm that gets it wrong, and the stall flatters that
arm.** Under the folder-only rule those files become episodes and search
`/search/tv` for a film's title — *coyote ugly*, *the bfg*. 1,689 rows stall on a
cache the oracle rightly will never hold, because no such TV entity exists.
Warming them would move them to `absent` or `wrong`, never to `correct`.

**Four instrument defects fixed**, each of which produced a published number:

- `run.sh` and `run_one.sh` defaulted `WT` to a stale tree whose harness
  re-derived the stored title with `parse_filename` alone. It still exists, so it
  did not fail — it reads `correct 58,186` where the corrected harness reads
  `63,830`. **Anyone running the oracle as documented got the wrong number.**
  There is now no default.
- **Strict mode was requested and never enforced.** Without the response-cache
  branch in `tmdb/mod.rs`, `NIGHTJAR_TMDB_CACHE_STRICT=1` is a no-op and the run
  goes live; the review's first run did. `preflight.sh` now checks the **built
  binary** for the variable, and that the harness calls `stored_title` and
  `stored_kind` rather than re-deriving either. It fails at the first step.
- `score_binding.py` dropped unmanifested rows silently. It prints the count now,
  either way.
- The guards' tests gained the positive-but-wrong cases they lacked.

## Measure

Both arms offline, distinct binaries, separate target directories.

### The oracle, on the new population

    runs 43   provider errors 0   http requests 0
    items 82806   stalled 0   unmanifested 0
    NOISE FLOOR: 0 of 82806 (0.0000%)   — both arms

    rows joined 82806   verdict changed 1712   same verdict different entity 0
       movie.seasondir   stalled -> correct       1687
       movie.seasondir   absent  -> correct         23
       movie.seasondir   stalled -> absent           1
       movie.seasondir   stalled -> wrong.entity     1

    CORRECT BINDINGS LOST: 0        correct gained: 1710

**Every moved row is `movie.seasondir`. Nothing else moved at all.**

| verdict | baseline | + the fix | delta |
|---|---:|---:|---:|
| correct | 64,862 | **66,572** | +1,710 |
| `wrong.kind` | 0 | **0** | 0 |
| `wrong.entity` | 5 | 6 | +1 |
| absent | 16,250 | 16,228 | −22 |
| stalled | 1,689 | **0** | −1,689 |

Shape pairings, both exact:

    movie.seasondir vs movie.yearfile   1712 joined, 0 differing
    movie.specials  vs movie.noyear     1712 joined, 0 differing

### `stored_kind` over all 25,043 database paths — not the capture

The comparison that found this. Three rules over the same paths:

| | flips `movie` → `episode` |
|---|---:|
| `origin/main` (parser alone) → branch tip (folder rule) | **7** |
| `origin/main` → this fix | **3** |
| branch tip → this fix | **4 revert to `movie`** |

The four are the Futurama films. The three that remain episodes are the Top Gear
`NNx00` specials the branch's own iteration 6 gained. Files under `Specials/` or
`Extras/`: **10 movie, 6 episode — identical under all three rules.**

### The parser corpus

    844 cases, 738 applicable, pass 532, fail 206 = 72.1%   — both arms
    fail->pass 0   pass->fail 0   parse moved, verdict did not 0

Not one case's parse moved.

### The parser sweep

    74,624 names   HEAD right BASE wrong 0   BASE right HEAD wrong 0

**Which kind of zero, for each finding:**

- Findings 1 and 2 — **insensitive by construction.** The sweep feeds bare
  basenames; not one of the 74,624 contains a `/`. There is no folder, so
  `stored_kind` is never reached.
- Finding 3 — **narrow by population.** The generator's widest episode number is
  two digits, and **zero** names carry a run of six or more after an episode
  marker. The rule's trigger never occurs.

Neither is a sensitive instrument reporting no change.

## Tests

**746 → 759.** Twelve from the branch, one from this slice
(`season_zero_is_a_season_directory_and_is_not_a_numbered_one`). `cargo test
--workspace -- --list` lists 759; `#[test]` and `#[tokio::test]` attributes agree.
`cargo fmt --check` and `cargo clippy --all-targets -D warnings` green on the
three changed crates.

## What this could not measure, named

- **A `Season 0` or `S00` directory.** Neither the database nor the capture holds
  one, and no shape generates one. The fix is guarded by tests alone.
- **An episode whose title carries a year and which has no episode number** — the
  file the new rule gives up. None exists anywhere measurable.
- **Whether the four Futurama films now bind to their movie records.** The fix
  restores the kind and the query; the binding needs a drain on a population the
  capture does not contain.
- **The dogfood strict pair cannot see the four files at all.** They are among
  the 610 database paths absent from the capture.
