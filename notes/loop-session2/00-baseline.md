# Session 2 — the baseline, read at `2723dcf`

**Base revision: `2723dcf`** — `core: five measured parser rules, and the two
that were refused (#199)`, the tip of `main`. Branch
`loop/parser-board-session2`, worktree `~/nightjar-wt-loop6`, target dir
`~/nightjar-wt-loop6-target`. Nothing is pushed. No PR. No merge.

A second worktree, `~/nightjar-wt-loop6-base`, holds `2723dcf` detached. It is
what every baseline arm is built from, so a baseline never comes from the same
tree as the change.

**A fresh worktree will not build.** `web/build/` is gitignored and the API
crate embeds it, so `cargo clippy` and `cargo test` exit 101 with
`#[derive(RustEmbed)] folder '.../web/build' does not exist`. Copied 740 KB from
the primary checkout into both worktrees. Not a code defect, not committed, and
the next reader hits it again.

## Gates at the base

Run from `server/`, not the repo root — there is no workspace `Cargo.toml` above
it.

| gate | result |
|---|---|
| `cargo fmt --all --check` | 0 |
| `cargo clippy --all-targets -- -D warnings` | 0 |
| `cargo test --workspace` | 0 — **853 passed, 0 failed** |
| `#[test]` + `#[tokio::test]` attributes, full paths under `server/` | **856** |

## The four instruments, at the base

**The corpus**, through `nightjar_scanner::stored_parse` —
`notes/loop3/scripts/corpus_results.sh` over
`~/nightjar-spikes/parser-corpus-2026-08-13`:

    pass 617  fail 117  not_applicable 110  total 844
    applicable pass rate: 84.1%

That is the figure the board carried, and it is re-read here rather than taken.

**`classify.py` at the tip.** It reconciles: 117 failures, 117 classified, 17
classes.

| class | cases | needs two fixes | note |
|---|---:|---:|---|
| drop-a-trailing-number | 22 | 2 | **refused** for the bare form |
| expand-a-range | 18 | 9 | |
| drop-a-trailing-word | 14 | 5 | `german`, `v2`, `Part N` refused |
| slash-inside-the-name | 9 | 3 | a `/` cannot be in a filename |
| split-one-run | 9 | 0 | 4 **blocked** on a codec vocabulary |
| strip-CJK-decoration | 8 | 1 | |
| keep-a-year | 6 | 4 | |
| keep-a-season-marker | 6 | 0 | **blocked** on a decision |
| a-path-form-the-product-refuses | 6 | 0 | outside the input contract |
| a-bare-marker-wants-a-season | 5 | 0 | **refused by decision** |
| drop-a-trailing-season-marker | 4 | 3 | blocked with K1 |
| read-a-year | 3 | 0 | |
| pick-a-title-before-a-slash | 2 | 0 | |
| no-title-in-the-name | 2 | 2 | |
| keep-a-subtitle / strip-decoration-both-ends / drop-a-spelled-marker | 3 | 1 | |
| **total** | **117** | | reconciles |

**The parser sweep** — the null control at the base, `ALLOW_IDENTICAL=1`,
`2723dcf` against itself:

    base tree 7991257f…  head tree 7991257f…   (same tree, deliberately)
    generated 74624 names
    names scored 74624   HEAD right BASE wrong 0   BASE right HEAD wrong 0
    gains by field: title 0  season 0  episode 0  year 0

**The dogfood parse probe** — the database's **25,043** paths, parsed twice
each (`parse_filename` on the basename, `stored_parse` on the path):
`rows 50086  paths 25043`. Kept at
`~/nightjar-wt-loop6-scratch/dogfood-base.tsv`.

## Where the failures actually are

The 117 were re-cut by **what the title diff looks like**, which the class names
do not say: **53 title failures are the got string with a suffix on the end**,
3 are a prefix, and 34 are neither. The suffix block is almost entirely the
refused trailing-number and trailing-word rules. That is why this board's
remaining mechanisms are small — the large ones have been priced and declined,
and what is left is vocabulary and marker shapes worth one to three cases each.

Last night's five iterations gained **+9 in total**. A two-case iteration is the
size of the work here, not a shortfall.

## What is left on the board, after the refusals

Re-derived at `2723dcf`, not carried:

* `drop-a-trailing-number` (22) — refused, and **`#NN` is refused too**; see
  iteration 1's note for the title that refused it.
* `expand-a-range` (18) — 9 want a season the name does not state, which N1
  refuses. Of the 9 that remain, the shapes are a `_` range separator (2), the
  cap at `MAX_EPISODE_RANGE = 8` (1), a glued `E1E3` (1), a spelled marker with
  a range (1), and `Cap.111_120` (1, also over the cap).
* `keep-a-season-marker` (6) + `drop-a-trailing-season-marker` (4) — one
  product decision, not a slice.
* `strip-CJK-decoration` (8) — **not one mechanism**. Two of the eight want the
  CJK run kept as the title (`關於我轉生後成爲史萊姆那件事`, `ABC123 17研究所！`),
  so "CJK is decoration" is refuted inside its own row.
