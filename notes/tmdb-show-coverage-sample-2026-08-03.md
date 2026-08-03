# 50-show TMDB coverage sample (ADR-0031 §7)

**Date:** 2026-08-03  
**Harness:** `metadata-show-coverage-sample`  
**Raw JSON:** `notes/tmdb-show-coverage-sample-2026-08-03.json`

## Purpose

ADR-0031 §7 leaves TVDB open until a coverage sample says whether TMDB
alone covers dogfood shows. This run is that sample. It is also the first
**live** exercise of ADR-0031 §4 credentials (secrets-file key against
TMDB search).

## Method (stated here; ADR names the sample, not the method)

- DB: `~/nightjar-data/nightjar.db`
- `EXCLUDE_TESTDATA=1` → skip libraries `Test Data`, `DV`, `DV2`
- Distinct shows by `clean_show_title` on episode rows (**719** available)
- Sample: **50** largest by episode count (tie-break title)
- Search: `match_search_with_series_shape(SearchKind::Tv, …)` at floor 0.80
- Credentials: `{NIGHTJAR_DATA_DIR}/secrets` field `tmdb_api_key` → source
  reported as `secrets file`

## Results

| Metric | Value |
|---|---|
| Key source | secrets file |
| Auth rejected | **0** |
| Sampled | 50 |
| Matched (≥0.80) | **47** (94%) |
| Below floor | **3** |
| No results | **0** |
| Errors | **0** |
| Coverage hit rate (any TMDB candidate) | **1.0** |
| Elapsed | ~22.5 s |

### Below floor (TMDB returned a candidate; matcher did not auto-match)

| Library title | Score | TMDB title |
|---|---|---|
| Will and Grace | 0.72 | Will & Grace |
| Top Gear | 0.72 | Top Gear |
| Shameless (US) | 0.55 | Shameless |

These are collision / orthography / regional-title cases, not
"missing from TMDB."

## Credential path

Secrets-file override resolved and was accepted by TMDB (no 401/403 named
refuse). Rejection paths remain unit-tested only for the fail cases;
this run validates the **happy** live path.

## TVDB implication

On this 50-show largest-library slice, TMDB returned a candidate for every
show. Nothing in the sample argues for investing in a TVDB terms read on
**coverage** grounds. Below-floor rows belong to the matcher / collision
slice, not a second provider.

Caveat: selection is largest shows first, not anime-absolute or obscure
long-tail stratified. Parse baseline already found zero absolute-numbered
anime files in dogfood. A different sample could still surface gaps; this
one did not.
