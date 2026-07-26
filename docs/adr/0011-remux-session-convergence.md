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
