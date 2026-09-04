# Iteration 5 — a bare four-digit season, in the shape a resolution cannot take

**Base: iteration 3 (`d30198e`), iteration 4 having been reverted. Kept.
Corpus `620 → 622`.**

## The refusal this narrows, and why that is not reversing it

`2016x231` is refused, and the reason is written in the file:

> The bare `2016x231` is not allowed the same width because it is the shape of
> a resolution: `1920x804` puts a plausible year on the left and a whole
> three-digit run on the right, so no range guard and no whole-run guard
> separates them.

**The second half of that sentence is a rule, and it was never written as
one.** What tells `2016x231` from `2009x09` is the width of the run after the
`x`. The refusal is kept exactly where it was stated — a three-digit right-hand
run stays refused — and the case it never covered is claimed.

## The population, counted in the instrument that judges it

Every `(19|20)NN x N…` in the evidence, split on the width of the run after
the `x`:

| population | right run ≤ 2 | right run ≥ 3 |
|---|---:|---:|
| corpus inputs (844) | **3**, in 2 names | 8 |
| dogfood basenames (25,043) | **0** | **0** |
| sweep names (74,624) | **0** | **0** |

The three narrow ones are `2009x09`, `2010x15` and `2010x16`, and both names
assert a year-season. The eight wide ones are `1920x1080` ×3, `1920x804`, and
the refused `2016x231` — **every one a resolution or the refused case, and not
one of them a season anybody wants read.**

**No resolution has a two-digit height.** That is the convention, and without
it the rule reads a year and a small number as television. The cost shape is
constructible — `Some Movie (1998) 2048x08 BluRay.mkv` becomes season 2048 —
and it is **unmeasured rather than absent**: nothing in the corpus, the library
or the sweep has that form, so it is a real cost with no example behind it.

## The two cases, and the second half of the mechanism

    2009x09 [SDTV].avi                                  title "", season 2009, episode 9
    World Series of Sonarr - 2010x15 - 2010x16 - HD TV  season 2010, episodes 15–16

The second needed the same rule in a second place. `read_repeated_season`
capped the bare `Nx` spelling at two digits, so `- 2010x16` read `20` behind a
claimed season of 2010, did not match, and the range died on the repetition
even though the first token had been claimed. **One spelling, one rule, both
places it is read** — and the guard there is not a width test but the caller's:
a repeated season is accepted only when it equals the season already claimed,
which a resolution cannot do.

## Predicted, before running

Corpus **620 → 622, +2**. Sweep and dogfood **0, insensitive by construction**.

## Measured

| instrument | before | after | |
|---|---|---|---|
| corpus | 620 / 734 — 84.5% | **622 / 734 — 84.7%** | **+2** |
| `classify.py --diff`, it3 → it5 | — | `+2 / −0`, gained none, fixed none, exit 0 | |
| parser sweep, `d30198e` → tip | 0 / 0 | **0 / 0** | **insensitive by construction** |
| dogfood probe, 25,043 paths | — | **0 rows of 50,086 changed** | **insensitive by construction** |
| `cargo test --workspace` | 860 passed | **862 passed** | +2 tests |
| `clippy -D warnings`, `fmt --check` | 0 | 0 | |

Both zeros are insensitive by construction and the count above says why: **no
name in either population puts a four-digit run in front of an `x` at all**, so
neither instrument could have moved whatever the change did.

## The negative controls

| control | test | on deletion |
|---|---|---|
| the four-digit branch out of `find_season_episode` | `a_bare_year_season_reads_a_two_digit_episode` | **FAILED**, 164 filtered out |
| `episode_digits <= 2` out of `bare_year_season_ok` | `a_bare_year_season_declines_a_resolution` | **FAILED**, 164 filtered out |
| `read_repeated_season` back to two digits | `a_bare_year_season_reads_a_two_digit_episode` | **FAILED**, 164 filtered out |

**The second control is the one that matters**, and its three names are real
corpus inputs rather than constructions: without the width guard `1920x1080`,
`1920x804` and `2016x231` all become episodes — a wrong kind, which the
matcher oracle ranks as the worst class it has.
