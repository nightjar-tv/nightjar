# Item 5 — `tv.scene` / ADR-0049: **leave it unaccepted, and here is the shape**

The board offered two moves: build the missing shape, or leave the ADR
unaccepted. **Left unaccepted** — and not implemented, as instructed. What
follows is the part that was worth doing: the shape is far closer to existing
than the record thinks.

## The cost of A has moved, and the ADR's number is stale

ADR-0049 prices the status quo at **1,295 of 5,644**. On this loop's corrected
baseline `tv.scene` reads:

| | ADR-0049 | this loop's base |
|---|---:|---:|
| files | 5,644 | **6,061** |
| correct | 4,349 | 4,349 |
| absent | 1,295 | **1,712** |
| wrong | 0 | **0** |
| correct% | 77.1% | **71.8%** |

The correct count is identical and the population is larger, so every entity the
warmed cache added to this shape came in unmatched. **`wrong` is still 0**, which
is the part of A's case that matters: A leaves files unmatched and never
misbinds, and `BLOCK1_LEAVE_BAR` prefers that.

Anyone accepting this record should re-price A first. 1,295 is a number from a
smaller instrument.

## The blocking evidence — and it is nearly in hand

> **No recommendation is offered here, because the cost is not measured.** The
> oracle can say what A costs. It cannot yet say what B, C or D cost, because no
> shape generates two *different* shows whose scene folders would merge under
> each rule.

True. But the record treats the missing shape as a thing to be created from
nothing, and **the entities it needs are already picked and already tagged.**

`out/entities.json` on this loop's base holds **773 TV entities in 80 groups
whose names fold to one string** — different shows, different years, kept
because `pick_entities.py` only excludes an ambiguous *name and year* together:

    'queer as folk'   134967 (2022)  |  2902 (2000)
    'avatar the last airbender'  82452 (2024)  |  246 (2005)
    'mr mrs smith'    10058 (1996)   |  118642 (2024)
    'archer'          10283 (2009)   |  26529 (1975)

Each already carries `collision.title` and `revival` or `remake`. **These are
the `Shameless (US)` / `Shameless (UK)` class the ADR names**, and option D folds
every one of them together.

## The shape to build, specified

`tv.scene.collide` — for each of the 80 folded pairs, render **both** shows into
sibling per-episode scene folders under one library root:

    Queer.As.Folk.S01E01.1080p.WEB-DL.x264-GRP/…mkv     -> 134967 (2022)
    Queer.As.Folk.S01E01.1080p.WEB-DL.x264-GRP/…mkv     -> 2902  (2000)

Then each option is priced against A on the same rows:

| option | what the shape would show |
|---|---|
| A | today's number — unmatched, never wrong |
| B | one media file per directory: does it merge the pair, or only the honest cases? |
| C | names differing only in the episode marker — the two shows differ elsewhere too, so this should *not* merge them |
| D | title fold — **this must merge all 80 pairs**, and every merged row is a `wrong.entity`, which is the number that decides the record |

D is the option that reintroduces the class Q2 removed, and this shape is what
turns that argument into a count. **B and C are only worth measuring once D's
cost is on the table**, because D is the cheap option everyone reaches for.

## Why it was not built here

Three reasons, in order of weight:

1. **It changes the population**, and this loop's base and tip are compared on
   one generated library — checksum `aa9657b`, 90,072 rows. Adding a shape
   invalidates both arms and forces a re-baseline.
2. **It needs warming**, which is a live, human-run step and is explicitly
   outside this loop. A new shape's searches are not in `tmdb-cache-kind`, and an
   unwarmed shape stalls — `movie.seasondir` stalled at 98.7% for exactly this
   reason and read as a shape that measured nothing.
3. The board said not to implement the rule, and building the shape without
   pricing the options would leave the record no better off.

## What no instrument here can say

- **How common scene-named show folders are in the wild.** `populations.py`
  answers "how much of the real library would this touch" and the answer is
  zero — the one dogfood library is Sonarr-named. That means *not this library*,
  never *not anywhere*, and this item cannot be dismissed on a dogfood count.
- **Two-part episodes and multi-file release folders**, which decide whether B is
  narrow enough to be useless. No shape generates either.

**Status unchanged: proposed.** The next person's first move is the shape, not
the rule.
