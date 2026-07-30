# ADR-0019: ASS burn-in extract at scan time

- Status: accepted
- Date: 2026-07-30
- Supersedes: ADR-0018 §5 session-dir ASS demux (playback-time extract)

## Context

ADR-0018 burns ASS/SSA with libass (`ass=`). Embedded tracks cannot use
`subtitles=<src>:si=N`: that filter re-opens the container and demuxes every
cue before the first frame, which stalls HLS on large NAS remuxes. The
working graph is demux to a local `.ass`, then `ass=<path>`.

ADR-0018 lands that demux in the HLS session directory at session start.
That unblocks dogfood but puts a full-file read on first play — the same
class of mistake ADR-0013 fixed for soft WebVTT. A Blu-ray remux on a
household share can take many minutes; the session waits, and a new session
repeats the demux. Soft text already extracts under
`{NIGHTJAR_DATA_DIR}/subs/{itemId}/{trackId}.vtt` at scan time (ADR-0013).
ASS burn input is the same derived-library-data problem with a different
extension.

Rule 4.8: session-dir extract is incomplete (scan-time cache not built yet),
not a design we intend to keep. This ADR is the successor shape.

## Decision

1. **Extract embedded ASS/SSA for burn-in at scan time**, on the same
   extract worker path as text WebVTT (ADR-0013). One demux pass may emit
   both `.vtt` (text) and `.ass` (burn) tracks for an item when both exist.
   Sidecar `.ass` / `.ssa` stay on the media tree; playback opens them in
   place (no copy into the data dir).

2. **On-disk shape (Rule 4.9).** Burn-ready ASS lives at
   `{NIGHTJAR_DATA_DIR}/subs/{itemId}/{trackId}.ass` — same directory
   contract as WebVTT, different extension. Key on `itemId` + `trackId`,
   never the media path. Not under `cache/`, not LRU-evicted, not written
   into the user's library folders.

3. **Playback.** Session start for embedded ASS burn-in opens the stored
   `.ass` (or  fails closed if `subtitle_status` is not ready for that
   track). No FFmpeg demux at session start. Seek restart reuses the same
   path. The encode graph stays `ass=<path>` (ADR-0018); only the file
   provenance changes.

4. **Readiness.** Reuse `media_items.subtitle_status` / per-track readiness
   already used for soft tracks. Burn-in tracks without a ready `.ass` are
   not selectable for a session (clear error), matching "serve what exists"
   for WebVTT.

## Consequences

ADR-0018 session-dir `burn_{trackId}.ass` is a way station until this lands.
PGS stays overlay-from-container (ADR-0018 §5); it does not get an extract
file. Soft WebVTT and burn ASS share the `subs/{itemId}/` tree and extract
scheduler, distinguished by extension and `render`.
