# Iteration 3 — `wrong.kind`: one prerequisite built, one refused, the rule left alone

**No product change.** The 573 stay, and this note says exactly why and exactly
what would unblock them.

## The population

573 rows, all in `tv.episodetitle`, unchanged at this base. The shape renders
`{show}/Season {n}/{episode title}.mkv` — `tv.handmade` with the leading `NN - `
removed and nothing else changed. It is the worst class in the suite: an episode
file linked to a film has left the TV library altogether.

## The mechanism, and that it is the product rather than the harness

Checked first, because the harness had already misreported a field this session
(note 00) and a `wrong.kind` that was an artefact would waste the whole item.

`parse_filename("Closure.mkv")` returns `kind = Movie` — not `Unknown` — so
`parsed.kind.as_str()` is `"movie"` and that is what **the scanner stores**. The
replay harness reaches the same string by a different route (`Movie | Unknown =>
"movie"`), and its comment claims `Unknown` "is not a kind the scanner stores",
which is only true because `parse_filename` never returns `Unknown` at all — the
enum has three arms and the parser sets two. The two agree here, so the 573 are
real.

## Prerequisite 2 — built. `movie.specials`

**A `Specials/` directory over a file whose correct binding is a movie record.**
No shape had one, and that gap is why "a season directory means the file is not a
film" scored as a free win on this instrument while it destroyed five correct
bindings in the real library: TMDB models `Top Gear: Polar Special` as a
standalone movie.

    Hannibal/Specials/After.Ever.Happy.1080p.BluRay.mkv     -> the film

The parent is a **show's** name, not the film's, because that is the real case.
It does not reach the query — checked with the shipped chain through
`oracle_query`:

    Top Gear/Specials/Polar Special.mkv
      kind=movie  query="Polar Special"  year=-  showfolder="Top Gear"  folderyear=-

which is exactly the query `movie.noyear` builds. The parent carries no `(YYYY)`
deliberately: `year_from_path` reads one two parents up, and it would resolve the
collision `movie.noyear` exists to keep unresolved.

**It needed no warming.** Every query it makes is `movie.noyear`'s, already
cached: 1,712 rows, `requests=0`, 0 stalled.

### The shape as a paired control

`movie.specials` is `movie.noyear` with one directory inserted, so it should give
**the same entity the same verdict**. It does, on both arms:

    movie.noyear: 1712 entities   movie.specials: 1712 entities
    entities joined: 1712   differing: 0
      the two shapes give every entity the same verdict and the same binding

That pairing is the guard, and it is sharper than either shape's rate.
`notes/loop3/scripts/paired_shapes.py` joins on the entity, because equal
totals are not the same claim — one shape can lose what the other gains and the
columns still agree.

**Any rule that reads `Specials/` as a season directory separates the pair**, and
nothing else in the suite does. Five dogfood files were the only guard before
this; there are 1,712 now.

### The population moved, deliberately

Adding a shape adds rows. The entity set did **not** move: `pick_entities.py` was
not re-run, `entities.json` is untouched, and all 39 pre-existing `capture.jsonl`
files are **byte-identical** to what was there before — checked by `sha256`
before and after regeneration, and the only new files are `movie.specials`'s two.

| | before | after |
|---|---:|---:|
| entities | 2,410 | **2,410** |
| shapes | 20 | 21 |
| rows | 79,382 | **81,094** |

`EXPECT_ROWS=81094` from here on, and both arms were re-measured on the new
population so every table in this loop still joins.

**One trap fired on the way.** `gen_library.py` reads `TMDB_CACHE` to decide
which `tv.shortfolder` entities are usable, and the first regeneration ran
without it — against the 8,185-entry unwarmed cache. `tv.shortfolder` silently
vanished (17 shows became 0) and the run printed `shapes 20` where it should
print 21. The population would have moved in a second way nobody asked for. The
checksum comparison is what caught it.

Baseline on the new population, warmed cache, `requests=0`, noise floor 0:

| shape | measured | correct | wrong.entity | absent | correct% |
|---|---:|---:|---:|---:|---:|
| movie.noyear | 1,712 | 1,006 | 1 | 705 | 58.8% |
| **movie.specials** | 1,712 | 1,006 | 1 | 705 | 58.8% |

## Prerequisite 1 — refused, and the rule with it

**"Expose the numbered-season distinction where the rule lives."**
`is_season_directory` is private to `nightjar-db/paths.rs` and deliberately
treats `Specials`, `Special`, `Extras`, `Extra`, `Season N` and `SNN` alike —
correctly, because `show_folder_relpath` must walk up past all of them.

Splitting out `numbered_season_directory` is fifteen lines. **It has no caller,
and Rule 4.7 says no.** "Abstract on the second concrete use case, not the
first" — and there is not yet a first, because the rule that would call it cannot
be measured. Shipping the predicate now would be a placeholder waiting for a
change that is blocked, which is Rule 4.8's shape as well.

## Why the rule is blocked: the instrument cannot judge it

The rule is *a file inside a numbered season directory is not a film*. Its effect
on `tv.episodetitle` is to move those files from a **movie** search to a **TV**
search — `clean_show_title` of the parsed title, which for these names is the
episode title.

The replay cache cannot serve those searches. Measured with the shipped cleaner
(`notes/loop3/scripts/tv_query_for_paths.sh` builds a throwaway wrapper on the
tree's own `nightjar-core` and `nightjar-metadata`; `kind_rule_warm_cost.py` asks
the cache's own key function):

    paths             5643
    empty query          0  (no search would be issued at all)
    tv search cached    51
    tv search missing 5592 rows, 4691 distinct queries
    cache dir         tmdb-cache-warm (20350 entries)

**Both halves of that estimate turned out wrong, and the warm on 2026-08-22
settled it at 1,758 calls, not 4,691.**

*Too high, by 4x.* This counts one query per **file**. The drain groups by show
folder and searches once per **group** — 697 groups for `tv.episodetitle`'s 5,643
files. The real cost was **1,154**.

*Too narrow.* It costed the shape carrying the 573. The rule flips the kind of
**any** file under a numbered season directory that parses as a movie, and
`tv.handmade` renders `Show/Season 1/01 - Closure.mkv`, which does. Warming only
`tv.episodetitle` left `tv.handmade` **100% stalled** and the whole measurement
unreadable until a second warm of **604** calls.

The lesson is the general one: **warm the shapes the rule reaches, not the shape
the defect is in.**

So shipping the rule turns 573 `wrong.kind` and 5,070 `absent` into **5,592
stalled**, and the scorer's own rule is that a stall is *not a result, not
measured*. The change would ship unjudged — and unjudged is what the previous
attempt shipped, on a shape that could not see its only failure mode.

**4,691 live TV searches would fix that, and warming is a human-run step.** This
loop's hard limits forbid it, so the rule stops here.

`oracle_query` also shows a second thing the rule does not fix. Under it,
`{show}/Season 1/Closure.mkv` becomes an episode whose *show* title is `Closure`
— the episode's name, not the folder's — so it searches for the wrong show. That
is better than binding a film (the file stays in the TV library, and it fails by
declining), but it is not a correct binding, and reaching one needs the folder's
title and season as well. That is item 5's signature change, not this rule.

## What was measured

| instrument | reading | notes |
|---|---|---|
| oracle, 81,094 rows, base | 64,836 correct / 573 `wrong.kind` / 11 `wrong.entity` / 8 `wrong.unk` / 15,654 absent / 12 stalled | `requests=0`, noise floor 0 |
| oracle, 81,094 rows, branch tip | 64,862 / 573 / 5 / 0 / 15,654 / 0 | the same 26 rows iteration 2 moved |
| `movie.specials` vs `movie.noyear` | 1,712 entities joined, **0 differing**, on both arms | the guard |
| dogfood, corpus, sweep | not re-run | **no product code changed in this iteration** |

## Judgement

**Kept as instrument work; the product is untouched.** The brief's own rule
applies: "If you cannot satisfy both prerequisites, leave the 573 and say so."
One prerequisite is now built and is stronger than the five dogfood files it
replaces. The other is refused on Rule 4.7, and would be trivial the moment the
rule has a caller.

## What this could not measure, named

- **What the rule actually costs.** 4,691 cached TV searches away.
- **`library_kind`.** `movie.specials` sits in a `movies` library because
  `gen_library.py` derives the library kind from the entity's kind; the real
  `Top Gear` case sits in a `shows` library. `library_kind` is read only by
  `snapshot_visible_proxy`, which decides drain *order*, not the match — so the
  gap changes no verdict, but it is a gap.
- **A `Specials/` file whose correct binding is an episode.** The other half of
  the same question, and no shape has it either. TMDB models some specials as
  season 0 episodes and some as movies, and only the second is generated here.
- **Two-file specials folders, `Extras/`, `SNN/`.** One directory spelling is
  generated.
