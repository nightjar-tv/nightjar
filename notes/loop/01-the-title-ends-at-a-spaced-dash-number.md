# 01 — the title ends at a spaced dash-number

**Kept.** Corpus 245 -> 337 of 738 all-fields (33.2% -> 45.7%). Structure-only
530, unchanged. Zero corpus cases regressed. Zero dogfood items bound today are
at risk.

## Mechanism

Anime releases number episodes absolutely and separate the number from the
title with a spaced dash — `[Commie] Show - 11 [65F220B4]`. Nothing else in the
name says where the title ends, so the title ran on through the number, the
release junk and the hash. `cut_at_absolute_episode` ends it at the dash.

The absolute number itself is still not parsed. `ParsedName` has nowhere to put
one and the corpus dropped `absoluteepisodenumber` during extraction, so that
half is unscorable. The title half is fully scorable and is where the failures
were.

## Population, counted not estimated

- Corpus: 126 title-only failures classified as this shape. The rule turns 95
  of their titles into the wanted title and 92 of the cases into all-fields
  passes.
- Dogfood: 2 basenames of 25,043 change. Both are `movie` / `unmatched` — the
  two Top Gear Perfect Road Trip specials.
- The **glued** variant was measured and rejected before anything was written:
  it changes 215 basenames, 213 of them `ready`, and turns `Stargate SG-1` into
  `Stargate SG`. That measurement is now a test.

## Prediction vs actual

Predicted +97 all-fields, structure unchanged, 2 dogfood titles moved.
Actual **+92**, structure unchanged, 2 dogfood titles moved.

The five-case gap is the classifier over-counting, not the implementation
missing cases. Named:

- 3 cases (`Anime - 15.5 (S00E01) ...`) now have the **right title** and still
  fail on season and episode. A title fix, not an all-fields pass.
- 1 case (`SERIES / SERIES 靦腆英雄 - 11`) is a CJK dual title; the cut fires and
  the remaining title is still wrong. Separate class.
- 1 case (`My Series - １５８`) uses full-width digits. The Python classifier's
  `\d` matches them; `is_ascii_digit` does not. Not fixed, correctly so — a
  full-width numeral is its own mechanism.

## Dogfood check, and what it is not

**The strict replay harness is not on this machine.** There is no
`server/crates/api/src/bin/replay.rs` in this tree or anywhere in its history,
no media or NFO capture, no TMDB response cache, and no
`NIGHTJAR_TMDB_CACHE_STRICT` in the shipped code. A control pair with
`requests=0` could not be run and none is claimed.

What was run instead, offline and read-only against a copy of
`nightjar-data-v9/nightjar.db`:

1. **Parse diff** over all 25,043 basenames. 2 items move, both title-only.
2. **Group diff** using the shipped grouping predicates — `clean_movie_title`,
   `query_key`, `year_from_path`, `show_folder_relpath`, `clean_show_title`,
   `series_library_year`, `pick_reference_episode` — reproducing the search
   input `drain_pending` builds. 2 group keys move, 2 search inputs move, and
   no unmoved item shares a moved group. **0 items bound today are at risk.**

This bounds which items *could* change outcome. It does not run the matcher, so
it is weaker than a replay pair and should not be read as one.

Movies group by title key, not by folder, which is why the folder holding the
two moved files is not the blast radius. That was checked in `queue.rs`, not
assumed.

## Control and treatment really differed

Baseline built from a stashed tree and re-measured at 245 — the same number as
`a04e7c2`. `corpus_run`, `parse_all` and `group_keys` binaries have different
sha256 on the two sides.
