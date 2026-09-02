# Iteration 2 — `Se N` and `afl N`, the Dutch season and episode marker

**Base: iteration 1 (`80407db`). Kept. Corpus `617 → 618`.**

## The population, counted in the instrument that judges it

`Se` is *seizoen* and `afl` is *aflevering*. Three of the 844 corpus inputs
carry the pair:

    Series T Se.3 afl.3                  title `Series T`, season 3, episode 3
    Series T Se.3 afl.3 en 4             title `Series T`, season 3, episodes 3–4
    13 Series Se.1 afl.2-3-4 [VTM]       title `13 Series`, season 1, episodes 2–4

## Every word got its own counterexample search

| | corpus inputs | corpus titles | `db_title` (2,475) | basenames (25,043) | sweep names (74,624) |
|---|---:|---:|---:|---:|---:|
| `afl` followed by digits | 3 | 0 | 0 | 0 | 0 |
| `afl` as a bare word | 3 | 0 | 0 | 0 | 0 |
| `se` + optional separator + digits | 3 | 0 | 0 | **1** | **32** |
| `se` as a bare word | 3 | 0 | 0 | 2 | 0 |

**`afl` is free everywhere.** It is not an English word, which makes it cheaper
than `cap` and `ep`, both of which are and both of which are already in.

**`se` is not free, and the name is `Se7en`.** The library holds
`Se7en (1995) Bluray-1080p.mp4`, a bound film, and the sweep renders that title
32 times. Two more basenames carry the bare word — `The Tales of Ba Sing Se`
and `Sí Se Puede`, both episode titles — with no digits behind either.

**`Se7en` is refused, and by a guard that was already there.** `7` is followed
by `e`, so `find_bare_season`'s boundary check sees a digit run that is not a
whole number and declines.

## The guard that was written, measured, and removed

A separator requirement was added first — `se` may only claim `Se.3`, never
`Se3` — on the reasoning that two letters is the shortest marker in the file.

**Its negative control came back green.** Deleting the check left
`se_glued_to_its_digits_is_not_a_season` passing, because the boundary check
gets there first. The guard refused nothing that was not already refused, and
it would have cost the glued Dutch spelling `Se3 afl3` for nothing. This file
already records the principle — *a guard with a demonstrable cost and no
demonstrable case is not one to keep* — so it came out, and the test was
retargeted at the guard that does the work.

**That is why a control is run before the claim is written.** The comment on
the guard said what it refused. It was wrong, and only deleting it said so.

## Predicted, before running

Corpus **617 → 618, +1**. `Series T Se.3 afl.3` closes. The other two keep
failing on `episodes`: `find_spelled_episode` reads one number and returns, so
`afl.2-3-4` yields 2 and `afl.3 en 4` yields 3 — but both gain their title and
their season. So `--diff` should report fields **fixed** and none **gained**.

## Measured

| instrument | before | after | |
|---|---|---|---|
| corpus | 617 / 734 — 84.1% | **618 / 734 — 84.2%** | **+1** |
| `classify.py --diff`, it1 → it2 | — | `+1 / −0`, gained none, fixed `{title: 2, season: 2}`, exit 0 | |
| `classify.py --diff`, base → it2 | — | `+1 / −0`, same, exit 0 | |
| parser sweep, `80407db` → tip | 0 / 0 | **0 / 0** | **genuinely sensitive, and zero** |
| dogfood probe, 25,043 paths | — | **0 rows of 50,086 changed** | **narrow by population, sensitive on one path** |
| `cargo test --workspace` | 854 passed | **858 passed** | +4 tests |
| `clippy -D warnings`, `fmt --check` | 0 | 0 | |

**The sweep's zero is the valuable one.** 32 of its 74,624 names are `Se7en`
renderings — `Se7en.2019.1080p.BluRay.x264-GRP.mkv`, `Se7en (2019)
Bluray-1080p.mkv`, `Se7en_2019_1080p_BluRay.mkv` and the rest. Had `se` claimed
them, all 32 would have lost their title and gained a season, and the sweep
would have said so. It is not insensitive here; it looked and found nothing.

**The dogfood zero is narrow by population**: exactly one of the 25,043 paths
can reach this rule, and it is the same `Se7en`.

## The negative controls

Multi-word heads, so each isolates its own guard.

| control | test | on deletion |
|---|---|---|
| `"se"` out of `SEASON_ABBREVIATIONS` | `a_dutch_season_and_episode_marker_is_one_marker` | **FAILED**, 160 filtered out |
| `"afl"` out of `SPELLED_EPISODE_WORDS` | same | **FAILED**, 160 filtered out |
| `is_ascii_alphabetic` out of the boundary check | `se_glued_to_its_digits_is_not_a_season` | **FAILED** — title `The Film` instead of `The Film Se7en` |

The filtered-out counts are quoted because `cargo test <name>` reports
`ok. 0 passed; N filtered out` for a test that no longer exists.

## One mechanism, two words, and why they are one commit

The board's rule is one word per commit, and a word earning zero comes out.
**Neither word earns anything alone**, and that is a property of the code, not
an excuse: `find_spelled_episode` runs only in the bare-season arm, so `afl`
without `se` is never consulted, and `se` without `afl` reads a season and
leaves the episode absent. The pair is one marker written in two Dutch words.
That is stated here rather than assumed, and the test is red on deleting
either.

## What this leaves on the board

`expand-a-range` keeps the two Dutch names, both now failing on `episodes`
alone. `find_spelled_episode` returns one number where `find_season_episode`
calls `extend_episode_span`. **Giving the spelled arm the same range extension
is the next mechanism**, and it reaches a third case that has nothing to do
with Dutch: `Series Title Season 01 Episode 05-06 720p`.
