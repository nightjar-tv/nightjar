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

8. **Watcher polling fallback.** `notify` does not reliably deliver create
   events over SMB. If index insert for a newly copied title is late, the
   watcher is the cause, not the probe queue. The library watcher keeps the
   debounced `notify` path and adds a periodic mtime-incremental scan of
   every library root so "add a file and it appears" holds on network
   shares. On the household macOS SMB mount, `notify` did fire for a nested
   create (~2.4 s to the watch event); the poll remains the fallback when
   it does not.

   Poll cost on the household NAS (2026-07-26), Movies library only
   (~1,748 media files, 1,763 dirs over SMB at ~15 MB/s):

   | Pass | Wall |
   |---|---|
   | Cold full tree walk (readdir + file stat) | 73–152 s |
   | Warm poll: re-stat every known dir, readdir only if that dir's mtime changed | 0.02–11 s (0 dirs changed) |

   TV Shows on the same share is ~23,058 media files (~110 s for a
   filename find alone). A fixed 60 s full-walk poll is longer than one
   Movies walk and would stack I/O on top of probe and extract. Changes
   that follow from the numbers:

   1. **Directory-mtime walk cache.** Unchanged directories reuse the prior
      file list and child set; only dirs whose own mtime moved are
      re-listed. Immediate-parent mtime updates when a file is added;
      ancestors need not. This is the steady-state poll path.
   2. **Interval scales with index duration.**
      `poll_interval = max(60s, 2 × last_index_duration)`. After a cold
      ~150 s Movies index the next poll waits ~300 s, so walks cannot
      pile up.
   3. **Dirty follow-up after a busy scan.** `start_scan_job` reuses an
      active job. An fs change that arrives after the walk has already
      passed that directory would otherwise wait for the next poll. The
      watcher marks the library dirty when an fs event hits an active job;
      when that job finishes, a follow-up scan starts immediately. Poll
      reuse does not set the dirty bit (it would force a double cold walk
      on every long index).
   4. **Pause extract during the index walk.** Extract demuxes are
      multi-minute SMB reads. Running them concurrently with a Movies
      cold walk stretched that walk past 22 minutes with zero rows
      committed; the same tiny library indexed in 12 ms idle and 11 s
      under extract load. Workers still prefer probe over extract, and
      additionally refuse to start new extracts while any library is in
      its walk. One in-flight demux may finish; new ones wait.

9. **Probe and scan-job resume across restarts.** Items left
   `probe_status = indexed` after a process exit are stranded if the pool
   only accepts work from the current index pass: unchanged mtime skips
   them forever, and subtitle extract behind that queue never runs.
   Startup drains `indexed` rows into the probe queue the same way it
   drains pending extracts; an unchanged index pass also re-queues
   still-`indexed` items. Measured on the dogfood DB before the fix:
   1,006 indexed / 739 probed / 1,745 total (57.6% stranded). After the
   fix, a restart logged `resumed indexed items awaiting probe count=1006`
   and the indexed count drained.

   The same restart leaves `scan_jobs` rows in `queued` / `indexing` /
   `probing`. `POST /scan` reuses an active job id, so a zombie probing
   row blocks every later scan and the poll fallback never indexes new
   files. Startup fails those rows with "scan interrupted by process
   restart" before accepting work.

## Consequences

- First play of an already-extracted title shows captions immediately.
  Measured on a ready title: WebVTT GET 0.5–4 ms; HLS master playlist
  included `#EXT-X-MEDIA:TYPE=SUBTITLES` on the first fetch (0.38 s
  including session create) with playlist entries pointing at the stored
  VTT URLs. Chrome and Safari both opened the item page; browser JS
  automation was not available to scrape the `<track>` element, so the
  API/HLS path is the verified number. First play of a title still
  `pending` shows the preparing line, not a multi-minute hang inside the
  GET.
- Add-file timings on the household NAS (2026-07-26), isolated library
  rooted at a single title directory (~4 KB probe MKV + sidecar):

  | Path | Until listed | Until playable | Until subtitled |
  |---|---|---|---|
  | Explicit `POST /scan` (idle) | 0.26 s | 0.26 s | 0.51 s |
  | Watcher (`notify` fired; concurrent Movies extract on same share) | 27 s | 27 s | 27 s |

  The first column is the property this slice exists to deliver. Under a
  full Movies cold walk fighting extract demuxes, a new title under the
  Movies root had still not been indexed after 22 minutes (walk alive,
  `added=0`); that is why extracts pause during index and why the dirty
  follow-up exists. Do not treat the 27 s watcher row as a Movies-scale
  guarantee until a walk completes without extract contention.
- A first extract pass is wall-clock bound by sequential NAS read speed,
  not by pool width: extract is serialised by a process-wide lock (shared
  tmp paths), and subtitle packets are interleaved throughout the
  container, so each text-bearing title costs roughly one full read.
  Parallel extract would only split the same pipe.

  Measured on the dogfood Movies library (2026-07-26):

  | Quantity | Value |
  |---|---|
  | Library size | 1,745 titles, 16.5 TB, mean 9.5 GB, p90 17.8 GB |
  | Titles with an extractable text track (subrip/mov_text/webvtt/text) | 68% by count, **74% by bytes** (200-file sample) |
  | Demux throughput (two 8–10 GB subrip titles, isolated) | 54 and 56 MB/s, ~18 s/GB, flat across 1 vs 4 tracks |
  | Header-only classify probe (`ffprobe -select_streams s`) | ~0.26 s/title |

  Text-bearing bytes ≈ 0.74 × 16.5 TB = 12.3 TB at ~18 s/GB ≈ **61 hours
  (~2.6 days)** for the Movies first pass, single stream, uncontended.
  Filtering to text-bearing items removes only ~26% of the read volume
  here; this library is subtitle-heavy (subrip dominant), so
  extract-only-where-text-exists is not the large win it would be on a
  library without embedded subs. Image/ASS tracks (PGS, dvd_subtitle,
  ass) are skipped and cost only the classify probe.

  The earlier **21 items/hour** figure was measured while a cold Movies
  index walk was starving the same share; it is a contention artifact, not
  the steady-state rate, and is superseded by the throughput number above.
  Extracts now pause during the index walk for exactly this reason. TV
  Shows (~23,058 files, smaller per title) is not yet byte-measured; the
  full 24,800-item estimate needs that pass before it is stated.
- Schema migration `005` adds `subtitle_status` and the source mtime/size
  stamp columns (append-only). OpenAPI gains `subtitleStatus` on item and
  playback-info schemas (additive, v0).
- ADR-0010 §2–6, §8–11 (track identity, WebVTT delivery, sidecar
  discovery, API shape, HLS MEDIA skin) stand. Only the cache and the
  playback trigger are replaced.
- Image / ASS burn-in remains later Phase 2 work; those tracks stay listed
  without `url`.
- Directory-mtime polling can miss an add if the SMB server fails to bump
  the immediate parent mtime; the scaled full cold walk after process
  restart still heals that. Do not raise the poll frequency to compensate.