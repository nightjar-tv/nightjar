# The `wrong.kind` warming run — prepared, not run

Warming is the one thing in this loop that touches the network, and it is a
human-run step. This note is what a human needs to run it, and what it costs.

**Nothing here is committed to the branch as code.** The candidate rule is a
patch, deliberately: shipping a rule the instrument cannot judge is what the
previous attempt did, and the warming is what makes it judgeable.

## Why the rule has to exist before the cache can be warmed

`warm_cache.sh` warms by running the drain **non-strict**, so it issues exactly
the calls the shipped matcher wants, through the shipped call sites, stored under
keys that are correct by construction. Its own comment says why a bespoke fetcher
was rejected: it would reimplement the key function and the query shapes, and a
reimplemented predicate has misreported this project before.

That means the warm produces the queries of whatever tree it runs against. A base
tree asks for `search/movie?query=Closure`, which is already cached. Only a tree
carrying the rule asks for `search/tv?query=Closure`, which is not.

So the candidate is prepared, built and drained — and **not shipped**.

## The trap that would have eaten the whole attempt

`replay.rs` re-derives every field the scanner interprets. It has been caught
doing that differently from production twice, both times on `title`. **`kind` is
the next one.**

The rule lives in `nightjar-scanner::stored_kind`, beside `stored_title`. The
harness re-derives kind straight from `parse_filename`. Without a matching change
to `replay.rs`, **the oracle would show zero movement for a change that moved
everything**, and the honest reading of that zero is "the code never ran".

Both halves are prepared:

| half | where |
|---|---|
| the product candidate | `notes/loop3/scripts/wrong-kind-candidate.patch` (committed) |
| the harness, including `stored_kind` | `~/nightjar-wt-loop3-scratch/oracle-harness-with-kind.patch` |

The product half is `nightjar-db` gaining `is_numbered_season_directory` and
`under_numbered_season_directory` — beside `is_season_directory`, sharing its
walk, so the two cannot disagree — and `nightjar-scanner` gaining `stored_kind`,
called from both indexing paths. **`Specials/` is not numbered**, which is the
whole distinction: TMDB models `Top Gear: Polar Special` as a movie record.

`is_numbered_season_directory` now has a caller, which is what Rule 4.7 was
waiting for. It did not have one when this loop refused to ship it alone.

## What the candidate does, measured strict before asking for any request

Drained against the warmed cache, `requests=0`, candidate tree:

    tv.episodetitle-b0   groups=697  ready=0  unmatched=58  pending=5545  errors=685
    tv.episodetitle-b1   groups=5    ready=0  unmatched=0   pending=40    errors=5
    movie.specials-b0    groups=2689 ready=1005 unmatched=679 errors=0
    movie.noyear-b0      groups=2689 ready=1005 unmatched=679 errors=0
    tv.sonarr-b0         groups=1386 ready=5604 unmatched=0   errors=0

Three things, and the middle one is the point of the whole iteration-3 shape:

1. **The 573 `wrong.kind` are gone**, replaced by stalls. The rule reaches the
   oracle — which is the check that would have failed silently without the
   harness half.
2. **`movie.specials` is byte-identical to `movie.noyear`, still.** 1,005 ready
   and 679 unmatched on both, batch for batch. The candidate does not touch a
   file under `Specials/` whose right answer is a film. That is the failure mode
   that destroyed five real bindings last time, and it now has a 1,712-row guard
   saying it did not happen.
3. **`tv.sonarr` is untouched**, 5,604 ready — the ordinary layout pays nothing.

The stalls are the bill. It was estimated at 4,691 and **measured at 1,758** —
1,154 for `tv.episodetitle` and 604 for `tv.handmade`. The estimate counted one
query per file where the drain searches once per group, and it missed
`tv.handmade` entirely. See note 03.

## The command

**Warm a copy. Never the shared cache, and never `tmdb-cache-warm` in place.**
Every measurement in `notes/loop3/` is pinned to that cache at exactly 20,350
entries; warming it in place makes each of those tables unreproducible, which is
the same "the population must not move" rule `measure_warmed.sh` enforces on
entities.

    # 1. the candidate tree, product half plus harness half
    cd ~/Documents/GitHub/nightjar
    git worktree add --detach ~/nightjar-wt-kind loop/matcher-residual
    cd ~/nightjar-wt-kind
    cp -r ~/Documents/GitHub/nightjar/web/build web/build
    git apply ~/Documents/GitHub/nightjar-wt-loop3/notes/loop3/scripts/wrong-kind-candidate.patch
    git apply ~/nightjar-wt-loop3-scratch/oracle-harness-with-kind.patch
    (cd server && unset CARGO_TARGET_DIR && cargo build --release -q -p nightjar-api --bin replay)

    # 2. a copy of the cache, so every earlier table still reproduces
    cp -r ~/nightjar-wt-matcher-scratch/tmdb-cache-warm ~/nightjar-wt-matcher-scratch/tmdb-cache-kind

    # 3. the only networked step. 1,758 searches over TWO shapes — the rule
    #    reaches every file under a numbered season directory that parses as a
    #    movie, which is tv.handmade as well as tv.episodetitle.
    SHAPES="tv.episodetitle tv.handmade" TMDB_SECRETS_FILE=~/nightjar-data-v9/secrets \
      ~/Documents/GitHub/nightjar-meta/notes/loop-matcher/scripts/warm_cache.sh \
        ~/nightjar-wt-kind ~/nightjar-wt-matcher-scratch/tmdb-cache-kind

    # 4. the proof it finished: requests=0 strict, on the same 81,094 rows
    EXPECT_ROWS=81094 \
      ~/Documents/GitHub/nightjar-meta/notes/loop-matcher/scripts/measure_warmed.sh \
        ~/nightjar-wt-kind ~/nightjar-wt-matcher-scratch/tmdb-cache-kind kind1

**Free space first.** Step 4 writes ~4.2 GB into `out/run` and `out/run-b`. The
volume hit 100% during this loop and produced a `transcode` failure count that
looked like a regression and was the disk.

`warm_cache.sh` aborts on its own if requests are attempted and nothing is
written — `requests=N` counts attempts, not successes, and every request timing
out looks identical to a converged round if only that number is believed.

## What to read when it is done

The base to compare against is `scored-l3base3` / `scored-l3it5`, both on the
81,094-row population — but **on the old cache**, so they do not join a run
against the warmed copy. Re-run the base and the branch tip against
`tmdb-cache-kind` first; three arms on one cache, or the comparison is two
observations rather than one comparison. ADR-0047's amendment records what
happens when arms stall differently: the same measurement read 11-to-1 one way
and 1.1-to-1 the other.

Then the numbers that decide it:

- **`tv.episodetitle`** — 573 `wrong.kind` should become `absent` or `correct`,
  and `stalled` should be 0. Anything still stalled is unwarmed, not measured.
- **`movie.specials` against `movie.noyear`** — `scripts/paired_shapes.py` must
  still report **0 differing**. That is the guard, and it is the one number that
  would have stopped the previous attempt.
- **The dogfood strict pair** — the five `Specials/` files it caught last time.
  A regression there is real however the oracle reads.
