# The overnight loop — final report

> **Re-baselined 2026-09-02 — see [`09-rebaselined.md`](09-rebaselined.md).**
> Every corpus figure below is through `parse_filename`. Through `stored_parse`,
> which is what the harness calls now, the branch reads **`608/734` → `617/734`
> (82.8% → 84.1%)**. **Every per-iteration delta is identical**; the offset is
> +10 at every point.

**Base: `f527198`**, the tip of `origin/main` on 2026-09-01
(`docs: ADR-0020 §12's cache budget is a constant, not a setting (#197)`).
The primary checkout's local `main` ref is stale at `e3208cc`; nothing here was
read from it.

**Branch: `loop/parser-board-overnight`**, in a worktree at
`~/nightjar-wt-loop5`. **Nothing was pushed. No PR was opened. Nothing was
merged. No commit was made to `main`.**

## Iterations

**Seven, five kept, zero reverted, two refused before implementation.**

| # | mechanism | outcome |
|---|---|---|
| 1 | the title cut and the year come from one read of the date | **kept** `b62ecb0` |
| 2 | a year is not a file extension | **kept** `15707ac` |
| 3 | a trailing `N-M` batch range | **refused** on the counterexample search |
| 4 | a run of leading groups is stripped down to the prose | **kept** `82e797a` |
| 5 | a year does not outrank an episode marker behind it | **kept** `9196cf3` |
| 6 | `ep` added to the spelled episode words; `Part N` refused | **kept** `89e6cd6` |
| 7 | score the corpus through `stored_parse` — a measurement, no parser change | **kept** `f530a07`; its two harness files deleted 2026-09-02 once the board switched |

No revert was needed, and the two consecutive-revert stop condition was never
approached. Iteration 5 **did** produce a regression, caught by
`classify.py --diff` while the corpus rate rose; a missing guard was added and
the measurement re-run before the commit was kept.

## Per instrument, before and after

### The corpus — `corpus_run.rs`, the shipped instrument

    f527198   pass 598  fail 136  not_applicable 110   81.5%
    f530a07   pass 607  fail 127  not_applicable 110   82.7%

    classify.py --diff, base against tip:
      verdict: +9 / -0
      fields gained inside failing cases: none
      fields fixed inside failing cases:  {'year': 1}
      no regression: 0 case(s) worse          exit 0

Per field, from the classifier's own per-class counts at each end:

| class | `f527198` | `f530a07` |
|---|---:|---:|
| drop-a-trailing-number | 22 | 22 |
| expand-a-range | 20 | 20 |
| harness-gives-a-path | 16 | 16 |
| drop-a-trailing-word | 14 | 14 |
| split-one-run | 10 | **9** |
| keep-a-year | 8 | **6** |
| strip-CJK-decoration | 8 | 8 |
| read-a-year | **7** | **3** |
| slash-inside-the-name | 7 | 7 |
| keep-a-season-marker | 6 | 6 |
| a-bare-marker-wants-a-season | 5 | 5 |
| drop-a-trailing-season-marker | 4 | 4 |
| drop-a-leading-group | **2** | **0** |
| everything else | 9 | 7 |
| **total** | **136** | **127** |

The classifier reconciles at both ends — 136 of 136 and 127 of 127.

### The parser sweep — 74,624 generated names

    base tree f527198   head tree f530a07
    names scored           74624
    HEAD right, BASE wrong 0
    BASE right, HEAD wrong 0
    gains by field: title 0  season 0  episode 0  year 0
    no regression

Run as a **null control at the base first** (`ALLOW_IDENTICAL=1`, `f527198`
against itself) to prove the harness deterministic before either arm of a real
comparison was believed. It reported 0/0 there too.

**Each zero's reason, stated per iteration** — this is the reading the brief
asks for, and it is not the same reason five times:

| iteration | sweep zero | why |
|---|---|---|
| 1 | 0 of 74,624 | **narrow by population.** 24 names open with a year-shaped run and **1** would reach a date cut, and that one declines on the `9-1-1` head-has-a-letter guard. 0 satisfy both halves. |
| 2 | 0 | **narrow by population.** 0 names end in an all-digit suffix; every form ends `.mkv` or `.WEB`. |
| 4 | 0 | **genuinely sensitive, and zero.** 13,992 names begin with a group and **4,664 carry two**; the unguarded rule moves every one of the 4,664 and the shipped rule moves none. |
| 5 | 0 | **narrow by population.** 5 names carry a year and a marker; 3 go to the episode arm, 2 decline because the marker's head — `1923` — has no letter. |
| 6 | 0 | **genuinely sensitive, and zero.** **2,332** names carry `ep` and a number. None states a season, so none reaches the arm the word lives in — and if that guard were wrong, all 2,332 would move. |

### The dogfood parse probe — all 25,043 **database** basenames

**The database, not the capture.** The strict replay pair reads
`capture-media-mac.jsonl`, which holds 25,004 paths; the database holds 25,043
and the 610-path gap has hidden a regression before. This is the database, read
`mode=ro`, extracted once to a file, **nothing written to it**.

    f527198 -> f530a07 :  0 rows of 50,086 changed

50,086 because it parses each path **twice** — through
`nightjar_core::parse_filename` on the basename, and through
`nightjar_scanner::stored_parse` on the path. At the base the two disagree on
three paths, and they are the three `NNx00` Top Gear specials
`notes/loop3/09-file-kind-and-decline.md` already named. That reconciliation with
a fact recorded before the probe existed is the only check available that it
reads the layer it claims to.

| iteration | probe zero | why |
|---|---|---|
| 1 | 0 of 25,043 | **narrow by population.** 19 basenames open with a year-shaped run, **1** carries a three-token date, **0** carry both. |
| 2 | 0 | **narrow by population.** 0 basenames end in an all-digit suffix. |
| 4 | 0 | **insensitive by construction.** **0 basenames begin with a bracket group at all.** It could not have reported anything else. |
| 5 | 0 | **narrow by population.** 94 candidates — every one `Alice in Borderland (2020) - 1x01 - Episode 1` — and all 94 carry a season/episode token, so the episode arm owns them and the movie arm, the only arm changed, never sees them. |
| 6 | 0 | **insensitive by construction.** 0 basenames carry `ep` followed by digits. |

**No dogfood count was used to justify a change.** Every mechanism on this board
is near-zero there, and each zero above is recorded as a fact about the
population rather than as evidence.

### `classify.py --diff` — required, exit 0 at every kept commit

| commit | verdict | fields gained | fields fixed | exit |
|---|---|---|---|---|
| `b62ecb0` | `+3 / -0` | none | none | 0 |
| `15707ac` | `+1 / -0` | none | `{'year': 1}` | 0 |
| `82e797a` | `+2 / -0` | none | none | 0 |
| `9196cf3` **first cut** | `+2 / **-1**` | none | none | **1 — REGRESSION** |
| `9196cf3` with the guard | `+2 / -0` | none | none | 0 |
| `89e6cd6` | `+1 / -0` | none | none | 0 |

**It earned its place again.** The first cut of iteration 5 took the corpus from
604 to **605** — a gain — while `The.Movie.from.U.N.C.L.E.2015.1080p.BluRay
.x264-SPARKS` went from pass to fail. A run reporting only the rate would have
kept it.

### Gates

At `f527198` and at `f530a07`:

    cargo fmt --all --check          0
    cargo clippy --all-targets -D    0
    cargo test --workspace           0

`#[test]` + `#[tokio::test]` attributes, counted on **full paths**:
**844 → 856**, every one of the twelve in `core/src/filename.rs` (108 → 120).
No file lost a test and no test was deleted. One existing test —
`the_episode_marker_does_not_eat_codec_tokens_or_words` — had an assertion
**changed**: it recorded a known gap in its own words and now records the fix.

**A gate caught something `--lib` did not.** An indented block inside a `///`
comment is a doctest, and `cargo test -p nightjar-core --lib` is green on a
broken one. Only `--workspace` fails.

## Per-iteration one-liners

1. **`b62ecb0`** — `find_year` took the first four-digit run wherever it sat, so
   `2020 A Late Talk Show 2012 16 02` reported 2020 while the date cut had
   already put `2020` in the title. `cut_at_date` now reports its year and wins
   when the year found is the run the name opens with. **+3.**
2. **`15707ac`** — `is_extension` accepted four alphanumerics, which describes
   `1998` as well as `webm`, so `Movie.The.Final.Chapter.2016` lost the year that
   `…2016.mkv` kept. **+1**, and a wider "no extension is all digits" was
   measured and refused: it would have broken `[DRONE]Series.Title.100` and added
   a title failure to `Series.Title.525`.
3. **refused** — a trailing ascending `N-M` range. Six corpus cases against 182
   sweep names (`Deadpool 2 - 07` → `Deadpool`) and 114 library files
   (`24 - 2x01 - Day 2 - 8-00 A.M.`); glued-only still costs 12
   (`Chernobyl - 1x01 - 1-23-45`). It is `drop a trailing number` reached
   sideways, and the measurement says so as loudly as the board does.
4. **`82e797a`** — fansub releases stack tags and only one was stripped. The
   repeat continues only while a letter survives outside every bracket, because
   an unguarded loop turns `[GRP][12 Angry Men][07][1080p][AVC][GB]` into `[GB]`
   — 4,664 sweep names. **+2.**
5. **`9196cf3`** — the year cut removed everything behind the year, `E04`
   included, so `Series Show.2016.E04.Power…` was filed as a 2016 film. The claim
   is now asked twice, and refused three ways: a claim that survives the cut, a
   span that claims no number, and the year itself. **+2.**
6. **`89e6cd6`** — `ep` joined the spelled episode words, with the per-word
   search this file requires: 0 occurrences with digits anywhere except the
   sweep's own `Ep01` form, which states no season and never reaches the arm.
   **+1.** `Part N` refused in the same pass, on twelve real film titles.
7. **`f530a07`** — no parser change: the corpus scored through `stored_parse`,
   the call the scanner actually makes. **617 against 607, +10 / -0**, and every
   one of the ten in `harness-gives-a-path`.

## The branch, and its commits

`loop/parser-board-overnight`, thirteen commits on `f527198`:

    f530a07 notes: score the corpus through stored_parse, the call the scanner makes
    8bbcd48 notes: iteration 6, one word added and one refused
    89e6cd6 core: `ep` joins the spelled episode words
    06475c9 notes: iteration 5, and the regression --diff caught while the rate rose
    9196cf3 core: a year does not outrank an episode marker behind it
    d926dca notes: iteration 4, and a control that had to be measured rather than guessed
    c071083 notes: iteration 3, a trailing range refused with the titles that caused it
    82e797a core: a run of leading groups is stripped down to the prose
    72c9a40 notes: iteration 2, and the wider rule the corpus refused
    15707ac core: a year is not a file extension
    b4c8d5b notes: iteration 1, and what each of its two zeroes means
    b62ecb0 core: the title cut and the year come from one read of the date
    8c24619 notes: the overnight loop's baseline at f527198, and a dogfood parse probe

Five touch `server/crates/core/src/filename.rs`; nothing else in `server/` is
modified. Working tree clean, nothing stashed by this session, gates green at the
tip.

**One thing a reader will hit in a fresh worktree:** `web/build/` is gitignored
and the API crate embeds it, so `cargo clippy --all-targets` and
`cargo test --workspace` exit 101 until it exists. Copied from the primary
checkout (740 KB). Not a code defect, not committed.

## What I would do next, and why

1. **Take the `harness-gives-a-path` finding to the board.** Iteration 7 measures
   that the product's own entry point scores +10 with nothing worse. The next
   move is not a parser rule — it is deciding whether `corpus_run.rs` should call
   `stored_parse`. That changes the baseline, so it is a decision, and it should
   be taken deliberately rather than by a loop at 2 a.m.

2. **`keep-a-season-marker` (6) and `drop-a-trailing-season-marker` (4) are one
   decision, not two slices** — *when is a season marker part of the series
   name?* Ten cases sit behind it and none can move until it is answered. It is
   the largest thing on the board that a single human decision unblocks.

3. **`split-one-run`'s codec block may already be structural.** The board says it
   is blocked because a bare three- or four-digit rule reads `264` as season 2
   episode 64. But `x264` has a **letter** before the digits, and this file
   already carries the predicate that refuses that — `i == 0 ||
   is_token_boundary(bytes[i - 1])`, which `episode_marker_cut`,
   `find_chapter_season_episode` and the bare `NxNN` arm all use. Worth deriving
   whether the four blocked cases are blocked on a **vocabulary** at all, before
   anyone writes one. Five of the ten still want a season N1 refuses, so the
   ceiling is 5 either way.

4. **`strip-CJK-decoration` (8) is the largest unrefused, unblocked row left**,
   and it is eight different shapes — a dual title around `/`, a `【】` group run,
   a `·` join, a `第N季` season word. It wants a slice per shape, and it wants
   someone who reads Chinese to check the expectations.

## What I could not measure, named

* **Anything above the parser.** The sweep, the corpus and the replay pair all
  hand a basename to `parse_filename`. The probe added tonight also calls
  `stored_parse`, which closes that gap for the **library**; nothing closes it for
  the sweep, whose 74,624 names have no directories at all.

* **Whether the ten `harness-gives-a-path` gains are real product behaviour.**
  They are real for `stored_parse` on those strings. Nobody scanned a filesystem
  laid out that way tonight, and the brief forbade standing anything up.

* **The remaining 6 `harness-gives-a-path` cases.** They were not looked at one
  at a time. Some may be genuine parser failures.

* **The matcher, the drain, bindings, and anything a provider answers.** No live
  provider call was made — `requests=0` is trivially true because no drain was
  run at all. **That is a weaker statement than a strict replay pair with
  `requests=0`**, and it should be read as "no network was touched", not as
  "offline behaviour was verified".

* **Whether any of tonight's five changes helps a real match.** Every one moved
  the corpus and none moved the 25,043-file library. The library is one naming
  form and these are rules for forms it does not use. **That is not evidence they
  are wrong; it is the absence of evidence either way**, and no dogfood count was
  offered for any of them.

* **CI.** It cannot run — billing stop — and nothing was pushed, so this is moot
  rather than skipped. Local gates are the whole bar.

## What this does not claim

**Not that the parser works.** It claims that on 734 applicable Sonarr/Radarr
corpus cases it now answers 607 rather than 598; that 74,624 generated names over
2,332 real titles moved not at all in either direction; that 25,043 real library
paths parse byte-identically before and after; and that `classify.py --diff`
exits 0 at every commit on this branch. **127 corpus cases still fail**, and the
report above names which ones are refused, which are blocked on a decision, and
which are the harness rather than the parser.
