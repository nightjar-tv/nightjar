# Iteration 6 — `wrong.kind`: warmed, measured, kept

**Kept.** `cb780b8`. The 573 are gone. Nothing else moved.

Iteration 3 left this blocked because the instrument could not judge it. The
warm on 2026-08-22 removed the block, and the rule then passed on both
instruments — including the one that killed the previous attempt.

## The population

573 rows, all `tv.episodetitle`, unchanged from the base. An episode file linked
to a film has left the TV library altogether; no re-match inside that library
can fix it, which is why the scorer ranks `wrong.kind` first in `ORDER`.

## The change

**One rule, in the layer that has the folder.**

`parse_filename` takes a basename. `Closure.mkv` carries no season, no episode
and nothing that says television, so the parser calls it a movie — correctly, on
the evidence it has. `nightjar-scanner::stored_kind` is the sibling of
`stored_title`: it is the scanner, not the parser, that can see
`Show/Season 1/Closure.mkv` is not a film.

`nightjar-db` exposes `is_numbered_season_directory` and
`under_numbered_season_directory` **beside `is_season_directory`, sharing its
walk**, so the two cannot disagree about where the show folder starts. Rule 4.7
blocked this in iteration 3 for having no caller; it has one now, and the
predicate ships with it rather than ahead of it.

**`Specials/` is deliberately not numbered.** TMDB models `Top Gear: Polar
Special` as a standalone movie record. The previous attempt's rule — *a season
directory means the file is not a film* — is false for exactly that shape, and
it destroyed five correct bindings in the real library while the oracle called it
a free win.

## What was measured

Both arms on `tmdb-cache-kind`, fully warmed, `requests=0`, noise floor **0 of
81,094** on every run.

    rows joined: 81094
    verdict changed: 573
    same verdict, different entity: 0
       tv.episodetitle   wrong.kind -> absent   573

**Every moved row moved out of the worst class, and nothing else moved at all.**

| verdict | branch tip | + the rule | delta |
|---|---:|---:|---:|
| correct | 64,862 | 64,862 | 0 |
| `wrong.kind` | **573** | **0** | **−573** |
| `wrong.entity` | 5 | 5 | 0 |
| absent | 15,654 | 16,227 | +573 |
| stalled | 0 | 0 | 0 |

**No correct bindings are gained, and that is the honest result.** Those files
still search on the *episode* title, because the rule fixes the kind and not the
title. They fail by declining rather than by binding a film — better, and not
right. Reaching right needs the folder's title and season as well, which is the
parser signature change item 5 names.

### The guard, on the generated population

    movie.noyear: 1712 entities   movie.specials: 1712 entities
    entities joined: 1712   differing: 0

`movie.specials` — the shape iteration 3 built for precisely this — still gives
every entity the same verdict and the same binding as `movie.noyear`. The rule
does not touch a file under `Specials/` whose right answer is a film.

### The guard, on the real library

Strict pair, branch tip against the rule. Distinct binaries, separate target
directories, `NIGHTJAR_REPARSE=1` both, cache 8,185 before and after.

    ready 24953 = 24953   unmatched 51 = 51   errors 0   requests 0
    groups 3220 -> 3217   (-3)

**The `−3` was chased rather than netted.** Three files, all `Top Gear`:

    Top Gear/Season 16/… - 16x00 - The three wise men christmas special
    Top Gear/Season 22/… - 22x00 - Special Patagonia Part One
    Top Gear/Season 22/… - 22x00 - Special Patagonia Part Two

`16x00` is episode zero, which the matcher rejects, so they parsed as films.
Under the rule they are episodes. Comparing **every binding in both databases**,
exactly three differ and all three are **gains** — no link at all →
`tmdb:show:45`. 25,023 → 25,026 links.

And the number that decides it:

    files under Specials/ or Extras/
      control    5 movie ready, 5 movie unmatched, 4 episode ready, 2 episode unmatched
      treatment  identical

**The five `Specials/` movie bindings the previous attempt destroyed are
untouched.** That is the real library saying the distinction is right, not the
oracle.

| instrument | reading |
|---|---|
| oracle, 81,094 rows | `wrong.kind` 573 → **0**, nothing else moved |
| `movie.specials` vs `movie.noyear` | 1,712 joined, **0 differing** |
| dogfood strict pair | 3 bindings differ, **all gains**; `Specials/` identical |
| parser corpus / sweep | **insensitive by construction** — `git diff` over `nightjar-core` and `nightjar-metadata` is empty |
| `cargo test` | 65 → **67** db, 93 → **94** scanner, whole suite green |

`cargo fmt --check` and `cargo clippy --all-targets -D warnings` green.

**One counting trap fired and was caught.** `nightjar-db` briefly reported 68
tests where the base had 65 and two were added. A duplicated `#[test]` — my
edit swallowed the following function's attribute — registered one test twice.
The count is 67 and every one of the five new tests is listed by
`cargo test -- --list`.

## What the warm actually cost

**1,758 live requests, against an estimate of 4,691.** Both halves of that
estimate were wrong and both are corrected in note 03:

- it counted one query per **file**, where the drain searches once per **group**
  — 1,154 rather than 4,691 for `tv.episodetitle`;
- it costed only the shape carrying the defect. The rule reaches **any** file
  under a numbered season directory that parses as a movie, and `tv.handmade`
  does. Warming without it left that shape **100% stalled** and the first
  measurement unreadable. A second warm of **604** closed it.

**Warm the shapes the rule reaches, not the shape the defect is in.**

## What this could not measure, named

- **Whether the 573 could have been made *correct*.** They are `absent` now.
  Correct needs the folder's title and season, not just its kind.
- **A `Specials/` file whose correct binding is an episode.** TMDB models some
  specials as season 0 episodes and some as movies; only the second is
  generated, and the rule declines to touch either.
- **`Extras/`, `SNN/` and multi-file specials folders** beyond the single
  spelling each shape generates.
- **How common `NNxOO` specials are.** Three in one real library.
