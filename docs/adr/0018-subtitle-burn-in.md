# ADR-0018: Image and ASS subtitle burn-in

- Status: accepted
- Date: 2026-07-29

## Context

ADR-0010 / ADR-0013 deliver text subtitles as WebVTT. ASS/SSA and image
subs (PGS) are listed when discovered but never rendered. Soft conversion
of styled ASS or bitmap PGS into WebVTT is lossy or impossible; the only
honest path for those formats is burning pixels into the video encode.

Burn-in always costs a video re-encode. A title that would DirectPlay or
remux-copy must not silently start paying that cost. Audio already chose
explicit `audioTrackId` on session start with restart-on-switch
(ADR-0012); that pattern is the load-bearing contract for any track that
changes the encode graph.

**Dependency honesty.** Server `?audioTrackId=` map selection and DELETE
are built. DirectPlay → session when selection requires encode was stated
in ADR-0012 but not built (session start 415s all DirectPlay). Client
mid-play POST+DELETE+reattach is not wired. This ADR closes the server
gate for both over-ceiling audio and burn-in; client mid-play choreography
remains a shared follow-up with the item player UI.

## Decision

1. **One burn-in inventory (Rule 4.11).** Embedded ASS/SSA/PGS and sidecar
   ASS/SSA share `SubtitleTrack` with soft text tracks. They differ by
   `render`: `soft` | `burnIn`. Soft keeps `url` / readiness. Burn-in has
   no `url` and no readiness. Codecs in scope: `ass`, `ssa`,
   `hdmv_pgs_subtitle`. Unknown image codecs stay omitted.

2. **Explicit selection (Rule 2.1).** `POST /api/v0/items/{itemId}/sessions`
   accepts optional `subtitleTrackId` (same id scheme as soft tracks).
   Only `render: burnIn` ids are valid for this param; soft ids return
   404. Omit the param = captions off for burn-in (soft MEDIA/`<track>`
   unchanged). No auto-burn of a default burnable track.

3. **Session-start gate.** Allow a session when `decide` is Remux or
   Transcode, **or** when the request requires encode work the DirectPlay
   path cannot do: a burn-in `subtitleTrackId`, or an `audioTrackId` whose
   channel count exceeds the capability ceiling (ADR-0012 hybrid). Burn-in
   forces `SessionMode::Transcode` for video even when codecs would Copy.

4. **Restart-on-switch.** Changing burn-in on/off or between burn tracks
   is a fresh POST at `startMs` plus DELETE of the prior session — the
   same model as audio. Seek restart must not carry burn-in switches.

5. **FFmpeg graphs.** Burn-in is one user concept (`render: burnIn`) but
   two encode graphs. Unifying on one filter is wrong (Rule 4.11):
   - **ASS/SSA** need libass. Overlay/`sub2video` leaves ASS as
     non-bitmap even when libass is built in — silent blank burn. The
     working path is `-vf ass=<path>,…` (sidecar path, or a demuxed
     `.ass`). Never `subtitles=<src>:si=N` — that re-opens the container
     and demuxes every cue before the first frame, which stalls HLS on
     large/NAS remuxes. Paths are escaped for filter syntax (including
     spaces and parentheses). Mid-window `-ss` before `-i` resets frame
     PTS to ~0; wrap libass with `setpts=PTS+start/TB,…,setpts=PTS-start/TB`
     so absolute cue times still match, then restore PTS for
     `-output_ts_offset`.
   - **PGS** is already bitmap. `[0:v:0][0:s:N]overlay` streams from the
     same demux as video; no extract file, no libass, no setpts wrap —
     subtitle packets seek with the same `-ss`.

   Both compose with the existing SDR `sidedata=delete,setparams=…` chain
   (one graph, not two competing `-vf` flags). ASS burns against host
   fontconfig (not bundled; Rule 1.2); see the amendment for
   container-attached fonts and cross-host pin. FFmpeg without `ass`/
   `subtitles` fails closed on ASS burn-in, never falls back to overlay.

   **ASS file provenance (Rule 4.8).** This slice demuxes embedded ASS into
   the ephemeral HLS session dir (`burn_{trackId}.ass`) at session start
   and reuses it on seek restart. That is a way station.
   [ADR-0019](0019-ass-burn-extract-at-scan.md) owns when extract runs and
   the durable path/keying. The encode graph (`ass=`) stays.

6. **Corpus.** Existing `h264_aac_ass_mkv.mkv` and `h264_aac_pgs_mkv.mkv`.
   The PGS fixture is a minimal synthetic SUP (ffprobe-friendly); CI
   asserts encode success and segments, not visible glyphs. Richer PGS for
   dogfood is a follow-up if blank burn is observed.

## Consequences

DirectPlay titles with burnable tracks stay DirectPlay until the client
asks for burn-in. Selecting burn-in consumes a session-cap slot and
re-encodes video. Soft text and burn-in tracks on the same title remain
independently selectable. Mid-play client restart choreography for audio
and burn-in is one shared follow-up, not duplicated per feature.

Session-dir ASS demux is superseded by ADR-0019 once scan-time burn
extract lands. ADR-0010 / ADR-0013 “burn-in later” notes are superseded
by this ADR. Spike closeout (extract cost, progressive scope, cold path,
fonts, forced) is the amendment below.

## Amendment (2026-07-30): spike closeout

### Context

Spikes A–C answered the open burn-in questions: what extract actually
costs, whether image burns share the progressive path, and whether a
mid-session encoder splice can cross parsers without
`#EXT-X-DISCONTINUITY`.

### Decision

1. **Extract cost.** Embedded ASS demux is a full sequential container
   read. Wall time is approximately source size ÷ share throughput, not
   track byte size. Cite the Spike A table in
   [ADR-0019](0019-ass-burn-extract-at-scan.md) Measurement; do not copy
   the numbers here.

2. **Scan-time extract is the steady state.** The scanner reads every
   ASS-carrying title end to end. At about 60 MiB/s that is roughly
   5 hours per TiB of ASS-bearing media, on the same share as playback.
   Extract needs throttling, off-peak scheduling, and visible progress.
   Dogfood timings ran against a warm-leaning page cache; treat them as a
   floor.

3. **Progressive mechanisms are ASS/SSA only.** PGS and VobSub need no
   extract file (Spike A: PGS overlay stays inside one segment of encode
   cost). Bounded-wait UX, encoder splice, and any other progressive
   cold-path machinery in this ADR apply to ASS/SSA. Image burns stay on
   the overlay graph in §5.

4. **Cold path (ASS/SSA, extract not ready).** Bounded wait with a time
   estimate and an explicit viewer choice (wait / start without captions).
   Estimate is a conservative range from a rolling average of
   `src_mib_per_s` on the `ass_burn_extract_finished` info log
   ([ADR-0019](0019-ass-burn-extract-at-scan.md) Measurement), never a
   countdown; it may be wrong by about 3×. Waiting (or starting cold)
   enqueues the shared extract job so the durable store fills for the next
   viewer ([ADR-0019](0019-ass-burn-extract-at-scan.md) amendment).
   Tag-free encoder splice works on Chrome/hls.js, Safari native, and
   Firefox/hls.js with no `#EXT-X-DISCONTINUITY`
   (Spike C, `nightjar-meta/scripts/spike_c_FINDINGS.md`); proven viable and
   deferred to a later slice (Rule 4.5). Do not re-litigate the
   discontinuity tag without new binding-client evidence.

5. **Pre-splice captions.** Segments encoded before burn attaches carry
   no burned captions; captions start from the attach point
   (Spike C, `nightjar-meta/scripts/spike_c_FINDINGS.md`). Documented
   limitation, not a bug to paper with lead rewrite.

6. **Forced-track auto-select is text-only.** V1_PLAN Phase 2 item 5
   (matching forced track when audio is foreign-language) applies to soft
   text tracks. An image forced track must not auto-start a transcode from
   a stored preference; that contradicts explicit `subtitleTrackId`
   selection in this ADR.

7. **Fonts.** Burn-in does not use fonts attached inside the container.
   libass renders against the server's fontconfig. The same file can
   render differently across hosts unless the operator pins the font set.

8. **ADR boundaries.** This ADR owns burn-in inventory, explicit
   selection, encode graphs, and cold-path product UX (estimate, viewer
   choice, deferred splice, pre-splice limitation).
   [ADR-0019](0019-ass-burn-extract-at-scan.md) owns when extraction runs,
   where durable `.ass` lives, and that a cold miss only enqueues the
   shared worker. No separate cache ADR (Rule 4.11).

### Consequences

Scan extract is library-scale I/O, not a cheap sidecar convert. Operators
will see share contention unless scheduling and progress are real.
Progressive spend stays on ASS/SSA; PGS/VobSub keep the simple overlay
path. Cold play before extract is ready is wait-with-estimate or start without
captions; either path enqueues the shared extract so the next viewer does
not re-pay. Encoder splice stays off the critical path until its own slice.
Cross-host ASS look is an ops pin, not a bundled font tree (Rule 1.2).

Spike B changed nothing in the design (null result). Burned-track scrub
resume still has one unreproduced dogfood failure, bounded under ~1.25%
at 95% across 280 trials, tracked in
[#9](https://github.com/nightjar-tv/nightjar/issues/9).
