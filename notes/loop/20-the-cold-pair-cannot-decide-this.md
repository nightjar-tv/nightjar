# 20 — a cold strict pair cannot decide a difference this small

Written during the fourth read, running the pair at HEAD for the first time.
Iterations 17 and 18 ran it on the N150 at `75ca8ef`, before the F1 fix, so the
branch reached this point with the pair never run against the tree being merged.

## The finding

**`pair_compare_loop.py` reports `FAIL — a working binding moved` when a binary
is compared against itself.**

Measured here. The same treatment binary, the same capture, the same cache, run
twice:

    BROKEN  (ready -> not ready): 14
    GAINED  (not ready -> ready): 18
    REBOUND (ready both, different key): 0
    VERDICT: FAIL — a working binding moved

Every one of the 32 is a Red Dwarf episode moving `ready` <-> `matched`.

## Why

A cold cache stalls one folder. The strict run aborts that folder's enrichment
at its first miss, and **which of its seasons finish before the abort varies
between runs**. The folder's items land on `ready` or `matched` accordingly.

So under a cold cache the instrument has a **noise floor of about ±20 items**,
all inside the stalled folder, and `BROKEN`/`GAINED` count that churn as
movement. The verdict line reads the churn as a regression because it cannot
tell a stalled item from a broken one — `ready -> matched` and
`ready -> unmatched` are both "not ready".

Warm, the same comparison is clean: `errors=0`, no stall, no churn, and the
arms are identical.

## What that cost, here

The cold pair at HEAD showed the treatment differing from the pre-fix branch by
two items. That is well inside the noise floor, so it says nothing either way —
and it would have been easy to read as "F1 moved two items" or as "F1 moved
nothing", which are the two readings the number cannot separate.

The identity control is what separated them, and it is one extra run.

## The rule

**`BROKEN`, `GAINED` and the verdict line mean nothing unless `errors=0` on both
arms.** Read `errors` first. When it is not zero, the run has told you the cache
is cold and nothing else; warm it and run again rather than reading the
comparison.

**And when a pair does show a difference, run the identity control before
attributing it** — the same binary twice, through the same script. A harness
that cannot reproduce itself cannot attribute a difference to a change. This is
the same shape as iteration 19's amendment 2 and as the sweep comparing one
field of four: the instrument looked like it was answering the question asked.

## The warm result this branch was measured on

Cache warmed 8,182 -> 8,185, three requests, the cost iteration 18 recorded.
Both arms then strict and offline:

    control (main)        DONE groups=3221 ready=24953 unmatched=51 pending=0 errors=0 requests=0
    pre-F1  (ace69bf)     DONE groups=3220 ready=24953 unmatched=51 pending=0 errors=0 requests=0

`replay_add_year.py` runs **before** `replay_add_reparse.py`. Reparse's second
substitution matches text that add_year creates, so the other order fails on a
tree that has never carried the year column.
