# 05 — only a real extension is stripped

**Kept.** Corpus 413 -> 423 of 738 all-fields (56.0% -> 57.3%). Structure-only
576 -> 585 (78.0% -> 79.3%). Zero corpus cases regressed. **Zero dogfood items
change.**

## Mechanism

`strip_extension` cut at the last dot and threw away whatever followed it. A
release name is not a filename: `Series.S01E91-E100` has exactly one dot, so
the entire episode token went out as an "extension" and the parse saw `Series`
with no season and no episode.

The guard is one to four characters, all alphanumeric.

## Population

- corpus: **232** applicable cases lose a non-extension suffix today. Ten lose
  their only season/episode token; six lose their only year.
- dogfood: **0** of 25,043 basenames have a non-extension suffix, and every
  extension in the library satisfies the guard. The library cannot move.

## Prediction vs actual

Predicted +10 to +16 all-fields, structure ~+10, 0 dogfood items.
Actual **+10**, structure **+9**, **0** dogfood items — the bottom of the range.

The title side was deliberately not predicted: 232 cases change input, and the
over-eager strip was acting as an accidental junk cut. It turned out to change
no title outcome either way, because the junk cut takes the earliest token and
adding text at the end does not move it.

**All ten gains are season/episode recoveries.** The six year cases —
`A.I.Artificial.Movie.(2001)`, `A.Movie.Name.(1998)`, `The Series Bros. (2006)`
— gained their year and still fail on title. Naming that matters: the year half
of the prediction landed and the case-level half did not, and the difference is
a title mechanism that has not been done yet.

## The intermittent transcode test

`nightjar-transcode`'s `mapped_real_library_end_moov_mp4_copy_keeps_aac` failed
at iteration 03, passed at 04, and failed here. Stashing the only source change
at iteration 03 reproduced the failure on the unchanged tree, so it is not
caused by the parser work. It asserts a video start time against a real media
file. Not touched, not weakened.
