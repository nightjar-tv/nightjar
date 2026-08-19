# 18 — the warm strict pair: the loop and the slice, measured end to end

The two keys iteration 17 identified were warmed and the pair re-run.

## Warming cost three requests, not two

    cache entries before   8,182
    cache entries after    8,185
    warming run            DONE ... errors=0 requests=3

**Strict mode under-reports the warm cost.** It aborts a path at its first
miss, so it can only report the misses it *reaches* — it showed 2. Fetching the
first key let the drain reach a third behind it. Count the cache directory;
do not trust the miss lines. That is now in the recipe.

## The result

Both arms, warm cache, strict, `NIGHTJAR_REPARSE=1`, distinct binaries:

    control    DONE groups=3221 ready=24953 unmatched=51 pending=0 errors=0 requests=0
    treatment  DONE groups=3220 ready=24953 unmatched=51 pending=0 errors=0 requests=0

**`requests=0` on both arms. Identical counts. `REBOUND` = 0.**

The 30 Red Dwarf items that iteration 17 saw stall were **entirely a cold-cache
artefact**, as the evidence suggested and as this now proves. The S09 special
resolves, the folder binding is `Red Dwarf (1988) -> 326` in both arms, and
every episode link is back.

## One item moved each way, and both are the same ambiguity

| file | control | treatment |
|---|---|---|
| `Top Gear - The Perfect Road Trip - 1 - 2013` | unmatched | **ready** -> `tmdb:movie:238234` |
| `Top Gear - The Perfect Road Trip - 2 - 2014` | **ready** -> `tmdb:movie:301235` | unmatched |

TMDB calls them `Top Gear: The Perfect Road Trip` (2013) and **`Top Gear: The
Perfect Road Trip 2`** (2014). So the number is part of the second film's title
and absent from the first.

Iteration 01 strips a spaced dash-number. That is **right for the 2013 file**,
where `- 1` is noise the provider does not use, and **wrong for the 2014 file**,
where `2` is the title.

**No filename rule gets both.** A stripping rule wins #1 and loses #2; a
non-stripping rule wins #2 and loses #1. The counts are identical either way.

A guard was priced — "a dash-number followed by a year is a sequel number" —
and it buys nothing: it costs **0** corpus cases, because **0 applicable corpus
cases have that shape at all**, and on the library it only swaps which of the
two binds. Not added. Over-fitting a rule to one ambiguous pair is how
vocabulary rules go wrong.

## The methodological finding, which is the part worth keeping

Every iteration reported "**0 items bound today at risk**". That number came
from `metadata_status` in the **live** `nightjar.db` — and the live status
records what past drains did, not what a fresh drain does.

In the live database **both** Top Gear files are `unmatched`. In a fresh
control drain, one of them is `ready`. So `group_keys` was measuring
*bound-in-the-live-database*, which is staler and smaller than
*bound-by-a-drain-from-scratch*.

The claims were not wrong about what they measured. They were measuring
something slightly weaker than the words suggested, and only the replay could
show the gap. **A parse-level substitute can bound which items move; it cannot
tell you which of them a fresh drain would have bound.**

## Verdict

Not a clean pass by the brief's standard, which is *zero* working bindings
broken. One broke, one was gained, both Top Gear, both genuinely ambiguous, and
the totals are identical.

What is established end to end, for the whole loop plus the slice:

- `requests=0` on both arms — the comparison was genuinely offline;
- `ready` 24,953 and `unmatched` 51 on both arms;
- `REBOUND` 0 — no item bound to a different entity;
- exactly two items differ, in opposite directions, on one ambiguous pair.

Whether to keep iteration 01's rule as it stands is a judgement about which of
two equally-defensible readings to prefer, and it is a human's to make. The
measurement says the cost is one binding and the gain is one binding.

Artefacts on the N150 under `~/gate2/loop/`: both trees, both binaries, three
run logs, three databases, the run and comparison scripts. About 1.9 GB.
