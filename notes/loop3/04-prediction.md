# Iteration 4 prediction, written before any run

Change: the digit run after an episode marker in `find_season_episode` and
`extend_episode_span` must be **whole**, and may be five digits wide. Today it
is capped at three and **truncates**: `S22E5363` reads episode 536.

| instrument | prediction |
|---|---|
| corpus | **524 -> 530 (+6)**, 71.0% -> 71.8% |
| sweep | **0 gains, 0 regressions** — its names come from real library titles and carry 1-2 digit episode numbers |
| oracle | **0 rows** — every generated episode is 1-10 |
| dogfood pair | **0** — Sonarr names, two-digit episodes |

The six corpus cases, named before running:

    Shortland.Series.S22E5363-E5366.HDTV.x264-FiHTV   [536]     -> [5363..5366]
    Plus Series la title - S14E3533 FRENCH ...        Some(353) -> 3533
    Anime Title - S2020E1527 [1527] [2020-10-11] ...  Some(152) -> 1527
    The Series And the Show - S41 E10478 - 2014-08-15 Some(104) -> 10478
    The Series And the Show - S42 E10591 - 2015-01-27 Some(105) -> 10591
    The Series And the Show - S42 E10713 - 2015-07-20 Some(107) -> 10713

Two more carry the defect and will **not** pass: `S42 Ep10718 - Ep10722` and
`S42 Ep10688 - Ep10692` want the whole range, and ` - ` is not a range separator
`extend_episode_span` recognises. They should go from `[107]`/`[106]` to
`[10718]`/`[10688]` — still a fail, no longer a truncation.

**Anything the sweep reports is unpredicted.** A move there means the widening
reaches names built from real titles, which is the case this change is not
about.
