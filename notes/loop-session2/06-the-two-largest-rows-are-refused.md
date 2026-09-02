# Iteration 6 — the two largest rows, priced and refused

**Base: iteration 5 (`148c898`). No code. Corpus unchanged at `622 / 734`.**

Two mechanisms were taken to measurement and both came back refused by real
bound titles. Neither was implemented, because the counterexample search comes
before the code and it answered on its own.

## Refused: `#NN` is an episode number

The board's note on `drop-a-trailing-number` says the bare-number refusal does
not cover a hash-marked `#NN`, and that is true — it is a different rule. **It
is refused on its own evidence.**

**The gain.** Two corpus cases, and only one of them can reach a pass:

    [Shark-Raws] Series Title #957 (NBN 1280x720 x264 AAC).mp4   title only
    221205 ABC123 17研究所！ #17.ts                                also wants season 1 — N1 refuses it

**The population.**

| population | `#` followed by digits |
|---|---:|
| corpus inputs (844) | 2 |
| corpus title expectations | 0 |
| dogfood `db_title` (2,475) | **1** |
| dogfood basenames (25,043) | **12** |

**The title that refuses it is `Juror #2`** — a bound film in the library, which
a rule cutting at `#NN` turns into `Juror`. Eleven more basenames carry `#N`
inside an episode title: `Dealbreakers Talk Show #0001`, `Viewer Mail #2`,
`Family Guy Viewer Mail #1`, `Red Dye #40`, `Episode #2.1`.

**One corpus case against one broken bound title and eleven broken episode
titles.** That is the same trade the board already refused for the bare form,
in a rarer spelling.

## Refused: splitting a bare digit run into a season and an episode

`split-one-run` is 9 cases and the classifier marks 4 of them blocked on a
codec vocabulary. **The blocked column is a claim about the cases in the row,
so it was re-derived rather than carried** — and the row turns out to be
refused for a reason that has nothing to do with codecs.

**The rule priced.** A *whole* run of exactly three or four digits, not a year,
with a head that carries letters: the last two digits are the episode and the
rest is the season, exactly as `Cap.101` already splits. The whole-run test
disposes of the codec objection on its own — `x264` and `h264` have a letter
against the digits, so `264` is not a whole run, and all four blocked cases
carry the glued spelling.

**The population, and what it costs.**

| population | names with a whole non-year 3–4 digit run |
|---|---:|
| dogfood `db_title` (2,475) | **8** |
| dogfood basenames (25,043) | **122** |
| sweep names (74,624) | **256** |

Six of the eight are saved by the head-must-carry-letters guard, because the
number is the whole title: `1883`, `1899`, `300`, `300 Rise of an Empire`,
`3000 Miles to Graceland`, `500 Days of Summer`.

**Two are not, and they are the refusal:**

    Crime 101        →  Crime,     season 1  episode 1
    Prisoner 951     →  Prisoner,  season 9  episode 51

Both are bound titles in the dogfood library. **Up to five corpus cases against
two bound titles broken**, and the sweep renders those two titles in every form
it knows.

That is the same trade as `drop a trailing number` — 8 corpus gains against 420
broken bound titles — and the same trade as `german`, refused by
`The Good German`, and `complete`, refused by `A Complete Unknown`. **A word or
a shape that appears in a real title is refused, and the refusal is recorded
with the title.**

## The board, fully re-derived at `148c898`

112 failures, 112 classified, 16 classes. Every row now has a reason:

| class | cases | reachable | why |
|---|---:|---:|---|
| drop-a-trailing-number | 22 | 0 | refused; `#NN` refused above by `Juror #2` |
| expand-a-range | 15 | 0 | note 04 names all fifteen |
| drop-a-trailing-word | 14 | 0 | `german`, `v2`, `Part N` refused; the rest want N1 |
| slash-inside-the-name | 9 | 0 | a `/` cannot be in a filename |
| split-one-run | 9 | 0 | **refused above** by `Crime 101` and `Prisoner 951` |
| strip-CJK-decoration | 8 | 0 | refuted inside its own row — two cases want the CJK kept |
| keep-a-year | 6 | 0 | four need two fixes; the rest want a year inside the show name |
| keep-a-season-marker | 6 | 0 | one product decision, with `drop-a-trailing-season-marker` |
| a-path-form-the-product-refuses | 6 | 0 | outside the input contract |
| a-bare-marker-wants-a-season | 5 | 0 | N1, refused by decision |
| drop-a-trailing-season-marker | 4 | 0 | the same product decision |
| read-a-year | 3 | 1 | the year floor is 1900 and `Movie Name (1897)` is older |
| pick-a-title-before-a-slash | 2 | 0 | a `/` cannot be in a filename |
| keep-a-subtitle | 1 | 1 | a spaced dash-number cut eats `- 100 Years Quest` |
| strip-decoration-both-ends | 1 | 1 | an unmatched `(` and a quoted repeat of the name |
| no-title-in-the-name | 1 | 1 | `11-02 …` — and `9-1-1` is a real show |

**Four reachable cases remain, in four different mechanisms, and every one is a
single case with a contested convention.** That is the finding, and it is why
this session stops here rather than spending iterations at one case each on
rules the evidence cannot support.
