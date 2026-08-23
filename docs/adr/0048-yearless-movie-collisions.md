# ADR-0048: What may decide a yearless movie collision

- Status: **accepted** (option B)
- Date: 2026-08-20
- Supersedes: nothing
- Depends on: ADR-0026 §2 (the 0.90 auto-match floor this record argues against
  crossing); ADR-0028 (manual fix, which owns every key change and is where a
  below-floor suggestion would surface); ADR-0047 (the collision tier, whose
  coverage evidence has no movie analogue)
- Gate: Gate 3 — metadata auto-match ≥95% correct; every mismatch fixable in-UI
  in under 30 seconds
- Related: the matcher oracle warmed 2026-08-20 (`notes/loop-matcher/06`–`13`);
  `notes/loop-matcher/scripts/movie_noyear_evidence.py`, which produces every
  number below

## Context

`movie.noyear` renders `Name/Name.1080p.BluRay.mkv` — **no year in the filename
and none in the folder**. It is the largest failing block left in the suite:

| shape | measured | correct | wrong | absent |
|---|---:|---:|---:|---:|
| movie.noyear | 1,712 | 1,006 (58.8%) | 1 | **705** |

Every one of the 705 is `exact_title_collision_unpinned`, and the candidates are
**real distinct films sharing a title** — read out of the cached search responses:
`CODA` has 17, `The Visitor` 16, `Doubt` 9, `The BFG` 2. A human could not break
these ties from the filename either.

**A movie has no seasons.** Every discriminator that fixed the TV shapes in
ADR-0047 — season coverage, slots explained, per-season counts — has no analogue
here. There is nothing left to compare, which is why this is a question about
admitting *new* evidence rather than reordering existing evidence.

## What the instrument can and cannot weigh

**Runtime cannot be evaluated by this oracle, and that is a property of the
harness.** `gen_library.py:328` sets each generated file's `duration_ms` from the
correct entity's own `runtime`:

    rt = (shows if e["kind"] == "tv" else movies)[str(e["id"])].get("runtime")
    dur = int((rt or (45 if e["kind"] == "tv" else 110)) * 60000)

So a runtime tie-break would score near-perfectly here **by marking its own
homework**. Runtime may well be the strongest signal available in production — the
probe already reads a real duration and TMDB carries `runtime` — but adopting it
on this instrument's evidence would be adopting it on no evidence.

**`popularity`, `vote_count` and the provider's own result ordering are recorded,
not generated.** They come from the search response the drain actually received,
already on `SearchHit`, and cost no request. They are honest evidence here.

## The measurement

Over `movie.noyear`'s 1,712 entities, 1,342 whose search response this analysis
could locate (the remaining 370 are a limitation of the script, which keys on the
raw entity name where the matcher searches the *cleaned* title — not missing data):

| signal | right on the **673 currently unmatched** | right on the **668 currently matched** |
|---|---:|---:|
| **the provider's top-ranked result** | **88.9%** (598) | 97.9% |
| highest `vote_count` | 80.1% | 94.2% |
| highest `popularity` | 77.6% | 91.0% |

So the evidence exists and the provider's own ordering is the best of it. Adopting
"take the top-ranked exact candidate when nothing else pins" would bind roughly
**598 more correctly and 75 wrongly**, against a shape that currently carries
**one** wrong binding.

## Decision — the question, and the options

**May a yearless movie collision be settled by the provider's ranking, and if so
at what confidence?**

- **A. Auto-bind the top-ranked exact candidate.** +598 correct, **+75 wrong**, in
  a shape with 1 wrong today.
- **B. Score it below the floor and surface it in the fix flow** — a suggestion
  the user confirms, not a binding the matcher asserts. Costs nothing, gains
  nothing automatically, and turns a 30-second decision into a one-click one.
- **C. Require corroboration** — top-rank *and* highest `vote_count` agreeing.
  Narrows the population and raises precision; both numbers above are single
  signals, so the joint rate is unmeasured.
- **D. Do nothing.** 705 files stay unmatched.

**Recommended: B, and explicitly not A.**

**The measured precision is 88.9%. The auto-match floor is 0.90.** That is not a
coincidence to argue around — it is the floor doing its job. A signal that is
right seven times in eight is exactly what ADR-0026 §2 built a floor to keep out
of automatic bindings, and 75 wrong bindings is the cost of overruling it. Wrong
bindings trigger fetches and corrupt watch state; 705 unmatched files are
recoverable, and Gate 3 asks that each be fixable in under 30 seconds rather than
that none exist.

B is the shape that respects both: the evidence is good enough to *rank a
suggestion* and not good enough to *assert a binding*.

## Consequences

**Implemented as B.** `fix::search_candidates` marks at most one candidate
`suggested` — the id the shipped `score_search` chose over the same hits, which
below the floor is the `exact_title_collision_unpinned` pick. Movies only: the
88.9% is a movie number, and a show scored on this route would be scored without
the season shape the drain has. Nothing assigns on the flag; `assign` still takes
the id the user sent.

**Measurable, but not by the rate.** B moves no oracle row — a below-floor
suggestion still scores `absent` — so this record cannot be validated by
`correct%` and should not be. What it changes is the cost of the fix flow, which
the oracle does not model at all: no manual-match path, no rescan, no second pass.

**A is measurable and should be measured before anyone chooses it**, because the
598-against-75 estimate above is derived from search responses rather than from a
drain. If A is ever taken, the number to check is `wrong.entity` in
`movie.noyear`, which is 1 today.

**Runtime needs a different instrument.** Before it can be weighed, either the
oracle must generate durations that do not come from the answer — real
distributions, or jitter wide enough that an exact match is not the entity's own
number — or the evidence must come from the dogfood library, whose probe
durations are real. **Whoever takes runtime on should close that first**, the way
`tv.shortfolder` was added before ADR-0047's stronger option could be weighed.

**What this record does not settle.** The oracle is English names only, one drain
from an empty database, no NFOs. An NFO carrying a year would dissolve most of
this population and nothing here measures NFO precedence.
