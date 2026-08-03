# 50-show TMDB coverage sample — after soft-key fix

**Date:** 2026-08-03 (re-run)  
**Harness:** `metadata-show-coverage-sample`  
**Raw JSON:** `notes/tmdb-show-coverage-sample-2026-08-03-softkey.json`  
**Baseline:** `notes/tmdb-show-coverage-sample-2026-08-03.md` (47/50)

## Change under test

`clean_show_title` soft key only (parser unchanged):

- `&` ↔ `and` (via existing fold)
- hyphen / en-dash / em-dash → space (via existing fold; UTF-8-safe
  `strip_year_parens` so en-dash survives the year strip)
- case fold
- strip regional `(US)` / `(UK)` / `(AU)` / `(CA)` / `(NZ)`
- remaining parentheses → spaces (Inspired Unemployed mid-title parens)

## Results vs baseline

| Metric | Baseline 2026-08-03 | After soft key |
|---|---:|---:|
| Distinct shows (product libs) | 719 | **712** |
| Matched (≥0.80) | 47 | **47** |
| Below floor | 3 | **3** |
| Match rate | 0.94 | **0.94** |
| Coverage hit rate | 1.0 | 1.0 |
| Auth rejected | 0 | 0 |

### Below floor (unchanged count; Shameless score moved)

| Library soft key | Score | TMDB title | Notes |
|---|---:|---|---|
| will and grace | 0.72 | Will & Grace | Soft keys equal; 0.72 is multi-exact collision, not fold |
| top gear | 0.72 | Top Gear | Same — multi-version collision |
| shameless | 0.72 | Shameless | Was `Shameless (US)` at 0.55; regional strip helped score, still below floor (US/UK collision) |

## Verdict

Soft-key normalisation did what it was for: near-dup merge (719→712) and
honest `&` / regional / case / dash equivalence. It did **not** move the
50-show auto-match rate off 47/50. The remaining misses are collision-pin
territory, not orthography.
