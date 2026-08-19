# 13 — the season and the episode marker may be separated

**Kept.** Corpus 472 -> 488 of 738 all-fields (64.0% -> 66.1%). Structure-only
596 -> 612 (80.8% -> 82.9%). Zero corpus cases regressed. **Zero dogfood items
change.**

## Mechanism

`find_season_episode` required the `e` to touch the season digits, so
`Series Title.S6.E1`, `Series.Title.S01.Ep06` and `Series s90 e43` reported
**no episode at all** — not a wrong episode, none.

One optional separator before the `e`, an optional `p` for the `Ep` spelling,
and one optional separator before the digits.

A left boundary on the `s` was measured and adds nothing — the same 16 cases
either way — so the change stays minimal and does not add one.

## Prediction vs actual

Predicted **+13 to +15**, structure ~+14, 0 dogfood items.
Actual **+16**, structure **+16**, **0** dogfood items.

The extra case is `Series Title.S6.E1-S6E2`, which my classifier excluded: its
`already parses` filter matched `S6E2` on the right-hand side of the token and
so ruled the case out. The rule reaches it because the *left* half is what was
failing.

One case was predicted to keep failing and did: `Series Title.S6.E1E3` wants
`[1, 2, 3]`, and an unseparated repetition must land on the next number, so
`E1E3` yields `[1]`. That is iteration 12's guard and it is right — `E1E3` is
not a range.

## Two candidates rejected before this one

### A separator run between episode tokens — rejected

`S07E22 - S07E23` is one corpus case. The library refutes it: **58** dogfood
basenames have ` - N` after the episode token where N is a plausible range end,
and the ones that would break are bound today.

    9-1-1 - 2x02 - 7.1 - WEBDL-1080p.mkv               2 -> [2,3,4,5,6,7]
    American Horror Story - 7x04 - 11+9 - WEBRip-1080p 4 -> [4..11]
    Below Deck - 7x09 - 12 Seconds in Heaven           9 -> [9..12]
    Countdown (2025) - 1x09 - 10-33 - WEBDL-1080p      9 -> [9,10]

Iteration 12's marker guard is exactly what stops these, and a separator run
containing a dash would take it off. One case is not worth four bindings.

### An empty title when the token starts the name — blocked, not rejected

**28 corpus cases** want `""` and get the whole stem — `S03E09 WS PDTV XviD
FUtV`, `1x04`, `5x09 - 100 [720p WEB-DL]`. It is the largest single class left
and the parser change is one line.

It is **blocked on the caller**, and the block was verified by reading it:

- `scanner/src/lib.rs:233` and `:688` store `parsed.title` verbatim. There is
  no folder fallback, so the empty string reaches the database.
- `TmdbClient::search` builds `("query", title)` with no empty check. The only
  empty-title guard in the crate is inside a **test double**, not in
  `drain_pending`.

An empty title would therefore become an empty provider query. That is a
matcher-level defect the parse-level corpus cannot see, and the replay harness
that could see it is not on this machine. Doing the parser half alone would
raise the corpus by 28 and introduce a defect nothing here could measure — the
exact trade this loop exists to refuse.

**For a human:** the slice is parser + scanner folder fallback + queue guard,
measured on the replay pair.
