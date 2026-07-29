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

5. **FFmpeg graphs.** Burn-in uses one `filter_complex` overlay of the
   subtitle stream onto video, then the existing SDR
   `sidedata=delete,setparams=…` chain on the filtered video:
   - Embedded ASS/SSA/PGS: `[0:v:0][0:s:N]overlay,…[vout]` (`N` = subtitle
     ordinal among subtitle streams).
   - Sidecar ASS/SSA: second `-i` on the sidecar file, then
     `[0:v:0][1:s:0]overlay,…[vout]`.
   Mid-window seek applies `-ss` to each burn input the same way as the
   main media input. Host fonts still matter for ASS text rendering through
   the decoder (not bundled; Rule 1.2). Missing fonts fail closed to a
   failed session, not a silent blank burn. FFmpeg builds without the
   optional `subtitles`/`ass` libass filters are fine — overlay does not
   need them.

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

ADR-0010 / ADR-0013 “burn-in later” notes are superseded by this ADR.
