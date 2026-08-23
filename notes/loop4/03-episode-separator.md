# Iteration 2 — the padded ` - ` between two episode tokens

## The population, counted in the instrument that judges it

Parser corpus, `out/cases.json`, 738 applicable. Fails where a complete episode
token sits behind a padded dash:

| case | fails on | earnable |
|---|---|---|
| `Series Title - S07E22 - S07E23 - And Lots of Security..` | `episodes [22]` vs `[22, 23]` | yes |
| `The Series And The Code - S42 Ep10718 - Ep10722` | `episodes` | **no — see below** |
| `The Series And The Code - S42 Ep10688 - Ep10692` | `episodes` | **no** |

**One case, not the board's two or my predicted three.**

## The convention it depends on

That a separator run of `[ ._-]` carrying exactly one dash introduces a range
end, and that **an episode marker must follow whenever whitespace padded the
dash**. Without the marker requirement the rule reads an episode title as a
range: `Series - S01E04 - 6 Feet Under` becomes episodes 4 through 6.

A *bare* dash keeps its exemption, because `S15E06-08` is a range and always has
been.

## Predicted before running

Corpus 532 → 535 (+3). Sweep 0 regressions. Oracle unknown — 45,465 of 90,072
generated rows contain ` - `, so **not insensitive by construction**, and it had
to be run rather than reasoned about.

## Measured

| instrument | base | tip | reading |
|---|---:|---:|---|
| parser corpus | 532 / 738 (72.1%) | **533 / 738 (72.2%)** | +1 gained, **0 lost** |
| parser sweep | 74,624 names | 74,624 | 0 regressions, 0 gains |
| matcher oracle | correct 71,434 | **71,434** | **0 rows changed**, wrong 6, absent 18,153 |
| dogfood strict pair (capture, 25,004) | `ready=24953 unmatched=51 errors=0 requests=0` | **identical** | 0 changed |
| `cargo test -p nightjar-core` | 111 pass | 111 pass | 0 failures |

Oracle drain: `requests=0`, 55 batches, 479 stalls (0.5%, unchanged), noise floor
0 of 90,072. Same generated library on both arms, checksum `aa9657b`.

## The prediction was wrong, and the reason is a different mechanism

`S42 Ep10718 - Ep10722` does not move, and **not because of the separator**. The
extension reads a **one-letter** marker: it consumes the `E` of `Ep`, then looks
for a digit and finds `p`. `MAX_EPISODE_DIGITS` is 5, so `10722` would fit; the
marker is the blocker. That is a second mechanism and it is not this commit's.

## The zero on the oracle is a result, not insensitivity

This is the zero worth being careful about, so it was checked rather than
assumed. **2,877 of the 90,072 basenames carry a padded dash directly after an
episode token** — the instrument is fully sensitive to this change by
population. Every one of them looks like this:

    Revival - S01E01 - Episode 1.mkv
    Luther  - S01E04 - Episode 4.mkv

**These are exactly the input the guard must refuse**, and the oracle supplied
2,877 of them without being asked. The marker check consumes the `E` of
`Episode`, finds `p` where it needs a digit, and declines — 2,877 times, with
zero rows moved.

So the three zeros read:

- **oracle — genuinely sensitive, genuinely zero.** 2,877 rows of the exact
  shape; the guard rejected all of them and the one corpus case it must accept
  was accepted. This is the guard tested for what it *permits*, at a scale no
  unit test would reach.
- **sweep — narrow by population.** It can see parser changes; none of its
  74,624 generated names pads a dash between two episode tokens.
- **core tests — sensitive, and unchanged.** 111 pass on both arms.

- **dogfood strict pair — sensitive, and zero.** **1,632 of the 25,004 captured
  basenames** carry a padded dash after an episode token. `30 Rock - 2x10 -
  Episode 210 - Bluray-1080p.mkv` is a real file, and it is the adversarial case
  written by a real library rather than by a generator.

**The oracle cannot see the exemption; the dogfood pair can.** The oracle holds
**no bare-dash range at all** (`S15E06-08`, 0 of 90,072), so the behaviour this
change deliberately preserves is invisible to it. The real library has **33**,
unchanged across the pair. The exemption is measured only because both
instruments were run, and the population each speaks for is different.

## Judgement

Corpus up by 1 with nothing lost, every other instrument flat with the flatness
explained. **Kept.**

## It reaches further than the parser, and that was checked

`EpisodeSlot::season_episodes` (`metadata/src/queue.rs:1696`) re-parses the
basename to expand ranges "so we do not need an `episode_end` column". It is
production code inside the matcher, and it consumes `episode_numbers()` — so a
change to the episode span changes which slot a file occupies. That is the
reason the oracle was run for a parser change, and the reason its zero is worth
more than the corpus's +1.
