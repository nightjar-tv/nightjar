# Iteration 1 — a title that opens with a year does not thereby state one

Base `f527198`. Commit `b62ecb0`. **Kept.**

## The population, counted in the instrument that judges it

`classify.py` puts 7 cases in `read-a-year` (Y1) at `f527198`. Every one fails on
`year` **alone** — no title, no season, no episode — so the row cannot be lost to
the N1 season refusal, and it is the only row on the board with that property.

The seven are five different mechanisms. **Three of them are one:**

    2020.A.Late.Talk.Show.2012.16.02.PDTV.XviD-C4TV    got 2020, want 2012
    2020.A.Late.Talk.Show.2012.13.02.PDTV.XviD-C4TV    got 2020, want 2012
    2020.A.Late.Talk.Show.2011.12.02.PDTV.XviD-C4TV    got 2020, want 2011

The other four are one each — a year at the very end after scene junk, a year
before 1900, a date the episode arm throws away, and a `(1955)` the corpus wants
overruled by an air date. None of them is this rule and none is touched.

**Population: 3.**

## The convention the rule depends on

A daily show's filename ends its title and then states the air date as three
numeric tokens, one of them a four-digit year. `cut_at_date` already reads
exactly that shape, and its own doc comment already names this case — *"the year
cut landed at index 0 because the title starts with a year-shaped number
(`2020 A Late Talk Show`)"*.

**So the parser had already decided.** It cut the title at the date and kept
`2020` as the title's first word. Then `find_year` — which takes the first whole
four-digit run in range wherever it sits — reported that same run as the year.
One name, two reads of it, and they disagreed.

**Without the convention** — a name that puts its release year in front and a
date behind it — the rule takes the date's year. That is the reading the title
cut has already committed to: the cut put the leading run inside the title, and a
year inside the title is not the year the file was released. The change makes the
two agree; it does not add a new opinion.

**Nothing here finds a year the parser could not already see.** It is a
precedence between two years the name carries, which is why the sweep and the
library cannot move on it in either direction.

## Predicted, before running

| instrument | predicted |
|---|---|
| corpus | 598 → **601**, `read-a-year` 7 → 4 |
| `classify.py --diff` | `+3 / -0`, no field gained, exit 0 |
| parser sweep | 0 of 74,624 |
| dogfood probe | 0 of 25,043 |

## Measured

Baselines taken on a tree without the change — the corpus at `f527198`, the
sweep's base arm built from `f527198` by `git archive`, the probe at `f527198`.

| instrument | base | head | movement |
|---|---|---|---|
| corpus | `pass 598 fail 136 n/a 110` — 81.5% | `pass 601 fail 133 n/a 110` — **81.9%** | **+3** |
| `classify.py --diff` | — | `verdict: +3 / -0`, fields gained **none**, `no regression`, **exit 0** | — |
| parser sweep, 74,624 names | `5b0014bc` | `0f5fcb8d` | `HEAD right BASE wrong 0`, `BASE right HEAD wrong 0`, gains `title 0 season 0 episode 0 year 0` |
| dogfood probe, **25,043 database** paths | — | — | **0 rows of 50,086 changed** |

Every prediction held.

### What each zero means

**The sweep's zero: narrow by population, not insensitive by construction.**
Measured over the 74,624 generated names rather than read off `gen_names.py`:

* **24** names open with a four-digit year-shaped run — the library really does
  hold `1923`, and eleven forms are rendered from it.
* **1** name would reach `cut_at_date` at all, and it does not: the head of
  `www.Torrenting.com - 9-1-1.2019.720p.X264-GRP.mkv` is `9` once
  `strip_site_prefix` has run, and the head-has-a-letter guard declines. That is
  the `9-1-1` case the existing test already names.
* **0** names satisfy both halves of the rule.

So the sweep can see `find_year` change on 24 names and saw nothing, which is the
useful half of this zero. It cannot see the date half at all, because no form in
`gen_names.py` emits three numeric tokens.

**The probe's zero: narrow by population.** Over the 25,043 **database**
basenames — not the capture's 25,004:

* **19** open with a four-digit year-shaped run.
* **1** carries a three-token date with a lettered head.
* **0** carry both.

The library is one naming form and this rule needs a form it does not use. The
zero says the change is inert there. It says nothing about whether the rule is
right, and no dogfood count is offered as a reason to keep it.

**`fields fixed inside failing cases: none`** is not a third zero. `--diff`
reports field movement only inside cases that fail in **both** trees; all three
cases moved to `pass`, so they leave that population and are counted in the
verdict line instead.

## Guards, and their controls

Four guards, four controls, each **red on its own deletion** and each failing the
test written for it:

| deleted | test that went red |
|---|---|
| `opening_year(&normalized) == Some(found)` | `the_opening_year_precedence_declines_where_it_should` |
| the fifth-digit refusal in `opening_year` | `an_opening_year_is_a_whole_four_digit_run_in_range` |
| the `1900..=2100` range in `opening_year` | `an_opening_year_is_a_whole_four_digit_run_in_range` |
| `cut_at_date` reporting its year | `a_title_that_opens_with_a_year_takes_the_date_s_year` |

**The width guard has no filename that isolates it.** `20201013 Show 2019 06 05`
reads 2019 whether the refusal is there or not: the eight-digit run and the year
found differ either way, so the precedence declines for the *other* reason and
the control cannot go red. A first draft asserted `Some(2020)` for that name and
was simply wrong about the code. `opening_year` is called directly instead — the
only way each of its guards gets a control that fails alone.

**The input the rule wrongly accepts**, constructed before the guard was written:
`2012 Movie 2009 11 22 BluRay.mkv` — a film whose title opens with a year, and
three tokens behind it that read as a date. The rule takes 2009. That is
arguable, and it is the reading `cut_at_date` has **already** committed the title
to at `f527198`: the title is `2012 Movie` today, with or without this change.
The change makes `year` agree with a cut that was there before it.

`Series Title (1955) - 1954-01-23 …` is recorded as a **decline**, not a pass.
`find_year` prefers a parenthesised year, so the opening run is not the year
found and the guard refuses. That corpus case still fails, on a different
mechanism, and is still in `read-a-year`.

## Gates

    cargo fmt --all --check          0
    cargo clippy --all-targets -D    0
    cargo test --workspace           0

`#[test]` + `#[tokio::test]` attributes, counted on full paths:
**844 → 847**, all three in `core/src/filename.rs` (108 → 111). No test moved and
none was removed.
