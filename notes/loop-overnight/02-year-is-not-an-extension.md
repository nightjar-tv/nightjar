# Iteration 2 — a release name should parse the same with or without a container

Base `f527198`, on top of `b62ecb0`. Commit `15707ac`. **Kept.**

## The population, and how it was found

`read-a-year` was 7 and is now 4. One of the four is
`Der.Movie.German.Bluray.…scene.rules.1998`, which wants the year 1998 and gets
`None` — and `find_bare_year` reads a run exactly like that everywhere else. So
the year was gone before the year rule ran.

It was: **`strip_extension` ate it.** `is_extension` accepted one to four
alphanumeric characters, which describes `1998` as well as it describes `webm`.

    Movie.The.Final.Chapter.2016       title "Movie The Final Chapter"  year None
    Movie.The.Final.Chapter.2016.mkv   title "Movie The Final Chapter"  year 2016

**One release name, two answers, on a suffix that says nothing about the
release.** A dotted scene name with no container is an ordinary form and the
corpus holds several.

### Counted before implementing, in all three instruments

Names whose last dotted suffix is all digits and would currently be stripped:

| instrument | population |
|---|---:|
| corpus (844 inputs) | **15 records, 14 distinct inputs** |
| dogfood library, 25,043 **database** basenames | **0** |
| parser sweep, 74,624 generated names | **0** |

The 15 are not all years. `.100`, `.264`, `.265`, `.525`, `.70`, `.07`, `.1`,
`.3` are in there too, and eight of those cases **pass today** — some of them
because a real token was thrown away. `Series.Title.525` scores its title
correct only because `525` was deleted; the case still fails on season and
episode, and restoring `525` would have added a `title` failure to a case that
already fails. `[DRONE]Series.Title.100` passes outright and would have broken.

**So "an extension is never all digits" was measured and refused.** Predicted at
`+1 / -1` with a field gained — a regression by `--diff`, whatever the verdict
count said. The rule shipped is the narrower true one.

**The rule: a year is not an extension.** Four digits in 1900–2100, the same
range every other year guard in this file uses.

* `.264` is a **real** extension — a raw H.264 elementary stream — and stays one.
* `1080` is a resolution that lost its `p`, and stays one.
* A width test would be a comment, not a guard: no one-, two- or three-digit
  number reaches 1900. It is not written.

**Population after the narrowing: 4 corpus records, 3 distinct inputs** — two
`.1998` and two `.2016`.

## The convention the rule depends on

That a release year is written as four digits in 1900–2100, and that no media
container extension is a number in that range. Without it — a container really
called `.2016` — the rule keeps four characters that should go. No such
extension exists, and neither the 25,043-path library nor the 74,624 generated
names contains one.

## Predicted, before running

| instrument | predicted |
|---|---|
| corpus | 601 → **602** |
| `classify.py --diff` | `+1 / -0`, gained **none**, fixed **`{year: 1}`**, exit 0 |
| parser sweep | 0 of 74,624 |
| dogfood probe | 0 of 25,043 |
| `#[test]` attributes | 847 → 849 |

Per case, predicted:

* `Der.Movie.German.…rules.1998` — fails on `year` alone → **pass**.
* `Der.Movie.James.German.…rules.1998` — two records; the one asserting title
  and year has its `year` fixed and still fails on title. **A field fixed inside
  a failing case, not a verdict.**
* `Movie.The.Final.Chapter.2016` — asserts title only, already passing, **stays
  passing** with a year it did not have.
* `My.Movie.GERMAN.Extended.Cut.2016` — `not_applicable`, unscored either way.

## Measured

| instrument | base | head | movement |
|---|---|---|---|
| corpus | `pass 601 fail 133` (`b62ecb0`) | `pass 602 fail 132 n/a 110` — **82.0%** | **+1** |
| `--diff` `b62ecb0` → `15707ac` | — | `+1 / -0`, gained **none**, fixed **`{'year': 1}`**, `no regression` | exit **0** |
| `--diff` `f527198` → `15707ac` | — | `+4 / -0`, gained **none**, fixed **`{'year': 1}`** | exit **0** |
| parser sweep, 74,624 names | `b62ecb0` | `15707ac` | 0 / 0, gains `title 0 season 0 episode 0 year 0` |
| dogfood probe, 25,043 **database** paths | `f527198` | `15707ac` | **0 rows of 50,086 changed** |

Every prediction held, including the per-case ones.

### What each zero means

**Both zeroes are narrow by population, and both were counted, not assumed.**

* Sweep: **0 of 74,624** generated names end in an all-digit suffix. Every form
  in `gen_names.py` ends `.mkv`, and `ep.noext` ends `.WEB`. The instrument
  cannot reach this rule at all.
* Probe: **0 of 25,043** database basenames end in an all-digit suffix. The
  library is one naming form and every file in it carries a real container.

Neither zero is evidence the rule is right. The corpus is the only instrument
that can see this change, and it is the one that moved.

## Guards and controls

| deleted | test that went red |
|---|---|
| `!is_year_token(suffix)` in `is_extension` | `a_year_is_not_a_file_extension` |
| the `1900..=2100` range in `is_year_token` | `a_number_that_is_not_a_year_is_still_an_extension` |

Each control has a head of its own — `Movie The Final Chapter` and `Der Movie
German` for the first, `[DRONE]Series.Title.100` and `Another Show.264` for the
second — so neither line can be masked by the other guard declining.

## A gate caught something a test did not

`cargo test --workspace` failed at first, and `cargo test -p nightjar-core --lib`
had passed:

    Doc-tests nightjar_core
    test crates/core/src/filename.rs - filename::is_extension (line 2116) ... FAILED
    error: expected one of `!` or `::`, found `.`

An indented block inside a `///` comment is a **doctest**. The surrounding file
writes those blocks inside `//` comments, where they are not. Fenced as `text`.
Worth recording because `--lib` is the faster command and it is green on this.

## Gates

    cargo fmt --all --check          0
    cargo clippy --all-targets -D    0
    cargo test --workspace           0

`#[test]` attributes on full paths: **847 → 849**, both in
`core/src/filename.rs` (111 → 113).
