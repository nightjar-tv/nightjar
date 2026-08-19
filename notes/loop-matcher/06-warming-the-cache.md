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

---

## The trap in measuring afterwards

**Do not re-measure with `./run.sh`.** `run.sh all` re-runs `inventory.py` and
`pick_entities.py`, and those pick entities *from the cache* — an entity is kept
only when the cache can serve it offline. A warmed cache serves more, so a full
`run.sh` after warming **silently enlarges the entity set**, and every before/after
in notes 00–05 is then computed over a different population. The rows would not
join, and the difference would read as a result.

`measure_warmed.sh` re-drains the libraries already in `out/lib` — which encode
the 2,410-entity population — against the warmed cache, and scores those. It
**aborts unless the population is still 2,410 entities and 67,982 rows**, and
aborts if the cache count changes during the run, because that arm would not have
been offline.

Verified: the guard reads 2,410 and 67,982 off the current tree.

## The tranche split is exactly one shape

Checked against `warm-list.tsv` rather than assumed:

| tranche | calls | shapes asking them |
|---|---:|---|
| **1 — candidate details and seasons** | **457** | the other 16 shapes |
| **2 — titleless searches** | **4,876** | **`tv.handmade`, alone** |

`tv.numbered` no longer contributes titleless searches: after the harness fix it
asks the same real-title queries as `tv.sonarr.plain`, and those are already
cached. So tranche 1 is `SHAPES` set to the 16 non-`tv.handmade` shapes, and
tranche 2 is `SHAPES="tv.handmade"`. One shape, cleanly separable, which keeps the
attribution between the two measurements clean.

## The four commands, in order

    W=~/Documents/GitHub/nightjar-wt-matcher
    OR=~/Documents/GitHub/nightjar-wt-matcher-oracle
    C=~/nightjar-wt-matcher-scratch/tmdb-cache-warm

    # 1. tranche 1 — 457 calls, 3,862 groups
    SHAPES="movie.noyear movie.scene movie.sonarr movie.yearfile movie.yearfolder \
    tv.flat tv.flat.titled tv.noyear tv.numbered tv.partial tv.root tv.scene \
    tv.single tv.sonarr tv.sonarr.plain tv.twoseason" \
    TMDB_API_KEY=… $W/notes/loop-matcher/scripts/warm_cache.sh $OR $C

    # 2. measure
    $W/notes/loop-matcher/scripts/measure_warmed.sh $OR $C warm1

    # 3. tranche 2 — the 4,876 searches that make M2 measurable
    SHAPES="tv.handmade" TMDB_API_KEY=… $W/notes/loop-matcher/scripts/warm_cache.sh $OR $C

    # 4. measure
    $W/notes/loop-matcher/scripts/measure_warmed.sh $OR $C warm2

The key must be a shell substitution, not a literal — the text of a `!` command
lands in the transcript.

---

## Attempt 1 — the key works, the sandbox does not

The key was found at `~/nightjar-data-v9/secrets` (32 characters, TMDB v3 shape,
not the replay placeholder) and the script accepted it. **The warm still could not
run, for an unrelated reason.**

Every request failed:

    provider error: https://api.themoviedb.org/3/tv/9877?…&api_key=REDACTED:
      Network Error: timed out reading response

`api_key=REDACTED` confirms `scrub_tmdb_url_secret` keeps the key out of logs.

### Triage — the binary is the only thing that cannot reach TMDB

| client | result |
|---|---|
| DNS | resolves, 4 addresses |
| `curl` → api.themoviedb.org | **http 401 in 0.45s** (correct without a key) |
| `python3 urllib` | **HTTP 401** (correct) |
| **raw TLS socket, port 443** | **hangs — no response, killed at 120s** |
| the replay binary (`ureq`) | connect succeeds, **read times out at 30s** |

No proxy variables are set. So the sandbox lets recognised HTTP clients through
and blackholes a raw TLS connection opened by an arbitrary binary: TCP is
accepted, nothing ever answers. `ureq` opens its own socket, so the replay
connects and then waits out its 30-second read deadline on every call.

**Nothing was written.** The cache copy is still 8,185 entries and the shared
cache was never a target. The run was stopped rather than left to grind through
8 rounds at 30 seconds per call.

## What this nearly hid — `requests=N` counts attempts, not successes

`tmdb/mod.rs`:

    let _permit = self.limiter.acquire();
    self.http_requests.fetch_add(1, Ordering::Relaxed);   // before the call
    let mut url = format!("https://api.themoviedb.org/3{path}");

The counter increments **before** `agent.get(..).call()`. So a request that times
out still counts. Had the run completed, `warm_cache.sh` would have printed a
large `TOTAL LIVE REQUESTS` while the cache grew by **zero** — and I had told the
user to read that total as the number to report.

That is the loop's oldest trap in a new place: a number that means "the code ran"
being read as "the work happened". **The cache count is the ground truth; the
request count is an intention.**

`warm_cache.sh` now compares them per round and **aborts when requests are
attempted and nothing is written**, naming the timeout signature and pointing at
`run.err`. It also reports entries written alongside requests attempted, so the
two can never be conflated again.

This also means `requests=0` elsewhere in the loop is still sound as a proof of
offline-ness — zero attempts is zero calls. The asymmetry only bites in the other
direction, where a non-zero count is taken as evidence of success.

## Where it stands

Warming needs a network path this sandbox does not give the replay binary. Two
ways forward, neither of which the loop can take on its own:

1. **Run it outside the sandbox** — a plain terminal. The command needs no key on
   it, only a path:

       SHAPES="…16 shapes…" TMDB_SECRETS_FILE=~/nightjar-data-v9/secrets \
         ~/Documents/GitHub/nightjar-wt-matcher/notes/loop-matcher/scripts/warm_cache.sh \
         ~/Documents/GitHub/nightjar-wt-matcher-oracle \
         ~/nightjar-wt-matcher-scratch/tmdb-cache-warm

2. **Re-run with the sandbox disabled**, which turns off a safety boundary
   wholesale and is the user's call to make explicitly, not something to assume
   from "warm the cache".

Until one of those happens, **every rate in this loop remains over a
collision-poor sample**: 34.0% of rows stalled, `tv.handmade` 5,644 rows never
measured, and M2 still a stall wearing a wrong-bind's clothes.

---

## Attempt 2 — the sandbox was not the blocker. I was wrong about that.

I reported the sandbox as the cause and asked for it to be disabled. **That was a
misdiagnosis, and disabling it was unnecessary.** The real cause:

- TMDB publishes **AAAA records**, and `getaddrinfo` on this machine returns the
  **IPv6 addresses first**.
- **This machine's IPv6 is broken.** `curl -6` to TMDB returns `http=000`, rc=28.
- **`ureq` connects to the first address from `to_socket_addrs()` and does not
  fall back.** So every request opened a black-holed IPv6 connection and waited
  out the 30-second read deadline.
- `curl` and Python succeeded because both do Happy Eyeballs across the whole
  address list. That difference is exactly what made it look like a
  per-binary network restriction.

It reproduced **unsandboxed and in the foreground**, which is what falsified the
sandbox theory. Confirmed the other way afterwards: with the resolver fixed, a
warm batch runs **with the sandbox back on** — 939 requests, 0 timeouts, rc=0.

The tell I had and misread: a raw IPv4 socket worked while the binary did not. I
took "curl works, binary doesn't" as evidence about the *binary's permissions*
when it was evidence about *address selection*. Two clients differing is not
proof of a policy boundary between them.

### The fix, in the harness only

An IPv4-only resolver on the `AgentBuilder`, in the **harness overlay** —
`~/nightjar-wt-matcher-scratch/harness-title.patch`, never committed to the
product:

    .resolver(|netloc: &str| -> std::io::Result<Vec<std::net::SocketAddr>> {
        use std::net::ToSocketAddrs;
        Ok(netloc.to_socket_addrs()?.filter(|a| a.is_ipv4()).collect())
    })

It affects only which address a warm connects to. **Strict runs issue no requests
at all**, so nothing that gets measured passes through it.

**There is a real product question here that this does not answer**, and it should
not be settled by a loop: shipped Nightjar uses the same `ureq` agent with no
resolver, so on any user's machine with broken IPv6 *every provider call takes 30
seconds and then fails*. That is a plausible field defect, it is not what this
loop was asked to do, and the oracle cannot see it — the replay never makes a
request. Recorded here as a finding for someone to take on deliberately.

### First verified warm

    tv.flat.titled-b0   600 requests, 0 timeouts, 0 errors, cache 8185 -> 8785
    tv.single-b0        939 requests, 0 timeouts, 0 errors  (sandboxed)

Cache growth equals request count exactly, which is the check the earlier version
of this script could not make.

---

## Tranche 1 — measured

**2,011 live requests. Cache 8,185 → 10,196, a delta of exactly 2,011.** Request
count and entries written agree 1:1, which is the check that matters given the
counter counts attempts. The shared 8,185-entry cache is untouched.

    tv.flat.titled-b0  600 requests   (verification batch)
    tv.single-b0       939 requests   (verification batch, sandboxed)
    round 1            472 requests, 472 entries written
    round 2              0 requests  -> converged

The scripted run converged in one effective round. The plan said 457 and the true
cost was 2,011 — the lower-bound effect, 4.4×, exactly as `warm_list.py` warned.
It converged immediately because the replay's own drain already makes several
passes inside one batch, so it follows the raised calls without needing another
round.

Re-measured strict, `requests=0` on all 34 runs, population pinned at 2,410
entities / 67,982 rows, noise floor **0**.

### Stalls are gone everywhere except `tv.handmade`

| shape | pre-warm | warmed | measured rows |
|---|---:|---:|---:|
| tv.sonarr | 100.0% | **100.0%** | 4,311 → **5,644** |
| tv.flat.titled | 100.0% | **100.0%** | 4,311 → **5,644** |
| tv.twoseason | 100.0% | **100.0%** | 3,562 → **6,278** |
| tv.flat | 100.0% | 99.9% | 4,083 → 5,644 |
| tv.numbered | 100.0% | 99.9% | 3,594 → 5,644 |
| tv.sonarr.plain | 100.0% | 99.9% | 3,594 → 5,644 |
| **tv.root** | 88.4% | **63.9%** | 4,012 → 5,644 |
| **tv.noyear** | 95.9% | **63.9%** | 3,526 → 5,644 |
| **tv.scene** | 89.5% | **61.6%** | 3,832 → 5,644 |
| movie.noyear | 58.8% | 58.8% | unchanged |
| tv.handmade | — | — | **still 0 of 5,644** |

Overall stalled **23,086 → 5,644**; measured **44,896 → 62,338**; correct% **96.2%
→ 88.8%**.

**Every transition is `stalled → something`. Nothing already measured moved:**

    stalled -> correct                12204
    stalled -> absent                  3421
    stalled -> wrong.unknownepisode    1737
    stalled -> wrong.entity              80

They sum to 17,442, exactly the fall in `stalled`. That is what warming should
look like: information added, nothing perturbed.

**This is note 05 in reverse, as written in advance.** The rate fell 7.33 points
and no binding got worse. `tv.noyear` at 95.9% was 95.9% *of the 62% of its rows
the cache could serve*; at 63.9% it is over all of them. The lower number is the
truer one.

## The result that matters: 1,707 confident wrong bindings, previously invisible

Warming did not just move rows into `absent`. It moved **1,817** into wrong
classes, and the bulk arrived under a label the loop had never seen —
`wrong.unknownepisode`, which `compare.py` **aborted on** rather than counting as
zero. That guard was added after the `wrong.entity` mistake in note 00, and this
is the second time it has earned its place.

The label is ambiguous by construction: the scorer rebuilds id → (season, episode)
from cached season payloads, so an episode id from an uncached season cannot be
placed, and **a correct bind into an uncached season looks identical to a wrong
one.** So it was resolved from the other side — the resolver logs the entity it
bound (`notes/loop-matcher/scripts/unknown_episode.py`):

| | rows |
|---|---:|
| bound a **different** entity — a real wrong bind | **1,607** |
| bound the same entity — scorer blind spot | **0** |
| could not join to a resolver line | 130 |

Not one was a scorer artefact. So the honest wrong total after tranche 1 is
**1,707 confirmed** (100 `wrong.entity` + 1,607) with 130 unresolved — against
**20** before warming.

### And the mechanism is named

| method that chose the wrong candidate | rows |
|---|---:|
| `exact_title_episode_count` | 989 |
| `exact_title_season_count` | 604 |
| `exact_title_year` | 14 |

These are the **0.90-confidence collision pins** — the tie-breakers that fire
after `pin_collision` and bind without hesitation. Examples: `Cross` bound 225001
instead of 213306; `Lucifer` bound 156218 instead of 63174; `Archer` bound 26529
instead of 10283.

So the collision tier does not merely fail to pin (M5, `absent`). **When it does
pin, it is often confidently wrong**, and no instrument on this project could see
that until the cache was warm. Wrong beats absent in severity, and this is 1,707
of the former.

A new wrong-bind family also surfaced in the entity table — a show binding to its
own spin-off or sequel series:

     16  Queer as Folk              -> Queer as Folk (2022)
     10  Suits                      -> Suits LA (2025)
      8  Gilmore Girls              -> Gilmore Girls: A Year in the Life (2016)
      8  Stranger Things            -> Stranger Things: Tales from '85 (2026)
      8  Avatar: The Last Airbender -> Avatar: The Last Airbender (2024)
      3  Sherlock                   -> Sherlock & Daughter (2025)
      2  Battlestar Galactica       -> Battlestar Galactica (2003)

## A quiet join defect in my own analysis, found and fixed

`(shape, path)` is **not a unique row key.** Two entities with the same title and
no year render the same relpath — two films called `Aladdin` both become
`Aladdin/Aladdin.1080p.BluRay.mkv` — and `gen_library.py` puts them in different
batches precisely so they cannot share a database. 120 of 67,982 rows collide.

`compare.py` keyed on `(shape, path)`, so **every transition table in notes 01–05
silently dropped up to 120 rows** while the verdict totals stayed correct. The tell
was arithmetic: transitions summed to 17,359 where `stalled` fell by 17,442.

Fixed to `(shape, batch, path)` — 67,982 unique — with an assertion that the key
is unique, so a future collision fails loudly instead of quietly. Transitions now
reconcile exactly.

**The claim that depended on it was re-checked and survives**: zero rows that were
`correct` on `origin/main` stopped being correct across iterations 1–3, under the
corrected key.
