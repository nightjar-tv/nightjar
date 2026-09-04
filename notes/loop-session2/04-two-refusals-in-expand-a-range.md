# Iteration 4 — two refusals, and the names that refused them

**Base: iteration 3 (`d30198e`). Reverted. Corpus unchanged at `620 / 734`.**

`expand-a-range` is the largest row on the board that is neither blocked nor
refused: **16 cases at the tip**, 7 of which need two fixes. Two of its
sub-mechanisms were taken to measurement and both came back refused. They are
recorded here with the names that refused them, so the row's blocked column can
be re-derived rather than re-argued.

## Refused: `_` as a range separator

**The gain.** Two cases, both the same Sonarr fixture:

    Series's Sonarr - 8x01_02 - Free Falling      [1] wants [1, 2]
    8x01_02 - Free Falling                        [1] wants [1, 2]

**The population.** A marker, digits, one underscore, a whole digit run:

| population | same width | different width |
|---|---:|---:|
| corpus inputs (844) | 3 | 1 |
| dogfood basenames (25,043) | **0** | **0** |
| sweep names (74,624) | **0** | **0** |

**The name that refused it is in the corpus, and it passes today:**

    The_Series_US_s06e19_04.28.2014_hdtv.x264.Poke.mp4

`s06e19_04` — the `04` is the month of the date `04.28.2014`, and it is the
same width as the episode. This one survives by accident: `04` is below `19`,
so the range test `next > end` refuses it. **One digit's difference and it does
not.** `s06e02_04.28.2014` is the same real Sonarr spelling with a smaller
episode number, and an underscore range would read it as episodes 2 through 4.

An equal-width condition was considered and does not help — this name is equal
width. What protects it is arithmetic, not a rule, and the file's own principle
says which way to fail: *an absent claim costs a range, a wrong one costs the
episode a file binds to.*

**So `_` stays a token separator and not a range separator**, and the two
fixture cases stay red.

## Refused: a glued repetition, `E1E3`

**The gain.** One case:

    Series Title.S6.E1E3.Episode Name.1080p.WEB-DL     [1] wants [1, 2, 3]

**The population** is two corpus inputs and nothing else — 0 of the 25,043
dogfood basenames and 0 of the sweep's 74,624 names carry a glued `E<n>E<n>`.
The second, `S6.E1E2`, reads the same either way and passes today.

**It was implemented, and an existing test refused it**:

    a_separated_token_still_spans   left [1, 2, 3]   right [1]

with the reason written beside the case:

> An unseparated repetition must land on the next number, so `E1E3` is one
> episode. That guard is iteration 12's and it is right — `E1E3` is not a
> range.

**That is a taken decision from a merged slice** — `d9c8076`, #149 — not an
accident of the code. Reversing it to earn one corpus case is a product
decision, which this loop's stop conditions put outside its own authority. The
change was reverted rather than argued with, and the disagreement is recorded:
**Sonarr reads `E1E3` as a range and Nightjar does not.** Whoever revisits it
is revisiting #149, with two corpus cases and no library evidence on either
side.

## What is left in the row, and why each is out of reach

| case | why |
|---|---|
| `Series.Title.103.104`, `the.Series.101.102`, `Series.10708`, `Series.10910`, `E.010910` | want a season the name does not state — **N1** |
| `Series.Title.E07-E08.180612`, `Series Title? E11-E12` | same |
| `Series.S01E91-E100` | 10 wide against `MAX_EPISODE_RANGE = 8` — a constant tied to ADR-0025 |
| `Series falls - … [Cap.111_120]` | 10 wide, and needs the refused `_` as well |
| `World Series of Sonarr - 2010x15 - 2010x16` | a bare four-digit season, refused as the shape of a resolution |
| `Season 2\E05-06 - Episode Title` | a backslash path, outside the input contract |
| `Series T Se.3 afl.3 en 4` | wants `en` — Dutch *and* — as a list separator |
| `【高清剧集网发布…】…S01…` | CJK decoration and a season marker as well |
| `8x01_02` ×2, `S6.E1E3` | **refused above** |

**The row is 16 and its reachable remainder is 0.** That is the answer to
"re-derive the blocked column whenever cases move", and it is why this
iteration produced two refusals instead of a rate.
