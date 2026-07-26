# ADR-0013: Subtitle extraction at scan time

- Status: accepted
- Date: 2026-07-26
- Supersedes: ADR-0010 §7 (byte-capped subtitle cache and playback-time extract)

## Context

Embedded text subtitles are interleaved throughout the container. Extracting
them requires reading the whole source. On a household NAS at about 15 MB/s,
that is minutes for a large title. ADR-0010 put that cost on first play:
video starts in under a second (HLS session), captions arrive minutes later.
Session-start warming (ADR-0011) races the same demux against playback and
loses on real NAS sizes. Measured cold extract on a NAS-hosted DTS MKV was
about 255 s to the first WebVTT byte; a cache hit was about 0.08 s.

Playback-time extraction also competes with the probe pool for the same
share reads. That contention is the likely cause of a single new show taking
hours to become available while the library is still probing and captions
are being demuxed for plays in progress.

ADR-0010 locked `trackId`, the VTT URL, and a cache under
`{NIGHTJAR_DATA_DIR}/cache/subs/` with `NIGHTJAR_SUBS_CACHE_BYTES` LRU. The
cache shape and the playback trigger are the wrong irreversible decisions
(Rule 4.9 / 4.8). Extracted WebVTT is derived library data, not a disposable
transcode artifact. At a 24,800-item library the permanent store is roughly
1.3–1.5 GB — negligible against the media, and consistent with Jellyfin,
whose instances keep tens of thousands of extracted files permanently. A
512 MiB cap at that size thrashing under LRU is what makes captions feel
unreliable.

Jellyfin has two open defects this slice must not copy. Their cache filename
incorporates the media file path, so reorganising a library orphans every
extracted file and the directory nearly doubles. Their key uses container
mtime plus subtitle stream index, so adding a sidecar shifts stream indexes
and playback serves the wrong language. Our `trackId` discipline
(`e{streamIndex}` for embedded, `s…` for sidecars) exists to prevent both;
this ADR records that as the reason, and the tests below lock it.

## Decision

1. **Extract at scan time, never at playback.** Subtitle extraction is a
   background job enqueued when an item is indexed or its source
   mtime/size changes. Playback never starts FFmpeg for subtitles.
   `GET /api/v0/items/{id}/subtitles/{trackId}.vtt` serves a file that
   already exists, or 404. No cold-fetch, no 503-while-extracting on that
   path.

2. **One job type on the existing bounded worker pool, below probe.** The
   scan worker pool (ADR-0004) accepts two work kinds: `probe` and
   `extract`. Probe is always preferred. A library becomes browsable
   (index) and playable (probe) before it becomes subtitled (extract).
   Scan job state stays `queued → indexing → probing → completed|failed`;
   extraction continues after the job reports completed, because a first
   pass over a large library takes a long time and must not hold the gate
   metric. Work is durable: `media_items.subtitle_status` is
   `pending | ready | none | error`, so a restart re-enqueues every
   `pending` row without a full rescan.

3. **Single FFmpeg pass per file.** All embedded text tracks for an item
   extract in one demux (already the batch path in ADR-0010). Per-track
   passes re-read the whole file. Jellyfin measured roughly 50 seconds for
   four tracks in one pass versus four full reads; we keep that shape.
   Sidecar `.srt` / `.vtt` convert in-process in the same job; they do not
   need a second source read of the video.

4. **On-disk shape (Rule 4.9).** Extracted WebVTT lives under
   `{NIGHTJAR_DATA_DIR}/subs/{itemId}/{trackId}.vtt`, beside the database,
   not under `cache/`. It is derived library data: covered by the same
   backup as the DB, never LRU-evicted, never written into the user's media
   folders. Library directories stay read-only (and may be genuinely
   read-only mounts).

   - Key on stable item identity (`itemId`), never the media path. That is
     the fix for Jellyfin's reorganise-orphan defect.
   - Filename is `trackId` only (`e2.vtt`, `s-en.vtt`). Embedded ids derive
     from absolute container stream index; sidecars use the `s…` namespace
     (ADR-0010). Adding a sidecar cannot renumber or shadow an embedded
     track's stored file — that is the fix for Jellyfin's wrong-language
     defect, and a required test.
   - Validity is the source mtime and size recorded on the item when the
     extract finished (`subtitle_source_mtime_ms`,
     `subtitle_source_size_bytes`). A later index pass that sees a
     different mtime or size sets `subtitle_status = pending` and the next
     extract overwrites the item directory. Stale filenames do not
     accumulate under a path-shaped key.
   - Remove `NIGHTJAR_SUBS_CACHE_BYTES` and all LRU eviction. Keep a
     free-space check before extraction: refuse the job (leave `pending`,
     log clearly) when the data volume cannot hold a conservative minimum
     headroom.

5. **Cleanup.** A pass after index removals deletes
   `{NIGHTJAR_DATA_DIR}/subs/{itemId}/` for items no longer in any library.
   Startup also sweeps directories under `subs/` whose `itemId` is absent from
   `media_items`. Jellyfin still lacks this; it is why their subtitle
   directories grow without bound.

6. **Status and honesty.** `PlaybackInfo.subtitleStatus` (and the same
   field on list items) is `pending | ready | none | error`. While
   `pending`, serveable tracks may be listed without `url` so the UI can
   say captions are being prepared — same mono register already used for
   ASS files that are found but not rendered. Do not imply the title has
   no subtitles. `sessionSubtitlesPreparing` ("may take a moment on first
   play") and any cold-fetch handling on subtitle GETs go away.

7. **Deletions (Rule 4.5).** Remove the session-start warm path
   (`warm_embedded_webvtts`), any remux warm remnant, playback-time
   `ensure_*` on the VTT GET, `SubsCache` byte cap and eviction, and
   `NIGHTJAR_SUBS_CACHE_BYTES`. Net complexity must fall: extraction moves
   to one place (the worker), serve becomes a file read.

8. **Watcher polling fallback.** `notify` / inotify does not deliver
   create events over SMB. If index insert for a newly copied title is
   late, the watcher is the cause, not the probe queue. The library
   watcher keeps the debounced `notify` path and adds a periodic
   mtime-incremental scan of every library root so "add a file and it
   appears" holds on network shares. Probe-queue contention remains a
   separate diagnosis when the row exists as `indexed` for a long time.

## Consequences

- First play of an already-extracted title shows captions immediately
  (Chrome and Safari). First play of a title still `pending` shows the
  preparing line, not a multi-minute hang inside the GET.
- A 24,800-item first extract pass is wall-clock bound by NAS read speed
  and pool width. Measured rate after dogfooding on the household share:
  _TBD items/hour; estimate for 24,800 = TBD_. Fill this blank from the
  same run that verifies the slice; do not invent it.
- Schema migration `005` adds `subtitle_status` and the source mtime/size
  stamp columns (append-only). OpenAPI gains `subtitleStatus` on item and
  playback-info schemas (additive, v0).
- ADR-0010 §2–6, §8–11 (track identity, WebVTT delivery, sidecar
  discovery, API shape, HLS MEDIA skin) stand. Only the cache and the
  playback trigger are replaced.
- Image / ASS burn-in remains later Phase 2 work; those tracks stay listed
  without `url`.
