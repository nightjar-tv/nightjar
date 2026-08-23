# Iteration 2 — F5: a root group read the whole library's episode titles

**Kept.** `5c52e01`. Every wrong binding outside `movie.*` and `tv.episodetitle`
is gone, and nothing traded for it.

## The population

Counted in the instrument that judges it. `tv.mixedroot` renders one library
holding **both** loose episode files at the root and shows in their own folders;
the shape total hides which half a row is in, so
`notes/loop3/scripts/mixedroot_population.py` splits it on the row's own path.

| half | rows | correct | absent | stalled | wrong.entity | wrong.unknownepisode |
|---|---:|---:|---:|---:|---:|---:|
| root | 2,885 | 2,806 | 53 | 12 | **6** | **8** |
| foldered | 2,759 | 2,759 | 0 | 0 | 0 | 0 |

**Every non-correct row in the shape is in the root half.** The foldered half is
2,759 of 2,759. So the population at risk is 79 rows, of which **14 are wrong
bindings** — and at this base those 14 were every wrong binding in the whole
suite outside `movie.*` and `tv.episodetitle`.

## The mechanism, and that it can be reached

`folder_titles_from_db` builds `LIKE '{show_folder}/%'`. A file directly in the
library root has no show folder — `show_folder_relpath` returns `""` for it — and
the empty string became `LIKE '%'`: **every episode in the library.**

`usable_episode_titles` does not narrow that. It drops a title that is only the
show's own name and a generic one; a title belonging to a *different* show passes.

The evidence then reaches `candidate_confirms_any_episode_title`, which compares
**title-anywhere on both sides** — deliberately, because a folder title sits at a
different number on a renumbered candidate — so **one** agreement with a
neighbour's episode title returns `Some(true)`.

`confirmation_beats_pick` then either endorses the ladder's pick or redirects to
another candidate, and **all three arms raise the confidence to `f64::max(conf,
0.90)`** — which is the auto-match floor. A root group could bind automatically
on a title belonging to a show it has nothing to do with.

Reached, not merely reachable: the 14 wrong rows above.

**The line is unchanged from before root-level grouping landed.** What changed is
what it costs. Those groups used to bind wrong anyway, so contaminated evidence
cost nothing; they bind correctly now, and the contamination became live.

## The convention it depends on

`episode_group_key`'s answer to "what groups with this file": same library, no
show folder, same `query_key(clean_show_title(title))`. The fix reads exactly
that key, from the shipped `show_folder_relpath`, `clean_show_title` and
`query_key`, rather than writing a second reading of the same convention beside
them.

**Without a title in the basename it does nothing.** A root file named
`S01E01.mkv` folds to an empty key, and `resolve_episode_group`'s empty-title
refusal already covers that. `tv.root` is the case: its files are
`Show.S01E01.1080p.WEB-DL.mkv`, whose basenames carry no episode title at all, so
`usable_episode_titles` was already returning nothing there and the fix has
nothing to remove. That was predicted before the run, and holding it is what
makes the `tv.mixedroot` movement attributable.

## The prediction, written before the run

`~/nightjar-wt-loop3-scratch/it2-prediction.md`:

- `tv.mixedroot` root half moves; foldered half does not
- `tv.root` **does not move** — no episode titles in its basenames
- every other shape does not move — every file has a non-empty show folder
- dogfood does not move — the real library has no root-level episode file
- corpus and sweep are insensitive — `nightjar-core` untouched
- direction: the 14 wrong rows should fall, **and some correct rows should fall
  to absent**, because confirmation is what lifts a 0.72 pick to 0.90 and
  removing evidence can drop a right answer back below the floor

Five of six held. The sixth — the expected cost — did not appear at all.

## The change

One mechanism: **a root group's folder evidence is the root files that group with
it.**

- `folder_titles_from_db` returns empty for an empty folder instead of falling
  back to `%`, so the old line is **dead rather than unreached**.
- `root_group_episode_basenames` reads every root-level episode file once for the
  pass and buckets it by the key `episode_group_key` gives it. Basenames, not
  titles: the caller still applies `usable_episode_titles` with **its own** soft
  key, so the shipped rejection runs against the right show name.
- The scan is skipped entirely when nothing pending sits at a library root, which
  is the ordinary case. A library organised into show folders pays nothing.

**Once, not per group.** There is no `LIKE` for "the files that group with this
one" — the key is `query_key(clean_show_title(title))`, which SQL cannot compute
— and filtering in Rust per group would keep the per-group full scan the `%`
pattern already cost. One pass serves every root group in every library, so this
also removes a full `media_items` scan per root group.

## What was measured

Oracle, same 2,410 entities and 79,382 rows, warmed cache, `requests=0`, noise
floor **0 rows of 79,382** on both runs.

**Measured twice, on two harnesses.** Mid-iteration the replay harness was found
to derive the stored title with `parse_filename` alone rather than with the
shipped `stored_title` (see note 00). Both arms were re-run on the corrected
harness; the numbers below are the corrected ones, and the delta is identical on
both — the same 26 rows, the same shape, the same direction.

| verdict | base | iteration 2 | delta |
|---|---:|---:|---:|
| correct | 63,830 | **63,856** | **+26** |
| absent | 14,949 | 14,949 | 0 |
| `wrong.kind` | 573 | 573 | 0 |
| `wrong.entity` | 10 | **4** | **−6** |
| `wrong.unknownepisode` | 8 | **0** | **−8** |
| stalled | 12 | **0** | **−12** |
| provider errors | 2 | **0** | −2 |

**Every wrong binding outside `movie.*` and `tv.episodetitle` is now zero.** The
4 remaining `wrong.entity` are one each in the four `movie.*` shapes; the 573
`wrong.kind` are item 3.

Row-level join over all 79,382 rows — because an identical summary can hide one
shape losing what another gains:

    rows joined: 79382
    verdict changed: 26
    same verdict, different entity: 0
       tv.mixedroot   stalled              -> correct   12
       tv.mixedroot   wrong.unknownepisode -> correct    8
       tv.mixedroot   wrong.entity         -> correct    6

**Every moved row moved to `correct`, and every one is in `tv.mixedroot`.**
Nineteen other shapes are byte-identical, `tv.root` among them. Nothing rebound
within a verdict. `tv.mixedroot`'s root half is now 2,832 correct and 53 absent,
with no wrong binding and no stall.

The 12 stalls are worth naming: a stall is a cache miss, and a cache miss here
means the matcher asked the provider for something the dogfood drain never
fetched. Contaminated confirmation was sending it to walk candidates it had no
business walking. Removing the contamination stopped the requests — which is also
why `provider errors` fell to zero.

| instrument | base | after | reaches this change? |
|---|---|---|---|
| oracle | 63,830 correct / 10 wrong.entity / 8 wrong.unk / 12 stalled | **63,856 / 4 / 0 / 0** | **yes** |
| dogfood strict pair | `groups=3220 ready=24953 unmatched=51 errors=0 requests=0` | identical on every counter | yes, and reads flat — see below |
| parser corpus | 71.0% (524/738) | **71.0% (524/738)** | links `nightjar-metadata`, but not the changed code |
| parser sweep | 0 gains / 0 regressions at base | not re-run | **no** — `git diff origin/main -- server/crates/core` is empty |
| `cargo test` | see below | 262 metadata tests pass, +1 new | yes |

**The dogfood zero is explained, not assumed.** The real library has no
root-level episode file — that is why `tv.mixedroot` had to be generated in the
first place — so the changed branch never executes there. The pair still earns
its place: it proves the two binaries differ (`8fdbcaf7…` control, `156c245a…`
treatment), that both ran offline (`requests=0`), that both were error-free
(`errors=0`), and that the added scan did not disturb a 25,004-file drain.

## Tests

One new, and it was checked for sensitivity rather than assumed to have it. With
the guard removed, the old code returns

    ["Blame it on the Rain", "Cold Open", "Pilot", "Spree"]

for a root group in a library holding one foldered show — the last two belong to
the neighbour. With the guard in place it returns the group's own two. The test
also asserts `folder_titles_from_db(conn, 1, "", …)` is now empty, so the route
that produced the defect cannot be reached with an empty folder again.

Full suite: every crate green except `transcode`, which fails
`hls::tests::mapped_real_library_end_moov_mp4_copy_keeps_aac` — **identically at
the base**, and only when the package's 158 tests run together. My diff cannot
reach `transcode`.

**A disk-full episode is worth recording, because it produced a fake regression.**
Mid-iteration the volume hit 387 MiB free of 228 GiB and the `transcode` suite
went from 1 failure to 8 at head and 20+ at base — a difference that looked like
a result and was the machine. After freeing ~950 MiB of this loop's own scratch,
both trees returned to the same single known-flaky failure. The volume is still
at 100%; see the closing note.

`cargo fmt --check` and `cargo clippy --all-targets -D warnings` green.

## Judgement

**Kept.** The instrument that can see it moved 26 rows and all of them toward
`correct`; 19 other shapes are byte-identical; the wrong class it targeted is now
zero; nothing traded the other way.

## What this could not measure, named

- **A root group whose neighbour's title agreement would have been *right*.** The
  fix removes evidence, and the oracle shows no row losing by it. That is one
  generated population, English names, mostly season 1.
- **The real cost of the removed scan.** The dogfood library never enters the
  branch, so the `any_root_group` gate is exercised only in the false direction
  there. No instrument here times a library that *does* have root-level files.
- **`stored_id_confirmed_by_episode_titles`**, the other consumer of
  `folder_episode_titles`. Root groups have no `series` row — `load_series_rows`
  skips an empty relpath — so it is not reached for them, and nothing here proves
  that by measurement.
