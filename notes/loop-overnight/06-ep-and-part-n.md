# Iteration 6 — one word added, one word refused

Base `f527198`, on top of `9196cf3`. Commit `89e6cd6`. **Kept.**

Two vocabulary candidates came out of the same counterexample search. One earns
a case and shipped; the other is refused with the titles that caused it.

## `ep` — added

### The population

`split-one-run` (N2) is 10, and five of them want a season the N1 decision
refuses. Two of the remaining five fail on the **episode alone**, and one of
those is a word:

    221208 ABC123 Series Title Season 39 ep11.mp4
       season 39 claimed, episode wanted 11, got None

`SPELLED_EPISODE_WORDS` held `episode`, `episodio`, `capitulo`. It did not hold
the two-letter abbreviation. **Population: 1.**

### The per-word search

Followed by digits — which is the only shape the scan will read — `ep` appears:

| where | count |
|---|---:|
| the corpus's title **expectations** | **0** |
| the 2,475 dogfood `db_title`s | **0** |
| the 25,043 dogfood **basenames** | **0** |
| the sweep's 2,332-title pool | **0** |

As a **bare word** it appears **once** in the whole library:

    Smiling Friends - 3x08 - The Glep Ep - WEBDL-1080p.mkv

and no digits follow it. That is the same shape, and the same reasoning, that
`cap` records for `Dad's Red Cap`.

**The sweep does carry the word with digits — 2,332 times.** It renders
`{title} Ep01 1080p x264.mkv` for every one of its titles. None of them states a
season, so none reaches the bare-season arm, which is the guard that makes this
list safe at all. **If that guard were wrong, all 2,332 would move.**

### Predicted, then measured

Predicted: corpus 606 → **607**; `--diff` `+1 / -0` clean; sweep 0; probe 0.

| instrument | base | head | movement |
|---|---|---|---|
| corpus | `pass 606 fail 128` (`9196cf3`) | `pass 607 fail 127 n/a 110` — **82.7%** | **+1** |
| `--diff` `9196cf3` → `89e6cd6` | — | `+1 / -0`, gained **none**, fixed **none** | exit **0** |
| parser sweep, 74,624 names | `9196cf3` | `89e6cd6` | 0 / 0, gains `title 0 season 0 episode 0 year 0` |
| dogfood probe, 25,043 database paths | `f527198` | `89e6cd6` | **0 rows of 50,086 changed** |

**The sweep's zero is genuinely sensitive**: 2,332 of its names carry `ep` and a
number, and the only thing keeping them out is the arm this list runs in. Zero is
the answer that guard predicts.

**The probe's zero is insensitive by construction**: 0 of the 25,043 basenames
carry `ep` followed by digits at all. It could not have said anything else.

### Controls

| deleted | test that went red |
|---|---|
| `"ep"` from `SPELLED_EPISODE_WORDS` | `a_spelled_season_reads_an_abbreviated_episode_marker` |

`a_bare_ep_in_an_episode_title_claims_nothing` locks the **arm** rather than the
word — `Smiling Friends - 3x08 - The Glep Ep` carries `3x08`, so
`find_season_episode` claims it and the spelled list is never consulted. It is
recorded as locking the arm, not as a second control for the word.

`#[test]` attributes on full paths: **854 → 856**, both in `core/src/filename.rs`
(118 → 120).

## `Part N` — refused

### What it was

`drop-a-trailing-word` (D3) is 14, and its largest coherent group is five records
of Sonarr's mini-series form:

    The.Big.Series.Leader.Part.2.DSR.XviD-SYS          want episode 2
    24 7 Series-Title - Road to the Sonarr Part01 …    want S1 E1
    24 7 Series-Title - Road to the Sonarr Part 02 …   want S1 E2
    App.Sonarr.Made.in.Canada.Part.Two.720p …          want S1 E2
    John.Smith.The.Series.Title.5of9.The.Universe …    want S1 E5

Four of the five want season 1 out of a name that states none, so the N1 decision
caps this at **one verdict** however well it works.

### The counterexample search, and the refusal

**`Part` followed by a number is a sequel, not an episode.** Twelve real titles
in the dogfood library carry it, every one of them a film:

    Dune Part Two
    Harry Potter and the Deathly Hallows Part 1
    Harry Potter and the Deathly Hallows Part 2
    The Hunger Games Mockingjay Part 1
    The Hunger Games Mockingjay Part 2
    The Twilight Saga Breaking Dawn Part 1
    The Twilight Saga Breaking Dawn Part 2
    Mission Impossible Dead Reckoning Part One
    Rebel Moon Part One A Child of Fire
    The Descent Part 2
    Top Gear - 22x00 - Special Patagonia Part One
    Top Gear - 22x00 - Special Patagonia Part Two

All twelve are in the sweep's title pool as well, which renders **384 of its
74,624 names** with the word, and **117 of the 25,043 library basenames** carry
it.

**One corpus verdict against 384 generated names and 117 real files.** Refused,
and recorded with the titles.

`german` and `v2` — the other two vocabulary groups in D3, seven records between
them — stay refused on the precedents already written down: `The Good German`,
and `Operation V2 (2021)` measured to `Operation`. Neither was re-derived.

**What is left of D3 after all three refusals is two CJK cases**, which are D5's
mechanism rather than a word list.
