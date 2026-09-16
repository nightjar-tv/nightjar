# ADR-0013: Subtitle extraction at scan time

- Status: accepted; **§1 and §2 superseded 2026-08-06 by
  [ADR-0041](0041-subtitle-classification-and-client-gated-extraction.md)**
  (probe-time classification and client/method-gated on-demand trigger,
  replacing the unconditional scan-time enqueue below). §3–§13 stand, with
  one exception marked in place: **§4's mtime/size validity stamps are
  superseded by [ADR-0023](0023-cluster-map-byte-offset-start.md) §6**
  (columns dropped in migration `019`; `subtitle_content_id` is the sole
  stamp). Marker added 2026-08-30. **§13 added 2026-09-15 (D2B.2 standalone
  immutable artifact contract); §13.2–§13.5 amended 2026-09-15 to add the
  per-item artifact-revision candidate layout and finalize-without-overwrite;
  §13.8 amended 2026-09-15 (D2B.3 piggyback and HLS/session composition).**
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

**§1 and §2 below are superseded by
[ADR-0041](0041-subtitle-classification-and-client-gated-extraction.md)**:
extraction is no longer enqueued unconditionally at scan time; it is
classified at probe time and triggered on demand, gated by client and
method. Read them here only for the "never at playback" and "below probe
priority" framing that ADR-0041 keeps; the enqueue trigger they describe no
longer runs.

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

     > **Superseded by [ADR-0023](0023-cluster-map-byte-offset-start.md) §6
     > (amended 2026-08-07); marker added 2026-08-30.** Both columns are
     > **dropped**, in migration `019_drop_subtitle_source_stamps.sql`.
     > `subtitle_content_id` is the sole validity stamp for extracted
     > subtitles, the same `content_id` every other derived artifact is
     > keyed on. The rule is unchanged in kind — a changed source still
     > sets `subtitle_status = pending` and the next extract overwrites —
     > only what detects the change moved, from an mtime/size pair nothing
     > read back to the fingerprint ADR-0023 §6 already owned.
     >
     > The marker is late by three weeks. This ADR's Status line says
     > §3–§12 stand, ADR-0023 recorded the deletion in its own file, and
     > nothing pointed here — so a reader opening the record that
     > *established* the stamps found them presented as live. That gap is
     > the failure Rule 6.1 asks a superseding change to close in the
     > superseded ADR's own file.
   - Remove `NIGHTJAR_SUBS_CACHE_BYTES` and all LRU eviction. Keep a
     free-space check before extraction: refuse the job (leave `pending`,
     log clearly) when the data volume cannot hold a conservative minimum
     headroom.

     > **Amended 2026-09-05 — nothing was removed, because nothing was
     > built.** The env var and LRU this bullet names never existed in
     > code; they were a paper promise in
     > [ADR-0010](0010-text-subs-webvtt.md) §7 (corrected in place there
     > the same day). This bullet's "Remove" was satisfied by construction.
     > What ships is the free-space refusal floor this bullet also names:
     > `MIN_FREE_BYTES`, 256 MiB, in `transcode/src/subs/mod.rs`, refusing
     > an extract (leaving `pending`) when the volume cannot hold the
     > headroom.

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
      pile up. (Later measurement showed the 2× term never moves the
      answer for warm walks under 30 s; the floor is the steady-state
      policy. Interval redesign is a separate brief.)
   3. **Dirty follow-up after a busy scan.** `start_scan_job` reuses an
      active job. An fs change that arrives after the walk has already
      passed that directory would otherwise wait for the next poll. The
      watcher marks the library dirty when an fs event hits an active job;
      when that job finishes, a follow-up scan starts immediately. Poll
      reuse does not set the dirty bit (it would force a double cold walk
      on every long index).
   4. **Pause extract for the whole indexing phase.** Extract demuxes are
      multi-minute SMB reads. Running them concurrently with a Movies
      cold walk stretched that walk past 22 minutes with zero rows
      committed; the same tiny library indexed in 12 ms idle and 11 s
      under extract load. Workers still prefer probe over extract, and
      refuse to start new extracts from `begin_index` through
      `set_scan_job_index_done` (walk, upserts, and sidecar rediscovery),
      not only during the walk itself. One in-flight demux may finish;
      new ones wait.
   5. **Sidecar rediscovery is not a full-library readdir.** A pass that
      called `discover_sidecars` for every unchanged title cost ~700 ms
      per parent on this SMB mount (~20 min for ~1,750 Movies). Added and
      updated items associate sidecars in the upsert flush. Unchanged
      media are re-checked only when the walk cache was already warm and
      the parent was re-listed (a new `.srt` bumps that dir's mtime). A
      cold cache after restart skips the bulk pass; existing sidecar rows
      stay in the DB.

      **Amended 2026-09-15 for explicit reconciliation.** The cold-cache skip
      remains the automatic-poll rule, but it does not apply to a fresh/manual
      scan requested by the operator. That path must reconcile supported
      sidecars for unchanged media after restart, using the shared per-directory
      discovery cache so siblings cause one listing rather than per-item
      listings. A failed or incomplete listing preserves prior rows; it is not
      interpreted as an empty successful directory.
   6. **Defer recursive `notify` until the first index pass finishes.**
      Arming recursive watches on an SMB Movies root during the cold walk
      competed for metadata IOPS and pushed walks past 15–20 minutes.
      The watcher polls until `last_index_duration_ms > 0`, then arms
      notify. `NIGHTJAR_POLL_ONLY=1` keeps notify off for shares where
      poll alone is preferred.
   7. **Parallelise directory re-stat (metadata), not file reads.**
      Warm walks are latency-bound: one SMB RTT per directory, issued
      sequentially (~7 ms × 3,046 dirs ≈ 22 s on TV). Concurrent stats
      hide that latency. This is the opposite of parallel *file* reads
      (subtitle extract): those move real bytes through one pipe, and
      more threads make the share worse (the Jellyfin concurrency lesson
      already in this ADR). Extract stays serial; the walk uses a
      bounded worker pool. Default **8** workers
      (`NIGHTJAR_WALK_CONCURRENCY`, clamp 1..=256). Not scaled by CPU
      core count. Chosen below the measured knee (~16–32 on this link)
      so a Pi, a Synology, or a rate-limited rclone mount is not pushed
      to the MacBook Wi-Fi ceiling.

      Serial warm baselines (2026-07-27, before parallel walk; keep for
      legibility of earlier reasoning), MacBook Air, Wi-Fi 802.11ax,
      SMB, Nightjar stopped, Movies 1,754 files / 1,770 dirs, TV 23,062
      / 3,046:

      | Library | Condition | Timings (s) | min / med / max |
      |---|---|---|---|
      | Movies | idle warm (serial) | 0.084, 7.941, 12.925, 12.222, 12.412, 14.931 | 0.08 / 12.3 / 14.9 |
      | TV | idle warm (serial) | 21.292, 22.391, 21.683, 21.506, 21.484, 22.180 | 21.3 / 21.6 / 22.4 |
      | Movies | cold after remount (serial) | 120.7, 115.7, 129.2, 136.0, 124.1, 127.5 | 115.7 / 125.8 / 136.0 |
      | TV | cold after remount (serial, n=3) | 723.7, 728.8, 686.7 | 686.7 / 723.7 / 728.8 |

      Concurrency sweep, same host/link, three warm runs each after a
      serial prime (concurrency 1 is this code's serial path):

      | Library | Conc. | Run timings (s) | median |
      |---|---:|---|---:|
      | Movies | 1 | 12.133, 12.916, 11.928 | 12.1 |
      | Movies | 4 | 0.431, 2.882, 0.471 | 0.47 |
      | Movies | 8 | 1.664, 0.307, 1.637 | 1.64 |
      | Movies | 16 | 0.241, 1.066, 0.333 | 0.33 |
      | Movies | 32 | 0.832, 0.155, 0.780 | 0.78 |
      | Movies | 64 | 0.181, 0.716, 0.161 | 0.18 |
      | TV | 1 | 23.089, 21.238, 21.513 | 21.5 |
      | TV | 4 | 3.060, 2.893, 3.554 | 3.06 |
      | TV | 8 | 1.733, 1.937, 1.461 | 1.73 |
      | TV | 16 | 1.215, 0.979, 1.166 | 1.17 |
      | TV | 32 | 1.046, 0.836, 0.794 | 0.84 |
      | TV | 64 | 0.922, 0.685, 0.819 | 0.82 |

      Knee: TV gains flatten after ~16 (1.17 s → 0.84 s → 0.82 s). RSS
      stayed ~25 MB at the high end; no pathological memory spike.

      At default 8, same day: warm during ffmpeg x264 read of an ~8 GB
      title from the share: Movies 1.9–3.9 s, TV 4.3–7.5 s (ffmpeg still
      running). Cold after one remount: Movies 7.9–21.9 s (first after
      remount slowest), TV 146–150 s (was 687–729 s serial). Cold remains
      file-count-bound; concurrency still helps a lot.

      **Implication for poll scheduling:** TV idle warm at 8 workers is
      ~1.7 s. A fixed 60 s poll is ~3% duty cycle on this link. Adaptive
      interval / confidence / backoff machinery is not justified by walk
      cost alone. SMB-over-Wi-Fi MacBook Air numbers; ethernet and weaker
      hosts are separate data points.

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

10. **Lazy background extraction is the accepted backfill strategy.**
    A first pass over tens of thousands of titles is multi-day wall clock
    (numbers below). That is acceptable as library hygiene: probe makes
    titles playable first; extract drains `pending` in the background;
    restarts re-enqueue. It is not a description of first play for a
    title the queue has not reached yet.

11. **First-play captions: play-priority plus client-driven `<track>`
    reload, chosen.** The growing-`<track>` experiment (Consequences)
    ruled out append-in-place: no browser under test picks up new cues on
    an unchanged URL. Two things close first-play instead:

    - **Play-priority.** Starting playback (`playback_info`, session
      start) on an item whose `subtitle_status` is `pending` calls
      `LibraryPool::prioritize_extract`, moving that item's extract ahead of
      the backfill queue. This does not change the backfill wall clock
      (§10); it changes which title the single serial extract slot works
      on next.
    - **Per-track readiness.** `SubtitleTrack` gains `readiness`
      (`preparing | partial | complete`) and `revision`, server-declared,
      never inferred client-side from file size or a timer. Extraction
      publishes a growing sidecar WebVTT under a `partial` readiness as
      SRT batches land, and flips to `complete` once FFmpeg exits; each
      publish bumps `revision`. `url` is present once a track is
      serveable (`partial` or `complete`); absent while `preparing` and
      for tracks that are listed but never rendered (ASS/SSA, image).
      `subtitle_status` keeps its item-level meaning (§6); it does not
      replace this per-track field.
    - **Client reload, not append-in-place.** Direct-play attaches
      `<track>` elements from the initial `playback_info` response, then
      polls the same endpoint only while any track is `preparing` or
      `partial`. A `revision` increase removes and recreates the changed
      `<track>` element (a fresh URL, per the experiment's finding — see
      `web/src/lib/subtitleProgressive.ts`). This closes the gap for
      direct play. HLS keeps its own path (item 12).

    Not yet run in this session: an in-player dogfood confirming Chrome
    and Safari actually display growing cues end-to-end through this
    reload mechanism during a live extract. The per-browser reload
    mechanics (Firefox's disabled→showing toggle, WebKit's `load` event)
    are carried over from the experiment above, not re-measured here.

12. **HLS MEDIA for ready tracks: 2s slices from the item store VTT.**
    Complete text tracks are declared in the master. The subtitle playlist
    is multi-segment VOD (`subs/{trackId}/segNNN.vtt`) aligned to video
    `SEGMENT_MS`. Segment bodies are sliced in-process from the scan-time
    item VTT on disk — no second FFmpeg demux beside the encode (measured
    failure 2026-07-27: session-inline demux on NAS timed out and Safari
    blocked start). A single whole-title VTT URI as one EXTINF was also
    insufficient for Safari/hls.js cue display on dogfood; segmented
    WebVTT is the delivery shape. Tracks still `preparing` / `partial`
    are omitted from the master so a cold URI cannot hang start;
    play-priority extract (§11) fills the store; captions appear on a
    later session once `complete`. Session-inline demux remains only for
    fixtures / an explicit cold path that does not contend with encode.

    **Cue timing vs segment windows (corrected 2026-07-27).** Segmenting
    delivery and truncating cue times are different operations. An early
    slice clipped each cue's end to the 2s window so the same absolute
    cue would not appear in every overlapping segment (Safari had
    double-painted). That conflation is wrong for hls.js: Chrome renders
    each cue independently and drops the line at the clipped end while
    dialogue continues. The rule is now start-segment ownership: a cue
    is emitted only in the segment that contains its start time, with
    its original start and end untouched, and a stable cue id equal to
    that start time in milliseconds. Players keep displaying past the
    segment boundary until the real end. Duplicating the full cue into
    every overlapping window is rejected (Safari double-paint). Scrub
    into the middle of a long cue without the start segment loaded is an
    accepted gap until a separate client seek/reload fix; it is not
    solved by clipping.

    **hls.js sticky baseline vs title-absolute cues (2026-07-29).** Wire
    times stay title-absolute (native TextTrack / inject). Measurement
    (`rawFirstStart` vs TextTrack after parse) showed hls.js does not add
    each fragment's live `frag.start`; it freezes a load-cycle baseline
    (≈ first frag start of that cycle) and adds that constant to every cue
    until the next reassert. Per-fragment `−frag.start` only cancels for
    the first frag and collapses later cues (pile-up / “works at land then
    dies”). The hls.js path therefore, after each subtitle `FRAG_LOADED`,
    rewrites TextTrack cue `startTime`/`endTime` from the fetched VTT using
    the stable cue id (start ms). Same segment URLs (Rule 4.11); native
    inject unchanged. Dogfood closed the ADR-0017 subtitle gate.

    **Safari native HLS after seek: client cue injection (2026-07-28).**
    Delivery stays EXT-X-MEDIA → `subs/{trackId}/segNNN.vtt` (one wire
    shape; Rule 4.11). There is no OpenAPI or other client-visible API
    contract change: injection only consumes those existing segment URLs.
    Linear Safari playback can populate the native TextTrack from that
    rendition. After a user scrub, WebKit does not reliably reload those
    cues: dogfood traces showed teardown → media advance → re-show → 15s
    cover wait still at `cues=0` with the segment endpoint confirmed
    correct via curl. Mode bounce, Off→English dwells, and seeking-time
    disable all failed the same way. This matches the class of Safari
    native-HLS subtitle reload failures reported by other players (e.g.
    Shaka). The web client therefore, after the first scrub on the
    native-hls attach path only, disables the HLS-managed TextTrack,
    fetches the playhead segment (and the next) from the same `segNNN.vtt`
    URLs, parses WebVTT locally, and `addCue`s onto a distinctly labeled
    `video.addTextTrack` track (e.g. `English (Nightjar)`), continuing
    segment-by-segment on `timeupdate`. Same-label native + inject tracks
    let WebKit non-deterministically restore DEFAULT `showing` on the HLS
    MEDIA track after Off→On; mode is re-asserted on every
    `change`/`addtrack` while inject mode is active. hls.js remains a
    separate backend: scrub uses `startLoad` to retarget TEXT fragments.
    Pattern: when a browser's default subtitle-reload path fails after
    seek, the client drives cues explicitly against the existing
    segmented VTT URLs rather than inventing a third delivery shape.
    Native scrub again sends playlist `?startMs=` for A/V land accuracy
    (ADR-0011 amendment): inject no longer depends on that fetch completing
    for captions, and the native handler does not hold `seekInFlight`
    across startMs so rapid seeked events are not swallowed. Full
    rapid-scrub soak of inject under chaotic encode restarts remains a
    separate dogfood check from land-accuracy verify.

13. **Standalone immutable subtitle artifacts (D2B.2).** This section fixes
    the contract that D2B.2 implements and D2B.3 extends. It composes the
    [ADR-0058](0058-revision-safe-probe-snapshots.md) coherent probe snapshot
    with the [ADR-0010](0010-text-subs-webvtt.md) §4 serveable sidecar
    membership and generations. It does not adopt
    [ADR-0042](0042-derived-artifact-versioning-and-reconciliation.md)
    wholesale: there is no second fingerprint, no history table, no eager
    backfill, and no playback-time probe.

    1. **Per-track identity and the bounded generation token.** An artifact
       belongs to exactly one track. The URL is
       `GET /api/v0/items/{itemId}/subtitles/{trackId}.vtt?g={token}`. The
       server mints `g`; clients treat it as opaque and never construct it.
       The token is versioned and bounded by this grammar:

       ```text
       token := "v1" "-m" media_rev "-p" probe_rev [ "-s" sidecar_gen ]
       rev   := "0" | [1-9][0-9]*     (unsigned decimal, no leading zero)
       ```

       - `media_rev` and `probe_rev` are the ADR-0058 revisions of the
         certified snapshot.
       - `sidecar_gen` is present only for a sidecar track, and is that
         track's own ADR-0010 §4 durable generation. It is never the
         complete sidecar set, and no other track id enters the token.
       - The token is at most 65 ASCII bytes and matches `[a-z0-9-]`. It is
         validated by grammar before it is used as a path segment or
         compared. Embedded tracks of one certified snapshot share a token;
         sidecar tracks differ when their generations differ.

    2. **On-disk layout and artifact revision.** The immutable artifact for
       `(itemId, trackId, token)` is
       `{NIGHTJAR_DATA_DIR}/subs/{itemId}/{token}/{trackId}.r{artifactRevision}.vtt`.
       `artifactRevision` is a positive decimal integer allocated
       monotonically per item by the DB from
       `media_items.subtitle_artifact_sequence`. Allocation reserves a
       candidate identity; it does not publish readiness. The filename is
       resolved exclusively from the committed publication row of §13.4, so
       no caller constructs it. The token is the only source-derived path
       component; track ids keep their ADR-0010 shape. A candidate is
       finalized without overwrite: an attempt never renames over an
       existing final artifact. A generation directory is immutable once
       complete: no later attempt overwrites its bytes, and a different
       generation is a different directory. For D2B.2 artifacts this
       replaces the mutable §4 path `subs/{itemId}/{trackId}.vtt`.

    3. **Publication.** For one captured certified source, the worker:

       1. captures item, media, probe, and track revisions and, for a
          sidecar, its generation;
       2. validates embedded and sidecar source identity before extraction,
          after extraction, and before final publication;
       3. reserves an `artifact_revision` of §13.2, writes a complete
          artifact to a temporary file, fsyncs the file, finalizes it into
          the generation directory without overwriting an existing final
          artifact, then fsyncs every newly created ancestor directory entry
          — the generation directory and the item directory — before any DB
          publication;
       4. publishes readiness and the URL through one DB compare-and-swap
          that commits the reserved `artifact_revision` and requires the
          captured certification, membership, revisions, and generation
          still to match.

       A CAS mismatch, or a source identity check that fails, preserves the
       old committed reference and bytes and leaves only the temporary or
       finalized candidate bytes unreferenced. A finalized but uncommitted
       artifact is an orphan: it is never partial or servable, including
       after restart.

    4. **Committed per-track publication reference.** Serving requires a
       committed per-track publication reference for the requested
       `(itemId, trackId, token)` that matches the current coherent
       certification and membership. The reference is one row keyed by
       `(itemId, trackId, token)`, written only by the source CAS of
       §13.3.4. It records the captured certification `content_id`, the media
       and probe revisions, the track's own sidecar generation when it has
       one, the allocated `artifact_revision` of §13.2, the per-track
       `revision` of §11, and the partial/complete state. The CAS commits the
       row to that exact `artifact_revision`; `revision` remains the
       client-visible update counter. A listed track that has no such
       committed reference is never given a URL, and readiness is never
       inferred from a file on disk.

       `partial` and `complete` are keyed by that same identity and become
       visible only through the committed reference. A `partial` row is
       written by a progressive publication that passed the source CAS; a
       `complete` row is written per track, only after that track's extraction
       reached successful EOF. A cancelled or failed embedded demux may
       salvage the cues it already flushed only as `partial`, and must still
       report the cancellation or unavailability. The coarse item-level
       `subtitle_status` becomes `ready` only after every serveable track of
       the captured source is durable. Complete immutable bytes are never
       overwritten by another attempt: a run skips a track whose complete
       reference already exists for the token, so one sidecar edit cannot
       re-demux or overwrite an unrelated track under its unchanged URL. A
       partial-to-complete transition may reference the identical finalized
       bytes and must not rewrite them. A failure, `none`, or `partial` write
       from stale work cannot mutate newer subtitle state.

       The item-level `subtitle_status` keeps its §6 meaning as the coarse
       preparing/lifecycle field. It is not the serving gate, and
       reconciliation does not clear it item-wide: only the affected per-track
       references are invalidated, so unchanged rows and their committed
       artifacts remain serveable.

    5. **Progressive output.** A growing sidecar VTT is published under the
       same captured source checks and the same CAS as the complete
       artifact. Each progressive publication is keyed by the complete
       per-track generation identity. Every changed partial or complete body
       reserves its own `artifact_revision`, validates the source before and
       after conversion, finalizes without overwrite, and CASes the
       publication to that exact revision. A CAS or fault failure preserves
       the old reference and bytes and leaves only an unreferenced
       candidate. Stale progressive work is rejected by the CAS and cannot
       become ready for a newer generation.

    6. **Single-flight.** Combined-generation single-flight identity includes
       the captured source generations. A work item executes only the
       generation it captured; it cannot silently re-read and execute as
       another generation. When a newer generation arrives during older
       work, the newer demand is preserved as a successor and runs after
       the older item.

    7. **Serving and stale URLs.** A request is served only when it names a
       current certified member at matching generations. A stale URL, or a
       URL whose generation no longer matches the current token, is rejected
       on cache miss even if cleanup has not run. Old artifacts are not
       served after restart.

    8. **D2B.3 gating, bounds, and API.** Until D2B.3, piggyback and
       HLS/session subtitle writers fail closed for subtitle publication.
       They cannot expose legacy mutable artifacts through this contract.
       AV playback remains available, and the exact D2C mapping is
       preserved. The existing global extraction and conversion limits
       stand. No automatic polling change, per-item relisting, playback
       probe or retry, piggyback migration, or cleanup dependency is
       introduced. `g` is a required opaque query parameter documented in
       the OpenAPI spec.

       **Amended 2026-09-15 (D2B.3).** The gating above is lifted; the
       migrated writers obey the same §13.3 contract.

       - **Piggyback.** The session captures the certified source at create
         (§13.3.1) and the run EOF publishes the assembled side output
         through the one publication path of §13.3.3–§13.3.4: the same
         source revalidation, immutable finalize and per-track compare-and-
         swap the standalone extract uses. A source that changed during the
         run, or a track the capture does not mint a token for, publishes
         nothing and the item stays `eligible`. The D2C exact one-stream
         mapping is unchanged and regression-tested.
       - **HLS/session.** A session declares a subtitle rendition only for a
         track whose complete artifact is committed for the current certified
         source, and the declared path is that committed generation-addressed
         artifact. Session create is therefore the generation gate: the
         session serves the one immutable generation it resolved, which is
         the session snapshot ADR-0010 already decides. A partial track is
         omitted so a cold URI cannot hang start. The session-inline mutable
         copy is never declared as a rendition.

       The existing global extraction and conversion limits stand, and no
       playback-time probe, timer retry or failure backoff is added.

## Consequences

- First play of an already-extracted title shows captions immediately.
  Measured on a ready title: WebVTT GET 0.5–4 ms; HLS master playlist
  included `#EXT-X-MEDIA:TYPE=SUBTITLES` on the first fetch (0.38 s
  including session create) with playlist entries pointing at the stored
  VTT URLs. First play of a title still `pending` shows the preparing
  line, not a multi-minute hang inside the GET. Whether that preparing
  line can become near-immediate captions is still open (§11).
- Add-file timings on the full household Movies library over SMB
  (2026-07-26), extracts paused during index, tiny probe MKV + sidecar,
  ~1,750 titles on the maintainer's household Movies library over SMB:

  | Path | Until listed | Until playable | Until subtitled | Discovering walk |
  |---|---|---|---|---|
  | Poll fallback (`NIGHTJAR_POLL_ONLY=1`; natural poll after cold 111.6 s, interval 223 s) | 148.7 s | 148.7 s | 148.7 s | 567 ms |
  | Watcher (`notify` armed after first index; fs create) | 4.4 s | 4.4 s | 4.4 s | 524 ms |

  Cold index on the same mount was 103–112 s with `sidecar_checked=0`
  after the rediscovery change (earlier contended cold walks ran past
  20 minutes). Listed/playable/subtitled landed in the same sample for
  the probe title: warm walk plus probe plus sidecar convert finished
  inside one poll tick. Poll wall time is dominated by waiting for the
  scaled interval, not by the walk. An idle single-directory library
  still indexes in 0.26 s listed / 0.51 s subtitled via `POST /scan`;
  that is not the Movies-scale number.

  The earlier 27 s watcher row was a tiny library under concurrent
  Movies extract load and is superseded by the Movies-scale table above.
- Growing-`<track>` experiment (Playwright Chromium / Firefox / WebKit,
  partial WebVTT served to `<video>` while cues were appended):

  | Browser | Append in place (no reload) | Reload `<track>` (new URL) |
  |---|---|---|
  | Chromium | No | Yes |
  | WebKit (Safari) | No | Yes |
  | Firefox | No | Yes (cues empty until `mode` toggled `disabled`→`showing`) |

  Progressive growing VTT does not give near-immediate first-play
  captions without client cooperation. Play-priority plus client-driven
  `<track>` reload (§11) is the chosen fallback; an in-player Chrome/
  Safari dogfood of that path has not been run as of this writing.
- A first extract pass is wall-clock bound by sequential NAS read speed,
  not by pool width: extract is serialised by a process-wide lock (shared
  tmp paths), and subtitle packets are interleaved throughout the
  container, so each text-bearing title costs roughly one full read.
  Parallel extract would only split the same pipe.

  Movies (2026-07-26):

  | Quantity | Value |
  |---|---|
  | Library size | 1,745 titles, 16.5 TB, mean 9.5 GB, p90 17.8 GB |
  | Text-bearing (subrip/mov_text/webvtt/text) | 68% by count, **74% by bytes** (n=200) |
  | Demux throughput (8–10 GB subrip titles, isolated) | 54–56 MB/s, ~18 s/GB |
  | Header-only classify | ~0.26 s/title → ~7.5 min for 1,745 titles |

  Text-bearing bytes ≈ 0.74 × 16.5 TB = 12.3 TB at ~18 s/GB ≈ **61 hours
  (~2.6 days)** demux, plus classify.

  TV Shows (2026-07-26), same share:

  | Quantity | Value |
  |---|---|
  | Library size | 23,060 files, est. 32.5 TB (n=500 sizes; mean 1.41 GB, p50 0.96, p90 3.20) |
  | Text-bearing | **54% by count, 62.3% by bytes** (n=200) |
  | Demux throughput (three ~1.15–1.38 GB episodes, isolated) | 26.2 / 26.5 / 31.2 MB/s (walls 44 / 52 / 43 s) |
  | Header-only classify | mean 0.167 s (p50 0.142, p90 0.263) → **~64 min** for 23,060 titles before demux |

  Text-bearing bytes ≈ 0.623 × 32.5 TB ≈ 20.2 TB. At ~28 MB/s (~36 s/GB)
  that is ≈ **202 hours (~8.4 days)** demux, plus the classify overhead.
  Episode-sized files are still demux-dominated at these sizes: ~50 s
  read versus ~0.17 s classify. The 0.26 s × 23k ≈ 100 min figure is an
  upper bound using the Movies classify sample; the measured TV mean is
  lower. Very small files would invert that ratio; this library's p50 is
  near 1 GB.

  Combined first-pass estimate (~24,800 items), serial, uncontended:

  | Library | Classify | Demux (text-bearing bytes) | Total |
  |---|---|---|---|
  | Movies | ~7.5 min | ~61 h | **~2.6 days** |
  | TV Shows | ~64 min | ~202 h | **~8.5 days** |
  | Both | ~1.2 h | ~263 h | **~11 days** |

  Lazy background extraction accepts that wall clock (§10). It does not
  close first-play for a title still waiting in the queue (§11).

  The earlier **21 items/hour** figure was measured while a cold Movies
  index walk was starving the same share; it is a contention artifact,
  superseded by the throughput numbers above.
- Schema migration `005` adds `subtitle_status` and the source mtime/size
  stamp columns (append-only). OpenAPI gains `subtitleStatus` on item and
  playback-info schemas (additive, v0).
- ADR-0010 §2–6, §8–11 (track identity, WebVTT delivery, sidecar
  discovery, API shape, HLS MEDIA skin) stand. Only the cache and the
  playback trigger are replaced.
- Image / ASS burn-in is [ADR-0018](0018-subtitle-burn-in.md): listed with
  `render: burnIn`, no WebVTT `url`, selected at session start.
- Directory-mtime polling can miss an add if the SMB server fails to bump
  the immediate parent mtime; the scaled full cold walk after process
  restart still heals that. Do not raise the poll frequency to compensate.
