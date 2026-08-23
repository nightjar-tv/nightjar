# Iteration 3 — `Ep`, the two-letter spelling of the same marker

Completes the cluster the board named as "the ` - ` range separator — 2 cases".
Iteration 2 earned one of them; these are the other two, and the separator was
never what blocked them.

## The population

Parser corpus. Two cases, each failing on `episodes` alone:

    The Series And The Code - S42 Ep10718 - Ep10722   [10718] vs [10718…10722]
    The Series And The Code - S42 Ep10688 - Ep10692   [10688] vs [10688…10692]

## Why iteration 2 did not move them

`extend_episode_span` reads a **one-letter** marker. It consumed the `E` of `Ep`,
then looked for a digit and found `p`.

Not the width: `MAX_EPISODE_DIGITS` is already 5, and its own comment says why —
*"five, because a daily serial really does reach one: `S42 Ep10722` is a real
name"*. The constant was sized for exactly this input and the marker never let
the parser reach it.

## The convention, and the guard

`ep` is the marker, and **the digit must come immediately after the `p`.**

That immediacy is the entire safety argument, and it was chosen from the
instrument rather than from taste. The generated library holds **2,877 rows** of

    Revival - S01E01 - Episode 1.mkv
    Luther  - S01E04 - Episode 4.mkv

`episode` puts an `i` where the rule requires a digit, so every one of them is
declined. A looser consume — skipping the word, or tolerating a space before the
number — would turn 2,877 episode titles into episode ranges.

## Predicted before running

Corpus 533 → 535. Sweep 0 regressions. Oracle **0 rows changed**, because the
2,877 rows of the dangerous shape must all be declined.

## Measured

| instrument | base | tip | reading |
|---|---:|---:|---|
| parser corpus | 533 / 738 | **535 / 738 (72.5%)** | +2 gained, **0 lost** — exactly the two named |
| parser sweep | 74,624 names | 74,624 | 0 regressions, 0 gains |
| matcher oracle | correct 71,434 | **71,434** | **0 rows changed**; wrong 6, absent 18,153, stall 479 all identical |
| dogfood strict pair (capture, 25,004) | `ready=24953 unmatched=51 errors=0 requests=0` | **identical** | 0 changed |
| `cargo test -p nightjar-core` | 111 pass | 111 pass | 0 failures |

Oracle: `requests=0`, 55 batches, noise floor 0 of 90,072, same library
(`aa9657b`) on both arms.

## Judgement

Prediction met exactly on every instrument. Corpus up 2 with nothing lost, the
oracle flat with 2,877 rows of the shape that could have broken it. **Kept.**

## The zeros

- **Oracle — sensitive and zero.** 2,877 rows of `- Episode N` after an episode
  token. This is the instrument being handed the adversarial input for free, and
  it is the reason to trust the guard more than the corpus's +2.
- **Sweep — narrow by population.** No generated name spells a marker `Ep`.
- **Core tests — sensitive, unchanged.**
