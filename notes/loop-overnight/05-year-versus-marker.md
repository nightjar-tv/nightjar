# Iteration 5 — a year does not outrank an episode marker behind it

Base `f527198`, on top of `82e797a`. Commit `9196cf3`. **Kept — after `--diff`
caught a regression the verdict count had hidden.**

## The population

`keep-a-year` (K2) is 8. Three of the eight are single-fix, and **two are one
mechanism** — a name that states a four-digit run and then marks an episode:

    Series Title 2018 EP06 720p x265 AOZ.mp4
       want title "Series Title 2018"; got "Series Title"
    Series Show.2016.E04.Power.720p.WEB-DL.DD5.1.H.264-MARS
       want title "Series Show 2016" and episode 4; got "Series Show" and None

The year cut removes everything behind the year, and the marker is behind it.
`Series Show.2016.E04.Power…` came out as a **2016 film** called `Series Show`,
with no episode, on a name that says episode 4 as plainly as a name can.

The other six are other mechanisms: `Movie.Klasse.von.1999.1990` is two adjacent
years, three are N2 digit runs, one needs `Part 1`, one needs a bare `06` that
nothing marks. **Population: 2.**

Counted first over all three instruments — names carrying a year followed by a
bare episode marker:

| instrument | candidates |
|---|---:|
| corpus, 844 inputs | 7 |
| dogfood, 25,043 **database** basenames | 94 |
| parser sweep, 74,624 names | 5 |

Of the corpus's 7, three carry `S..E..` and go through the episode arm, which
this does not touch; one is the span case below. All 94 library names are
`Alice in Borderland (2020) - 1x01 - Episode 1 - WEBDL-1080p.mkv` — they carry
`1x01`, so the episode arm owns them too. All 5 sweep names come from the title
`1923`; three go through the episode arm and two — `1923 Ep01 1080p x264.mkv`,
`1923 Episode 5 1080p.mkv` — decline because the marker's head, `1923`, carries
no letter.

## The convention

That the run in front of an episode marker is the series' name. `Series Title
2018` is a show called that; `Series Show 2016` is another. A name that marks an
episode is **not a film and has no film year**, which is exactly what the episode
arm already does — it sets `year: None` for every name it owns.

**Without the convention** — a film whose name states its year and then something
that reads as a marker — the rule turns a film into an episode. That is not
hypothetical, and it is the regression below.

## Predicted, and what actually happened

Predicted: corpus 604 → **606**, `--diff` `+2 / -0` clean, sweep 0, probe 0.

**The first measurement was `+2 / -1`**, and `classify.py --diff` exited 1:

    verdict: +2 / -1
    1 case(s) got worse:
      PASS -> FAIL  ['title'] | The.Movie.from.U.N.C.L.E.2015.1080p.BluRay.x264-SPARKS

    REGRESSION: 1 case(s) worse

**The corpus verdict count rose, 604 to 605, while a passing case went red.** A
run that reported only the rate would have called this a gain. This is the third
slice `--diff` has caught, and the reason it is a required gate.

`U.N.C.L.E.` ends in a standalone `E` at a separator, and the digits behind it
are `2015`. Uncut, the name reads as **episode 2015** titled `The Movie from U N
C L`, kind `Episode`. **The year cut was holding that line** — it removed `2015`
before the marker could reach it.

**The guard: a retry may not claim the year itself.** It is not a second claim;
it is the same four digits read twice. Added, and the measurement re-run.

## Measured, with the guard

| instrument | base | head | movement |
|---|---|---|---|
| corpus | `pass 604 fail 130` (`82e797a`) | `pass 606 fail 128 n/a 110` — **82.6%** | **+2** |
| `--diff` `82e797a` → `9196cf3` | — | `+2 / -0`, gained **none**, fixed **none** | exit **0** |
| `--diff` `f527198` → `9196cf3` | — | `+8 / -0`, gained **none**, fixed **`{'year': 1}`** | exit **0** |
| parser sweep, 74,624 names | `82e797a` | `9196cf3` | 0 / 0, gains `title 0 season 0 episode 0 year 0` |
| dogfood probe, 25,043 database paths | `f527198` | `9196cf3` | **0 rows of 50,086 changed** |

### What each zero means

**Both are narrow by population, and both had real candidates.**

* Sweep: 5 names carry a year and a marker. Three go to the episode arm; two
  decline on the head-has-a-letter guard. The instrument had candidates and they
  all declined for stated reasons — this is not a rule it cannot see. It reads
  `kind`, `year`, `season`, `episode` and `title`, and this change moves all
  five.
* Probe: 94 candidates, every one of them `Alice in Borderland (2020) - 1x01 -
  Episode N`. They carry a season/episode token, so `find_season_episode` claims
  them and the movie arm — the only arm changed — never sees them. The probe
  reads `kind` and `year`, which is what this change moves, so a movement here
  would have shown.

## Guards and controls

| deleted | test that went red |
|---|---|
| `cut.1.is_none()` | `the_year_is_only_given_up_for_a_claim_that_is_better` — `Show Ep06 2018` loses its year |
| `retry.1.is_some()` | same test — and **39 tests in all**; the guard is heavily load-bearing |
| `retry.1 != year` | `a_marker_may_not_claim_the_year_as_its_episode` |
| the retry itself | `a_year_does_not_outrank_an_episode_marker_behind_it` |

Four heads, all different: `Show`, `Some Anime Show`, `The Movie from U N C L E`,
`Series Title 2018`.

## A test that changed what it records

`the_episode_marker_does_not_eat_codec_tokens_or_words` asserted `Anon Show` for
`Anon Show 2018 EP06 720p x265 GROUP.mp4`, and said in its own comment:

> the year branch cuts at `2018` long before the marker is looked at … it needs
> the year branch, not this one.

It does, and the year branch now asks the marker before it cuts. **The assertion
was updated, not removed** — it records the fix where it recorded the gap, and
two lines were added beside it for the episode and the year. `#[test]` attributes
counted on full paths: **851 → 854**, and the diff is `core/src/filename.rs`
115 → 118 — three added, one of them the guard's own control. Nothing moved out
of any other file.

## Gates

    cargo fmt --all --check          0
    cargo clippy --all-targets -D    0
    cargo test --workspace           0
