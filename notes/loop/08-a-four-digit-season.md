# 08 — a four-digit season, in the marked spelling only

**Kept.** Corpus 430 -> 435 of 738 all-fields (58.3% -> 58.9%). Structure-only
585 -> 591 (79.3% -> 80.1%). Zero corpus cases regressed. **Zero dogfood items
change.**

This is the prerequisite iteration 07 named when the year-boundary fix was
reverted.

## Mechanism

`find_season_episode` read at most three season digits, so `S2016E231` and
`S1936E18` produced nothing at all — no season, no episode, and a title running
to the end of the name. A four-digit season is a year-season and Sonarr writes
them.

## The guard is the spelling, and it cost three cases

Widening **both** spellings gives 9 corpus candidates and one false positive
that is exactly the hazard the two-digit cap exists for:

    [Kulot] Violet Evergarden ... [Dual-Audio][BDRip 1920x804 HEVC FLACx2]
      -> season 1920, episode 804

1920 is inside the year range and 804 is a whole three-digit run. **No range
guard and no whole-run guard separates `1920x804` from `2016x231`** — they are
the same shape.

`S2016E231` is not the same shape. The `S` and the `E` are what make the number
a season, and a resolution has neither. So four digits are accepted in the
marked spelling only. That gives up `2009x09`, `2016x231` and
`World Series ... - 2010x15`, and buys immunity from every resolution rather
than from the four that happen to be in this corpus.

A four-digit season must also be a plausible year, so `S1080E01` is still not a
season.

## Prediction vs actual

Predicted +5 all-fields, ~+6 structure, 0 dogfood items.
Actual **+5**, **+6**, **0**. Landed exactly.

The one case predicted to stay failing did: `S2009E09 [SDTV].avi` expects an
empty title, and the parser substitutes the whole stem when a title cuts to
nothing. That substitution is a documented decision, not an oversight.

## What this unblocks

The year-boundary fix reverted at iteration 07 disturbed six cases; four of
them are this form and are now parsed. Re-running that fix on top of this is
the obvious next step, and it is measured rather than assumed — see the final
report.
