# Iteration 6 — warming: everything offline is ready, the fetch needs a key

**Blocked on one thing: a TMDB API key.** Locating one was refused by the sandbox
twice in iteration zero, and I did not go looking for the user's credentials
another way. Everything that does not need the network is done and validated
below, so the fetch is one command once a key is supplied.

## Why the plan is bigger than the brief says

Re-derived against the current tree and the corrected harness, not taken from the
brief — the tree, the harness and the stall set have all moved since.

    distinct calls the runs asked for and the cache does not hold: 5333

| cause | calls | groups blocked |
|---|---:|---:|
| search: a query with no show title (M1/M2) | **4,876** | 4,877 |
| collision shape: a candidate with no reference season | 235 | 1,360 |
| collision shape: a candidate's season, for the collision pin | 222 | 2,502 |
| **first tranche — everything except M1/M2** | **457** | **3,862** |

457 matches the brief's figure. The 4,876 is the M1/M2 tranche and it is the
**only route to M2**: `tv.handmade` is 5,644 rows, 100% stalled, and every one of
them stalls on a search for a query like `01 Closure` that the dogfood drain never
made. Nothing else in the suite generates that shape.

Stalled items by shape, for scale: `tv.handmade` 5,644, `tv.twoseason` 2,716,
`tv.noyear` 2,118, `tv.numbered` 2,050, `tv.sonarr.plain` 2,050, `tv.scene` 1,812.

## The method: run the oracle non-strict, do not write a fetcher

The patched `tmdb/mod.rs` already warms. With `NIGHTJAR_TMDB_CACHE` set and
`NIGHTJAR_TMDB_CACHE_STRICT` **unset**, a miss falls through to a live call and
`measure_cache_write` stores the body under `measure_cache_key` — the same key the
strict read uses. A 404 is stored as the literal bytes `__404__`.

So warming is the oracle run non-strict. **A hand-written fetcher would
reimplement the key function and every query shape**, and a reimplemented
predicate has misreported this project before — 25 folders against the shipped
chain's 12. Running the shipped call sites makes the keys correct by construction.

It also solves the lower-bound problem `warm_list.py` warns about — *"answering
these can raise calls that nothing has asked for yet."* A candidate that becomes
reachable asks for its own seasons, and that call is in nobody's plan. So
`warm_cache.sh` **iterates until a whole round makes zero requests** and prints the
count per round, so convergence is visible rather than assumed.

## The cache is copied, not written

The loop's hard limits forbid writing to the cache, and the 8,185-entry cache is a
shared instrument other spikes read. So warming targets a copy at
`~/nightjar-wt-matcher-scratch/tmdb-cache-warm`. That is strictly better than
warming in place: revertible, and it cannot corrupt anything for another reader.

Validated offline:

| check | result |
|---|---|
| copy vs base | **byte-identical**, 8,185 entries each |
| key transcription re-derived against entries on disk | **400 of 400 found**, 0 missing |
| `tv.root-b0` strict, shared cache vs copy | `groups=1139 ready=3547 unmatched=457 pending=1600 errors=190 requests=0` — **identical on every counter** |

The third check is the one that matters: the copy is a faithful substitute, so a
warmed copy differs from the shared cache only by what warming added.

## The key never lands anywhere durable

`warm_cache.sh` requires `TMDB_API_KEY` in the environment of that one command. It
writes it only to each run's own `data/secrets` under the scratch tree, `chmod
600`, which is the file the resolver reads. It is never echoed, and the client
already scrubs it out of error strings (`scrub_tmdb_url_secret`). Nothing is
committed.

## Run it

    # first tranche only — 457 calls, 3,862 groups, no M1/M2
    TMDB_API_KEY=… notes/loop-matcher/scripts/warm_cache.sh \
      ~/Documents/GitHub/nightjar-wt-matcher-oracle \
      ~/nightjar-wt-matcher-scratch/tmdb-cache-warm

    # everything, including the 4,876 M1/M2 searches that make M2 measurable
    SHAPES= TMDB_API_KEY=… …same…

`SHAPES` restricts to a space-separated shape list; default is everything.

## What warming will and will not show

**Will:** `tv.handmade` gets a number for the first time, and M2 stops being a
stall wearing a wrong-bind's clothes. Expect it to read **worse** than
"unmatched", because `01 Closure` reaching a live provider is exactly the
wrong-bind risk the brief names. Also: 3,862 groups across the collision shapes
get verdicts instead of stalls, which is where `movie.noyear`'s 58.8% and
`tv.scene`'s 89.5% are least trustworthy today.

**Will not:** make any binding better. Warming moves rows out of `stalled` into
`correct`, `wrong` or `absent`, and some of the current rates will **fall** when
the rows they were hiding get scored. A rate that drops after warming is the
instrument improving, not the product regressing — the same shape as note 05 in
reverse, and it should be reported in those words.
