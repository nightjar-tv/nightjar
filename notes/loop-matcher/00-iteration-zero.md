# Iteration zero — warming, and re-baselining on `main`

## Warming did not happen

The brief permits live provider calls in this one place, to warm the 39.3% of
oracle rows that stall on a cache miss. **It was not possible in this run.**
Reading or locating a TMDB key was refused by the sandbox, and I did not go
looking for the user's credentials by another route.

So the consequence has to be stated in the brief's own words: **every rate below
is over a collision-poor sample.** `warm_list.py` still names the work — 463
calls outside M1/M2 — and it remains the highest-value change to the instrument.

Nothing else in this loop makes a provider call. `requests=0` on every run is
the proof, and the scorer refuses to read its own tables without it.

## The baseline in the brief is not the baseline on `main`

The oracle was built against `slice/title-structural-terminator` at `2f6b1a9`.
That slice then landed as **#149**, squash-merged, and `2f6b1a9` is *not* an
ancestor of `origin/main` (`6221c59`). Between them sit 2,249 changed lines:

    server/crates/core/src/filename.rs   | 2647 +++++++++++++++++-----------
    server/crates/metadata/src/fix.rs    |   49 +
    server/crates/scanner/src/lib.rs     |   99 +-

So the table in the brief describes a tree this loop is not working on, and
every mechanism had to be re-asked before it was worth fixing. Re-baselined at
`6221c59` with the harness commit `d2b26fa` reapplied on top (it applies
cleanly). Entity set unchanged: 2,410 entities, 17 shapes, 67,982 files,
`requests=0`.

### What moved

| shape | brief (`2f6b1a9`) | measured (`6221c59`) |
|---|---:|---:|
| tv.numbered | **all stalled** | **5,644 measured, 0.0% correct** |
| tv.root | 3.3% | 3.6% |
| overall stalled | 39.3% | **31.0%** |

Everything else is within a point of the brief. Noise floor **10 of 67,982
rows (0.0147%)**, all `correct → partial` on one entity — lower than the 13 the
brief quotes. Measure against that.

## The instrument cannot see M1 or M2 — a finding, not a fix

`tv.numbered` moving from *stalled* to *0.0% correct* looks like a regression
and is not one. It is the harness.

**M1 is already fixed in the product.** `scanner/src/lib.rs` grew
`title_from_folder`, and both production `parse_filename` call sites (`:263`,
`:722`) now do:

    title: if parsed.title.is_empty() {
        title_from_folder(&store_path, &library_root)
    } else { parsed.title }

`fix.rs:119` documents the same behaviour in prose: *"the scanner borrows the
folder's name"*.

**The oracle bypasses it.** `gen_library.py` writes the ground-truth entity name
into the capture's `title` field, which would be cheating, so `run_one.sh` sets
`NIGHTJAR_REPARSE=1` to re-derive the title from the path. But the replay's
reparse is `parse_filename(base)` **and nothing else** — it does not apply the
scanner's folder substitution:

    let parsed = reparse.then(|| {
        let base = path_str.rsplit('/').next().unwrap_or(path_str);
        nightjar_core::parse_filename(base)
    });

So the oracle's title derivation is a third thing: neither production's
(parser + folder) nor the capture's (ground truth), and strictly weaker than
production for exactly the shapes whose filenames carry no title.

The chain, proven end to end:

1. `NIGHTJAR_REPARSE=1` overwrites the title with `parse_filename(base).title`.
2. For `tv.numbered` (`S01E01.mkv`) that title is empty.
3. An empty title is not a query — `MetadataSource::resolve` filters it to
   `Miss` before any request.
4. `errors=0`, `requests=0`, `groups=693`, all `reason=NoMatch`.
5. 5,644 rows score `absent`; 0 correct.

In production the scanner would have supplied `Scrubs (2001)`.

### What follows

**M1 and M2 are unmeasurable by this oracle, and both are off the table.** Any
M2 fix belongs in the same scanner title/kind derivation, and reparse discards
it — so an M2 fix would measure flat for a reason that has nothing to do with
whether it works. The brief's own stop condition covers this: *a mechanism that
cannot be measured by any of the four instruments*.

I did not edit the oracle. Per the hard limit, the instrument being wrong is
reported, not repaired. **Closing this gap is the second-highest-value change to
the instrument, after warming** — the replay's reparse should call the scanner's
derivation rather than a subset of it.

## Populations, counted

From `scored-a.json` at `6221c59`, by the structural property rather than the
shape label (`notes/loop-matcher/scripts/population.py`):

| mechanism | measured | correct | wrong | absent | stalled |
|---|---:|---:|---:|---:|---:|
| **M6** root-level episodes | 4,148 | 150 (3.6%) | **2,553** | 1,439 | 1,496 |
| M1 `tv.numbered` | 5,644 | 0 (0.0%) | 0 | 5,644 | 0 | 
| M5 `movie.noyear` | 1,712 | 1,006 (58.8%) | 1 | 705 | 0 |
| M4 flat, folder has a year | 8,235 | 7,633 | 8 | 594 | 3,053 |
| M3 title ends in 4 digits | 10 | 2 (20.0%) | 4 | 4 | 0 |
| M2 `tv.handmade` | 0 | — | — | — | 5,644 |

Counted, not estimated. The first cut of this script reported `wrong=0` for
`tv.root` because it counted a verdict label — `wrong` — that the scorer does
not emit; the label is `wrong.entity`. A label that does not exist counts zero
exactly like a mechanism that is not there, so the script now aborts on a
verdict it does not know.

**M6 is the pick**: the largest measurable failing population *and* the largest
wrong-bind block, and wrong beats absent in severity.

M3 is real and confirmed live, but only 2 entities reach it — and it has changed
shape since the brief. Under `Title (Year)` it is now **correct**
(`movie.sonarr`, 2/2); it survives only in the scene form, where the parser
takes `2049` as the year *and* truncates the title to `Blade Runner`. The
brief's "binds Blade Runner (1982) in all four movie shapes" is now three, not
four.
