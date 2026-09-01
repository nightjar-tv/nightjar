# Iteration 7 — a measurement, not a change: what the product's entry point scores

Base `f527198`, at the tip `89e6cd6`. **No parser change.** Two scripts added.

## Why

Three of `classify.py`'s classes are not parser mechanisms:

| class | cases at `89e6cd6` |
|---|---:|
| `harness-gives-a-path` | 16 |
| `slash-inside-the-name` | 7 |
| `pick-a-title-before-a-slash` | 2 |

**25 of 127 failures — one fifth of the residual.** `corpus_run.rs` takes the
basename and calls `nightjar_core::parse_filename`, so
`S:\TV Drop\Series - 10x11 - Title [SDTV]\1011 - Title.avi` arrives as
`1011 - Title.avi` with the answer left behind in a directory.

**The product does not make that call.** `nightjar_scanner::stored_parse(path,
library_root)` reads the path: the immediate parent for season, episode and
year, the show folder for the title. `scanner/src/lib.rs` states the precedence
as a table and calls it "one layer decides".

So the residual has never been split into what the parser owes and what the
harness invented.

## What was added, and then removed

`notes/loop-overnight/scripts/corpus_run_stored.rs` and
`corpus_results_stored.sh` — **`corpus_run.rs`'s scoring, unchanged, against
`stored_parse`**. The same cases, the same checks, the same soft-key title
comparison. Only the call differed.

**`corpus_run.rs` was untouched, and every number reported that night came from
it.** This was a second measurement placed beside it, not a replacement. A
harness that scores better is not a parser that parses better.

> ### Both files are deleted — 2026-09-02
>
> The measurement below was taken to the board, and the board switched:
> `corpus_run.rs` now calls `stored_parse` and `corpus_run_merge.rs` is deleted
> beside it (`nightjar-spikes` `08fa226`). **These two files became a third copy
> of a harness that already exists**, sitting on an unmerged branch until
> someone ran it and got a different number.
>
> **That is exactly how the `590/734` and `600/734` pair survived** — both
> binaries were kept, one of them called a projection, and the two were quoted
> interchangeably for weeks. *A second number must never be produced without
> deleting the first*, and "it is only on a branch" is not an exemption; it is
> the hiding place.
>
> The measurement stands and is reproduced by the shipped harness. Everything
> below is kept as the record of how the decision was reached, and the numbers
> in it are read at the loop tip `f530a07` rather than at `main`.

## The result

    parse_filename (basename)   pass 607  fail 127  n/a 110   82.7%
    stored_parse   (path)       pass 617  fail 117  n/a 110   84.1%

    classify.py --diff, one entry point against the other:
      verdict: +10 / -0
      fields gained inside failing cases: none
      fields fixed inside failing cases:  {'season': 5, 'episodes': 2, 'episode': 2}

**+10 and nothing worse**, and nine more fields fixed inside cases that still
fail. **Every one of the ten is in `harness-gives-a-path`** — the exact class the
classifier says is a call site rather than a parser rule:

    S:\TV Drop\Series - 10x11 - Title [SDTV]\1011 - Title.avi
    /TV Drop/Series Title - 10x12 - 24 Hours of Development [SDTV]/1012 - …
    C:\Test\Unsorted\Series.Title.S01E01.720p.HDTV\tbbt101.avi
    E:\Downloads\tv\Series.Title.S01E01.720p.HDTV\ajifajjjeaeaeqwer_eppj.avi
    …and six more

Under the product's entry point that row falls **16 → 6**.

## The residual the parser actually owes

Re-derived at `89e6cd6` through `stored_parse`, 117 failures, reconciling:

| class | cases | needs two fixes | note |
|---|---:|---:|---|
| drop-a-trailing-number | 22 | 2 | **refused** for the bare form |
| expand-a-range | 18 | 9 | 9 more want a season N1 refuses |
| drop-a-trailing-word | 14 | 5 | `german`, `v2`, `Part N` all refused |
| slash-inside-the-name | 9 | 3 | a `/` cannot be in a filename |
| split-one-run | 9 | 0 | **blocked** on a codec vocabulary |
| strip-CJK-decoration | 8 | 1 | D5 |
| keep-a-year | 6 | 4 | K2 |
| keep-a-season-marker | 6 | 0 | **blocked** on a decision, with D-a-t-s-m |
| harness-gives-a-path | 6 | 0 | what the path rule still does not reach |
| a-bare-marker-wants-a-season | 5 | 0 | **refused by decision** |
| drop-a-trailing-season-marker | 4 | 3 | blocked with K1 |
| read-a-year | 3 | 0 | three different mechanisms |
| pick-a-title-before-a-slash | 2 | 0 | a `/` cannot be in a filename |
| no-title-in-the-name | 2 | 2 | needs a four-digit bare season |
| keep-a-subtitle | 1 | 0 | K3 |
| strip-decoration-both-ends | 1 | 0 | — |
| drop-a-spelled-marker | 1 | 1 | N5 |

## What this does not say

It does **not** say the parser is better than the corpus reports. It says the
corpus has been measuring a call the product does not make, and that 10 of its
failures are that and nothing else.

It also does not say the remaining 6 `harness-gives-a-path` cases are the
harness's fault. They may be; nothing here checks them one at a time.

And **no number in any other note tonight was taken from this instrument.** Every
kept-or-reverted decision was made on `corpus_run.rs`.
