# ADR-0049: What counts as a show folder when the directory is per-episode

- Status: **proposed**
- Date: 2026-08-21
- Supersedes: nothing
- Amends: ADR-0033 Q2, which this record asks to re-decide
- Depends on: ADR-0033 (Q2 folder scope, Q3 identity key); ADR-0046 (one browse
  unit per folder); ADR-0047 (collision evidence, all of which is folder-shaped);
  ADR-0026 §8.6 (the Visible proxy, which keys on the same grouping)
- Gate: Gate 3 — metadata auto-match ≥95% correct
- Census below produced 2026-08-29 by a group-census script over the
  dogfood library, alongside the matcher oracle's `tv.scene` shape. Script,
  method and raw data are maintainer-private

## Context

**One rule answers "what is this file's show folder", and six things depend on
it agreeing with itself.** `show_folder_relpath` walks up from the file and stops
at the first segment that is not `Season N/`, `Specials/`, `Extras/` or `SNN/`.
Whatever it returns is:

- the resolve group's key (`episode_group_key`),
- the `series` row's key (ADR-0033 Q3, `(library_id, relpath)`),
- the browse unit (ADR-0046),
- the `LIKE` in `folder_titles_from_db`,
- the source of the folder year (`year_from_show_folder`),
- and the denominator of every count ADR-0047 weighs — episode count, season
  count, per-season counts, season coverage, slots explained.

A per-episode scene release folder is not a season directory, so it is a show
folder:

    Some.Show.S01E01.1080p.WEB-DL.x264-GRP/Some.Show.S01E01.1080p.WEB-DL.x264-GRP.mkv
    Some.Show.S01E02.1080p.WEB-DL.x264-GRP/Some.Show.S01E02.1080p.WEB-DL.x264-GRP.mkv

Ten files, ten show folders, ten series rows, ten browse units, ten groups.

**Note also that ADR-0033 Q2 and the code do not say the same thing.** Q2 says
the show folder is "the **highest** directory under the library root that
contains episodes or season directories". The implementation is the opposite
walk — the deepest directory that is not itself a season directory — and
`queue.rs` already carries a comment saying the earlier wording "would put every
show under a genre or letter folder into one group". Whatever this record
decides, Q2's prose needs correcting to match whichever walk survives.

## The measurement

Counted from the generated paths through the shipped chain
(`show_folder_relpath`, `clean_show_title`, `norm_key`), not from the drain's
`groups=` counter — that counter is cumulative across passes and reports 1,386
groups for `tv.single`'s 693 files.

| shape | files | groups | entities | files per group |
|---|---:|---:|---:|---:|
| tv.sonarr, tv.flat, tv.flat.titled, tv.handmade, tv.root, tv.mixedroot | 5,644 | **698** | 698 | 8.09 |
| tv.episodetitle | 5,643 | 698 | 698 | 8.08 |
| tv.twoseason | 6,278 | 363 | 363 | 17.29 |
| **tv.scene** | **5,644** | **5,644** | 698 | **1.00** |

**One group per file.** Every other TV shape forms one group per show.

What it costs, on the same 2,410-entity population, warmed cache, `requests=0`:

| shape | measured | correct | wrong | absent |
|---|---:|---:|---:|---:|
| tv.flat | 5,644 | 5,644 (**100.0%**) | 0 | 0 |
| tv.scene | 5,644 | 4,349 (**77.1%**) | 0 | **1,295** |

`tv.flat` and `tv.scene` carry the same 698 entities and the same episode
numbers. The difference is the directory layout and nothing else. All 1,295
failures are `unmatched` — the shape fails by declining, not by guessing, which
is the floor working correctly on evidence that has been taken away from it.

## Why a one-file group cannot be matched as well as an eight-file group

Every discriminator ADR-0047 admitted is a statement about a **folder**:

- `library_episode_count` is 1, so the count tier compares a candidate's 62
  episodes against a library's 1 and is meaningless.
- `library_seasons` is one season, so `candidate_covers_folder_seasons` cannot
  discriminate — every candidate covers a single season.
- `folder_season_counts` has one entry with the value 1.
- `folder_episode_titles` holds one title, so
  `candidate_confirms_any_episode_title` gets one chance instead of eight.
- `sole_season_coverer` and `primary_by_slots_explained` — 30 correct and 74
  correct respectively, 0 wrong between them — cannot fire at all.

So this is not a matcher weakness that better ranking would fix. **The evidence
is real and present in the library; the grouping rule throws it away before the
matcher sees it.**

## The decision this record asks for

**When may files in sibling directories share one show folder?**

- **A. Nothing changes.** ADR-0033 Q2 stands, a scene release folder is a show
  folder, and a library named this way matches at 77% where the same files
  named flat match at 100%. This is the status quo and it is a real answer: the
  merge rule Q2 forbids exists to stop `Shameless (US)` and `Shameless (UK)`
  sharing an identity, and that class is worse than an unmatched file.

- **B. A directory that holds exactly one media file and whose name carries a
  season/episode marker is not a show folder** — the walk continues past it, the
  same way it continues past `Season 01/`. Narrow, path-shaped, and it needs no
  title fold, so Q2's "never merge by fold collision" is untouched. It does not
  help a per-episode folder holding two files (a video and an `.nfo` counts as
  one media file; a two-part episode does not).

- **C. Sibling directories whose names differ only in their episode marker are
  one show folder.** Covers more real layouts than B, including multi-file
  release folders. It is a comparison between folder *names*, which is closer to
  the fold-collision merge Q2 forbids, and it needs a rule for what "differ only
  in the marker" means that will be argued about.

- **D. Group by the parsed show title when the folder is per-episode**, the way
  `episode_group_key` already does for root-level files. Consistent with a
  mechanism that already ships and was measured this loop. But it reintroduces
  exactly the library-global title fold Q2 removed, and the D2 class comes back
  with it — `Shameless.US.S01E01-GRP/` and `Shameless.UK.S01E01-GRP/` fold
  together.

**No recommendation is offered here, because the cost is not measured.** The
oracle can say what A costs (1,295 of 5,644, and 0 wrong). It cannot yet say what
B, C or D cost, because no shape generates two *different* shows whose scene
folders would merge under each rule — which is the only number that decides
between them. **That shape should be built before this record is accepted**, the
way `tv.shortfolder` was built before ADR-0047's stronger option could be
weighed, and the way `tv.mixedroot` was built before F5 could be seen.

## Consequences, whichever option wins

**Six consumers must agree, or this makes things worse.** Grouping, the `series`
row key, the browse unit, `folder_titles_from_db`, the folder year and every
ADR-0047 count all read the same answer today. A rule that changes grouping and
not the `series` key produces a group whose identity row does not exist; one that
changes grouping and not browse produces a unit the grid cannot render. That
coupling is the reason this is an ADR and not a patch.

**A migration question.** ADR-0033 Q3 keys identity on the folder relpath, and Q5
already retro-derived rows for existing libraries. Any option but A re-keys every
scene-named folder's series row. Q3 accepts re-keying on a folder rename because
Q6 keeps the row non-watch, so the same argument covers this — but it is a
migration, and this record does not write it.

**What is not in scope.** The 1,295 are `unmatched`, never wrong. Nothing here is
a wrong-binding fix, and an option that trades unmatched files for wrong ones is
worse under `BLOCK1_LEAVE_BAR` however the rate reads.

## What this record could not measure, named

- **How common scene-named show folders are.** The one real library is
  Sonarr-named and has none. `populations.py` answers "how much of the real
  library would this touch" and the answer is zero, which means "not this
  library", never "not anywhere".
- **The cost of every option except A.** Named above; it needs a shape that does
  not exist.
- **Two-part episodes and multi-file release folders**, which decide whether B is
  narrow enough to be useless.
