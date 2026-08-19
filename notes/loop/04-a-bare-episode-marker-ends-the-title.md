# 04 — a bare episode marker ends the title

**Kept.** Corpus 402 -> 413 of 738 all-fields (54.5% -> 56.0%). Structure-only
576, unchanged. Zero corpus cases regressed. **Zero dogfood items change.**

## Mechanism

A release that numbers episodes absolutely often marks the number with `E` or
`Ep` and carries no season, so `find_season_episode` declines and the title ran
on through the marker and the episode title behind it —
`AnonShow.E1135.Ein.Titel.GERMAN.1080p...`.

`cut_at_episode_marker` ends the title at the marker.

**The number is not parsed.** It is an absolute episode number, `ParsedName`
has nowhere to put one, and the corpus dropped `absoluteepisodenumber` during
extraction — so setting it would score zero and change `kind` for no
measurable gain. Do the title half; stop before the number half.

## Prediction vs actual

Predicted **+11**, structure unchanged, 0 dogfood items. Actual **+11**,
structure unchanged, 0 dogfood items. The prediction landed exactly.

19 further cases change and still fail. They need a second mechanism as well —
repeated leading group brackets (`[Jumonji-Giri]_[F-B]_Series_Title_Ep04`) or
CJK dual titles.

## Guards

- **Two digits minimum.** `E06` is a marker; `E3` is as likely a title word.
  Measured: one digit gains nothing extra, so the tighter bound is free.
- **A separator on the left**, so the `e` in `HEVC` is not a marker.
- **No letter or digit on the right**, so `EAC3` and `E-AC3` are not either —
  the character after the `E` is not a digit at all.
- **The head must carry a letter**, so `Ep01 (D2201EC5)` has no title to end.

## The dogfood is silent, which is not the same as safe

**0 of 25,043 basenames match this shape.** The library cannot vote either way,
so the guards above rest on the token grammar and not on a measurement. That is
weaker evidence than the previous three iterations had and it is stated rather
than glossed.

## One thing this rule does not reach

`Anon Show 2018 EP06 720p x265` yields `Anon Show`, not `Anon Show 2018` — the
year branch cuts at `2018` long before the marker is looked at. That is the
`Wonder Woman 1984` shape on the TV side. It is a year-branch problem and it is
now pinned by a test that asserts the current behaviour, so the day someone
fixes the year branch this test will say so.

## The unrelated red test cleared

`nightjar-transcode`'s `mapped_real_library_end_moov_mp4_copy_keeps_aac` failed
during the iteration-03 gate and passes here on a tree that differs only by
this iteration. It is environment-dependent, as the iteration-03 stash test
already showed. Recorded, not touched.
