# 10 — a bracket run selects its title group

**Kept.** Corpus 444 -> 463 of 738 all-fields (60.2% -> 62.7%). Structure-only
591, unchanged. Zero corpus cases regressed. **Zero dogfood items change.**

The largest single gain of the run.

## Mechanism

`[GM-Team][国漫][Anime Title][2019][215][AVC][GB][1080P]` carries **no text
outside the brackets at all**, so a terminator has nothing to cut at and the
title ran to the end of the name. This is the bracket-delimited CJK release
convention and twenty corpus cases are written in it.

So the title is *selected*, not derived. The first group is the release tag —
the same assumption `strip_leading_group` already makes — and the title is the
first group after it that carries a Latin letter run of two or more and is not
release metadata. The selected group is then trimmed to its Latin side
(`ANIME SERIES 海賊王` -> `ANIME SERIES`) and, if a **spaced** `_`, `/` or `|`
remains, to the part after the last one. An unspaced `_` is a filename space
and must not split, or `Anime_Series_Title` becomes `Title`.

## Two guards keep it away from ordinary names

- **Nothing alphanumeric outside the brackets.** An ordinary name has text for
  a terminator to work on and the terminator does a better job.
- **Three groups minimum**, so a title with one or two trailing tags is
  untouched.

## The group-rejection list is not a title cut

`GROUP_METADATA` holds `mp4`, `gb`, `cht`, `batch` — words that would be
reckless in `TITLE_JUNK`, where a match truncates a title. Here a match can
only make the selector skip a group and look at the next one. The worst it can
do is pass over a group whose every word is one of them, and a title made only
of container tags is not a title. `Anon GB Title` keeps its middle word, and
that is a test.

## Prediction vs actual

Predicted **+19**. First measurement **+18**; the bracket-run path returned the
group body raw, so `Anime_Series_Title` kept its underscores and the shipped
soft key would not fold them. Running the picked title through `clean_title`,
like every other title path already does, brought it to **+19** — exactly the
prediction.

One case was predicted to miss and did: `[UHA-WINGS][Anime-Series Title S02]`
wants `Anime-Series Title S2`. That needs season-number normalisation in the
soft key and is a different mechanism.

## A shipped test changed, and why

`a_cut_inside_a_bracket_backs_out_to_the_bracket` asserted

    [Anon][Anon Title][2019][234][AVC][GB][1080P]  ->  "[Anon Title]"

Brackets and all, because backing the year cut out to the opening bracket was
the best available for a name with no text outside its groups. **The new
behaviour is right**: that shape is a bracket run and the title is `Anon
Title`, which is what the corpus wants.

The assertion moved to `Anon Title [2019] Bluray-1080p.mkv`, which is not a
bracket run, so the test still covers the year-cut route it was written for.
The old expectation is recorded in the test's own comment rather than deleted.

## Dogfood

**0 of 25,043 basenames are a bracket run.** No item can move, and none did —
parse diff and group diff both empty.
