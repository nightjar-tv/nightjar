# Overnight loop — the baseline, read at `f527198`

> **Read [`09-rebaselined.md`](09-rebaselined.md) first.** Every corpus figure in
> this note is through `parse_filename`. The harness calls `stored_parse` as of
> 2026-09-02, and the base is **`608/734` (82.8%)**, not `598/734`.

**Base revision: `f527198`** — `docs: ADR-0020 §12's cache budget is a constant,
not a setting (#197)`, the tip of `origin/main` on 2026-09-01. The local `main`
ref in the primary checkout is stale at `e3208cc`; every number below is read at
`f527198`, in a worktree of its own at `~/nightjar-wt-loop5`.

Branch: `loop/parser-board-overnight`. Nothing is pushed. No PR. No merge.

## The tree would not build, and it was the environment

`cargo clippy --all-targets` and `cargo test --workspace` both exited 101 in a
fresh worktree:

    error: #[derive(RustEmbed)] folder '.../web/build' does not exist

`web/build/` is gitignored — it is the built web client the API crate embeds. A
new worktree has none. Copied from the primary checkout (740 KB) rather than
built, because nothing tonight touches the web client and the bytes only have to
exist for the embed macro. **This is not a code defect and it is not committed.**
A reader picking this branch up in a fresh worktree will hit it again.

With that in place, at `f527198`:

| gate | result |
|---|---|
| `cargo fmt --all --check` | 0 |
| `cargo clippy --all-targets -- -D warnings` | 0 |
| `cargo test --workspace` | 0 |
| `#[test]` + `#[tokio::test]` attributes, full paths | **844** (`core/src/filename.rs`: 108) |

## The four instruments, at the base

**The corpus** — `notes/loop3/scripts/corpus_results.sh` over
`~/nightjar-spikes/parser-corpus-2026-08-13`:

    pass 598  fail 136  not_applicable 110  total 844
    applicable pass rate: 81.5%

**`classify.py` at the tip**, not the board's carried counts. It reconciles:
136 failures, 136 classified, 18 classes.

| class | cases | needs two fixes | row |
|---|---:|---:|---|
| drop-a-trailing-number | 22 | 2 | D1 — **refused** for the bare form |
| expand-a-range | 20 | 11 | N3 |
| harness-gives-a-path | 16 | 0 | H2, the harness half |
| drop-a-trailing-word | 14 | 5 | D3 |
| split-one-run | 10 | 0 | N2 — **blocked** on a codec vocabulary (4 of 10) |
| keep-a-year | 8 | 5 | K2 |
| strip-CJK-decoration | 8 | 1 | D5 |
| read-a-year | 7 | 0 | Y1 |
| slash-inside-the-name | 7 | 6 | H2, the parser half |
| keep-a-season-marker | 6 | 0 | K1 — **blocked** on a decision |
| a-bare-marker-wants-a-season | 5 | 0 | N1 — **refused by decision** |
| drop-a-trailing-season-marker | 4 | 3 | — (blocked with K1) |
| drop-a-leading-group | 2 | 0 | D4 |
| pick-a-title-before-a-slash | 2 | 0 | H2, the parser half |
| no-title-in-the-name | 2 | 2 | — |
| keep-a-subtitle | 1 | 0 | K3 |
| strip-decoration-both-ends | 1 | 0 | — |
| drop-a-spelled-marker | 1 | 1 | N5 |

**The parser sweep** — a deliberate null control at the base, `ALLOW_IDENTICAL=1`,
`f527198` against itself:

    base tree 5b0014bc…  head tree 5b0014bc…
    generated 74624 names
    names scored 74624   HEAD right BASE wrong 0   BASE right HEAD wrong 0
    gains by field: title 0  season 0  episode 0  year 0

The harness is deterministic before either arm of a real comparison is believed.

**The dogfood parse probe** — new tonight, and it reads the **database's 25,043**
paths, not the capture's 25,004. `notes/loop-overnight/scripts/dogfood_probe.sh`,
against `~/nightjar-wt-loop-scratch/dogfood-readonly.db` opened `mode=ro`; the
paths are extracted once to a file and the database is not reopened. Nothing is
written to it.

**It parses each path twice**, because the three instruments this project already
has share one blindness: the corpus harness, the sweep and the replay pair all
hand a **basename** to `nightjar_core::parse_filename`, and none of them can see
a change above the parser. The probe also calls `nightjar_scanner::stored_parse`
— the function the product actually calls, which reads the path.

At the base, 3 of 25,043 paths parse differently through the two entry points:

    Top Gear/Season 16/Top Gear - 16x00 -  The three wise men christmas special - 720p.mkv
    Top Gear/Season 22/Top Gear - 22x00 - Special Patagonia Part One.mkv
    Top Gear/Season 22/Top Gear - 22x00 - Special Patagonia Part Two.mkv

Each is `Movie` from the basename and `Episode` season *N* from the path. Those
are exactly the three `NNx00` specials `notes/loop3/09-file-kind-and-decline.md`
names. **The probe reconciles with a fact recorded before it existed**, which is
the only check available that it reads the layer it claims to.

## What the board's largest rows cost, re-derived

The blocked column is a claim about the cases in the row, so it is re-derived
here. Two derivations, both over tonight's 136:

* `classify.py --blocked`: `split-one-run` — 4 of 10 inputs carry a codec token
  with digits. `keep-a-season-marker` (6) and `drop-a-trailing-season-marker` (4)
  want opposite things about the same token and are one decision, not two slices.
* **The N1 refusal, counted per row.** *"It must not synthesise a season"* is a
  taken decision. Cases wanting `season: Some(1)` where the name states no season
  cannot reach a verdict pass however well their own mechanism works:

| class | cases | want season 1 out of nothing | can reach a pass |
|---|---:|---:|---:|
| drop-a-trailing-number | 22 | 1 | 21 |
| expand-a-range | 20 | **9** | **11** |
| harness-gives-a-path | 16 | 6 | 10 |
| drop-a-trailing-word | 14 | 4 | 10 |
| split-one-run | 10 | 5 | 5 |
| read-a-year | 7 | 0 | **7** |

`expand-a-range` reads 20 and is worth at most 11, and its 11 are eight different
sub-mechanisms — `_` as a separator, `E1E3`, a span wider than
`MAX_EPISODE_RANGE = 8`, a spelled marker, `Cap.NNN_NNN`, a four-digit `NNNNx`,
a Dutch `afl.2-3-4`, a CJK `第01-15集`. It is a row, not a slice.

`harness-gives-a-path` is 16 cases whose fix is in `corpus_run.rs` — **the
instrument, not the product**. Changing it tonight would move the baseline
without moving the parser, so it is left alone and named here instead.
