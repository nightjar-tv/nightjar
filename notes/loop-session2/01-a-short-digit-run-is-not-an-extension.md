# Iteration 1 — a one- or two-digit suffix is not a file extension

**Base: `2723dcf`. Kept, and flat on purpose — read the judgement at the end.**

## The population, counted in the instrument that judges it

Names ending in a dot and one to four digits:

| population | ends `.<1-4 digits>` | ends `.<1-2 digits>` |
|---|---:|---:|
| corpus inputs (844) | 15 | **4** |
| dogfood basenames (25,043) | 0 | **0** |
| sweep names (74,624) | 0 | **0** |

The four corpus names:

    Series T Se.3 afl.3                                     fails today
    Top Series - 07x03 - 2005.11.70                         passes
    Top Series - 06x11 - 2005.08.07                         passes
    Series Title - 2026-07-03 - Look Back At 2025 Part.1     passes

## The convention, and what the rule does without it

**A container is spelled with letters.** `is_extension` accepted one to four
alphanumeric characters, so `.3` and `.70` read as containers. The year rule
added in `15707ac` had already carved out the four-digit case — `.1998` is a
year, not a container — and this is the same defect one width down.

Without the convention the rule cuts a number the name meant to carry.
`Series T Se.3 afl.3` lost the `3` behind its episode marker before any marker
rule ran, so the name could only ever report a season with no episode.

**Three digits stays an extension.** `.264` is a raw H.264 elementary stream and
the file already records it as a real one; `[DRONE]Series.Title.100` wants its
`100` dropped. So the floor is two, not three, and the boundary is asserted.

## Predicted, before running

Corpus **617 → 617, +0**. The failing name needs a marker rule this change does
not add; the three passing names take their claim from a marker or a date
earlier in the name, not from the suffix. Sweep and dogfood **0, insensitive by
construction** — neither population contains a single name of this shape.

## Measured

| instrument | before | after | |
|---|---|---|---|
| corpus | 617 / 734 — 84.1% | **617 / 734 — 84.1%** | +0 |
| `classify.py --diff` | — | `+0 / −0`, no field gained, **no field fixed**, exit 0 | |
| parser sweep, `2723dcf` → tip | 0 / 0 | **0 / 0** | **insensitive by construction** |
| dogfood probe, 25,043 paths | — | **0 rows of 50,086 changed** | **insensitive by construction** |
| `cargo test --workspace` | 853 passed | 854 passed | +1 test |
| `clippy -D warnings`, `fmt --check` | 0 | 0 | |

Every prediction held, including the flat corpus.

**Both zeros are insensitive by construction and say so.** No name in either
population ends in a dot followed only by digits, so neither instrument could
have moved whatever the change did. They are recorded because a zero with a
stated reason is evidence and a bare zero is not.

## The negative control

`a_one_or_two_digit_suffix_is_not_a_file_extension` — three names, three heads,
so no line can be masked by another declining. **Red on deleting
`is_short_digit_run` from `is_extension`**, verified by deleting it:
`FAILED. 0 passed; 1 failed; 156 filtered out` — the count says the test was
run, not filtered away.

## Judgement: kept, flat, and why

**The board's rule is revert on flat unless the flatness is explained.** The
explanation is measurable rather than asserted: the one corpus name this change
unblocks is `Series T Se.3 afl.3`, whose marker is Dutch and unread, and
**iteration 2 reads it and closes the case**. The two changes were kept apart so
each is one mechanism and each is measured on its own tree — which is also how
it became visible that the marker work was blocked by an extension rule and not
by a marker rule.

**If iteration 2 had not earned, this commit came out with it.** It did earn,
and the pair is `+1`.
