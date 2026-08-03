# Search-terminal first-screen measure — status audit

**Date:** 2026-08-04
**Branch:** `metadata/two-tier-grid`
**Context:** T9 of the two-tier drain slice (ADR-0026).

## Question

Does the first-screen measure / `grid_measure` treat `matched` as
search-terminal, i.e. does `stop_when_visible_terminal` stop on
`matched | unmatched | ready` — not just `ready | unmatched`?

## Answer: already correct, no code change needed

- `MetadataStatus::is_terminal()` returns `matched | ready | unmatched`
  (`server/crates/metadata/src/queue.rs`).
- `proxy_terminal_progress` counts a unit as terminal unless it still has a
  `pending` item, and buckets `matched`/`ready` units as poster-bearing
  candidates (`ready_missing_poster` gate, ADR-0026 §8.2).
- `DrainOptions::stop_when_visible_terminal` therefore stops once every
  Visible unit is search-terminal, even if enrich has not run yet.
- `grid_measure.rs` `matched` counter is a matcher-confidence counter, not a
  `metadata_status` check — no flag alignment needed.

## Notes for follow-ups

- First screen painted from `matched` units lacks the full-detail poster only
  until `warm_poster_for_matched` (or ADR-0027 store) is wired in; the poster
  path is already captured in the search-tier canonical artwork refs.
- `grid-fast-vs-full-metadata-2026-08-04.md` documents the fast tier at
  100% poster-path capture.
