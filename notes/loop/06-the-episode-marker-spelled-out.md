# 06 — the episode marker spelled out

**Kept.** Corpus 423 -> 430 of 738 all-fields (57.3% -> 58.3%). Structure-only
585, unchanged. Zero corpus cases regressed. **Zero dogfood items change.**

## Two candidates rejected at the population step, before any code

### A trailing standalone number is the absolute episode number — rejected

The dogfood is a movie library and it is full of titles that end in a number.

| guard set | corpus gains | dogfood touched | **bound today** |
|---|---:|---:|---:|
| any trailing number | 8 | 420 | **420** |
| zero-padded or 3+ digits | 8 | 6 | **6** |
| the same, only when no year was parsed | 8 | 4 | **4** |

`Apollo 13` -> `Apollo`. `Deadpool 2` -> `Deadpool`. `District 9` -> `District`.
Even the tightest guard set breaks four bound `Prisoner 951` episodes and
touches eight passing corpus cases, for eight corpus gains. Never written.

This is the largest remaining class in the `- NN` family and it is closed as
far as this instrument can see. A safe version would need a signal the filename
does not carry.

### Strip leading bracket groups repeatedly — rejected

Only 2 of the 21 corpus cases with two or more leading groups suit it. The
other 19 are the bracket-delimited CJK form, where repeated stripping eats the
title and leaves `[1080P]`. That form needs a rule that *picks* the title
group, which is different machinery.

## What was done

`cut_at_episode_marker` now accepts the marker spelled out — `Episode 10`,
`Episodio 5`, `Episodes 12` — as well as `E` and `Ep`.

## The digit guard is asymmetric, and that is the point

**The longer the marker, the less the number has to carry.** `E3` could be a
title token, so the short marker still needs two digits — iteration 04 measured
that one digit gains nothing there. `Episode 3` cannot be anything else, so one
digit is enough after the word, and it is worth exactly two corpus cases.

## Prediction vs actual

Predicted **+6**. First measurement **+5** — two cases were `Episode 5`, a
single digit, which iteration 04's two-digit guard refused. The simulation had
used `\\d{1,4}` and so had not seen the guard. Adding the asymmetric bound
brought it to **+7**.

Both halves of the miss are the classifier, not the code: it modelled a rule
slightly looser than the one that exists.

## The dogfood measurement, which is real here

**0 of 25,043 touched** — and this is a measurement, not silence. **1,272
dogfood paths contain `episod`.** The rule fires on none of them, because they
are episode *titles* sitting behind a `SxxExx` or `NxNN` token that the
season/episode cut removes first. That is now a test.

## The intermittent transcode test, again

`mapped_real_library_end_moov_mp4_copy_keeps_aac` failed at iterations 03, 05
and 06 and passed at 04, on trees that differ only by parser work that the
iteration-03 stash test already cleared. Not touched.
