# Iteration 3 — a trailing `N-M` range, refused before it was written

Base `f527198`, on top of `15707ac`. **No code. Refused on the counterexample
search**, which is why there is no commit to revert.

## What it was

`drop-a-trailing-number` is the board's largest row at 22, and the refusal
recorded against it is about the **bare** form. Inside it sits a different
shape — a **range** of two numbers, which is how an anime batch release names
the episodes it covers:

    [ANBU-AonE]_SeriesTitle_26-27_[F224EF26].avi         want SeriesTitle
    Some Anime Show 1-13 (English Dub) [720p]            want Some Anime Show
    [Judas] Some Anime Show 091-123 [1080p][HEVC …]      want Some Anime Show
    [HorribleSubs] Some Anime Show 01 - 119 [1080p]      want Some Anime Show
    [HatSubs] One Series 1017-1088 (WEB 1080p)           want One Series
    Series Title 921-928 [English Dub][1080p][…]         want Series Title

**Six corpus cases, every one a title-only failure**, so none is lost to the N1
season refusal. It would have been the largest single reachable mechanism found
tonight. The rule: an ascending `A-B` of whole digit runs, head must carry a
letter, and the name must state no year — that last guard aimed at
`Fahrenheit 9-11 (2004)`, which the shape otherwise eats.

Both of D1's named counterexamples decline it structurally, which is what made it
look separable:

* `86 - 01` — `01 < 86`, so not ascending, and the head is empty as well.
* `Anime Title 300-nen` — `nen` is not a digit run.

## What the counterexample search found

Run over all three populations before a line was written.

**The spaced form — `A - B` — breaks 182 sweep names and 114 library files.**

    [GRP] Deadpool 2 - 07 [720p][ABCD1234].mkv    ->  Deadpool
    [GRP] Big Hero 6 - 07 [720p].mkv              ->  Big Hero
    [GRP] Despicable Me 3 - 07 [720p].mkv         ->  Despicable Me

**That is D1's own population, reached from the other side.** 110 of the sweep's
2,332 real titles end in a bare number — `300`, `Deadpool 2`, `Apollo 13`,
`District 9`, `1883` — and the anime form puts ` - 07` after the title. `2 - 07`
and `01 - 119` are the same string shape. The 420 broken bound titles the refusal
was measured against are exactly these.

And in the library, 114 files of the form

    24 - 2x01 - Day 2 - 8-00 A.M. - 9-00 A.M - Bluray-1080p.mkv

where `2 - 8` reads as an ascending range and the title becomes `24 - 2x01 - Day`.

**The glued form — `A-B` — still breaks 12 library files.** Narrowing to a glued
dash drops `01 - 119` from the gains and does not save it:

    Chernobyl - 1x01 - 1-23-45              -> Chernobyl - 1x01 -
    Ted Lasso - 3x03 - 4-5-1                -> Ted Lasso - 3x03 -
    Star Trek - Voyager - 5x23 - 11-59      -> Star Trek - Voyager - 5x23 -
    Grey's Anatomy - 14x09 - 1-800-799-7233 -> Grey's Anatomy - 14x09 -
    Them - 2x05 - Luke 8-17                 -> Them - 2x05 - Luke
    The Resident - 2x01 - 00-42-30          -> The Resident - 2x01 -

**An episode title that is a pair of numbers is not a batch range**, and there is
nothing in the string that separates them. In the corpus the same shape collides
with the multi-episode markers `S02E03-04-05`, `S01E01-02-03`, `Episode 05-06`,
`afl.2-3-4` and the episode titles `6-50 to SLC` and `2-45 PM`.

Those twelve go through the **episode** arm, so placing the cut in the movie arm
alone would have spared them. That is placement luck, not a property of the rule,
and it is not a reason to ship one.

## Refused, and recorded

**This is `drop a trailing number` reached sideways.** The board says do not, and
the measurement agrees rather than merely the instruction: the gain is 6 corpus
cases and the cost is 182 generated names and 114 real library files in the
spaced form, 12 in the glued one.

Refused with the titles that caused it: **`Deadpool 2`, `Big Hero 6`,
`Despicable Me 3`, `Code 3`, `District 9`** in the sweep — and
**`Chernobyl - 1x01 - 1-23-45`, `Ted Lasso - 3x03 - 4-5-1`,
`Star Trek - Voyager - 5x23 - 11-59`, `Grey's Anatomy - 14x09 - 1-800-799-7233`,
`24 - 2x01 - Day 2 - 8-00 A.M. - 9-00 A.M`** in the library.

**`#NN` is still a different rule**, as `classify.py` records, and it is still
open: `[Shark-Raws] Series Title #957` is one corpus case that can reach a pass
and `221205 ABC123 17研究所！ #17` is a second that cannot, because it also wants
season 1. One case is not a slice, so it was not taken tonight either.

## What this cost, and what it bought

No commit, no gates, no revert. One population count and one counterexample
search, and the search is the only reason the loop did not spend an iteration
shipping and reverting a rule the board had already refused under another name.
