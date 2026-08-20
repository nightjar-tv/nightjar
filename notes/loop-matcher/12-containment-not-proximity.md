# Iteration 12 — containment, not proximity: the last piece of the collision tier

## The residual, restated

`episode_count_close(le, ce)` asks *is the candidate's episode count near the
folder's file count*. For The Firm the folder holds **10 files of a 22-episode
season 1**; the wrong candidate's season 1 holds about **10**. `close(10, 22)` is
false and `close(10, 10)` is true, so **exactly one** candidate matches — the wrong
one — and `try_pin` pins it.

Proximity rewards a candidate for being the same *size* as an incomplete folder.
That is the same inversion as comparing totals, one level down: restricting the
count to the folder's seasons removed the cross-season form and left the
within-season form untouched.

**There is no count evidence that separates these two.** Both can hold 10 files.
The right outcome is to decline, and the way to reach it is a predicate that both
candidates satisfy.

## Why not `candidate_eps >= folder_files`

Because that is **the containment test refuted on 2026-08-18**, and
`slots_explained`'s docstring records why: compared on totals it inverts on a
folder holding *more* files than the entity has — `Firefly`'s 14 files against an
11-episode entity failed it while `Firefly Lane` passed.

The safe form is per-season with min-capping, which is exactly `slots_explained`:
a folder season with more files than the entity's season contributes the entity's
count and never disqualifies it.

## The change

The episode-count discriminator matches when the candidate **explains the whole
folder** — `slots_explained(shape, library) == library.episode_count` — instead of
when its count is *near* the folder's.

**With no per-season list, proximity on a comparable total is kept.** That path is
what the three shipped count tests exercise (`long_run_…_over_short_reboot`,
`episode_count_pins_supernatural_shape`,
`season_count_pins_when_episode_count_ambiguous`) — 181 files against candidates of
181 and 12, 311 against 327 and short. Containment on totals would break the
Firefly case those tests do not cover, and refusing outright would silence the rule
where it is right, which is the mistake half two's first cut made.

## Prediction

| | predicted |
|---|---|
| The Firm's 20 rows | **decline → absent**, because both candidates explain all 10 files and `try_pin` sees two |
| `exact_title_episode_count` wrong (616 → after half two, fewer) | **falls further** |
| its correct | **falls too** — containment matches more candidates, so it declines more often |
| net wrong | **down**; net correct **down slightly or flat** |
| overall correct% | **80.1% → 79.5–80.5%** — flat is the expected shape |
| the three shipped count tests | **pass**, via the no-per-season-list fallback |
| tv.shortfolder | **74, unchanged** |

**This one is not expected to raise the rate.** It trades a coin-flip pin for an
absent, so the number that says whether it worked is *wrong down with correct
roughly held* — and if correct falls materially further than wrong, it is not worth
keeping and I will say so.

---

## Two implementations, both wrong before the third

**Containment as equality with the folder's total.** Match when
`slots_explained(shape, library) == library.episode_count`. It broke
`a_close_slots_race_does_not_pick_a_primary`: folder 20 files, candidate A
explains 20, candidate B explains 18, so exactly one matched and it pinned.

**That test is right.** An 18-against-20 difference is a missing special or a
mis-numbered file, and making it decisive is the same brittleness as proximity
through a different door. `primary_by_slots_explained` declines that race
deliberately, with a 2× clear-winner rule.

So the fix was never a sharper predicate. **`slots_explained` already asks the
containment question, and it already runs first.** If it declined, the folder is a
race no count can settle — and the right change is to stop a worse rule overriding
a better rule's declination.

**Third implementation, the one measured:** the count discriminators return `None`
when any candidate carries a per-season list. Where better evidence was available
and declined, worse evidence does not decide. Without a per-season list nothing
better has run, so the counts stay — the path 181 files against candidates of 181
and 12 takes, and the three shipped count tests exercise.

## Measured — and it fails the bar I set

Fully warm, 0 new requests, `provider errors 0`, `stalled 0`, noise floor 0. Three
transitions, every one to `absent`:

    correct              -> absent   836
    wrong.unknownepisode -> absent   698
    wrong.entity         -> absent    60

| | before | after | delta |
|---|---:|---:|---:|
| correct | 59,101 | 58,265 | **−836** |
| wrong.entity | 64 | **4** | −60 |
| wrong.unknownepisode | 698 | **0** | −698 |
| absent | 13,302 | 14,896 | +1,594 |
| **correct%** | **80.1%** | **79.0%** | **−1.13 pt** |

`tv.noyear`, `tv.root` and `tv.scene` reach **zero wrong bindings of every class**.
`tv.noyear` 87.4% → 81.1%, `tv.scene` 79.2% → 77.1%, `tv.shortfolder` 65.5% →
56.6%.

**The Firm declined to `absent` exactly as predicted** — the mechanism worked.

## Verdict — REVERT

The prediction said *"if correct falls materially further than wrong, it is not
worth keeping and I will say so."* **Correct fell 836 against wrong's 758.** It
fails the bar, and the oracle rate fell 1.13 points, so it is reverted. Reverting
is a normal outcome and this is the first in twelve iterations.

## What the number actually says, because it is not a clean loss

The gated firings were **52.4% precise** — 836 right against 758 wrong. That is a
coin flip deciding bindings at 0.90 confidence, and removing it eliminates *every
remaining wrong binding* in the three shapes it touches.

So the trade is **758 wrong bindings for 836 unmatched files.** Under
`BLOCK1_LEAVE_BAR`'s own reasoning — a wrong binding triggers fetches and corrupts
watch state, an unmatched file is recoverable in the fix UI — that exchange is
arguably favourable, and a reasonable reading of the leave bar would take it.

**That is a severity-weighting decision, not a measurement one, and it is not a
loop's to make.** The measurement is above; the exchange rate belongs in ADR-0047
as an amendment. Recorded here rather than decided:

- **keep the coin flip**: 80.1%, 762 wrong bindings remain (698 + 64)
- **gate it**: 79.0%, **4** wrong bindings remain, 836 more files unmatched

If the answer is to gate it, the patch is
`~/nightjar-wt-matcher-scratch/adr47-cont.patch` and it applies cleanly to the
half-two tree.

## The residual, now precisely bounded

With the gate reverted, what remains in the collision tier is **762 wrong
bindings, all from a 52%-precise magnitude comparison on partial libraries**, and
no count-based evidence separates the candidates in any of them. Sharpening the
predicate cannot fix it — that was implementation one. The options are to gate it
(above) or to bring evidence that is not a count: the episode-title comparator
already in `LibrarySeriesShape::folder_episode_titles`, measured to distinguish
exactly one candidate in 17 of 20 folders and not consulted by this tier at all.

**That is the next thing, and it is a better lead than either version of
containment.**

---

## Iteration 12b — the gate is applied, and my next lead was wrong

The gate is reapplied on the decision recorded as an ADR-0047 amendment. Measured
twice: **0 of 73,738 rows differ between runs**, which checks the whole pipeline's
reproducibility rather than only the scoring noise floor.

## Correcting the lead I gave

I said `folder_episode_titles` was *"evidence sitting unused"* and that this tier
*"does not consult it"*. **Both halves of that are wrong.**

`confirmation_beats_pick` already runs the wide comparator —
`candidate_confirms_any_episode_title`, all folder titles against all fetched
candidate episode names — after the ladder picks, as promotion or redirect, and it
reaches even the unpinned 0.72 fallback. `sole_candidate_confirmed` uses it too.
The evidence is consulted, one-directionally by construction, and the docstrings
record the measurement behind that: confirmation held on 598 working folders and
refutation never once identified a wrong entity.

**And it cannot rescue the 836 rows the gate cost, for a reason nothing about the
code:**

| shape | filename form | episode title? | correct% |
|---|---|---|---:|
| tv.sonarr | `Show - S01E01 - Title.mkv` | **yes** | **100.0%** |
| tv.flat.titled | `Show - S01E01 - Title.mkv` | **yes** | **100.0%** |
| tv.noyear | `Show - S01E01.mkv` | no | 81.1% |
| tv.root | `Show.S01E01.1080p.WEB-DL.mkv` | no | 81.1% |
| tv.scene | `Show.S01E01.1080p.WEB-DL.x264-GRP.mkv` | no | 77.1% |

**The shapes that carry episode titles are already at 100%. The shapes that fail
are exactly the ones that carry none.** Title evidence is already doing its whole
job; there is nothing for it to work with in the failing population, because those
filenames contain no title to compare.

So the residual is not "evidence unused". It is **the population where the filename
carries neither a year nor an episode title, and the show's name collides.** For
those rows there is no evidence in the filename, and `absent` is the correct
answer, not a shortfall. That is what the gate now returns.

**This also means the oracle cannot test a title-based fix for those shapes** — it
would need a shape carrying titles *and* a collision *and* no year, and the two
shapes that carry titles resolve at 100% without needing one.

I should have checked the filename forms before proposing the lead. The claim was
made from reading the call graph and not from reading what the shapes contain.
