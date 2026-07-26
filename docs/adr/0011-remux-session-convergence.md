# ADR-0011: Remux converges onto the session model

- Status: accepted
- Date: 2026-07-26

## Context

ADR-0006 delivered remux as a whole-file MP4 stream-copy behind
`remuxState` polling. ADR-0007 delivered transcode as HLS sessions and left
remux whole-file "for this slice." Measured dogfooding on the household NAS
showed the whole-file path fails the start-in-seconds bar that sessions
already meet, and that two video delivery paths (plus two caches and two
subtitle-warm triggers) were not earning their keep (Rule 4.11).

## Decision

1. **Whole-file remux is removed.** Remux is a playback session that produces
   HLS fMP4 with `-c copy` instead of re-encoding. The decision engine still
   returns three outcomes (`directPlay | remux | transcode`); remux now means
   "codecs OK, container needs a session," not "wait for a cached MP4."

2. **API removals (v0 unfrozen).** `POST /api/v0/items/{itemId}/remux`,
   `PlaybackInfo.remuxState` / `remuxError`, and the remux arm of
   `GET .../stream` are deleted. Remux items expose `sessionsUrl` like
   transcode. Clients start a session; there is nothing to poll for remux
   readiness.

3. **One session per POST.** Session reuse by `(itemId, startMs)` and the
   fork-on-scrub 409 path are removed. Each POST creates a session; seek via
   `?startMs=` restarts that session in place. This is the simplification
   named in ADR-0007 consequences, taken now that remux joins the same path.

4. **Subtitle warm at session start.** Embedded WebVTT warming runs when any
   session starts (remux or transcode). The remux-job warm path dies with the
   remux registry.

5. **Caches.** `cache/remux/` and `NIGHTJAR_REMUX_CACHE_BYTES` go away. HLS
   session dirs remain the only video segment cache; subtitle cache is
   unchanged (ADR-0010).

## Evidence (NAS benchmark)

Timings below are from the household NAS (~15 MB/s sustained). A local-disk
comparison was not measured: the magnitude is environment-specific; the
direction is not. A design that only works on local-disk libraries is not
shippable for this audience.

| Title size | Whole-file remux first-play wait | Copy-mode session first segment |
|---|---|---|
| 8.98 GiB (The Holiday) | 645 s | 0.20 s |
| 7.70 GiB (Project Hail Mary) | ~360 s | 0.11 s |
| 11.15 GiB (It's a Wonderful Life) | could not remux (over 10 GiB cap) | 9.10 s |

Cached second play after a completed remux was 2.8–6.0 s, but the 10 GiB cap
holds one film rather than two, so that win was largely theoretical at real
library sizes: eviction often lands before a second viewer arrives. Subtitle
warm did not complete inside the remux window on either finished title.

Post-convergence numbers above are from `POST .../sessions` to the first
servable `seg000.m4s` on the same NAS and titles, with
`videoEncoder`/`encoderKind` = `copy`.

The 9.10 s for the 11.15 GiB title is not a container-parse limitation of large
files. Re-measuring four times put the same title anywhere from 0.65 s to
7.31 s, and the 8.98 GiB title that first read 0.20 s later read 7.21 s under
NAS contention. First video packet sits at byte 8166 (header at the front), so
first-segment latency tracks NAS read contention, not moov size. The old
whole-file wait (minutes) came from copying the entire file before playback;
the session path pays only for the first segment's source reads, so it stays in
the low seconds regardless of file size, jittering with the NAS rather than
with the title. No large-file limitation to document beyond NAS throughput.

## Consequences

**Lost.** Byte-range seeking simplicity on a finished MP4; the instant cached
second play; and the relative calm of a job that was not a live session.
Session lifecycle surface (idle reap, seek restart, client DELETE) is now on
the remux path too — the same surface that took several rounds of patches to
stabilise for transcode.

**Gained.** One video delivery path instead of two. One subtitle warm trigger
(session start). One video cache instead of two. Audio-track switching can be
solved once for sessions rather than separately for remux MP4 and HLS
transcode. Titles larger than the old remux cap play. Remux and transcode
differ by a field (`SessionMode::Copy` vs encode), not by architecture
(Rule 4.11).

**Copy-mode caveat.** Stream-copy HLS cannot force 2 s IDRs; segment
boundaries follow source keyframes. The generated VOD playlist still uses
`SEGMENT_MS` for indexing; real segment duration may vary. That is acceptable
for v0 remux; ABR alignment (ADR-0008) still applies to encoded sessions.

**No session sharing.** Session reuse by `(itemId, startMs)` is removed with
fork-on-scrub (§3), so two viewers of the same title get two FFmpeg processes.
With `-c copy` that is cheap CPU, but both count against the global session cap
(`NIGHTJAR_HLS_MAX_SESSIONS`, default 3): a household watching the same film on
two devices holds two of three slots. That is the accepted trade — sharing was
the source of most session bugs — and the per-user cap model (ADR-0007
consequences, Phase 3) is where fairness accounting returns.

This supersedes ADR-0006's delivery decisions (async remux job, remux cache,
`remuxState`, stream-from-cache). ADR-0006's decision-engine shape
(`directPlay | remux | transcode` × capability profile) stands. ADR-0007's
"remux stays whole-file for this slice" is closed by this ADR.

## Amendment: full-title VOD playlist and load-bearing 503 (2026-07-26)

Safari dogfood capture during audio-track switching (ADR-0012) showed native
HLS never had a design bug with an honest mid-window playlist: after a switch
at ~10 s it requested `seg004`, not `seg000`; both switches POSTed
title-absolute `startMs`; scrub drove window moves through segment fetches
alone (no `?startMs=`). The remaining symptom is cosmetic — after a mid-title
attach the native scrubber shows a zero-based clock (e.g. `0:00:02`) while
playback is correctly mid-title.

That reframes a full-title playlist: it is a scrubber correctness fix, not a
client workaround. Forcing hls.js on Safari to hide a lying playlist is
rejected (contradicts ADR-0007 §6 native-on-Safari; loses the hardware HLS
path iOS/tvOS need).

**Decision (accepted; implementation follows this ADR).**

6. **Playlist lists the full probed duration from `seg000`.**
   `EXT-X-PLAYLIST-TYPE:VOD` with `ENDLIST` again means what it says: the
   media playlist claims the entire title. `MEDIA-SEQUENCE` stays 0.
   Mid-title session start and audio switch still spawn FFmpeg at the window
   (`-ss` / `-start_number` / `-output_ts_offset`); they do not pre-encode
   the whole title.

7. **Out-of-window segment requests are load-bearing 503s, not 404s.**
   A segment the current encode window has not produced (and that is not
   retained on disk from a prior window) returns **503** while the session
   restarts at that offset and the segment cooks — the same retry contract
   already used for not-yet-ready in-window segments (ADR-0007 §5). 404
   means the session is gone or the name is illegal. Under a full-title
   VOD claim, 503 is the normal path for scrub/prefetch into a cold region,
   not an edge case. Clients must retry; giving up on 404 is wrong here.

8. **Restart triggers are deliberate, not incidental.** Safari prefetched
   ~50 segments on a cold attach in the capture; an unguarded "any miss
   restarts" rule would turn prefetch overshoot into an encoder storm.
   Restart on segment fetch only when:
   - the requested index is outside the current window by more than a small
     tolerance (same class as today's far-ahead `CATCH_UP_SEGMENTS` guard),
     and
   - a minimum interval has passed since the last restart on that session,
     and
   - for requests *behind* the window: either the session is `primed`
     (real scrub back), or the miss is within `ALIGN_BEHIND_SEGMENTS` of
     the encode start (player settling near `#EXT-X-START`). Unprimed
     misses farther behind than that return 503 immediately without
     restart, so attach prefetch of `seg000` cannot yank a mid-title
     encode back to zero, while a first request a few segments behind
     the land point still converges instead of deadlocking.
   Encode windows lead the play land point by eight segments. The 2026-07-26
   switch capture measured Safari's first request exactly eight segments
   behind `#EXT-X-START`; four would not contain that request. A 16-segment
   lead worked but increased measured seek-to-first-segment time from 1.84 s
   to 3.93–4.56 s, outside Gate 2's three-second budget. Eight is therefore
   the measured correctness floor, pending the full Gate 2 timing rerun.
   Far-ahead restart uses `max(frontier, play_start)` as the band end so
   land-point prefetch inside that lead-in does not thrash-restart the
   encoder.
   In-window cooking continues to wait/503 without restart. Playlist
   `?startMs=` remains an explicit restart signal from the web client's
   `seeked` handler; native Safari often skips it and hits this segment path
   instead.

**Not decided here.** Content-addressed multi-window encode (serve any
range without serial restart) stays deferred as too large; this amendment is
the smaller "session follows the player" step.

This supersedes the interim mid-window-only listing (playlist omits
pre-window segments via `MEDIA-SEQUENCE`) used while audio switch landed.
ADR-0007 §5's "503 while cooking" contract is unchanged in kind and
extended in scope to cold regions of a full-title VOD.
