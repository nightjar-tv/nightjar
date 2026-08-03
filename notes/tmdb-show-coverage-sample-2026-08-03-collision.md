# 50-show TMDB coverage — after ADR-0032 episode-title pin

**Date:** 2026-08-03 (post-#38)  
**Harness:** local `show_coverage_sample` with reference-episode fields wired  
**Raw JSON:** `notes/tmdb-show-coverage-sample-2026-08-03-collision.json`  
**Baseline soft-key:** `notes/tmdb-show-coverage-sample-2026-08-03-softkey.md` (47/50)

## Results

| Metric | Soft-key | After #38 (+404-as-miss) |
|---|---:|---:|
| Distinct shows (product libs) | 712 | 712 |
| Matched (≥0.80) | 47 | **50** |
| Below floor | 3 | **0** |
| Errors | 0 | **0** |
| Match rate | 0.94 | **1.0** |

### Former residues

| Soft key | Ref episode | Method | TMDB | Notes |
|---|---|---|---|---|
| will and grace | S01E02 `A New Lease on Life` | `exact_title_episode_title` @ 0.90 | Will & Grace (4454) | Pin as designed |
| shameless | S01E02 `Frank the Plank` | `exact_title_episode_title` @ 0.90 | Shameless (34307, US) | Pin as designed |
| top gear | S10E04 `Botswana Special` | `exact_title_episode_title` @ 0.90 | Top Gear (45) | Distinctive mid-season title; not the Episode-N honesty case |

Method mix in the 50: exact_title 33, episode_count 10, library_year 4,
episode_title 3.

## Dogfood bug found and fixed

First run: Top Gear hard-errored — ureq 404 on
`/tv/7038/season/10/episode/4` (a tied candidate lacking that episode)
propagated as `ResolveError::Provider` and aborted the group. Missing
episode on a tied candidate must be `Ok(None)` for that row (decline that
candidate), not a show-level failure. Fix: optional GET for episode names;
scrub `api_key=` from ureq error text so Status URLs do not leak the key.

## Verdict

On this 50-show sample, ADR-0032 step 4 clears the soft-key residues.
Top Gear pinning here is expected once the reference is a real special
title; the ADR honesty check (placeholder vs placeholder) still stands
for libraries whose only usable refs are `Episode N`.
