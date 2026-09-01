# Iteration 4 — a run of leading groups is stripped down to the prose

Base `f527198`, on top of `15707ac`. Commit `82e797a`. **Kept.**

## The population

`drop-a-leading-group` (D4) is 2 cases, and they are one mechanism:

    [Jumonji-Giri]_[F-B]_Series_Title_Ep04_(0b0e2c10).mkv   got "[F-B] Series Title"
    [Jumonji-Giri]_[F-B]_Series_Title_Ep08_(8246e542).mkv   want "Series Title"

`strip_leading_group` stripped **one** group. Fansub releases stack them.

**Population: 2**, both title-only failures, neither blocked by anything.

A third name changes and is `not_applicable`, so it is unscored either way:
`[scnzbefnet][509103] 2.Developers.Series.S03E18…` goes from
`[509103] 2 Developers Series` to `2 Developers Series`.

## The convention, and where a naive repeat goes wrong

The convention is that a leading `[...]` is a tag and the title is the prose
behind it. **A run of groups with no prose is not that name.** It is the anime
bracket-run form, whose title `bracket_run_title` chooses from the groups, and
whose stem every other rule in the file reads.

Counted before implementing — **an unguarded loop moves 4,664 of the sweep's
74,624 names**:

    [GRP][12 Angry Men][07][1080p][AVC][GB].mkv   stem becomes  [GB]
    [GRP] [1923] [07] [1080p] [AVC].mkv           stem becomes  [AVC]

**So the repeat asks for prose**: it continues only while a letter survives
**outside** every bracket. With that guard, the same count over the same three
populations:

| instrument | names with two leading groups | changed by the guarded rule |
|---|---:|---:|
| corpus, 844 inputs | — | **3** |
| dogfood, 25,043 **database** basenames | **0** | **0** |
| parser sweep, 74,624 names | **4,664** | **0** |

## Predicted, before running

corpus 602 → **604**; `--diff` `+2 / -0`, gained none, fixed none, exit 0;
sweep **0**; probe **0**; `#[test]` 849 → 851.

## Measured

| instrument | base | head | movement |
|---|---|---|---|
| corpus | `pass 602 fail 132` (`15707ac`) | `pass 604 fail 130 n/a 110` — **82.3%** | **+2** |
| `--diff` `15707ac` → `82e797a` | — | `+2 / -0`, gained **none**, fixed **none**, `no regression` | exit **0** |
| parser sweep | `15707ac` | `82e797a` | 0 / 0, gains `title 0 season 0 episode 0 year 0` |
| dogfood probe, 25,043 database paths | `f527198` | `82e797a` | **0 rows of 50,086 changed** |

Every prediction held.

### What each zero means

**The sweep's zero is the good kind: genuinely sensitive, and zero.** 13,992 of
its 74,624 names begin with a group and **4,664 carry two**. The unguarded rule
moves every one of those 4,664. The shipped rule moves none. This instrument was
able to fail and did not.

**The probe's zero is insensitive by construction.** **0 of the 25,043 database
basenames begin with a bracket group at all** — the library is one renamer's
output and that renamer writes none. The probe could not have reported anything
else, and it is recorded that way rather than as evidence.

## Guards and controls

| deleted | test that went red | how it failed |
|---|---|---|
| `has_letter_outside_brackets` from the loop | `a_name_that_is_only_groups_keeps_them` | `[GRP][Sub][Anime Title][2019][234][AVC][GB][1080P]` lost its **year**: `Some(2019)` → `None` |
| the repeat itself | `a_run_of_leading_groups_is_stripped_down_to_the_prose` | `"[F-B] Series Title"` instead of `"Series Title"` |

**The control was found by measurement, not by guessing.** The obvious candidate
— `[GRP][12 Angry Men][07][1080p][AVC][GB]` — parses **identically** with and
without the guard, because `bracket_run_title` reads the whole name and rescues
the title. A control written on it would have been green in both trees. Two
inputs do separate, and both are asserted: the Latin run loses its **year**, and
the CJK run loses its **title** to the literal `[MP4]`.

`has_letter_outside_brackets` walks `chars()`, not bytes, and slices nothing.
`【` is three bytes and this rule reads CJK names.

## Gates

    cargo fmt --all --check          0
    cargo clippy --all-targets -D    0
    cargo test --workspace           0

`#[test]` attributes on full paths: **849 → 851**, both in
`core/src/filename.rs` (113 → 115).
