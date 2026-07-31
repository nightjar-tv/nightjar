# ADR-0007: HLS software-transcode sessions

- Status: accepted
- Date: 2026-07-25

## Context

ADR-0006 delivers remux as a whole-file stream-copy. Titles that need
re-encoding (`playbackMethod: transcode`) still cannot play. Gate 2 requires
the corpus to play in the web player and seeks into an untranscoded region to
start under three seconds. Waiting for a whole-file encode fails both criteria
on long titles.

## Decision

1. **Delivery.** Transcode is served as HLS fMP4. Software encode uses
   `libx264` and AAC stereo. Remux stayed the ADR-0006 whole-file cache for
   this slice; unifying remux onto HLS sessions was deferred (two risks in
   one PR). **Closed by [ADR-0011](0011-remux-session-convergence.md):** remux
   is now a copy-mode HLS session, and session reuse / fork-on-scrub are
   removed.
2. **Session API.** `POST /api/v0/items/{itemId}/sessions?startMs=` returns 202
   with `sessionId` and `playlistUrl`. Reuse is keyed by `(itemId, startMs)` with
   a refcount so two browsers at the same encode window share one FFmpeg.
   `DELETE` decrements the refcount and only reaps when it hits zero. Playback-info
   exposes `sessionsUrl`; `streamUrl` stays for byte streams (direct/remux).
3. **Lifecycle (Gate 2: no orphaned FFmpeg).** Concurrent sessions are capped by
   `NIGHTJAR_HLS_MAX_SESSIONS` (default 3, the Gate 2 N100 figure). Idle
   timeout: no playlist or segment request for 60 seconds force-reaps the
   session **regardless of refcount**. Crashed or sleeping tabs never DELETE;
   without idle beating the counter, refs only go up. Explicit DELETE on
   teardown (and `pagehide`) still releases a holder. Startup sweeps leftover
   session dirs. Process kill waits and reaps so no zombies remain.
4. **Seek = restart or fork, retain prior segments.** A lone holder may restart
   in place via playlist `?startMs=` or a far-ahead segment fetch (kill FFmpeg,
   `-ss` / `-start_number` / `-output_ts_offset`, do **not** wipe prior-window
   segments — Gate 2: in-flight fetches must not 404 while the new window
   cooks). When refs > 1, that path returns 409; the seeking client POSTs a new
   session at the offset (fork) and DELETEs its prior hold so the ref moves.
   Waiting for the encoder to crawl forward is rejected.
5. **The playlist is a generated VOD, not FFmpeg's.** The server builds a
   playlist covering the probed duration (2s segments, `ENDLIST`) and serves
   it once the first window segment exists. Segment requests that are still
   cooking return 503 (retryable), not 404. Segments live under
   `{NIGHTJAR_DATA_DIR}/cache/hls/{sessionId}/`; disk is bounded by session
   idle/stop cleanup of the whole dir.

   **Amended by [ADR-0020](0020-copy-mode-segment-boundaries.md):**
   playlists follow the producer (EVENT while cooking, real EXTINF,
   time-keyed URIs via a per-run map). Each producer run gets a distinct
   playlist URI; far scrub is `?startMs=` plus attach to that URI, not a
   synthetic full-title grid.
6. **Client.** iOS/iPadOS Safari (and other iPhone/iPad/iPod WebKit) plays
   HLS natively when `video.canPlayType('application/vnd.apple.mpegurl')` is
   non-empty. Desktop Safari and other MSE browsers use hls.js (Apache-2.0);
   see [ADR-0017](0017-desktop-safari-hlsjs.md). Seek handling uses the
   `seeked` event only (not `seeking` ticks) on the hls.js path; native
   iOS also arms a quiet `seeking` timer for land-ensure. Hand-rolling MSE
   is well over Rule 4.4; iOS-only native would fail Gate 2 on desktop.

## Consequences

Hardware acceleration, subtitle burn-in, and multi-bitrate ladders remain
later Phase 2 work. Remux→HLS unification is decided in ADR-0011. First audio
and video streams only (same map as remux). A full disk still fails SQLite
writes; session idle cleanup is the mitigation for this path.

The eventual cap model is a global cap protecting server CPU plus a per-user
cap for household fairness, with the settings UI arriving in Phase 3 alongside
the admin screens. `NIGHTJAR_HLS_MAX_SESSIONS` stays global until users exist
(Rule 4.7). Item-keyed session sharing and fork-on-scrub were removed in
ADR-0011 (one session per POST; seek restarts in place).
