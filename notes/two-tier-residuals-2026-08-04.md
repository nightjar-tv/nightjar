# Two-tier metadata — residuals (do not drop)

**Branch / PR:** `metadata/two-tier-grid` (lands ADR-0026 §8 two-tier + drain split).  
**Recorded:** 2026-08-04  
**Owner:** Phase 3 Block 1 leave track.

These are **follow-ups after merge**, not reasons to reopen TVDB / 0.80 / kids UI.

## R1 — Search tier still may fetch detail HTTP

- **Gap:** Live TMDB search path can still call `movie_detail` / `tv_detail` before status `matched`. Season bind is deferred; full search-only first-screen model (~28 s measure) is not fully realized.
- **Plan:** Separate PR — match → `canonical_from_search_hit` only; enrich does detail-by-id + seasons. Pin shapes for multi-exact may still hit light `/tv/{id}` for counts.
- **Done when:** Counter test: search phase does not full-detail; enrich does; measure note updated.
- **When:** After dogfood if adult T_first_screen still fails or measure shows detail dominates.

## R2 — TV poster warm key vs artwork serve (high priority)

- **Gap:** Drain warms under provisional `tmdb:show:{id}`. Serve path may only resolve `tmdb:movie:` / `tmdb:episode:` (and path), so matched TV posters can 404 despite warm.
- **Plan:** Serve (or warm key) alignment without making `tmdb:show:` a watch key (`effective_item_key` already filters).
- **Done when:** Matched TV → warm → `GET /api/v0/artwork/.../poster` 200 when file exists.
- **When:** **Before claiming dogfood grid paint for shows.** Movies may already work via `tmdb:movie:`.

## R3 — ArtworkStore CDN concurrency

- **Gap:** Global gate serializes downloads; ADR-0027 cap of 8 concurrent CDN does not fully engage.
- **Plan:** Artwork follow-up PR — per-key lock or gate only mkdir/metadata, not HTTP body.
- **Done when:** Warm of N posters approaches min(N, 8) in flight.
- **When:** After leave measures / when poster **bytes** (not path-known) gate first screen.

## R4 — Leave measures + CONTINUITY (ops)

- **Gap:** N150 / CONTINUITY still describe pre-merge spine and ready-only first screen.
- **Plan:** L0 redeploy tip after merge; re-queue pin-order residue; publish req/1k (with seasons), unmatched %, adult **search-terminal** T_first_screen; refresh CONTINUITY “where things stand.”
- **When:** Immediately after merge to `main`.

## Sequencing

```
Merge two-tier
  → R2 (TV art serve) ASAP if shows grid matters
  → R4 dogfood + leave measures
  → R1 if first screen still slow
  → R3 artwork concurrency later
```

## Explicitly not residuals here

Kids ladder UI (Block 2), Fix UI (Block 3), TVDB, palette/blurhash, retune 0.80.

## PR honesty line

v1 two-tier defers season bind and splits status; live search may still fetch show/movie detail before `matched`. TV poster serve for `tmdb:show:` warm keys is a known follow-up (R2).
