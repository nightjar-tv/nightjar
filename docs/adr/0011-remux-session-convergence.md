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
   retained on disk from a prior window) returns **503** — the same retry
   contract already used for not-yet-ready in-window segments (ADR-0007 §5).
   Under a full-title VOD claim, 503 is the normal path for scrub/prefetch
   into a cold region. Clients must retry; giving up on 404 is wrong.

   **Rejected amendment (2026-07-28).** Mapping "policy will not
   `desire_restart` toward this index" misses to **404** (to silence
   forever-503 console noise on abandoned prior-land URIs after preempt)
   is reverted. Dogfood on muxed A+V Safari showed a **worse failure than
   the one it fixed**: stalled playback with no recovery, not merely
   continued retries. The useful claim for later readers is not "404 failed
   to stop GETs" and not a blanket "404 is unsafe." It is: **repeated
   errors on a segment Safari still considers in-window may wedge the
   pipeline, independent of which status code caused them.** Do not reopen
   404-scoping as if status were the actual variable. Unreachable cold
   holes stay **503** / no restart (same as before the amendment). Far
   preempt and client playhead timing remain separate levers.

   **Preempt polarity and fix (a) (2026-07-29).** Product default remains
   preempt **on** when `NIGHTJAR_DISABLE_PREEMPT` is unset (HEAD
   `disable_preempt` / `matches!` polarity). Explicit `=1` / `true` / `yes`
   disables preempt for investigation. A short-lived worktree flip to
   unset→off is reverted: scrub-before-play under that polarity failed
   Safari flat across leads 3–7; the validated Config D path used preempt on.

   Same-build mid-URI mechanism (item 33, A then B while A cooking) still
   stands: preempt-on can kill A's encoder before A's land exists; mid stays
   no-fill until IDLE → empty **204**. **Fix (a) chosen:** far preempt may
   still select, but `restart_at` defers `stop_child` while a cooking-land
   segment waiter is held (`may_kill_cooking_encode` / `segment_waiters`).
   Land-ready still kills (land-then-yank). Fix (b) not taken.

   Fix (a) targets **mid-playback double-scrub under preempt-on** (kill
   before mid land). Scrub-before-play ablation under preempt-on passed
   5/5 both engines with the waiter gate forced always-kill; that harness
   does not exercise fix (a)'s purpose. **Owed before treating the stack as
fully confirmed:** mid-playback double-scrub N≥5 on the preempt-on
mechanism probe (engine-agnostic mid-URI startMs A=258s then B=748s,
GAP=300ms) with fix (a) on.
Verified on the current stack: N=5 passing, and a second confirmation
batch also passing. In each trial, the mid-uri GET ended as HTTP 503
with waited_ms ~0.6–1.7s, and final-land ended as HTTP 200; no
`abandoned hold ended` / HTTP 204 teardown events occurred.

   **Settled: abandoned / superseded miss status shape.** A segment GET
   that will not be filled on the current encode trajectory (abandoned
   after preempt, or a land-waiter superseded by a newer pending land)
   enters a **no-fill hold**: no 503/404 while the session lives. When
   the hold reaches `IDLE_TIMEOUT` (or the session is torn down mid-hold),
   the request ends with empty **HTTP 204**. That is product policy, not
   an open experiment. Safari must not see an application-level media
   failure on a URI the encode will never produce. Session teardown of a
   dead session remains **404** (`NotFound`), distinct from this ceiling.

   **Rejected (do not revive): immediate 204 on supersede.** Ending the
   wait the instant pending moves, with 204 before the hold ceiling, wedged
   native Safari on double-scrubs (zero further segment GETs after the
   middle 204). No-fill hold first; 204 only at the ceiling.

   **Not a scrub-wedge proof (2026-07-28).** The same hold/ceiling shape
   was first tried as the sole fix for a double-scrub picture stall under
   full-title VOD + distance preempt. Dogfood: land segment 200,
   `currentTime` at target, recover watch `advanced=false`, picture never
   left the pre-scrub frame. Status shape alone did not restore playback.
   Keep the contract above for abandoned-URI hygiene; do not relitigate it
   as if it were the proven scrub fix (coalesce, land-ensure, ADR-0017
   attach backend, and related levers are separate).

8. **Restart triggers are deliberate, not incidental.** Safari prefetched
   ~50 segments on a cold attach in the capture; an unguarded "any miss
   restarts" rule would turn prefetch overshoot into an encoder storm.
   Restart on segment fetch only when:
   - the requested index is outside the current window by more than a small
     tolerance (same class as today's far-ahead `CATCH_UP_SEGMENTS` guard),
     and
   - a minimum interval has passed since the last restart on that session,
     and
   - for requests *behind* the window: only when the miss is within
     `ALIGN_BEHIND_SEGMENTS` of **play_start** (player settling near
     `#EXT-X-START`) *and* the miss does not retreat a committed land
     (cooking `play_start` and/or pending from playlist `?startMs=`).
     Near-ALIGN dig-back behind a deliberate scrub must Wait / 503 without
     `desire_restart` — otherwise Safari steals pending two segments behind
     the land, releases the land long-poll, and yanks FFmpeg when the real
     land finishes. Farther behind returns 503 without restart — attach
     prefetch of `seg000`, and Safari still probing a *prior* land after a
     jump (fill-forward leaves retained segs; the next index is a hole)
     must not yank the encode. Real scrub-back is playlist `?startMs=`.
     Primed near-land misses still respect `RESTART_MIN_INTERVAL`.
     `scrub_shaped` (record pending while min-interval is hot) uses the
     same dig-back gate so it cannot write a retreated pending under Wait.
   Encode windows lead the play land point by [`ENCODE_LEAD_SEGMENTS`]
   **(8)** so Safari dig-back near `#EXT-X-START` (including post-land
   `#t=` reload under `PRECISE=YES`, ADR-0017 amendment) hits on-disk
   segments without retreating a committed `?startMs=` land. Binary search
   under the corrected stack (preempt on, PRECISE=YES, post-land `#t=`):
   dual-engine scrub-before-play fails Safari at lead 5/6/7 (Chrome 5/5);
   lead **8** is the minimum where both engines hit 5/5, confirmed with a
   second N=5 batch both engines. A 16-segment lead previously worked but
   increased measured seek-to-first-segment time outside Gate 2's budget;
   zero lead left dig-back 503-forever once pending retreat was blocked.
   **Gate 2 concurrent-session / land-time cost at lead=8 is still
   unmeasured**; do not treat 8 as budget-proven, only as the scrub-before-play
   dual-engine floor for this title/harness.

   **`PRECISE=YES` restored (couple with lead).** `#EXT-X-START` keeps
   `PRECISE=YES`. A temporary PRECISE removal shrank dig-back under post-land
   `#t=` when lead was too short; that was a workaround for insufficient lead
   depth, not a preferred playlist contract. With lead sized to 8, PRECISE
   returns. Do not re-remove PRECISE to paper over dig-back without first
   checking whether lead covers the measured hole.

   Far-ahead restart uses `max(frontier, play_start)` as the band end so
   land-point prefetch inside that lead-in does not thrash-restart the
   encoder.
   In-window cooking continues to wait/503 without restart. Playlist
   `?startMs=` remains an explicit restart signal from the web client's
   `seeked` handler. Native Safari previously often skipped it and hit this
   segment path instead; after ADR-0013 cue injection, native also sends
   `?startMs=` on user scrub so land matches the playhead (prefetch segment
   misses no longer redefine the land). Native startMs is fire-and-forget so
   rapid `seeked` events are not gated behind an in-flight fetch (the
   failure mode that had motivated skipping startMs when captions still
   depended on HLS TEXT reload).

**Not decided here.** Content-addressed multi-window encode (serve any
range without serial restart) stays deferred as too large; this amendment is
the smaller "session follows the player" step.

This supersedes the interim mid-window-only listing (playlist omits
pre-window segments via `MEDIA-SEQUENCE`) used while audio switch landed.
ADR-0007 §5's "503 while cooking" contract is unchanged in kind and
extended in scope to cold regions of a full-title VOD.
