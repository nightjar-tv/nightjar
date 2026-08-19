# Iteration 2 — M6, second path: a folder that does not exist has an identity

## Why this is the same mechanism, and why that is the point

Iteration 1 split the *query group* key when there is no show folder. It
deliberately left `folder_key` on the real relpath, because the `series` table is
keyed on relpath and conflating the two would have made a root-level group look
up an identity row under a synthesised name.

That was one path into M6, not the mechanism. The loop's own trap says so: *"A
fix that removes a route is not a fix for the mechanism. Ask whether your change
alters the mechanism or one path into it."* It altered one path. Here are the
other two, and the oracle names them out loud:

    discard stored series id 296206 — name 'Agent Kim Reactivated' does not
      match folder title '30 rock'; falling through to search
    unmatched 30 rock reason=BelowThreshold {
      confidence: 0.72, method: "negative_cache" }

`series` rows are keyed `(library_id, relpath)`. A root-level file's relpath is
`""`, so **one show in the pool wins, writes a row under `''`, and all 20 shows
in that root then read it as their own identity.** Two consequences:

1. Every one of the 20 gets a stored id belonging to another show. The ADR-0033
   §8 name cross-check catches it and discards it — which is why iteration 1
   removed the wrong binds — but the churn is real.
2. **ADR-0033 Q4 then keys the negative cache on `series:{that id}`.** All 20
   share one key, so the first show's miss suppresses the other nineteen's
   fall-through search. The comment beside `series_key` states this exact
   hazard — *"one folder's miss must never suppress the other's fall-through
   search"* — and the empty folder inverts it.

`resolve.rs:503`:

    let series_key = input
        .series_show_id
        .filter(|_| input.tmdb_id.is_none())
        .map(negative_cache::series_cache_key);

The root cause sits upstream of both: `series_show_id_for_folder` answers for a
folder that is not there, and `upsert_series_row` writes one for it.

## Population, counted

Groups from the resolver's own reason lines, items from `scored-a.json`
(`notes/loop-matcher/scripts/reasons.py`), on the iteration-1 tree:

| shape | reason | groups | items |
|---|---|---:|---:|
| **tv.root** | `negative_cache` | **271** | **1,834** |
| tv.root | `exact_title_collision_unpinned` | 34 | 263 |
| tv.root | (stalled) | — | 914 |

`tv.root` holds 2,461 absent rows after iteration 1. **1,834 of them are this
path** — the largest remaining measurable failing population in the suite that is
not a harness artefact.

Zero of the 271 `negative_cache` queries ever reported
`exact_title_collision_unpinned`, so the two are separate mechanisms and not one
decision seen twice. Checked, because assuming they were the same would have
merged a real population into an unrelated one.

## The convention it depends on, and what it does without it

**It depends on nothing about filenames.** The rule is structural: an empty
relpath is the absence of a folder, so it cannot carry a folder's identity.
There is no naming form under which that is false, which makes this a narrower
claim than iteration 1's — that one needed the basename to carry a title.

What it does *not* fix: a genuine folder whose identity is wrong. That is the
ADR-0033 §8 cross-check's job and it already runs.

## Dogfood regression risk

Three guards, all on `show_folder.is_empty()`:

- the read declines, so a legacy `''` row already in a database stops being
  consulted;
- the write declines, so no new one is created;
- `load_series_rows` skips `''` rows, so the group unit key cannot resurrect one.

Every library whose episodes sit in show folders is untouched — the guards never
fire. The risk is confined to a library storing episodes at the library root,
which is the shape this is fixing. The dogfood pair will show it: iteration 1's
pair was identical on every counter, which is consistent with that library having
no root-level episode file.

Not a wins argument. Purely: what could break.

## Prediction, written before running

| shape | after iter 1 | predicted | why |
|---|---:|---:|---|
| **tv.root** | 47.8% | **75–88%** | 1,834 absents lose the shared negative key; ceiling is `tv.flat` at 88.4%, the same basename form with no folder year |
| tv.root wrong.entity | 8 | **8, or fewer** | the shared id was already discarded by the cross-check, so removing it should create no new wrong bind |
| tv.root stalled | 914 | **rises** | 271 groups that were short-circuited now really search, and some will want candidate calls the cache never recorded |
| every other shape | — | **unchanged** | `tv.root` is the only shape with an empty show folder; all others have at least one directory level |
| overall correct% | 79.0% | **81–83%** | |

I got the stall direction wrong in iteration 1 by reasoning from query volume.
The correction there was that stalls come from candidate shaping, not from
queries — and that reasoning predicts a *rise* here, because a suppressed search
does no candidate shaping at all and an unsuppressed one does.

---

## The change

Three guards in `queue.rs`, all on `show_folder.is_empty()`, all saying one
thing: **a folder that does not exist has no identity.**

- `series_show_id_for_folder` returns `Ok(None)`. Guarding the shared read
  rather than each call site keeps the drain, the browse proxy and the manual
  retry on one answer (Rule 4.11), and stops a `''` row already in a database
  from being consulted.
- `upsert_series_row` returns without writing, so no new one is created.
- `load_series_rows` skips `''` rows, so the group unit key cannot resurrect a
  legacy row this build would no longer write.

36 lines. `nightjar-core` and `nightjar-scanner` remain byte-identical to
`origin/main`; the whole loop still touches one file.

## Measured — four instruments

### 1. The oracle

| shape | correct b | correct a | wrong b | wrong a | rate b | rate a |
|---|---:|---:|---:|---:|---:|---:|
| **tv.root** | 2,261 | **3,547** | 8 | 8 | 47.8% | **88.4%** |
| every other shape | — | — | — | — | unchanged | unchanged |

One shape moved. Sixteen byte-identical.

| verdict | before | after | delta |
|---|---:|---:|---:|
| correct | 37,532 | 38,818 | **+1,286** |
| wrong.entity | 28 | 28 | **0** |
| absent | 9,945 | 7,941 | **−2,004** |
| stalled | 20,477 | 21,195 | +718 |
| **correct%** | **79.0%** | **83.0%** | **+3.96 pt** |

**Two row transitions in the whole suite, and no regression of any kind:**

    absent -> correct    1286
    absent -> stalled     710

Nothing left the `correct` class. Noise floor **0 of 67,982**.

### tv.root has converged on its controlled comparator

`tv.root` now matches `tv.flat` on every column: measured 4,012, correct 3,547,
wrong 8, absent 457, stalled 1,632.

Two identical columns are exactly how this harness has misled before — its first
cut shipped a `tv.root` that was `tv.flat` with a different string in it. So the
two were checked apart rather than assumed distinct:

| | dir components | library root | sample path |
|---|---:|---|---|
| tv.root | **0** | `/oracle/tv.root/shows/pool0` (20 shows share it) | `Dept..Q.S01E01.1080p.WEB-DL.mkv` |
| tv.flat | **1** | `/oracle/tv.flat/shows/245703` (one show) | `Dept. Q (2025)/Dept..Q.S01E01.1080p.WEB-DL.mkv` |

Different paths, different roots, different sharing. And the verdicts agree on
**all 5,644 `(entity, season, episode)` slots — zero disagreements.**

So the claim the oracle now supports is narrow and strong: **for this filename
form, a shared library root costs nothing relative to a folder per show.** The
457 absents and 8 wrongs that remain are `tv.flat`'s own causes — the exact-title
collision family and the Queer as Folk revival — and not the shared root.

### 2. The parser sweep

74,624 names, 0 gains, 0 regressions. Insensitive by construction:
`git diff --quiet origin/main -- server/crates/core` is silent, and the sweep
builds only `nightjar-core`. Recorded as insensitivity, not as a clean result.

### 3. The parser corpus

**71.0%** (524 of 738 applicable) — unchanged.

### 4. The dogfood strict pair

    control   sha256 df06d521…   DONE groups=3220 ready=24953 unmatched=51 errors=0 requests=0
    treatment sha256 2627ddc9…   DONE groups=3220 ready=24953 unmatched=51 errors=0 requests=0
    cache 8185 before, 8185 after

Distinct binaries, separate target directories, `NIGHTJAR_REPARSE=1` both arms,
`errors=0` and `requests=0` both arms. Every counter identical: the library
stores no episode at a library root, so all three guards are inert there.

### Shipped tests

740 pass, 0 fail, 3 ignored (pre-existing).

## The prediction

Landed on every line, including the one iteration 1 got wrong.

| | predicted | measured |
|---|---|---|
| tv.root | 75–88% | **88.4%** |
| tv.root wrong.entity | 8 or fewer | **8** |
| tv.root stalled | rises | **+718** |
| other shapes | unchanged | **16 byte-identical** |
| overall correct% | 81–83% | **83.0%** |

The stall direction came from iteration 1's correction — stalls come from
candidate shaping, not query volume, so a search that stops being suppressed
starts shaping candidates and can miss an unwarmed cache. That reasoning is now
tested twice: it failed as "volume" and held as "shaping".

**710 rows went `absent → stalled`.** That is not a cost; it is the instrument
declining to answer. Those groups now issue a real search where they were
short-circuited before, and the cache has no recording of the candidate calls
that search wants. Warming would turn them into verdicts. Until then they are
unmeasured, and `tv.root`'s 88.4% is over the 4,012 rows that were measured, not
over its 5,644.

## Verdict — KEEP

Oracle up on the target shape and the total, no regression anywhere in 67,982
rows, the other three instruments clean or provably insensitive, tests green,
noise floor zero.
