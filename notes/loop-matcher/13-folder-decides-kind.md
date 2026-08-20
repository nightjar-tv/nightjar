# Iteration 13 — a season directory does not mean the file is not a film. REVERTED.

The dogfood library found a defect the oracle could not, for the first time in
this loop.

## What was tried

`parse_filename` sees a basename, so `Season 1/Closure.mkv` parses as a **film
called `Closure`** — non-empty title, `MediaKind::Movie`, no episode marker — and
the drain binds a film. `tv.episodetitle` measures **573 episode files bound to
films**, the worst outcome in the suite.

So: `stored_kind_and_title` in the scanner, replacing `stored_title` at both
indexing call sites and called by the replay harness too. Where the parser said
*movie*, found no season and no episode, and the file sat inside a season
directory, the kind became `Episode` and the title came from the show folder.

## What the oracle said

Exactly one transition in 73,738 rows:

    wrong.kind -> absent   573

**`wrong.kind` 573 → 0.** Nothing else moved. correct unchanged, rate unchanged at
79.0%, noise floor 0. `tv.handmade`'s internal state improved too — its reasons
went from 4,686 `NoMatch` on nonsense movie queries to 163 `BelowThreshold` on the
real show, same verdict, honest query. Sweep 0/0, corpus 71.0%.

By the oracle alone this was a clean severity win: the worst class in the suite
traded for a recoverable one, at no cost to anything measured.

## What the dogfood pair said

    control    groups=3220  ready=24953  unmatched=51
    treatment  groups=3202  ready=24948  unmatched=56
    PAIR MOVED: ready -5, unmatched +5

13 files changed kind, every one of them a special in a TV library —
`Sherlock/Specials/`, `Star Trek (1966)/Specials/`, `Top Gear/Specials/`,
`Top Gear/Season 16/`, `Top Gear/Season 22/`. `Specials` **is** a season directory
by `is_season_directory`, so the rule fired.

**For 8 of the 13 the kind flip is right.** `Sherlock/Specials/The Abominable
Bride (2016).mkv` is a Sherlock special, not a film.

**For 5 it is wrong, and they were bound correctly before.** `ready` alone cannot
say whether a binding was right, so the links were read:

    Top Gear - Polar Special.mkv              -> tmdb:movie:436511  Top Gear: Polar Special (2007)
    Top Gear - Winter Olympics special.mkv    -> tmdb:movie:286835  Top Gear: Winter Olympics Special (2006)
    Top Gear Apocalypse.mkv                   -> tmdb:movie:407862  Top Gear: Apocalypse (2010)

**TMDB models those specials as standalone movie records.** They are films as far
as the provider is concerned, they sat in a `Specials/` directory, and the matcher
had them right. The change took five correct bindings away.

## The premise was false, and only one instrument could say so

*"A season directory above a file is evidence the file is not a movie"* is **false
for `Specials/`**. A numbered season directory is a strong claim; `Specials` is a
catch-all that legitimately holds provider-modelled movies.

The oracle cannot see this. No generated shape puts a `Specials/` directory over a
file whose correct binding is a movie record — `tv.partial` gaps a season and
nothing models a special at all. So the oracle scored this change perfect and the
dogfood library, the narrow one whose narrowness caused this whole project,
supplied the only evidence against it.

**That is worth recording on its own.** Every earlier iteration had the dogfood
pair identical and the oracle carrying the argument. This one inverts, and it is
the reason the pair is run every time rather than when a change looks risky.

## Verdict — REVERT

Judge rule: any instrument down. Five correct bindings lost, verified correct by
reading what they bound rather than trusting `ready`. Second revert in thirteen
iterations.

## What a narrowed version needs

**Numbered season directories only** — `Season 12`, `S03` — excluding `Specials`,
`Extras` and the rest. The measured benefit was real (573 rows out of the worst
class) and the measured cost is confined to the catch-all directory.

Two things it needs first, and neither is free:

1. **A way to name "numbered season directory" without restating
   `is_season_directory`.** That predicate is private to `nightjar-db` and
   deliberately includes `Specials`. A narrower one written in the scanner is a
   second predicate about the same naming convention, which is how a reimplemented
   `norm_key` gave 25 against the shipped chain's 12. The honest route is to expose
   the distinction from `nightjar-db`, where the rule lives.
2. **An oracle shape holding a special whose correct binding is a movie record.**
   Without it the next attempt is measured by an instrument blind to the only
   failure mode found so far — and the dogfood library has just 5 such files, which
   is a thin guard for a rule this broad.

Until both exist, the 573 stay. They are the worst class in the suite and they are
not worth 5 correct bindings plus a rule whose only known counter-example is
unmeasurable.
