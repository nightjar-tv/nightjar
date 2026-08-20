# Iteration 10 — ADR-0047 half one: an exact fold beats a title extension

## The change

`prefer_exact_over_extension` in `match_score.rs`. A TV candidate admitted only
because its name *extends* the query is dropped when an exact fold survives.

**It runs after the empty-shell exclusion, and the first cut ran it before.** That
broke the shipped test `short_query_matches_long_official_title_over_empty_shell`
— query `Charlie`, an exact-fold candidate with zero episodes, and a real
`Charlie: Extended Official Title` the shell shadowed into nothing. The shell is
not a candidate at all under ADR-0026 amended, so letting it suppress a viable
extension discards the answer rather than expressing a preference.

**That is the "what does it return when it narrows to nothing" trap**, and the
shipped test caught it. The test was right; my implementation was wrong; nothing
was weakened to make it pass.

The set is deliberately *not* narrowed at `tmdb/mod.rs`'s filter site — the shapes
there are zipped with the hit set by index, and the precedence needs the extensions
still present to choose between them.

One test added, `an_exact_fold_beats_a_title_extension`, on the real case:
`Gilmore Girls` (153 eps) against `Gilmore Girls: A Year in the Life` (4 eps) with
a 10-file library. 259 metadata tests, 742 workspace, 0 failed.

## Measured — fully warm, four instruments

63 further live requests to close the misses the change itself created (narrowing
the candidate set changes which shapes get fetched). Then strict:
`provider errors 0`, `http requests 0`, `stalled 0 of 73,738`, noise floor 0.

| shape | before | after |
|---|---:|---:|
| **tv.noyear** | 63.9% | **81.2%** |
| **tv.root** | 63.9% | **81.2%** |
| **tv.scene** | 61.6% | **78.3%** |
| tv.flat, tv.numbered, tv.sonarr.plain, tv.partial, tv.single | 99.9% | **100.0%** |
| **tv.shortfolder** | 51.3% | **51.3%** |
| movie.*, tv.handmade, tv.episodetitle | — | unchanged |

| verdict | before | after | delta |
|---|---:|---:|---:|
| correct | 55,435 | 58,336 | **+2,901** |
| wrong.entity | 100 | 64 | −36 |
| wrong.unknownepisode | 1,738 | 1,037 | **−701** |
| wrong.kind | 573 | 573 | 0 |
| absent | 15,892 | 13,728 | −2,164 |
| **correct%** | **75.2%** | **79.1%** | **+3.93 pt** |

**Total wrong 2,411 → 1,674, down 737.**

**`tv.shortfolder` held at exactly 58 correct.** That shape exists to catch this
change breaking the legitimate extension, and it did not move by a row — because
in every surviving row there is no exact fold competing, which is what the
provider-level ambiguity filter guarantees. The guard earned its place by staying
flat.

- **Sweep** 74,624 names, 0 gains, 0 regressions — `nightjar-core` byte-identical.
- **Corpus** 71.0% (524/738), unchanged.
- **Dogfood pair** control `ac392035`, treatment `ac865a39`, distinct binaries,
  identical on every counter: `groups=3220 ready=24953 unmatched=51 errors=0
  requests=0`, cache 8,185 before and after.

## What it cost, and why it belongs to half two

**42 rows that were correct are not: 24 → absent, 18 → wrong.unknownepisode.**
Three entities only — The Firm (20), Red Dwarf (12), Criminal Minds (10).

The cause is not what I assumed. Each has **two genuine exact folds**:

    The Firm         39255 (2012, correct)   236692 (2023)
    Red Dwarf          326 (1988, correct)   225119 (2010)
    Criminal Minds    4057 (2005, correct)    71795 (2017)

Before this change the candidate set also held the extensions, so **`try_pin`
found two or more matches for the count predicate and returned `None`** — no pin —
and the code fell through to a later, better branch that got it right.

Narrowing the set made the loose predicate **decisive where it had been safely
ambiguous.** `try_pin`'s "two or more means no pin" was acting as an accidental
safety valve, and removing candidates disabled it.

So these 42 are caused by the **count discriminators** — the exact thing half two
replaces. **Prediction for half two, written now: it should recover most or all of
the 42**, because per-season coverage can separate a 1988 series from a 2010
revival where a total-episode count cannot. If it does not, my model of half two
is wrong.

## Verdict — KEEP

Oracle up 3.93 points with wrong down 737 and the regression guard flat; the other
three instruments clean or provably insensitive; tests green with one added; noise
floor 0. The 42 losses are named, attributed, and have a stated test in the next
iteration.
