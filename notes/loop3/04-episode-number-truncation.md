# Iteration 4 — the parser asserted an episode number the name never carried

**Kept.** `539c477`. Corpus 71.0% → **71.8%**, six gains, no losses.

## The population

Counted in the instrument that judges it. Thirteen corpus cases carry a digit
run of four or more after an episode marker; five of them pass already (they are
`1920x1080` resolutions the parser correctly declines). **Eight fail, and every
one of the eight fails by asserting a wrong number rather than none:**

    Shortland.Series.S22E5363-E5366.HDTV.x264-FiHTV     episode 536   want 5363-5366
    The Series And The Code - S42 Ep10718 - Ep10722     episode 107   want 10718-10722
    The Series And The Code - S42 Ep10688 - Ep10692     episode 106   want 10688-10692
    The Series And the Show - S41 E10478 - 2014-08-15   episode 104   want 10478
    The Series And the Show - S42 E10591 - 2015-01-27   episode 105   want 10591
    The Series And the Show - S42 E10713 - 2015-07-20   episode 107   want 10713
    Plus Series la title - S14E3533 FRENCH WEBRIP       episode 353   want 3533
    Anime Title - S2020E1527 [1527] [2020-10-11]        episode 152   want 1527

The digit run was capped at three and the loop simply **stopped**, leaving the
fourth digit unread. So `S22E5363` reported 536 — a number the name does not
contain, taken to the provider, binding a real episode that is not this file's.
An absent claim leaves a file unmatched and recoverable; this is the other kind.

## The convention it depends on, and what it does without it

**An episode marker — `S`/`E`, `Ep`, or `NxNN`.** The change touches nothing
else: a bare number with no marker is not reached by this code at all, and
`cut_at_absolute_episode` and `cut_at_episode_marker` are untouched.

Without a marker in front, five digits would be reckless — that is why
`cut_at_episode_marker`, which handles the bare form, keeps its four. The `S` is
the same evidence the season arm already leans on to carry four digits: "the `S`
and the `E` do; a resolution has neither."

## The prediction, written before the run

`~/nightjar-wt-loop3-scratch/it4-prediction.md`, naming the six cases
individually:

| instrument | predicted | measured |
|---|---|---|
| corpus | 524 → **530** (+6), 71.8% | **530, 71.8%, six gains, zero losses** |
| sweep | 0 gains, 0 regressions | **0 / 0** |
| oracle | 0 rows | **0 rows** |
| dogfood pair | 0 | **identical on every counter** |

Also predicted, and held: `S42 Ep10718 - Ep10722` and `S42 Ep10688 - Ep10692`
would **not** pass — they want a range written across ` - `, which
`extend_episode_span` does not recognise as a separator — but would stop
truncating. They go from `[107]`/`[106]` to `[10718]`/`[10688]`.

Four for four, and the six cases were the six named.

## The change

One rule, in the one place two scanners already disagreed.

`cut_at_episode_marker` **already** required a whole run: it reads up to four
digits and rejects the token when another digit follows (its `bounded` check).
`find_season_episode` and `extend_episode_span` did not — same file, same
question, two answers. `read_episode_digits` is now the single reader for both.

    fn read_episode_digits(bytes: &[u8], at: &mut usize) -> (i32, usize, bool)

It returns the value, the width, and **whether the run is whole**. A run that
keeps going past `MAX_EPISODE_DIGITS` (five) means the token is not an episode
marker, and the caller declines it rather than using the prefix.

**Five, and why.** `S42 Ep10722` and `S22E5363` are real names — a daily serial
reaches five digits. Six is not an episode number.

**Only a following digit breaks the run.** A following *letter* must not:
`S01E01E02` and `8x01x02` are the multi-episode spellings, and rejecting on a
letter would refuse every one of them. There is a test.

## What was measured

| instrument | before | after | reaches this change? |
|---|---|---|---|
| parser corpus | 524/738, **71.0%** | **530/738, 71.8%** | **yes, and it is the only one that does** |
| parser sweep, 74,624 names | 0/0 at base | **0 gains, 0 regressions** | sensitive to the crate, **blind to this defect** — see below |
| oracle, 81,094 rows | 64,836 correct | 64,862, the same 26 rows iteration 2 moved and no others | no — generated episodes are 1–10 |
| `movie.specials` vs `movie.noyear` | 0 differing | **0 differing** | the guard still holds |
| dogfood strict pair | `ready=24953 unmatched=51 errors=0 requests=0` | identical | no — Sonarr names, two-digit episodes |
| `cargo test` | 108 core tests | **108** (3 new) | yes |

**The sweep's zero is not a clean bill, and this is the important line.** The
sweep is sensitive to this diff in principle — `nightjar-core` did change, which
is the condition that made it insensitive in iterations 1 and 2. It is blind
here for a different reason: **its generated names carry one-digit episode
numbers only.** Checked, not assumed —

    gen_names.py | max episode digits: 1

So the sweep says "no name in this population has a multi-digit episode number",
never "the change is safe on names that do". The guard for that is the three unit
tests, and they are the guard precisely because no instrument here holds the
shape.

The corpus diff was read case by case
(`notes/loop3/scripts/corpus_diff.py`), not as a rate — including the two cases
whose **parse changed while the verdict did not**, which a rate cannot show and
which is the class this project keeps being bitten by.

Full suite green except the known-flaky
`hls::tests::mapped_real_library_end_moov_mp4_copy_keeps_aac`, which fails
identically at the base. `cargo fmt --check` and `clippy --all-targets -D
warnings` green.

## Judgement

**Kept.** The instrument that can see it went up by exactly the predicted six;
no case regressed; the three instruments that cannot see it were predicted not
to and did not.

## What this could not measure, named

- **How often a four-digit episode number occurs in a real library.** The
  dogfood library has none, and the oracle generates none. The corpus says the
  form exists in the wild; nothing here says how much of anyone's library it is.
- **Whether five is the right width.** It is a judgement from real serial
  numbering, not a measurement. A six-digit run is now declined outright, and no
  instrument holds one.
- **`S01E1080p`.** Glued digits followed by a letter now read 1080 where they
  read 108. Both are wrong and neither is a real release name; no corpus case
  and no library file has the shape.
- **The ` - ` range separator**, which is what the two remaining truncation cases
  now want. Left alone: it is a different mechanism in a different function.
