# Re-baselined against `608/734` — 2026-09-02

**Every number in notes 00 to 08 was measured through `nightjar_core::parse_filename`.**
The corpus harness no longer calls it. `corpus_run.rs` calls
`nightjar_scanner::stored_parse` as of `nightjar-spikes` `08fa226`, merged into
the product side as **#198 (`24b0cea`)** for the wrapper's scanner dependency.

**So the branch cannot be read against `598/734` any more**, and this note is the
correspondence. Nothing in the branch's code was re-measured for correctness —
the five commits are unchanged and the gates were green at each.

## The correspondence

Both columns are the same corpus, the same scoring and the same five commits.
Only the entry point differs.

| commit | what it did | `parse_filename` | `stored_parse` |
|---|---|---:|---:|
| `f527198` | the base | 598 / 734 — 81.5% | **608 / 734 — 82.8%** |
| `b62ecb0` | the date's year beats a title that opens with one | 601 | **611** |
| `15707ac` | a year is not a file extension | 602 | **612** |
| `82e797a` | a run of leading groups is stripped to the prose | 604 | **614** |
| `9196cf3` | a year does not outrank an episode marker behind it | 606 | **616** |
| `89e6cd6` | `ep` joins the spelled episode words | 607 — 82.7% | **617 / 734 — 84.1%** |

**The offset is +10 at every point, and that is the whole of it.**

## Every per-iteration delta is identical

Re-run under the new entry point, `--diff` between consecutive commits:

| step | verdict | fields gained | fields fixed | exit |
|---|---|---|---|---|
| `f527198` → `b62ecb0` | `+3 / −0` | none | none | 0 |
| `b62ecb0` → `15707ac` | `+1 / −0` | none | `{'year': 1}` | 0 |
| `15707ac` → `82e797a` | `+2 / −0` | none | none | 0 |
| `82e797a` → `9196cf3` | `+2 / −0` | none | none | 0 |
| `9196cf3` → `89e6cd6` | `+1 / −0` | none | none | 0 |
| end to end | `+9 / −0` | none | `{'year': 1}` | 0 |

**Every line is what note 08 already records.** Same verdicts, same field
movements, same `{'year': 1}` inside a still-failing case.

**The five parser changes and the entry point are orthogonal**, and that is a
measured claim rather than an assumed one: the ten cases the entry point closes
are all season-and-episode reads out of a parent directory, and the five commits
all touch title and marker rules in `nightjar-core`. Nothing overlaps.

## The mechanism table, re-derived

Both columns through `stored_parse`. Derivations: `nightjar-spikes`
`out/classes-f527198.txt` and `out/classes-89e6cd6.txt`.

| class | `f527198` | `89e6cd6` | |
|---|---:|---:|---|
| drop-a-trailing-number | 22 | 22 | refused for the bare form |
| expand-a-range | 18 | 18 | |
| drop-a-trailing-word | 14 | 14 | `german`, `v2`, `Part N` all refused |
| split-one-run | 10 | 9 | **−1** |
| slash-inside-the-name | 9 | 9 | a `/` cannot be in a filename |
| strip-CJK-decoration | 8 | 8 | |
| keep-a-year | 8 | 6 | **−2** |
| keep-a-season-marker | 6 | 6 | blocked on a decision |
| a-path-form-the-product-refuses | 6 | 6 | renamed from `harness-gives-a-path` |
| read-a-year | 7 | 3 | **−4** |
| a-bare-marker-wants-a-season | 5 | 5 | refused by decision |
| drop-a-trailing-season-marker | 4 | 4 | blocked with K1 |
| drop-a-leading-group | 2 | 0 | **−2** |
| pick-a-title-before-a-slash | 2 | 2 | |
| no-title-in-the-name | 2 | 2 | |
| keep-a-subtitle / strip-decoration-both-ends / drop-a-spelled-marker | 3 | 3 | |
| **total** | **126** | **117** | reconciles at both ends |

**−1, −2, −4, −2 is the same nine, in the same rows**, as note 08's table records
under the old entry point.

## What did not need re-measuring, and why

**The sweep.** It stayed on basenames deliberately — 0 of its 74,624 generated
names carries a separator, and `gen_names.py` now says so in its header. It
reported `0 / 0` at every commit, and the entry point cannot change that: it does
not run it.

**The dogfood probe.** It already called **both** `parse_filename` and
`stored_parse` on all 25,043 database paths — that was the point of it — and
reported `0 rows of 50,086 changed` end to end. Unaffected.

**So every zero's reason in notes 01 to 06 stands as written.** Three were
genuinely sensitive and zero; the rest narrow by population or insensitive by
construction. None of those readings was about the corpus entry point.

## How #198 was merged, recorded because the green arrived afterwards

**It was merged on `mergeStateStatus: UNSTABLE`, with `web` still in flight.**
`openapi` had passed in 14s; `web` (27s) and `server` (4m16s) went green after the
merge, and `gate1` after that. **They were not waited for.**

The change was two files under `notes/`, so nothing `server` or `web` covers could
have broken — but that is a reason it was low risk, not a reason it was checked.
**The merge records on this project are careful about this**, and a later reader
seeing four green checks against `24b0cea` would otherwise conclude they were the
gate. They were not; the authorization was.

## The one thing this closes

Note 08's *what I would do next*, item 1, was **take the entry-point finding to
the board**. That is done: `nightjar-meta` `adb8ae3`, and #198 here. The board's
rule *"quote `590/734`, never quote `600/734` as the rate"* rested on
`parse_with_parent` being *"a function nothing in the product calls yet"*, and
`2af44de` (#172) had wired it four days before the rule was written.

**Items 2 to 4 stand unchanged** — the season-marker decision (10 cases), whether
`split-one-run`'s codec block is structural rather than a vocabulary, and
`strip-CJK-decoration`'s eight shapes.

## What is still not measured

The same list as note 08, minus nothing. In particular: **no live provider call
was made and no replay pair was run**, so `requests=0` remains trivially true
rather than verified offline, and **none of the five changes moved the 25,043-file
library** — which is the absence of evidence, not evidence of absence.
