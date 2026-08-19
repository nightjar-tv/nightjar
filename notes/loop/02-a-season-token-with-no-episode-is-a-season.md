# 02 — a season token with no episode is a season

**Kept.** Corpus 337 -> 390 of 738 all-fields (45.7% -> 52.8%). Structure-only
530 -> 576 (71.8% -> 78.0%). Zero corpus cases regressed. One dogfood item
moves; 67 bound items share its folder group and the evidence their binding
rests on is unchanged — read, not run. See the caveat below.

## Mechanism

`find_season_episode` requires an episode marker after the season digits, so a
season pack — `Anon.Show.S02.720p.x264-GROUP`, `Anon Show Season 4`,
`Anon.Show.Stagione.3` — produced no season at all and no title cut either.
`find_bare_season` accepts the season token on its own, sets the season, leaves
the episode `None`, and ends the title at the token.

166 corpus cases failed on season. In 77 the wanted number was already sitting
in the basename.

## Prediction vs actual

Predicted +54 all-fields. Actual **+53**. Structure +46.

The one-case gap: the simulation scored only the fields already failing, so a
case that fails on season while passing on title could lose the title unseen.
The runner is the check and it found no such loss — `newly failing 0`.

## Three guards, each bought by a passing case

1. **A spaced dash-number anywhere means the token belongs to the title.**
   `Anon Anime Show S3 - 12` is the anime absolute form; Sonarr keeps the `S3`
   and twelve corpus cases pass today because Nightjar keeps it too.
2. **A year straight after the number means the `s` is not a marker.**
   `V.H.S.2.2013.LIMITED` is the film V/H/S/2.
3. **`series` is not a season word.** It earns one case and costs a daily show
   (`..._2018_06_22_...` becomes season 2018). It appears 392 times in the
   corpus with no number after it. The words used are the four the corpus
   contains: season (23), temporada (6), stagione (2), saison (1).

A fourth guard has no corpus case behind it and is there on inspection: an
apostrophe is not a token boundary, or `Ocean's 11` is season 11.

## The dogfood item, and the 67 that share its folder

Exactly **1** basename of 25,043 changes:

    Red Dwarf (1988)/Season 9/Red Dwarf S09 Back to Earth DC 1080p ... .mkv
    title  'Red Dwarf S09 Back to Earth DC' -> 'Red Dwarf'
    season None -> 9
    kind   movie -> episode

It is `unmatched` today and it is genuinely Red Dwarf season 9.

Because the kind changes, the item joins the `Red Dwarf (1988)` show-folder
group, and that group's search input moves for **67 items that are `ready`
today**: `episode_count` 67 -> 68, `season_count` 11 -> 12,
`folder_season_counts` gains `(9, 1)`, `library_seasons` gains `9`.

**No replay was run, so this was checked by reading the predicates and the
stored provider payload.** What was checked:

- `metadata_raw_payloads` for tmdb tv `326` declares
  `seasons[] = [0,1..8,9,10,11,12]` with `(9, 3)`. So
  `candidate_covers_folder_seasons`, which is
  `folder_seasons.iter().all(|w| have.contains(w))`, still holds.
- `slots_explained` sums `min(folder files, candidate episodes)` per season and
  contributes nothing for a season the candidate lacks. It is monotone
  non-decreasing in folder seasons; season 9 adds `min(1, 3) = 1`.
- `episode_count_close(68, 73)` is `diff 5 <= max(ceil(73*0.15), 5) = 11`. It
  was close at 67 and is close at 68 — the verdict does not move.
- `season_count` 12 now equals the candidate's 12, so the count pin would fire
  where it did not before. That is a stronger match, not a weaker one.
- The folder has a row in `series` (`Red Dwarf (1988)` -> 326), and a resolved
  folder binds new episodes without searching.

**This is reading, not running.** The strict replay harness is not on this
machine, so the outcome for those 67 items was not observed. Every predicate
their binding depends on was traced and none of them moves against the folder;
that is the strongest statement the available instruments support and it is
weaker than a control pair.

## Season packs after this

Season failures 166 -> 93. What remains is mostly `episode+season` (33) and
`episodes+season+title` (22) — multi-episode and date-based forms, a different
mechanism.
