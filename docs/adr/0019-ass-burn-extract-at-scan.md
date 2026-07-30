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

## Measurement (dogfood NAS, 2026-07-30)

Method: `extract_embedded_ass` over `/Volumes/media` (SMB), three runs per
title, OS page cache not purged (`sudo purge` unavailable in the bench
session - treat later runs as warm-leaning; even the fastest is decisive).
HLS segment duration is 2 s (ADR-0008); 2–3 segments of playback ≈ 4–6 s.
Throughput is source-file bytes / wall time (demux walks the interleaved
container, not track size alone).

| Item | Title | Size | Container | Track | Wall (s) ×3 | MiB/s ×3 |
|---|---|---|---|---|---|---|
| 1574 | The Simpsons Movie | 5.98 GiB | mkv | eng ASS 90 KiB | 130 / 135 / 209 | 47 / 45 / 29 |
| 1233 | Star Wars The Last Jedi | 22.28 GiB | mkv | vie ASS 151 KiB | 429 / 304 / 303 | 53 / 75 / 75 |
| 705 | Kill Bill The Whole Bloody Affair | 42.37 GiB | mkv | eng ASS 23 KiB | 623 / 692 / 554 | 70 / 63 / 78 |

PGS does not extract. Bros (248, 28.52 GiB remux), cold-leaning first segment
to playlist-ready (`libx264` when burn-in selected; remux-copy with no
subtitle):

| Case | First segment (s) ×3 |
|---|---|
| no subtitle | 0.28 / 0.11 / 0.16 |
| PGS eng overlay | 2.56 / 2.16 / 2.09 |

PGS stays inside one segment of encode cost; it does not pre-buffer a full
container demux the way ASS session extract does.

**Recommendation:** ASS extract runs to minutes (2–11 min here), far outside
2–3 segments of playback - do not keep blocking session-start demux or try
to paper it with seek-ahead caps; scan-time extract stands, and any
first-play-before-ready path needs a progressive ASS design (same class as
ADR-0013 §11), not a longer wait.

## Consequences

ADR-0018 session-dir `burn_{trackId}.ass` is a way station until this lands.
PGS stays overlay-from-container (ADR-0018 §5); it does not get an extract
file. Soft WebVTT and burn ASS share the `subs/{itemId}/` tree and extract
scheduler, distinguished by extension and `render`.
