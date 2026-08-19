# 07 — the year boundary, reverted

**Reverted.** Corpus 430, unchanged. No code from this iteration is in the tree.

## What was tried

`find_bare_year`'s boundary check is digit-only:

```rust
let before_ok = i == 0 || !bytes[i - 1].is_ascii_digit();
let after_ok  = i + 4 == bytes.len() || !bytes[i + 4].is_ascii_digit();
```

So in `1920x1080` the `x` satisfies it and `1920` parses as a year. Sixteen
corpus cases read a year out of a resolution, a season marker (`S2014`) or a
domain (`descargas2020.org`), and **every one of them wants no year at all**.
The change required a non-alphanumeric boundary on both sides.

## What it measured

Corpus **430 -> 430**. Zero newly passing, zero newly failing.

The code did run — this is not the absence-is-not-evidence case. **13 corpus
years changed, all of them from a wrong value to `None`, all of them correct.**
Six titles changed with them.

## Why it is reverted anyway

Flat on the scoreboard, and worse underneath it. Of the six titles that moved,
**two went from right to wrong** and none went from wrong to right:

    World Series of Sonarr - 2010x15 - 2010x16 - HD TV.mkv
      'World Series of Sonarr'  ->  'World Series of Sonarr - 2010x15 - 2010x16 - HD TV'
    Series - 2016x231
      'Series'                  ->  'Series - 2016x231'

The bogus year was acting as an **accidental title terminator**. Taking the
year away takes the terminator away with it, and nothing else cuts there. The
scoreboard did not move because those cases were already failing on season and
episode — the title regression hides inside a flat number.

## The ordering this implies, which is the actual finding

The year fix is correct and it cannot land first. The six cases it disturbs are
the four-digit season form — `2009x09`, `2016x231`, `S1936E18`, `S2009E09` —
where `find_season_episode` refuses a season of more than two digits. That
refusal is itself deliberate: it is what stops `1080x1920` parsing as season 80
episode 192, and there is a test saying so.

So the order is: **teach `find_season_episode` the four-digit season form
first, then fix the year boundary.** With a real terminator at `2016x231` the
year fix costs nothing and the year field gets thirteen corrections for free.

Doing the year fix alone trades two working titles for thirteen year values the
corpus does not score. Doing it second trades nothing.
