# Iteration 5 — a date is not part of the title

**Kept.** `36048bb`. Corpus 71.8% → **72.1%**.

## The population

Counted in the corpus, which is the instrument that judges it. Nineteen failing
cases keep trailing numerals in the title; **eleven of those numerals are a
date**, written four ways:

    2020.A.Late.Talk.Show.2012.16.02.PDTV     title '2020 A Late Talk Show 2012 16 02'
    The Show Series 2015 02 09 WEBRIP s01e13  title 'The Show Series 2015 02 09'
    Judge Developer 2016 02 25 S20E142        title 'Judge Developer 2016 02 25'
    The_Series_US_04.28.2014_hdtv             title 'The Series US 04 28'
    A.Late.Talk.Show.140722.720p              title 'A Late Talk Show 140722'
    Series and Title 20201013 Ep7432          title 'Series and Title 20201013'

A prototype of the rule, run over the corpus **before** any Rust was written,
said it would correct **six** titles and break one. The break turned out to be
Python: `'٢٠٢٤'.isdigit()` is `True`, so an Arabic-Indic date in an Arabic title
matched. `is_ascii_digit` in Rust does not, and the case is untouched. The
glued forms (`140722`, `20201013`) are left alone — a six- or eight-digit run is
also what an absolute episode number looks like, and `cut_at_absolute_episode`
owns that question.

## The convention it depends on

**A date is three number groups, and one of the outer two is a four-digit year.**

Without the year the rule has nothing: `1 2 3` is three groups and not a date,
and the test says so. Without the *third* group it would eat half the movie
library — a title followed by its release year is two groups, which is what
`Blade Runner 2049 2017` and `1883 2019` are, and both are in the sweep.

The small groups are 1–31 and **not** checked for which is month and which is
day: `2012.16.02` is day-before-month and `04.28.2014` is month-before-day.
Guessing would reject one of them.

## The prediction

Corpus +2 passing, 4 more titles corrected without passing (they also fail on
`year`, which is a different mechanism), 0 losses. Sweep, oracle and dogfood
flat.

Measured: **+2 passing, 4 titles corrected, 0 losses**, and all three others
flat. `notes/loop3/scripts/corpus_diff.py` reports the four separately, because a
rate cannot show a fail that becomes a *better* fail.

## The guard that was written, measured, and removed

The first draft carried a fourth guard: **the token before the date must not be
numeric.** It was written for `9-1-1`, a real show whose name offers `1 1 2016`
as a perfectly good date, and which would otherwise become `9`.

The sweep holds **five `9-1-1` names**. So the guard was tested rather than
trusted: a variant with it removed, built as a `git stash create` commit so the
branch never carried it, run through the whole sweep.

    base (48dc403): 74624 parsed
    head (ce54bd2 — guard removed): 74624 parsed
    HEAD right, BASE wrong 0
    BASE right, HEAD wrong 0

**Nothing moved.** The reason is that `9-1-1` has no letter in it, and the
head-must-contain-a-letter rule — the same one `cut_at_title_junk` and
`cut_at_absolute_episode` apply — already declines. The corpus was re-run
without the guard as well: identical, 532 pass, 2 gains, 0 losses.

So the guard has a demonstrable **cost** — it also refuses the correct cut in
`Show 5 2016 02 25` — and no demonstrable case. It was removed, and the reason
is in the code where the next reader will find it.

That `9-1-1` survives is now a test of its own, which is where the protection
belongs.

## What was measured

| instrument | before | after | reaches this change? |
|---|---|---|---|
| parser corpus | 530/738, 71.8% | **532/738, 72.1%**, +2 pass, 0 fail, 4 titles corrected | **yes** |
| parser sweep, 74,624 names | 0/0 | **0 gains, 0 regressions** | **yes — and this time it holds the shape** |
| oracle, 81,094 rows | 64,836 correct | the same 26 rows iteration 2 moved, and no others | no — no generated name carries a date |
| `movie.specials` vs `movie.noyear` | 0 differing | **0 differing** | the guard holds |
| dogfood strict pair, 25,004 files | `ready=24953 unmatched=51` | identical, `errors=0 requests=0` | yes — 25,043 real names, none with a date |
| `cargo test` | 108 core | **111** (3 new) | yes |

**The sweep's zero means something here.** Unlike iteration 4, its population
does contain the shape this rule could damage: 332 names with a numeric run
around a four-digit year, including `1883.2019`, `1899.2019`, `1923.2019`,
`300.2019` and the five `9-1-1`s — titles that *are* numbers, followed by a
year. None moved.

Full suite green except the known-flaky
`hls::tests::mapped_real_library_end_moov_mp4_copy_keeps_aac`, which fails
identically at the base. `fmt` and `clippy -D warnings` green.

## Judgement

**Kept.** The two instruments that can see it moved the right way and did not
move the wrong way; the two that cannot were flat.

## What this could not measure, named

- **The glued date forms.** `140722` and `20201013` are three more corpus cases
  and are left alone: a six- or eight-digit run is also the shape of an absolute
  episode number, and separating the two needs evidence no instrument here has.
- **The year the date carries.** Four of the six corrected titles still fail
  their case, because the corpus wants the *date's* year in the `year` field and
  the episode arm sets `year: None` by construction. That is a different rule.
- **`The_Series_US_04.28.2014`.** The movie arm's year cut runs first and eats
  the `2014`, leaving `04 28` — two groups, not a date. Recovering it belongs to
  the year branch, and the test records the behaviour rather than wishing it
  away.
- **How common date-numbered files are anywhere.** The dogfood library has none
  and the oracle generates none. The corpus says the form exists; nothing here
  says how much of a real library it is.
