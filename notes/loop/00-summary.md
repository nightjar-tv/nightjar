# The parser loop — summary

Fifteen iterations on `nightjar-core`'s `parse_filename`, measured against the
reduced Sonarr/Radarr corpus. Fourteen kept, one reverted.

Branch `loop/parser-corpus` in the worktree `~/Documents/GitHub/nightjar-wt-loop`.
**Nothing is pushed.** The branch does not exist on the remote.

## The number

| | before | after |
|---|---:|---:|
| **all-fields** | **245/738 — 33.2%** | **499/738 — 67.6%** |
| **structure-only** | **530/738 — 71.8%** | **612/738 — 82.9%** |

Baseline measured at `a04e7c2` on a tree without any change, reproducing the
33.2% already in circulation. Every iteration measured a fresh before and after.
**Zero corpus cases regressed at any point** — `newly failing 0` on every kept
iteration, verified positionally after the comparator defect found at
iteration 03.

## Per iteration

| # | change | all-fields | structure |
|---|---|---:|---:|
| — | baseline `a04e7c2` | 245 (33.2%) | 530 (71.8%) |
| 01 | the title ends at a spaced dash-number | 337 **(+92)** | 530 |
| 02 | a season token with no episode is a season | 390 (+53) | 576 |
| 03 | a bracket group is atomic | 402 (+12) | 576 |
| 04 | a bare episode marker ends the title | 413 (+11) | 576 |
| 05 | only a real extension is stripped | 423 (+10) | 585 |
| 06 | the episode marker may be spelled out | 430 (+7) | 585 |
| 07 | **the year boundary — reverted** | 430 (+0) | 585 |
| 08 | a four-digit season, marked spelling only | 435 (+5) | 591 |
| 09 | the terminator runs on every branch | 444 (+9) | 591 |
| 10 | a bracket run selects its title group | 463 **(+19)** | 591 |
| 11 | a leading site prefix is not the title | 468 (+5) | 591 |
| 12 | a dash is not the only separator | 472 (+4) | 596 |
| 13 | the season and episode may be separated | 488 **(+16)** | 612 |
| 14 | a title ends at a bracket it never opened | 494 (+6) | 612 |
| 15 | the episode marker may follow the number | 499 (+5) | 612 |

## Dogfood

**Zero bindings broken, and zero items bound today are even at risk.**

Across all fifteen iterations, exactly **three** of the 25,043 dogfood items
changed parse at all:

- two Top Gear specials (iteration 01), `movie` / `unmatched`, title only;
- one Red Dwarf season-9 special (iteration 02), `movie` / `unmatched`, which
  gains a correct season 9 and becomes an episode.

Iterations 03 to 15 changed **nothing** in the library — not one item, not one
group key, not one search input.

The Red Dwarf item is the only one with a blast radius: its kind change moves
it into the `Red Dwarf (1988)` show folder, so 67 bound items see that folder's
counts move. Every predicate their binding rests on was traced and holds; see
notes/loop/02. **That was read, not run** — see the limits below.

## What could not be measured, named

**The strict replay harness is not on this machine.** No
`server/crates/api/src/bin/replay.rs` in this tree or anywhere in its history,
no media or NFO capture, no TMDB response cache, and no
`NIGHTJAR_TMDB_CACHE_STRICT` in the shipped code. No control pair was run and
none is claimed.

The substitute, built for this loop and used every iteration:

1. **`parse_all`** — the shipped parser over all 25,043 dogfood basenames.
2. **`group_keys`** — the search input `drain_pending` would build for every
   item, using the shipped predicates (`clean_movie_title`, `query_key`,
   `year_from_path`, `show_folder_relpath`, `clean_show_title`,
   `series_library_year`, `pick_reference_episode`). A bind is a function of
   the group key and the group's search fields, so an item is at risk exactly
   when its key moves, its group's fields move, or it shares a key with
   something that moved.

This **bounds** which items could change outcome. It does not run the matcher.
It is weaker than a control pair and must not be read as one.

**The corpus is parse-level.** It says nothing about provider search, candidate
scoring, or binding. A green corpus is not a working pipeline.

## The ceiling, measured

239 failures remain. Counted from the case file, not estimated:

| cause | cases | can a parser change earn it? |
|---|---:|---|
| addressable in the parser | 174 | yes |
| wants an empty title | 29 | the parser can; **the caller cannot** |
| the season is in a path component | 22 | no — the runner passes the basename, by design |
| the corpus stores a squashed title | 9 | no — a harness artefact |
| the corpus stores absent as `0` | 5 | no — `None` never equals `Some(0)` |

**The ceiling for a parse-level, parser-only change is 673/738 = 91.2%.**

## What I would do next, and why in this order

**1. The empty title — 29 cases, and it is a three-part slice, not a one-liner.**
The largest single class left. `S03E09 WS PDTV XviD FUtV` has no series title in
the filename; the folder has it. The parser change is one line and it is
**blocked on the caller**, verified by reading it: `scanner/src/lib.rs:233` and
`:688` store `parsed.title` verbatim with no folder fallback, and
`TmdbClient::search` builds `("query", title)` with no empty check — the only
empty-title guard in the crate is inside a test double. Doing the parser half
alone trades 29 corpus cases for an empty provider query. The slice is parser +
scanner folder fallback + queue guard, and it needs the replay pair.

**2. The four-digit season in the bare spelling, then the year boundary — 13
year corrections that currently cost two titles.** Iteration 07 is reverted and
notes/loop/07 has the argument: the bogus year from `1920x1080` is acting as an
accidental title terminator, and two titles go from right to wrong when it is
removed. Both are the bare `2016x231` form. Give that form a real terminator
first and the year fix costs nothing. It needs a discriminator between
`2016x231` and `1920x804` that a filename may not carry — start there.

**3. Season-number normalisation in the soft key — a handful of anime cases.**
Several want `Anime-Series Title S2` and get `Anime-Series Title S02`. That is
`clean_show_title` in `nightjar-metadata`, not the parser, and it is
matcher-visible, so it wants the replay pair.

**Not worth doing, and each has the measurement that says so:**

- **A trailing standalone number.** 8 corpus gains against 420 broken bound
  library titles at its loosest guard and 4 at its tightest — `Apollo 13`
  becomes `Apollo`. notes/loop/06.
- **A glued dash-number.** 213 bound items broken; `Stargate SG-1` becomes
  `Stargate SG`. notes/loop/01.
- **A separator run between episode tokens.** One corpus case against four
  bound items; `9-1-1 - 2x02 - 7.1` becomes six episodes. notes/loop/13.
- **`german` in the junk vocabulary.** Seven corpus gains, and the corpus holds
  its own refutation: `The.Good.German.2006` is a real film. notes/loop/09.

## Method notes worth keeping

- **The population check paid for itself four times**, each time before a line
  of code was written. Every rejection above came from querying the library
  first.
- **A prediction that misses is a finding.** Iteration 03 predicted +18 and got
  +1: the rule fixed one route into the mechanism and seventeen cases came
  through two others. Extending it to all routes gave +12.
- **My own instrument had a defect.** `analyse.py` compared results through a
  dict keyed on the input string, and the corpus holds duplicate inputs, so it
  invented both a gain and a loss. Fixed to compare positionally; iterations 01
  and 02 were re-verified.
- **One shipped test changed**, at iteration 10, because the new behaviour is
  right rather than to make a change pass. The old expectation is recorded in
  the test's own comment. No test was weakened or deleted.

## One red test, unrelated

`nightjar-transcode`'s `hls::tests::mapped_real_library_end_moov_mp4_copy_keeps_aac`
fails intermittently with `video starts at 63.646s, land was 58.975s`. It
**passed and failed on the same tree in two consecutive runs** during the final
gate, and stashing the only source change at iteration 03 reproduced the failure
on the unchanged tree. It asserts a start time against a real media file. Not
touched, not weakened.

Everything else is green: 100 core, 256 metadata, 89, 65, 43, 15, 3 and 2 across
the other crates. Clippy clean at `-D warnings`. `cargo fmt --check` clean.
