# Iteration 1 — `movie.noyear`: the fix flow keeps the pick the floor threw away

**Kept.** `d70b729`.

## The population

705 rows, counted in the instrument that judges them — `movie.noyear`, 1,712
measured, 1,006 correct, 1 wrong, **705 absent**, at this base. ADR-0048 reports
every one of the 705 as `exact_title_collision_unpinned`: several real distinct
films share the title, and neither the filename (`Name/Name.1080p.BluRay.mkv`)
nor the folder carries a year.

## The convention it depends on, and what it does without it

**The provider returning its results in a meaningful rank order.** ADR-0048
measured TMDB's top-ranked exact candidate right 88.9% of the time on this
population, against `vote_count` at 80.1% and `popularity` at 77.6%.

Without that order the suggestion degrades to "the first row of an arbitrary
list" — no worse than what the fix flow shows today, which is that same list
unannotated, but the 88.9% would not carry. Nothing in this change asserts the
order is good; it reuses the scorer that already consumes it.

**It does not depend on any filename convention.** The route never reads a
filename; it reads `media_items.title` and the search response.

## The prediction, made before running

**No oracle row moves, on any shape.** A below-floor suggestion still scores
`absent`, and the drain never calls `search_candidates` — `MetadataSource::resolve`
is the drain's route and this is the fix flow's. `nightjar-core` is untouched, so
the corpus and the sweep are insensitive by construction. The dogfood pair drains
and never opens the fix flow, so it is insensitive too.

So **every instrument was predicted to read zero, and every zero here is
insensitivity rather than a clean result.** That is exactly what ADR-0048 says
option B costs: "B moves no oracle row — this record cannot be validated by
`correct%` and should not be."

## The change

One mechanism. `fix::search_candidates` already re-runs the search; it now also
runs the **shipped** `score_search` over those hits and marks the id it returns.

    fn suggested_candidate(hits, title, year, kind) -> Option<i64> {
        match kind {
            SearchKind::Movie => score_search(hits, title, year, kind).map(|c| c.tmdb_id),
            SearchKind::Tv => None,
        }
    }

Below the floor that id **is** the `exact_title_collision_unpinned` pick — the
first non-empty exact candidate in provider order, scored 0.72 and declined by
the 0.90 floor. The route keeps it instead of discarding it.

**Not a second rule.** Reimplementing "the top-ranked exact candidate" beside the
scorer is the reimplemented-`norm_key` trap; calling the scorer is what makes the
suggestion the same object the floor rejected rather than a new opinion about it.

**Movies only, and the reason is evidence.** The 88.9% is a `movie.noyear`
number. A show scored on this route would be scored *without* `folder_seasons`,
without candidate detail counts and without a reference episode title — the three
things ADR-0047's ladder actually decides a show on. That would be a different
and unmeasured signal wearing the same badge, so TV gets no suggestion and a test
says so.

**It never binds.** `assign` takes the id in the request body and nothing else;
no caller in the crate reads `suggested`. The flag is a rank.

## What was measured

| instrument | before | after | can it see this change? |
|---|---|---|---|
| oracle, 79,382 rows | 58,186 correct / 573 `wrong.kind` / 10 `wrong.entity` / 8 `wrong.unk` / 20,593 absent / 12 stalled | **byte-identical per-shape table; 0 rows moved** | **no** — drain-only |
| oracle, row-level join | — | `rows joined 79382, verdict changed 0, same verdict different entity 0` | |
| dogfood strict pair | `groups=3220 ready=24953 unmatched=51 errors=0 requests=0` | identical on every counter | **no** — drain-only |
| parser corpus | 71.0% (524/738) | not re-run | **no** — `nightjar-core` untouched |
| parser sweep | 0 gains / 0 regressions at base | not re-run | **no** — `nightjar-core` untouched |
| `cargo test` | see below | see below | yes, and it does |

The per-shape table diffed byte-for-byte against the base, and
`notes/loop3/scripts/compare_scored.py` joined the two runs row by row —
because an identical summary can hide one shape losing what another gains, and
a row that rebinds to a different entity without changing verdict. 79,382 rows
joined, nothing moved.

The dogfood arms were `sha256`'d distinct (`8fdbcaf7…` control, `44d4c7df…`
treatment), separate target directories, `NIGHTJAR_REPARSE=1` and
`NIGHTJAR_TMDB_CACHE_STRICT=1` both, `errors=0` and `requests=0` both, cache
8,185 before and after.

## Tests

Two new, both passing. The first asserts that the fixture really is the
population this serves — `score_search` must **not** meet the auto-match floor
on it — and then that the suggestion equals the scorer's own `tmdb_id`. The
second asserts a TV search carries no suggestion *and* that the scorer does
answer for TV, so the abstention is this route's decision rather than an empty
result.

`candidates_from_hits` was split out of `search_candidates` for those tests to
reach: `search_candidates` takes a concrete `TmdbClient`, so nothing can call it
without a provider, and a flag that is right where it is computed and dropped
where it is mapped is the exact gap that would otherwise go unchecked.

Full suite: 743 pass, 3 ignored, **one failure** —
`hls::tests::mapped_real_library_end_moov_mp4_copy_keeps_aac`. It passes alone
three times on this tree and **fails identically at the base** when the
`transcode` package runs its 158 tests in parallel. Not this change; it cannot
reach `transcode`.

`cargo fmt --check` and `cargo clippy --all-targets -D warnings` both green.

## Judgement

Kept. The instrument that would show a regression showed none, and the
instruments that read zero were predicted to read zero for a stated reason.
**Nothing here shows the suggestion is right 88.9% of the time** — that number is
ADR-0048's, measured from cached search responses, and this change did not
re-derive it. What is shown is that the id offered is the id the shipped scorer
picked, and that nothing binds on it.

## What this could not measure, named

- **Whether a user is better off.** The oracle has no fix flow: no manual match,
  no rescan, no second pass. The cost B changes is the cost of the correction,
  and no instrument here models it.
- **Whether the fix flow's query is the drain's query.** It found one divergence
  and left it: the drain reads `it.year.or(folder_year)` and this route reads
  `year_from_path(..).or(item.year)`. For `movie.noyear` both are `None`, so the
  705 are unaffected — but for a movie with a year in one place and a different
  year in the other, the two clean to different titles and search different
  strings. That predates this change and is noted in the code, not moved.
- **A caller-supplied `q`.** The suggestion is scored yearless there, because a
  typed query carries no year that stands for the file. Unmeasured either way.
