# ADR-0053: Which layer decides a file's kind, once the parser can see its folder

- Status: **proposed**
- Date: 2026-08-23
- Supersedes: nothing
- Amends: nothing yet — whichever option wins changes where ADR-0033's
  scanner-level kind rule lives, not what it decides
- Depends on: ADR-0033 (Q2 folder scope); ADR-0025 (one item per episode slot);
  ADR-0049 (the same walk, still proposed, and still the neighbouring question)
- Gate: Gate 3 — metadata auto-match ≥95% correct
- Related: `nightjar_core::parse_filename_in` and `nightjar_db::season_number_for_path`,
  both landed unwired on `loop/board-residual`; the matcher oracle's
  `tv.handmade`, `tv.episodetitle` and `movie.seasondir` shapes

## Context

`parse_filename` takes a basename by design. A basename cannot carry the show's
title when the file is called `Episode 1.mkv`, and cannot carry the season when
the file is called `01 - Closure.mkv`. The folder can, and three production call
sites already hold it:

| site | what it is |
|---|---|
| `scanner/src/lib.rs:388` | the full-library walk |
| `scanner/src/lib.rs:843` | the notify / hint-ingest path |
| `metadata/src/queue.rs:1696` | `EpisodeSlot::season_episodes`, inside the matcher |

**The third is production code and is easy to miss** — it re-parses the basename
to expand `NxMM-NN` ranges "so we do not need an `episode_end` column", and it
is in the matcher rather than the scanner. A scoping note that counted two sites
would move two and leave the matcher reading the old answer.

Its need is narrower than the other two, and worth stating because it changes
what each option costs there. `EpisodeSlot` already carries the stored `season`
and `episode`; the re-parse exists only to recover the **episode span**, through
`episode_numbers()`. So this site does not want a title and does not want a
season — it wants the range. That also makes it the site a change to the episode
span reaches first, which is why the padded-separator work on
`loop/board-residual` was measured on the oracle rather than on the corpus
alone.

A seam now exists for handing the folder down —
`nightjar_core::parse_filename_in(file_name, FolderContext)` — and it is
deliberately unwired. **This record is the decision that has to be made before
anything moves through it.**

## The problem: two layers hold half the answer each

**`nightjar_scanner::stored_kind` decides the kind, above the parser.**

    pub fn stored_kind(parsed: MediaKind, parsed_year: Option<i32>,
                       stored: &str, library_root: &str) -> &'static str {
        if parsed == MediaKind::Movie
            && parsed_year.is_none()
            && under_numbered_season_directory(stored, library_root)
        { return MediaKind::Episode.as_str(); }
        parsed.as_str()
    }

**`nightjar_scanner::stored_title` decides the title, also above the parser** —
an empty parse takes the show folder's name.

**But the parser cannot fill a season without knowing the kind.** A season
belongs to an episode; putting one on a film is a field nobody asked for on a
row nobody checked. So `parse_filename_in` gates its season rule on the parsed
kind — and the parsed kind, for exactly the files this work exists for, is
`Movie`:

| basename | parsed kind | parsed title | parsed season |
|---|---|---|---|
| `Season 01/Episode 1.mkv` | `Movie` | `"Episode 1"` | `None` |
| `Season 1/01 - Closure.mkv` | `Movie` | `"01 - Closure"` | `None` |
| `Season 5/Futurama Bender's Big Score (2007).avi` | `Movie` | `…` | `None` |

The first two are episodes and `stored_kind` correctly says so. The third is a
real film in a real library and `stored_kind` correctly says *that*, because it
carries its own year. **The parser cannot tell them apart without the rule that
lives a layer up, and the layer up runs after the parser.**

Both are also non-empty titles, so `stored_title`'s substitution never fires for
them either: the folder would have to *override* a title the basename asserted,
which is a different rule from filling a silence — and one that likewise needs
the kind decided first.

### And the two layers already disagree about one path

`under_numbered_season_directory` and `season_number_for_path` walk the same
tail and differ by design on exactly one input:

| path | `under_numbered_…` | `season_number_for_path` |
|---|---|---|
| `Show/Season 03/x.mkv` | true | `Some(3)` |
| `Show/Specials/x.mkv` | false | `None` |
| `Show/Season 0/x.mkv` | false | `None` |
| `Show/Season 99999999999999999999/x.mkv` | **true** | **`None`** |

`season_directory_number` takes `u32::MAX` for a digit run too wide to parse, so
the segment does not stop being a season directory at sixteen digits — right for
the predicate, useless as a number.

**This is pinned by a test, not by a comment**
(`an_overwide_season_is_a_directory_but_not_a_number`), so it cannot be tidied
away by someone making the two functions "agree". **Whichever option this record
takes has to say what a file in that folder gets**: it is the one path where
"is this an episode" and "which season is it" cannot both be answered from one
value.

## The measurement

Matcher oracle, `origin/main` at `e3208cc`, 90,072 rows, cache
`tmdb-cache-kind`, `requests=0`, noise floor 0.

| shape | measured | correct | absent | correct% |
|---|---:|---:|---:|---:|
| `tv.episodetitle` — `Episode 1.mkv` | 5,844 | **0** | 5,844 | **0.0%** |
| `tv.handmade` — `01 - Closure.mkv` | 5,840 | **0** | 5,840 | **0.0%** |
| `movie.seasondir` — a film under `Season 05/` | 2,062 | 2,024 | 37 | 98.2% |
| `tv.numbered` — `S01E01.mkv` | 6,054 | 6,054 | 0 | 100.0% |

**11,684 rows at 0.0%**, the two largest blocks of failure the instrument has,
and neither can move without the folder.

`movie.seasondir` and `tv.numbered` are the guards on the other side.
`movie.seasondir` is what a kind rule must not break — 2,024 films correctly
bound while sitting under a numbered season directory. `tv.numbered` is what
`stored_title`'s substitution currently earns, at 100%, and it must survive
wherever the title rule ends up.

## The decision this record asks for

**Where does the kind rule live, once the parser can see the folder?**

- **A. Nothing moves.** `parse_filename_in` stays unwired, the three call sites
  keep calling `parse_filename`, and `stored_kind` and `stored_title` stay where
  they are. The status quo, and a real answer: it costs nothing, breaks nothing,
  and leaves 11,684 rows at 0.0% permanently. The seam is then dead code and
  should be deleted rather than left as an invitation.

- **B. The kind moves down.** `parse_filename_in` takes the folder's
  *numbered-season-ness* as well as its number, applies the yearless-movie rule
  itself, and then fills title and season knowing the kind. `stored_kind`
  becomes a passthrough and is deleted; `stored_title` follows it down. **One
  owner, one order of operations**, and the parser stops being a layer that
  cannot see what it needs. Costs: `nightjar-core` gains a rule about season
  directories expressed as a boolean it is handed — not a path walk, but still a
  semantic it did not previously hold. And the over-wide-season path must be
  answered explicitly, because the boolean and the number disagree there.

- **C. The kind stays up, and the parser is told it.** `FolderContext` gains the
  already-decided kind, so `parse_filename_in` fills title and season without
  deciding anything. The scanner runs `stored_kind` *before* the parse rather
  than after it. Two owners, but each with one job, and `nightjar-core` learns
  nothing new. Costs: the call sites gain an ordering constraint that is easy to
  get wrong silently. `queue.rs:1696` is the cheap one here: `EpisodeSlot` is
  `{ id, season, episode, path, duration_ms }`, so it already holds the stored
  season and episode and could be handed the stored kind the same way — it has no
  `library_root`, and under C it would not need one.

- **D. The scanner keeps everything and the parser is left alone.** The folder
  rules move *up* instead: `stored_title` grows the episode-title override and
  the `NN - ` episode-number extraction, using `parse_filename`'s output plus the
  folder. No new parser entry, no ordering constraint. Costs: filename grammar
  gets written in the scanner, a second place that parses basenames, which is the
  reimplemented-predicate trap that once reported 25 non-folding folders against
  the shipped chain's 12.

**No recommendation is offered here, because the cost is not measured on any
option.** The oracle can price A — it is today's number, 0.0% on both shapes —
and it cannot yet price B, C or D, because none of them exists to be run. What
would decide between them is the `wrong.kind` and `movie.seasondir` columns
under each rule, and those need the rule written.

This is the same gap ADR-0049 names in its own decision section, and it is
recorded the same way rather than papered over with a preference.

## Consequences, whichever option wins

**The three call sites must move together or not at all.** They are the scanner
walk, the notify path and `EpisodeSlot::season_episodes`. A tree where the
scanner stores one kind and the matcher re-derives another is worse than a tree
where both are wrong the same way, because only the second is diagnosable.

**Every instrument's harness calls `parse_filename` directly** — `corpus_run.rs`,
`sweep.rs`, `replay.rs`, `oracle_query.rs`, `kindprobe.rs`, plus 161 occurrences
in `core`'s own tests and 7 in the scanner's. Options B and C change what
production calls; the harnesses must follow in the same commit or the instruments
measure a function the product no longer uses. **That has already happened once
on this project** — `replay.rs` re-derived `stored_title` with `parse_filename`
alone and scored 5,644 rows `absent` for a reason that was the harness.

**The seam is behaviour-neutral and must stay that way until it is wired.**
`parse_filename_in(name, FolderContext::default())` is asserted byte-identical to
`parse_filename`, so a call site with nothing to offer behaves exactly as today.
That property is what makes anything that moves attributable to the decision
rather than to the API.

**`movie.seasondir` is the regression test for all of B, C and D.** 2,024 films
currently bind correctly from under a numbered season directory. Any rule that
turns them into episodes trades 2,024 correct bindings for a wrong kind, which
`BLOCK1_LEAVE_BAR` ranks as the worst class the oracle has.

## What this record could not measure, named

- **The cost of B, C and D.** Each needs writing before the oracle can price it.
  Only A is measured, and only because it is the status quo.
- **The `NN - ` episode-number rule** — `01 - Closure.mkv` needs both the folder
  title *and* the number out of the prefix. Whether that is safe depends on the
  kind decision, so it is not specified here. `tv.handmade` is 5,840 rows of it
  and no other shape generates the form.
- **Whether option C's ordering constraint is enforceable.** Nothing in the type
  system stops a future call site from parsing before deciding the kind, and the
  failure would be silent.
- **Anything about real libraries named this way.** `populations.py` reports how
  much of the dogfood library each mechanism would touch; the one real library is
  Sonarr-named, so for these two shapes the answer is near zero. That means *not
  this library*, never *not anywhere* — and no count here should be quoted from
  the dogfood database, whose 25,043 paths are a different population from the
  capture's 25,004.
