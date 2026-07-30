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
   `.ass` when ready. If not ready, do not demux in the session process:
   enqueue extract (play-priority) and apply the ADR-0018 cold-path product
   choice. Seek restart reuses the same path once ready. The encode graph
   stays `ass=<path>` (ADR-0018); only the file provenance changes.

4. **Readiness.** Reuse `media_items.subtitle_status` / per-track readiness
   already used for soft tracks. A burn-in track without a ready `.ass` is
   still listable; selecting it takes the ADR-0018 cold path (enqueue +
   wait/start choice), not a clear error and not a session-local demux.

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

**Estimate source.** Cold-path wait ranges (ADR-0018) derive from a rolling
average of `src_mib_per_s` on the `ass_burn_extract_finished` **info** log
from `extract_embedded_ass`. Settled fields: `src`, `stream_index`, `dest`,
`src_bytes`, `track_bytes`, `elapsed_ms`, `src_mib_per_s`. That log is the
throughput feed, not temporary instrumentation (Rule 4.8).

## Consequences

ADR-0018 session-dir `burn_{trackId}.ass` is a way station until this lands.
PGS stays overlay-from-container (ADR-0018 §5); it does not get an extract
file. Soft WebVTT and burn ASS share the `subs/{itemId}/` tree and extract
scheduler, distinguished by extension and `render`.

## Amendment (2026-07-30): store gaps (no sibling ADR)

### Context

Spike closeout asked whether durable `.ass` needs its own cache ADR. Six
store questions against this document: path and one-store are already here;
invalidation, doctor reporting, cold-miss behaviour, and concurrent writers
were incomplete or only implied by ADR-0013. A second ADR for the same
`subs/{itemId}/` tree would fork Rule 4.11. Extend in place.

V1_PLAN Phase 2 item 3 (text backfill default / opt-in / on-demand) names
neither a path nor a key; it stays parked on browser proof of items 1–2 and
does not constrain this shape. When it unparks, it uses this same store.

### Decision

5. **Invalidation.** Same as WebVTT (ADR-0013 §4):
   `subtitle_source_mtime_ms` / `subtitle_source_size_bytes` on the item.
   Source mtime or size change → `subtitle_status = pending`, next extract
   overwrites `{trackId}.ass` in place. No mtime/hash in the filename.

6. **One store, confirmed.** Soft `.vtt` and burn `.ass` share
   `{NIGHTJAR_DATA_DIR}/subs/{itemId}/` and the extract worker. Item-level
   and library-level derived subtitle data are not two trees. PGS and
   VobSub get no file under this store (Spike A: overlay cost stays inside
   one encode segment; no extract, no cache entry).

7. **No byte cap, no LRU.** Already §2. Free-space refuse before extract
   stays ADR-0013 §4 (leave `pending`, log). Phase 4 `nightjar doctor`
   (ADR-0014 §10 plan) reports `subs/` bytes as derived library data under
   `NIGHTJAR_DATA_DIR`, not as a capped cache — there is no
   `NIGHTJAR_SUBS_CACHE_BYTES` to compare against.

8. **Scanner pre-populates; cold miss enqueues, does not write.** §1 stands:
   scan (and play-priority on the same worker, ADR-0013 §11) fills the
   store. A cold session must not demux into `subs/{itemId}/` or revive
   session-dir `burn_*.ass` as the durable path. It **enqueues** the extract
   job (play-priority) and follows the ADR-0018 cold-path product choice
   (bounded wait vs start without). The worker is the single writer; the
   next viewer (or a replay) hits a ready file. Two concurrent cold
   sessions for the same item share that one job (§9).

9. **Concurrency.** One extract job per item on the shared worker. A second
   request for the same not-ready track waits on that job (or on readiness);
   it does not start a second demux into the same path. Minutes-class extract
   (Measurement table) makes a racing double-write a real window if two jobs
   were allowed.

10. **Deletes when this lands (Rule 4.8 / 4.5).** Remove session-dir
    `burn_{trackId}.ass` creation and any playback-time demux that only
    existed to feed `ass=`. Encode keeps `ass=<path>` with the path from
    this store (or a media-tree sidecar).

**Boundaries.** ADR-0018 owns burn-in selection, encode graphs, and cold-path
product UX (wait estimate, viewer choice, deferred splice). This ADR owns
when ASS extract runs, where durable `.ass` lives, and that a cold miss
only enqueues the shared worker (never a session-local write). Soft-text
store law that `.ass` inherits without change stays ADR-0013; do not fork
it here.

### Consequences

Operators get one `subs/` tree to back up and one doctor line for its size;
there is no separate ASS cache to tune. The first cold viewer still waits
(or starts without captions); later viewers of that title do not re-pay
the demux because the enqueue wrote through the same store. Racing writers
are forbidden because the Measurement table shows the race window is
minutes, not milliseconds.
