# ADR-0006: Phase 2 playback decision engine and remux delivery

- Status: accepted
- Date: 2026-07-25

## Context

Phase 1 plays H.264 + AAC in MP4 only. Most household libraries are MKV with
the same codecs, which needs a container change and nothing else. Full
transcoding (HLS, hardware encoders, re-encode) is the rest of Phase 2; the
decision shape and the delivery API are the irreversible parts (Rule 6.1), so
they are decided here before code.

## Decision

1. **Decision engine.** A pure function in `nightjar-core`:
   (probed streams × client capability profile) → `directPlay | remux |
   transcode`, with a human-readable reason. The seed profile is the Phase 1
   browser whitelist: H.264 family + AAC in MP4/M4V is direct play, the same
   codecs in another container is remux, everything else (including pending or
   failed probes) is transcode. Profiles become the compatibility contract for
   clients later in Phase 2; the function signature takes the profile from day
   one so that arrival is additive.
2. **Remux is an async job**, following the ADR-0004 scan-job pattern. `POST
   /api/v0/items/{itemId}/remux` starts (or reuses) a background stream-copy
   and returns 202 with the current state; clients poll
   `GET .../playback-info`, which reports `remuxState: notStarted | preparing |
   ready | failed`. No HTTP request ever waits on FFmpeg. A 30 GB MKV
   stream-copies for minutes and browsers and reverse proxies time out long
   before that.
3. **Retry is part of the contract.** Concurrent remuxes are capped (2). When
   all slots are busy a POST returns `notStarted` with a busy reason, and the
   client re-POSTs on each poll tick while it sees `notStarted`. Without the
   re-POST a third simultaneous viewer would wait forever.
4. **Delivery.** `ffmpeg -c copy -movflags +faststart` writes to
   `{NIGHTJAR_DATA_DIR}/cache/remux/{itemId}-{mtimeMs}-{sizeBytes}.mp4` via a
   `.tmp` rename. The existing `GET .../stream` endpoint serves the cache file
   with the same Range machinery as direct play, content type `video/mp4`.
   Streaming before the file is ready returns a structured 409.
5. **The cache is capped.** `NIGHTJAR_REMUX_CACHE_BYTES` (default 10 GiB).
   Before a job starts, ready files are evicted oldest-served first (file
   mtime, touched on serve) until the source size fits; in-flight files are
   never evicted. A source larger than the cap fails the job with a reason
   naming the file and the cap. Remux output is near source size, so an
   unbounded cache would silently double library disk usage, and a full disk
   is where SQLite writes start failing.
6. **Restart semantics.** Registry state is in-memory. On startup the server
   sweeps orphaned `.tmp` files from the cache directory; a killed remux
   leaves one that never matches the ready check and would silently eat the
   cap. Completed files are re-detected by existence.
7. **API break, recorded.** `directPlay` and `needsTranscode` are removed from
   `MediaItem` and `PlaybackInfo`, replaced by a single required
   `playbackMethod`. v0 is explicitly unfrozen (ADR-0004 precedent), and the
   booleans are ambiguous for remux (both false). Deleting now beats carrying
   the ambiguity into v1 (Rule 4.5). `streamUrl` is present only when the item
   is playable now: direct play, or remux with state `ready`. `mimeType` comes
   from the decision, not the source extension.
8. **FFmpeg** stays a bare `ffmpeg` on `PATH`, spawned exactly like the
   scanner's ffprobe. No wrapper crate, no schema migration in this slice;
   eligibility uses the stored container and codec names only.

This supersedes ADR-0003 §5 ("direct play only") for playback delivery. The
schema and auth decisions in ADR-0003 stand.

## Consequences

Only the first video and first audio stream are mapped (`-map 0:v:0 -map
0:a:0?`), so multi-language MKVs and commentary tracks lose alternate audio
until Phase 2 multi-track audio selection (planned with downmix rules and
capability profiles: `playbackInfo` inventory, stable `trackId`, session
switch model in a follow-up ADR). Subtitle tracks are dropped from the remux
MP4; text tracks are listed and served as WebVTT sidecars (ADR-0010), and
ASS/PGS burn-in remains later Phase 2. Seeking is only available once the
remux is complete; playback of a large title waits for the full stream copy
until the HLS segmenter arrives. LRU age uses file mtime, so a freshly
remuxed but never played file and a just-served old file look similar in
eviction order. Probe failures land in `transcode` and stay unplayable until
real transcoding exists.
