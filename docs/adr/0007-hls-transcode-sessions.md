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
   `libx264` and AAC stereo. Remux stays the ADR-0006 whole-file cache for this
   slice. Unifying remux onto HLS sessions is a candidate once sessions are
   proven; it is not done here (two risks in one PR).
2. **Session API.** `POST /api/v0/items/{itemId}/sessions` returns 202 with
   `sessionId` and `playlistUrl`. A second POST for the same item reuses the
   live session (refcount++) so two browsers do not burn two slots on one
   title. `DELETE` decrements the refcount and only reaps FFmpeg when it hits
   zero. Idle timeout still force-reaps abandoned sessions. Clients fetch
   `GET /api/v0/sessions/{sessionId}/index.m3u8` and the init/segment files it
   references. Playback-info for transcode exposes `sessionsUrl` (the POST
   target). `streamUrl` remains for byte streams only (direct play and remux).
3. **Lifecycle (Gate 2: no orphaned FFmpeg).** Concurrent sessions are hard-capped
   (default 3). Idle timeout: no playlist or segment request for 60 seconds
   kills FFmpeg and deletes the session directory. Explicit DELETE on player
   teardown does the same. Startup sweeps `{NIGHTJAR_DATA_DIR}/cache/hls/` of
   leftover session dirs. Process kill on session end waits and reaps so no
   zombies remain.
4. **Seek = restart at offset.** A seek beyond the currently encoded window
   kills FFmpeg and restarts with `-ss` at the seek target, wiping that
   session’s segment dir and regenerating the playlist. Waiting for the
   encoder to crawl forward is rejected; restart-at-offset is what meets the
   Gate 2 three-second seek criterion. The client signals seek via the
   playlist query `?startMs=`; a changed offset triggers restart.
5. **Segments are bounded.** Each session writes under
   `{NIGHTJAR_DATA_DIR}/cache/hls/{sessionId}/`. FFmpeg runs with a finite
   `hls_list_size` and `delete_segments` so the directory is a sliding window,
   not an unbounded grow. Same disk-full reasoning as the remux cache.
6. **Client.** Safari plays HLS natively when
   `video.canPlayType('application/vnd.apple.mpegurl')` is non-empty. Other
   browsers use hls.js (Apache-2.0). Hand-rolling MSE and playlist parsing is
   well over the Rule 4.4 line; Safari-only would fail Gate 2.

## Consequences

Hardware acceleration, subtitle burn-in, multi-bitrate ladders, and remux→HLS
unification remain later Phase 2 work. First audio and video streams only
(same map as remux). A full disk still fails SQLite writes; the sliding window
and session idle cleanup are the mitigations for this path.
