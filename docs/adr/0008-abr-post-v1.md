# ADR-0008: Adaptive bitrate is post-v1

- Status: accepted
- Date: 2026-07-26

## Context

Adaptive bitrate (ABR) means several renditions in a master playlist with the
client switching as network conditions change. That either runs several FFmpeg
processes per viewer or one process producing a ladder. Either shape lands on
the session model that took several rounds of patches to stabilise (session
accounting, cap arithmetic, fork-on-seek in ADR-0007).

v1 already chooses a single rendition server-side from the client capability
profile, and clients can offer manual quality (Auto / High / Original). That
covers most household use. Shipping ABR in v1 would reopen the encode path for
a feature most viewers will not need on a LAN.

Two irreversible choices do belong in v1, because changing them later invalidates
every cached segment and blocks clean ABR switches: segment duration, and
keyframe alignment across any future renditions.

## Decision

1. **ABR is post-v1.** v1 ships one rendition per playback, selected server-side
   from the client capability profile, plus manual quality selection in clients.
   Automatic quality adjustment for changing network conditions is a Later
   roadmap item, not committed work.

2. **Segment duration is locked at 2 seconds for encoded renditions.** The HLS
   media playlist for transcode (and any future ABR ladder) uses a 2s target
   duration (`SEGMENT_MS = 2000` in the transcode crate). Every encoded
   rendition must use the same duration. Changing it later means re-encoding
   the cache. The FFmpeg `-force_key_frames` expression, `-hls_time`, and
   playlist `EXTINF` / `TARGETDURATION` on that path are all derived from that
   constant so the locked value has one owner (Rule 4.9).

   **Amended by [ADR-0020](0020-copy-mode-segment-boundaries.md):** producer
   owns playlist `EXTINF` / `TARGETDURATION` for every session mode. Copy
   boundaries come from source keyframes; transcode still hits the 2 s grid
   because §3 forces IDRs there.

3. **Keyframes are time-based, not frame-count.** The encoder forces an IDR on
   every `SEGMENT_MS` boundary (today: `expr:gte(t,n_forced*2)` when the
   constant is 2000), with scenecut disabled. A frame-count `-g 48` is only 2s
   at 24 fps; at 60 fps it splits every 0.8s, and VFR sources drift. Identical
   GOP alignment across renditions is what makes ABR switches clean; locking it
   now is the condition ABR later depends on. This is the mechanism that makes
   §2 true for transcode; copy cannot satisfy it (ADR-0020).

4. **Playlist URLs stay additive** for media/master paths and for encoded
   segment grids. The playlist served today is a media playlist, not a master
   playlist. Adding a master playlist above it later must not change existing
   segment or playlist URLs. The URL shape stays rendition-addressable (a
   single-rendition path a variant list could reference), not a shape that
   assumes exactly one rendition forever.

   **First test (2026-07-26, ADR-0010 HLS subtitle renditions).** The condition
   held. `index.m3u8` remains the media playlist (same path, same VOD body,
   same segment URIs). The master was added at a new path, `master.m3u8`, and
   session `playlistUrl` points there. Seek via `?startMs=` works on both the
   master and the media playlist. Turning `index.m3u8` into a master and moving
   media to a new path was rejected: that would have changed a URL clients
   already fetch, which is not what “additive” meant.

   **Amended by [ADR-0020](0020-copy-mode-segment-boundaries.md):** segment URI
   shape moves from derived index (`segNNN.m4s`) to time-keyed names for all
   session modes. Deliberate: the index mapping was false for copy and only
   accidentally true for transcode. `master.m3u8` stays additive; the media
   playlist URI may include a per-run discriminator so restarts do not mutate
   one EVENT under a live player.

   **Relative URI note (2026-07-31, after ADR-0020 run dirs; applied).** This
   rule was written when master, media, init, and segments lived in one flat
   session directory. Relative hops were free. Per-run masters under
   `/runs/{n}/` made relative resolution the fragile part of the wire: three
   defects in one cutover (segment `../` vs `../../`, subtitle `../../subs/`,
   and a false alarm that bare `index.m3u8` would resolve to session-root).
   The class is closed by a client-shaped link-walk test
   (`session_hls_link_walk_resolves_to_real_bytes`) and by shipping
   path-absolute session URIs from playlist emitters: master → media, MAP,
   segments, and subtitle `EXT-X-MEDIA` URIs are `/api/v0/sessions/…` paths.
   Depth is no longer load-bearing. What absolute paths break: relocating a
   session by rewriting a single path prefix in clients. That trade-off is
   accepted.

5. **Cap accounting is already session-based.** Concurrent transcode caps count
   sessions in the registry, not FFmpeg processes. A future ABR session is one
   logical session with several encode outputs; that model fits the current
   counter. No change required here.

6. **Capability profiles grow in Phase 2.** Profiles gain max bitrate, max
   resolution, and HDR support alongside codecs and containers. ABR later
   selects from a ladder bounded by the same profile rather than inventing a
   parallel mechanism. Prefer HDR passthrough; tone-map only when the client
   cannot take HDR.

## Consequences

The session model in ADR-0007 (per-session directories, fork-on-scrub when shared)
stands until measurement says otherwise. A redesign needs its own ADR with
evidence (seek past three seconds, or dogfooding that shows restart-on-seek CPU
cost is intolerable). Content-addressed caches, encode-ahead schedulers, and
quality ladders are out of scope for this decision.

Manual quality plus a sensible remote default covers remote streaming once
Phase 2 profiles carry a bitrate ceiling and Phase 3 adds per-user remote caps
with trusted-proxy detection. Until local-versus-remote detection lands, any
remote cap is advisory.

VMAF encode-quality gates stay off until there is a representative corpus and a
known `libvmaf` build to run them against.
