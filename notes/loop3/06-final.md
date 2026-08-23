# The loop — six iterations, five kept, no reverts

**Amended 2026-08-22.** Item 3 was blocked when this was first written. The
warming run removed the block, the rule was measured and kept, and the tables
below are updated. See `08-wrong-kind-landed.md`.

Base `origin/main` at `2f6efb7`. Branch `loop/matcher-residual`, never pushed,
never merged, `main` untouched.

## Iterations

| # | what | outcome |
|---|---|---|
| 0 | re-measure every instrument at the base | — |
| 1 | ADR-0048 B: the fix flow keeps the candidate the floor declined | **kept** |
| 2 | F5: a root group's episode-title evidence is its own files | **kept** |
| 3 | `movie.specials`, and item 3 reported blocked | instrument only |
| 4 | an episode number is read whole, or the token is not an episode | **kept** |
| 5 | a date written as three number groups ends the title | **kept** |
| 6 | a numbered season directory means the file is not a film | **kept** |
| — | ADR-0049 written for `tv.scene` fragmentation | proposed |

**Five kept, none reverted.** No change was reverted, so the two-consecutive-revert
stop was never approached; the loop stopped because the remaining work is not
safe work, which is the first line of the brief.

## Per instrument, before and after

### The matcher oracle

2,410 entities throughout. 79,382 rows for iterations 0–2, **81,094** from
iteration 3 when `movie.specials` was added; both arms were re-measured on the
new population so every table joins. Warmed cache, `requests=0` on every run,
noise floor **0 rows** on every run.

| verdict | base | final | delta |
|---|---:|---:|---:|
| correct | 64,836 | **64,862** | **+26** |
| absent | 15,654 | 16,227 | +573 |
| `wrong.kind` | 573 | **0** | **−573** |
| `wrong.entity` | 11 | **5** | **−6** |
| `wrong.unknownepisode` | 8 | **0** | **−8** |
| stalled | 12 | **0** | **−12** |
| provider errors | 2 | **0** | −2 |

**Total wrong 592 → 5, and the worst class is empty.** The 5 remaining
`wrong.entity` are in the `movie.*` shapes. Every `wrong.kind` and every
`wrong.unknownepisode` is gone.

The base column is on the old cache and the final on `tmdb-cache-kind`; the
573 row is the tip-versus-rule comparison, both arms on the warmed cache, in
note 08. Nothing else differs between the two caches — the warm added only TV
searches the base never issues.

Twenty of twenty-one shapes are byte-identical between base and final. The one
that moved is `tv.mixedroot`, and every row it moved went to `correct`.

### The parser corpus

| | pass | applicable | rate |
|---|---:|---:|---:|
| base | 524 | 738 | 71.0% |
| after iteration 4 | 530 | 738 | 71.8% |
| final | **532** | 738 | **72.1%** |

**+8 cases, 0 regressions**, every gain named in a prediction written before the
run. Six more titles are corrected without their case passing — they fail on a
different field — which the rate cannot show and `corpus_diff.py` does.

### The parser sweep

74,624 names, base `2f6efb7` against final: **0 gains, 0 regressions**, recorded
at the base first as the brief asked.

The zero means two different things in two iterations, and the difference is
recorded rather than netted:

- **Iterations 1–2**: `nightjar-core` byte-identical to `origin/main`, so the
  sweep is insensitive by construction.
- **Iteration 4**: the crate changed, and the sweep is blind by *population* —
  `gen_names.py` emits one-digit episode numbers only, checked rather than
  assumed.
- **Iteration 5**: the crate changed and the sweep **does** hold the shape — 332
  names carry a numeric run around a four-digit year, including `1883.2019`,
  `1899.2019`, `300.2019` and five `9-1-1`s. None moved. That zero is evidence.

### The dogfood strict pair

**The capture and the database are two populations, and this note has quoted
them as one.** The pair replays `capture-media-mac.jsonl`, which holds **25,004**
paths. The dogfood database holds **25,043**. They are not the same 25,000:

    paths in the database, absent from the capture     610
    paths in the capture, absent from the database     571

So a row reading *25,004 files* and a row reading *25,043 files* — both appear
above — describe different libraries, and a claim proved on one is not proved on
the other. **All four Futurama films are in the 610.** The strict pair cannot
see them; "3 bindings differ, all gains" was true of the capture and false of
the library, and it took a `stored_kind` sweep over the database's own paths to
find that out. Wherever this pair's result is reported, say which population it
is a result about.

25,004 files, run at iterations 1, 2, 4 and 5. Every run: binaries `sha256`'d
distinct, separate target directories, `NIGHTJAR_REPARSE=1` and
`NIGHTJAR_TMDB_CACHE_STRICT=1` on both arms, `errors=0` and `requests=0` on both,
cache 8,185 entries before and after.

    groups 3220  ready 24953  unmatched 51   — identical on every arm, every time

**Every one of those zeros is explained rather than reported.** The real library
has no root-level episode file (iteration 2), no four-digit episode number
(iteration 4) and no date-numbered file (iteration 5). The pair earns its place
by proving the two arms are two things and that both ran offline — not by the
count.

### Tests

**746 → 758. Twelve new**, split `core +6, db +2, scanner +1, metadata +3`.

The figures published here first — *743 → 749 pass, nine new* — were wrong twice
over, and did not even reconcile with each other: `743 → 749` is a delta of six,
against a claim of nine, against an actual twelve. Counted two ways that agree:
`#[test]` and `#[tokio::test]` attributes under `server/` give 746 at
`origin/main` and 758 at the tip, and `cargo test --workspace -- --list` at the
tip lists 758. A per-iteration count that is never added up is how three tests
went missing from the total.

`cargo fmt --check` and `cargo clippy --all-targets -D warnings` green at every
commit.

**`hls::tests::mapped_real_library_end_moov_mp4_copy_keeps_aac` was blamed on the
wrong thing twice.** This note first recorded it as *one failure throughout*,
failing when the `transcode` package runs its 158 tests together and passing
alone. Both halves are wrong:

- `cargo test -p nightjar-transcode` runs all 158 together and **passes**.
- Under `cargo test --workspace` it sometimes fails and sometimes does not. Two
  consecutive runs on the same tree: one failed, one green at 756 passed, 3
  ignored, **0 failed**.
- On a full disk it fails for a third reason again — `Error submitting a packet
  to the muxer: No space left on device` — which is not the same failure at all,
  and reads as a regression to anyone who does not check the message.

It is **flaky under workspace-level concurrency**, and its two assertions fail
with different messages (`hls.rs:7833`, a seek landing at 84.333s against
58.975s; `hls.rs:7985`, the muxer running out of disk). Nothing on this branch
can reach it: `git diff origin/main..HEAD -- server/crates/transcode` is empty.
**Read the panic message before calling it a regression** — a green suite and
either failure are all reachable from one unchanged tree.

## Per-iteration one-liners

- **0** — the handed-over baseline did not reconcile: it summed to 79,950 against
  a stated population of 79,382, and three of its cells were wrong.
- **1** — `search_candidates` marks the id the shipped scorer chose and the 0.90
  floor declined; movies only, and nothing binds on it. Moves no oracle row by
  design, which ADR-0048 says in advance.
- **2** — `folder_titles_from_db` built `LIKE '%'` for a group with no show
  folder, so a root group's episode-title evidence was the whole library's; one
  agreement with a neighbour's title lifted a candidate to the auto-match floor.
  26 rows moved, all to `correct`.
- **3** — built `movie.specials`, 1,712 rows pairing exactly with `movie.noyear`,
  so the `Specials/`-shaped failure has a guard bigger than five dogfood files.
  Refused the second prerequisite on Rule 4.7 and left the 573.
- **4** — the episode digit run was capped at three and truncated, so `S22E5363`
  reported episode 536 — a wrong claim where the name carried a right one.
- **5** — a date is three number groups with a year at one end; and the guard
  written for `9-1-1` was measured out of existence rather than kept on faith.

## Two instrument defects found, both of a kind this project keeps finding

**The replay harness was not deriving the stored title as production does.** The
product's `stored_title` substitutes the show folder's name for an empty parse
and both scanner indexing paths call it; its own doc comment says the harness
needs the same answer and names the consequence — "5,644 generated rows scored
`absent` for a reason that was the harness rather than the product". The
inherited harness patch still used `parse_filename` alone. Corrected;
`tv.numbered` goes 0.0% → 100.0% and no other shape moves. **The loop base
re-reads 63,830 correct instead of 58,186 over the same rows**, and iteration 2
was re-run on both arms against the corrected instrument — same 26 rows, same
direction.

**`gen_library.py` reads `TMDB_CACHE` and silently drops a shape without it.**
Regenerating for `movie.specials` without the variable in the environment
produced 20 shapes instead of 21: `tv.shortfolder` went from 17 entities to 0,
because the entity filter verifies truncated names against the *cache*. Caught
by checksumming all 39 pre-existing `capture.jsonl` files before and after — they
are byte-identical in the run that counts.

## What I would do next, and why

1. ~~**Warm 4,691 TV searches, then take item 3.**~~ **Done** — 1,758 requests,
   not 4,691, and the rule is kept (note 08). The next thing here is making
   those 573 *correct* rather than merely `absent`, which needs the folder's
   title and season and is item 5's signature change.

   The superseded plan, for the record: That is the only thing standing
   between the 573 `wrong.kind` and a judgeable attempt. **The command, the
   candidate patch and the measured bill are in `07-wrong-kind-warming.md`** —
   prepared and drained strict after this note was first written, so the rule is
   one human-run step from being judgeable rather than an open question. Under
   the candidate the 573 `wrong.kind` become stalls, `tv.sonarr` is untouched,
   and `movie.specials` stays byte-identical to `movie.noyear` — the guard the
   previous attempt did not have, holding.

2. **Build the shape ADR-0049 needs before accepting it.** `tv.scene` forms one
   group per file — 5,644 for 5,644, against every other TV shape's 698 — and
   costs 1,295 unmatched. The oracle can price the status quo and cannot price
   any alternative, because no shape puts two *different* shows in scene folders
   that a merge rule would join. That number is what decides between the four
   options, and without it the record cannot be accepted honestly.

3. **Fix the harness's `duration_ms`, then weigh runtime for `movie.noyear`.**
   705 rows sit behind it and ADR-0048 says plainly that runtime is likely the
   strongest signal and is unmeasurable while the generator takes each file's
   duration from the correct entity's own runtime. Jitter it, or draw from a real
   distribution, and the 88.9% ceiling on provider rank stops being the best
   available evidence.

4. **The parser's remaining clusters, in this order:** the glued date forms
   (`140722`, `20201013`) at 3 cases, which need the boundary against
   `cut_at_absolute_episode`'s absolute number; the ` - ` range separator at 2;
   and then stop, because the two big ones — `Series.103` = S1E3 at 17 cases, and
   season/episode from the parent folder at 11 — are respectively too dangerous
   for any instrument here to clear and blocked on the signature change item 5
   names.

5. **Free the disk.** The volume sat at 100% for part of this loop and it
   produced a fake regression: the `transcode` suite went from 1 failure to 8 at
   head and 20+ at base, a difference that looked like a result and was the
   machine. A measurement drain needs 4.2 GB. Recorded in the oracle's own
   README, under **Amendments, 2026-08-21 — three ways this instrument lied**,
   with the `stored_title` and `TMDB_CACHE` defects, so the next person reads it
   before running rather than after.

## What could not be measured, named

- **Whether any of this helps a user.** The oracle has no fix flow — no manual
  match, no rescan, no second pass — so the cost ADR-0048 option B changes is
  the one thing this instrument does not model.
- **`movie.noyear`'s 705.** Untouched by design. A below-floor suggestion still
  scores `absent`, and raising the score to make it measurable is the thing
  ADR-0048 refuses.
- **`tv.episodetitle`'s 573.** 4,691 cached TV searches away from judgeable.
- **`tv.handmade`'s 5,644.** Untouched: `01 - Closure.mkv` needs both the folder
  title and the episode number out of an `NN - ` prefix.
- **Runtime, NFOs, non-English names, deep seasons, second-pass matching, manual
  match, rescan, movie versions and editions** — all still outside the population,
  exactly as the previous loop left them.
- **Frequency, anywhere.** Every count here is "this many generated rows have the
  shape that breaks". The one real library is the library whose narrowness caused
  all this, so its zeros mean "not this library", never "not anywhere".

## What is not claimed

**Neither the matcher nor the parser is shown to work.**

The matcher binds 64,862 of 81,094 generated rows to the entity their name came
from, carries 5 wrong bindings where the base carried 11, and no longer carries
any wrong binding at all outside the two shapes named above. That is what the
oracle says about a generated population of English-named, mostly-first-season,
NFO-free libraries drained once from empty.

The parser passes 532 of 738 applicable corpus cases against a measured ceiling
of 91.2% for parse-only work, and is byte-for-byte unchanged on 74,624 swept
names. Two of its four fields — absolute numbering and date-based numbering —
have no scorable expectation in the corpus at all.

Everything outside those populations is unmeasured.
