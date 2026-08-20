# Consolidation — the loop, stopped

Stopped because every remaining mechanism is blocked on an ADR decision or on an
instrument that cannot see it. That is the same stop the loop reached at iteration
4, with one difference: **the population is fully measured now rather than 39%
invisible.**

## The product delta, instrument held constant

`origin/main` and the final branch, both on the same warmed cache, both strict,
`requests=0`, same 2,410 entities and 73,738 rows.

| verdict | origin/main | final | delta |
|---|---:|---:|---:|
| correct | 49,608 | **58,265** | **+8,657** |
| wrong.entity | 2,661 | **4** | **−2,657** |
| wrong.unknownepisode | 2,201 | **0** | **−2,201** |
| wrong.kind | 573 | 573 | 0 |
| partial | 6 | 0 | −6 |
| absent | 18,346 | 14,896 | −3,450 |
| stalled | 343 | 0 | −343 |
| **correct%** | **67.6%** | **79.0%** | **+11.43 pt** |

**Total wrong 5,435 → 577. In the collision shapes, 4,862 → 4.**

The 573 that remain are `wrong.kind` in `tv.episodetitle`, a shape this session
added; its mechanism was attempted and reverted (note 13).

Ten shapes moved, nine byte-identical. **187 rows went `correct → absent`** — the
cost across everything, named rather than netted.

## Per shape, final

| shape | correct% | wrong | absent |
|---|---:|---:|---:|
| movie.sonarr, tv.sonarr, tv.flat, tv.flat.titled, tv.numbered, tv.sonarr.plain, tv.partial, tv.single, tv.twoseason | **100.0%** | 0 | 0 |
| movie.scene / yearfile / yearfolder | 99.9% | 1 each | 1 each |
| tv.noyear, tv.root | **81.1%** | **0** | 1,065 |
| tv.scene | **77.1%** | **0** | 1,295 |
| movie.noyear | 58.8% | 1 | 705 |
| tv.shortfolder | 56.6% | 0 | 49 |
| tv.handmade | 0.0% | 0 | 5,644 |
| tv.episodetitle | 0.0% | **573 films** | 5,070 |

Sonarr-shaped layouts are perfect. The yearless TV shapes now carry **no wrong
bindings at all** and fail by declining. `movie.noyear` is untouched — ADR-0048.

## The instrument, before and after

| | at the start | now |
|---|---|---|
| rows | 67,982 | **73,738** |
| stalled | **39.3%** | **0** |
| provider errors | 9,025 | 0 |
| verdict classes | 5 | 7 (`wrong.kind` added) |
| shapes | 17 | 19 |
| noise floor | 13 rows | **0** |
| per-shape rows reconcile | **no** | yes, asserted |

**11,972 live requests**, cache 8,185 → 20,350, delta exactly 11,972, the shared
cache untouched. Two shapes added — `tv.episodetitle` and `tv.shortfolder` — with
the 2,410-entity set unchanged so every earlier comparison still joins.

## Iterations

| # | mechanism | outcome |
|---|---|---|
| 0 | re-baseline; found the brief's baseline was a non-ancestor tree; M1/M2 unmeasurable | — |
| 1 | M6 grouping: root-level episodes keyed by title | **kept** |
| 2 | M6 identity: a folder that does not exist has no `series` row | **kept** |
| 3 | M4: show-folder year read from the show folder | **kept** |
| 4 | stopped — everything left ADR- or instrument-blocked | — |
| 5 | harness derives the title as production does | **kept** (instrument) |
| 6 | warming, both tranches | **kept** (instrument) |
| 7 | `tv.episodetitle` — M2's severity measured at 573 films | **kept** (instrument) |
| 8 | scorer gains `wrong.kind`; table derived from `ORDER` | **kept** (instrument) |
| 9 | `tv.shortfolder` — costs ADR-0047's stronger option at 58 rows | **kept** (instrument) |
| 10 | ADR-0047 half one: exact fold beats extension | **kept** |
| 11 | ADR-0047 half two: coverage before magnitudes | **kept** |
| 12 | containment — failed its own bar, then gated by decision | **reverted, then gated** |
| 13 | a season directory means not-a-film | **REVERTED** |
| — | ADR-0048 drafted for `movie.noyear` | proposed |

**Two reverts.** Never two consecutively.

## The four instruments, final

- **Oracle** 73,738 rows, 0 stalled, 0 provider errors, `requests=0`, noise floor
  0, measured twice with **0 rows differing**.
- **Sweep** 74,624 names, 0 gains, 0 regressions — insensitive by construction,
  `nightjar-core` byte-identical to `origin/main` throughout.
- **Corpus** 71.0% (524/738) at every iteration, unchanged.
- **Dogfood pair** identical on every counter at every iteration **except note
  13**, where it found five destroyed bindings the oracle scored as perfect.
- **Tests** 743 pass, 3 ignored. Two flaky (`transcode` HLS seek, scanner poll
  holdoff) verified to fail identically without these changes.

## What the session is actually about

Six of fourteen iterations changed no product code. The instrument was wrong more
often than the matcher, and in more interesting ways:

- **A zero that meant "the code never ran", four times.** `requests=N` counting
  attempts; `(shape, path)` silently dropping 120 rows; a panicking batch reading
  as convergence; the by-shape table hiding 603 rows per line while the totals
  stayed right. Each was caught by arithmetic that did not add up, not by
  suspicion.
- **The harness deriving a field differently from production.** `tv.numbered` read
  0.0% for a defect that was already fixed.
- **The harness deriving a field *from the answer*.** `duration_ms` comes from the
  correct entity's own runtime, so runtime can never be weighed here.
- **My own shell being the instrument**, twice: an inherited `CARGO_TARGET_DIR`,
  and four arguments passed to a three-argument script.

And once, the reverse: **the dogfood library found what the oracle could not.**
`Specials/` over a provider-modelled movie exists in one real library and in no
generated shape.

## Where to go next, and why each is blocked

1. **`movie.noyear`, 705 rows** — ADR-0048, proposed. The provider's ranking is
   right 88.9% of the time against a 0.90 floor, so the recommendation is a
   below-floor suggestion, which **moves no oracle row by design**. Runtime is the
   likelier signal and is **unmeasurable here** until the harness stops deriving
   duration from the answer.
2. **`tv.episodetitle`'s 573 films** — the worst class remaining. Needs
   `nightjar-db` to expose numbered-season-versus-`Specials`, and an oracle shape
   holding a special whose correct binding is a movie record. Five dogfood files
   are a thin guard for a rule that broad.
3. **`tv.scene`'s fragmentation** — 9,035 groups for 5,644 files, because a
   per-episode release folder is treated as a show folder. An ADR-0033 change.
4. **`tv.handmade`, 5,644 rows** — the same folder-context gap as 2, plus it needs
   the episode number out of an `NN - ` prefix.

## What is still not measured, named

- **No NFOs anywhere.** An NFO carrying a year would dissolve most of
  `movie.noyear`, and nothing here measures NFO precedence.
- **English names only**, a renderable subset, 3–45 characters.
- **Season 1 mostly, episodes 1–10 mostly.** Every TV row is within-season
  partial, which is why proximity inverted and why the residual is what it is.
- **One drain from an empty database.** No manual match, no rescan, no second
  pass, no stored-folder-identity reuse across runs.
- **Movies are single-file.** No versions, editions, split parts or collections.
- **Runtime, popularity and NFO evidence** — the first is circular here, the second
  is measurable but recommended below the floor, the third absent entirely.

**The matcher is not shown to work.** It is shown to bind 79.0% of a generated
population correctly, to carry 4 wrong bindings where it carried 4,862, and to fail
by declining rather than by guessing in the shapes that used to guess. Everything
outside that population is unmeasured.
