# Iteration 3 — a range spelled in words is the same file as a range in markers

**Base: iteration 2 (`965cd69`). Kept. Corpus `618 → 620`.**

## The population, counted in the instrument that judges it

A spelled episode word, digits, then a separator and more digits:

| population | matches |
|---|---:|
| corpus inputs (844) | **5** |
| dogfood basenames (25,043) | **0** |
| sweep names (74,624) | **0** |

The five, and what each needs:

    Series Title Season 01 Episode 05-06 720p          reachable — [5] wants [5,6]
    13 Series Se.1 afl.2-3-4 [VTM]                     reachable — [2] wants [2,3,4]
    Series T Se.3 afl.3 en 4                           needs `en` as a separator
    Some Anime Show (2011) Episode 99-100 …            states no season, never reaches the arm
    Series.Title.Ep01-12.Complete…                     12 wide, over MAX_EPISODE_RANGE = 8

## The convention, and what the rule does without it

`S01E05-06` is one file holding episodes 5 and 6, and ADR-0025's amendment
requires the item list to show one entry spanning the run rather than a gap
that reads as missing media. **`Season 01 Episode 05-06` is the same file
written another way, and only the marked spelling emitted the pair.**

Without the convention the spelled arm reports the first number and stops, so
the same library shows episode 6 as missing on one spelling and present on the
other.

**No second range rule was written.** `find_spelled_episode` now calls
`extend_episode_span`, the function the marked arm has always used, so every
guard arrives unchanged.

**And one of those guards is unreachable here, which is worth saying.**
`find_bare_season` declines any name carrying ` - N` anywhere, so a name that
reaches the spelled arm has no padded separator in it at all. The
padded-separator rule is doing nothing on this path; it is inherited, not
relied on. That is written into the doc comment rather than left for a reader
to discover.

## Predicted, before running

Corpus **618 → 620, +2**. Sweep and dogfood **0**.

## Measured

| instrument | before | after | |
|---|---|---|---|
| corpus | 618 / 734 — 84.2% | **620 / 734 — 84.5%** | **+2** |
| `classify.py --diff`, it2 → it3 | — | `+2 / −0`, gained none, fixed none, exit 0 | |
| parser sweep, `965cd69` → tip | 0 / 0 | **0 / 0** | **blind by construction** — see below |
| dogfood probe, 25,043 paths | — | **0 rows of 50,086 changed** | **narrow by population** |
| `cargo test --workspace` | 858 passed | **860 passed** | +2 tests |
| `clippy -D warnings`, `fmt --check` | 0 | 0 | |

**The sweep cannot see this change, and the reason is in `sweep.rs`.** It prints
`title`, `season`, `episode`, `year` and `kind` — **not `episode_end`**, which
is the only field this iteration writes. So its zero is not evidence about the
range at all; it is evidence that `episode` and the title did not move. Both
readings are worth having and they are different readings.

**The dogfood probe is the only instrument that can see it**, because
`dogfood_probe.rs` prints `episode_end` on both entry points. Its zero is
narrow by population — no basename in the library spells a marker with a range
— but it is a real look, not a structural blindness.

## The negative controls

| control | test | on deletion |
|---|---|---|
| `extend_episode_span` call out of `find_spelled_episode` | `a_spelled_episode_marker_carries_its_range` | **FAILED**, 161 filtered out |
| the `(!separated \|\| padded) && !marked` break out of `extend_episode_span` | `a_spelled_episode_marker_declines_what_is_not_a_range` | **FAILED** — `Some(6)` where `None` was asserted |

**The second control exists because the first proves nothing about it.** The
decline test asserts an absence, so it stays green when the extension call is
removed — it would pass against a parser that never extends anything. It was
negative-controlled against the guard it is actually about, and only then
recorded as a control.

Its five names are constructed inputs the rule would wrongly accept, one head
each: a hyphenated episode title behind the number (`Episode 3-D Printing`), an
unmarked repetition behind a space (`Episode 5 06`), the same behind a dot,
which is a space once the stem is normalised (`Episode 4.05`), a run wider than
the cap (`Episode 07-99`), and a second number below the first
(`Episode 08-07`).

## What this leaves

`Series T Se.3 afl.3 en 4` is the last Dutch name, and it wants `en` — Dutch
*and* — read as a list separator. That is a third vocabulary word for one case,
and `en` is an ordinary word in several languages. It is not attempted here.
