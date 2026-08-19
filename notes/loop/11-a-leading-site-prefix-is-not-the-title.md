# 11 — a leading site prefix is not the title

**Kept.** Corpus 463 -> 468 of 738 all-fields (62.7% -> 63.4%). Structure-only
591, unchanged. Zero corpus cases regressed. **Zero dogfood items change.**

## Mechanism

`www.Torrenting.com - Movie.2008.720p.X264-DIMENSION` parsed as
`www Torrenting com - Movie`. The prefix sits **before** the title and every
terminator cuts from the right, so nothing ever reached it.

`strip_site_prefix` removes a leading domain followed by a dash.

## Guards, all on what is left

The same shape of guard `strip_leading_group` uses.

- The head must be a domain: dot-separated, no whitespace, alphanumerics and
  `.-_` only, and a last label of two to four letters.
- A dash with whitespace after it must follow, so a film titled `example.com`
  is untouched.
- What remains must carry a letter, or the prefix stays — stripping to nothing
  is worse than keeping junk, which is the trade `cut_at_title_junk` already
  makes.

## Prediction vs actual

Predicted **+5**, structure unchanged, 0 dogfood items.
Actual **+5**, structure unchanged, **0** dogfood items.

## Measured this iteration and not done

**The season marker stays in the title for the anime form.** Sonarr keeps
`S03` in `Series Title S03 - EP14` while still reporting season 3; iteration 02
cuts the title at the token. Five corpus cases want the marker kept.

Only two of the five would land on that change alone. The others need a
bracketed absolute number (`Series Title S2 [05]`) or a bare trailing one
(`Anime Title S21 999`) — and the bare trailing number was measured at
iteration 06 to break 420 bound library titles at its loosest guard and four at
its tightest. So this class is blocked behind a rule that cannot be made safe
with the signals a filename carries.

## The intermittent transcode test

Failed again, as at iterations 03, 05, 06 and 09. Cleared as unrelated by the
iteration-03 stash test.
