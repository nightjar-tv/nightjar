# Grid measure — fast tier (search hit) vs full metadata (drain)

**Date:** 2026-08-04
**Harness:** `metadata-grid-measure` (`server/crates/metadata/src/bin/grid_measure.rs`)
**DB:** `~/nightjar-data/nightjar.db` (24 933 items, all `pending`), `EXCLUDE_TESTDATA=1` (Movies + Shows only)
**Key:** secrets file, `NIGHTJAR_DATA_DIR=$HOME/nightjar-data`

## Method

Two phases over the same group set (band, then newest-first — the production
drain's own order), both through the production limiter (10 req/s, 4 in-flight):

- **Fast tier (Phase A):** one TMDB search per group, scored with the production
  matcher (`score_search_with_shape`); poster path read from the search response
  — the data the two-tier design would write at `matched`.
- **Slow tier (Phase B):** production `drain_pending` on a copy of the DB
  (search + detail + season bind + store) — today's full metadata path.

`SearchHit` gained `poster_path` / `backdrop_path` (serde-default, additive) so
the fast tier can capture them.

## Results

### Run 1 — 12 groups (all TV, newest-first)

| Phase | Wall | Requests | Ready | Seasons | Poster paths |
|---|---|---|---|---|---|
| Fast | **3.1 s** | 12 | — | — | 12/12 (100%) |
| Slow | 44.2 s | 59 | 188 items | 23 | — |

Ratio: **14.0×**

### Run 2 — 60 groups (20 movies / 40 shows)

| Phase | Wall | Requests | eff. req/s | Ready | Seasons | Poster paths |
|---|---|---|---|---|---|---|
| Fast | **16.4 s** | 60 | 3.66 | — | — | 60/60 (100%) |
| Slow | 92.1 s | 285 | 3.09 | 1 238 items, 1 unmatched | 101 | — |

Ratio: **5.6×**. Fast tier: 58/60 auto-matched, 2 below floor, 0 errors, 0 misses.

Per group: fast 0.27 s (1 request); slow 1.54 s (4.75 requests: 1 search + 1
detail + ~2.5 season fetches).

### CDN poster download (additive to both tiers)

12 × `w342` posters (~38 KB each), latency ~0.52 s/request:

| Concurrency | Wall |
|---|---|
| Serial | 6.6 s |
| **8 in-flight (ADR-0027 cap)** | **1.2 s** |

## Full-library extrapolation

Completed dogfood drain counts the real library at **2 422 groups** (1 725
movies + 697 shows), 22 048 non-testdata files.

| Whole library | Fast tier | Full metadata |
|---|---|---|
| Wall | ~11 min (2 422 × 0.27 s) | ~62 min (2 422 × 1.54 s) |
| Requests | ~2 422 | ~11 500 |

First screen (~80 units, ADR-0026 §8 baseline): fast tier ≈ 22 s search + ~6 s
poster download ≈ **~28 s** — inside the 30 s pass bar with **no detail pulls**.

## Findings

1. **The fast tier is latency-bound, not limiter-bound** (3.7 eff. req/s vs
   10/s budget at 4 in-flight). The slow tier is the same — neither is limited
   by the rate limiter; both pay ~0.27 s per TMDB request in latency.
2. **TV share drives the ratio.** All-TV subsets are ~14× (23 season fetches
   dominate: 59 requests for 12 shows); the mixed library is ~5.6×. Season
   pulls are the slow tier's real cost (~101 of 285 requests), and the fast
   tier never needs them.
3. **Poster coverage is 100% of groups** at search time, including the 2
   below-floor groups (search returns the top hit's poster regardless of
   score). Matched coverage (what the two-tier design would paint) is 96.7%.
4. Poster CDN bytes are the same cost in both tiers — the fast tier just knows
   the URL minutes earlier.

## Implications for the two-tier design

- A `matched`-state canonical row carrying search-hit fields + poster/backdrop
  gives a painted poster grid for the whole library in ~1/6 the wall time of
  the full drain, and the first screen inside the existing 30 s pass bar.
- The slow tier remains required for: regional certification (kids fail-closed),
  cast/genres, episode rows (season view), collections.
- No limiter change needed; the fast tier fits the existing 10 req/s budget
  comfortably (it is latency-bound anyway).

## Artifacts

- `server/crates/metadata/src/bin/grid_measure.rs` (bin `metadata-grid-measure`)
- `SearchHit.poster_path` / `backdrop_path` (match_score.rs)
- Measure DB copies: `~/nightjar-data/nightjar-grid-g12.db`,
  `~/nightjar-data/nightjar-grid-g60.db` (same pattern as queue-measure DBs;
  deletable).
