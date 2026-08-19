# 12 — a dash is not the only separator

**Kept.** Corpus 468 -> 472 of 738 all-fields (63.4% -> 64.0%). Structure-only
591 -> 596 (80.1% -> 80.8%). Zero corpus cases regressed. **Zero dogfood items
change.**

## Mechanism

`extend_episode_span` accepted only `-`, so `Series.S03E01.S03E02`,
`The Series S01e01 e02`, `Series.Title.2x04.2x05` and
`Hell on Series S02E09 E10` each returned a single episode **and reported
success** — worse than returning nothing, because a caller cannot tell a
single-episode file from a range whose tail was dropped. That is the same
defect the multi-episode-spellings test was written for; a space, a dot and an
underscore are three more spellings of it.

## The marker is the whole guard

A space, dot or underscore is accepted only when a repeated season or an
`e`/`x` marker follows. Without that the corpus is full of counterexamples
where the token after the separator is a numeral in the *episode title*:

    Series Title S01E06 3 Beers For Batali DVDRip XviD SPRiNTER
    Series.S01E04.2-45.PM.[HDTV-720p].mkv
    Series Title S02E21 18 5 4 720p WEB DL DD5 1 h 264 EbP

All three parse correctly today and all three are now tests.

**A soft separator means repetition, not a range.** The dash keeps its range
meaning — `S15E06-08` is 6, 7 and 8 — while a space or dot requires the next
number to be exactly the next episode. Stricter than the dash, and it is what
every corpus case of this shape actually is.

## Prediction vs actual

Predicted **+4** all-fields, ~+4 structure, 0 dogfood items.
Actual **+4**, structure **+5**, **0** dogfood items.

Two cases were predicted to keep failing and did: `S03E01.S03E02.720p...` and
`8x01_02 - Free Falling` both expect an empty title as well. One case is
deliberately out of reach: `Series's Sonarr - 8x01_02` repeats a **bare**
number with no marker, and reaching it means giving up the guard above.
