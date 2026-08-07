# ADR-0041: Subtitle classification and client-gated extraction

- Status: accepted; amended 2026-08-07 (Decision 8.7 scope note)
- Date: 2026-08-06
- Supersedes: [ADR-0013](0013-subtitle-extraction-at-scan.md) §1 and §2 (this
  ADR's Decision 1–2 replace them; ADR-0013 §1/§2 carry a pointer to this
  file). ADR-0013's remaining sections (§3–§12: single-pass demux, on-disk
  shape, cleanup, status/API shape, watcher polling, restart resume, HLS
  MEDIA delivery) stand unchanged.
- Related: Rule 4.13 (derived, on demand — this ADR is its subtitle
  application), [ADR-0022](0022-capability-profiles.md) (client profile
  ids), [ADR-0019](0019-ass-burn-extract-at-scan.md) (`.ass` store,
  inherited unchanged), [ADR-0014](0014-library-availability.md)
  (`unavailable` failure class, reused for extract timeout/IO),
  [ADR-0026](0026-metadata-pipeline.md) §3 (backoff shape, reused)

## Context

ADR-0013 §1 enqueues subtitle extraction as a background job for every
indexed item, unconditionally: "a background job enqueued when an item is
indexed." §2 sizes that queue as a scan-time sweep on the shared worker
pool, below probe. That is the same mistake Rule 4.13 now names: a derived
artifact queued for the whole library as a consequence of scanning, rather
than built when a consumer needs it.

Three independent measurement runs on 2026-08-06 (dogfood library, WiFi over
a degraded array — every timing here is an upper bound; re-measure once the
array rebuild finishes and cite n + transport) found the same shape of
waste:

- **Subtitle inventory, n=500:** 66.8% of items have an embedded text or ASS
  track that needs a pass; 20.8% are sidecar-only (no source read needed);
  12.0% are image-only (ADR-0019 §6 burn-in owns those, no WebVTT); 0.4%
  have no subtitles at all. Codec mix is fully known for this library
  (`n_unknown = 0`), so the classification the probe already computes is
  complete, not a guess.
- **Client timeline, n=200 unbiased:** standalone extraction was needed by
  **0 of 200** sessions. Projected from the container cross-tab: ~199
  items library-wide, all `mov_text` in MP4-family. Every other session
  either reads the container itself (a native/MPV-class client) or already
  has ffmpeg open on the file for a remux/transcode, which produces the
  subtitle rendition as a side output.
- **What the dogfood DB holds today:** `subtitle_status` is 23,053 pending
  / 1,034 ready / **783 error** / 64 none. Almost the entire library is
  queued to rediscover, by demux, information the probe pass already knew
  at index time — because ADR-0013 §2 discards the ffprobe subtitle stream
  list instead of persisting it.

## Decision

1. **Persist the subtitle stream inventory at probe time.** ffprobe already
   returns the subtitle stream list; the parser drops it. New table
   `media_item_subtitle_tracks`, one row per stream: `media_item_id`
   (FK), `stream_index`, `codec`, `language`, `title`, disposition flags
   (`forced`, `sdh`/hearing-impaired if the container carries it), and a
   derived `kind` (`text` | `ass` | `image` | `unknown`). A table, not a
   JSON column, because `trackId` needs stable identity across re-probes
   (ADR-0025's inventory shape, ADR-0038's stored track descriptions) and
   because "which items have text tracks" must be a query the classifier
   and any future UI can run directly. An unrecognised subtitle codec
   classifies as **`unknown`, counted**, never silently dropped as
   harmless — the measured library has `n_unknown = 0`, so this path is
   exercised only by future or unusual sources, and it must not be a
   silent no-op when it is.

2. **Classification replaces discovery.** `subtitle_status` is derived at
   probe time, not by a later extract pass finding out:
   - no text stream and no sidecar → `none`
   - image-only (PGS, VobSub) → `none` (ADR-0019 §6 burn-in owns rendering;
     there is no WebVTT for these)
   - sidecar text only → convert in process from the sidecar file; the
     source video is never opened
   - one or more embedded text or ASS streams → `eligible`

   Today's 64 `none` rows are an *outcome* of a completed demux. Under this
   ADR they are a probe-time classification with zero ffmpeg invocations —
   the fixture in Acceptance asserts exactly that (no source read).

3. **Client gating — the filter nothing applies today.** From ADR-0022's
   profiles: `BROWSER_V0` cannot read embedded container subtitles and
   needs the extracted file; `MPV_V0`, `MEDIA3_V0`, `AETHER_V0` read
   subtitles from the container themselves and need nothing extracted. A
   session on one of those profiles must never enqueue extraction —
   enqueuing it today is pure waste, since nothing downstream consumes the
   result.

4. **Method gating — the larger reduction.** A remux or transcode session
   already runs ffmpeg reading the file, and HLS carries subtitles as a
   separate WebVTT rendition (ADR-0013 §12) regardless of client, so those
   paths produce the artifact as a side output of work already happening.
   **Only browser direct play needs a standalone extraction job.** Measured:
   0 of 200 unbiased client-timeline sessions needed one; ~199 items
   library-wide by projection, all `mov_text` MP4-family. Standalone
   extraction wall on that population (`--only direct_play`, n=24, 11 with
   text tracks): 5.0 s median, 7.2 s max, 235 MB median source, 55 MB/s,
   zero failures.

5. **Trigger: on demand, via the ADR-0013 §11 play-priority hook. Not at
   scan, not on poll.** `playback_info` or session start for an item that
   is `eligible`, being played by a client that needs the extraction
   (Decision 3) via a method that will not produce it as a side output
   (Decision 4), calls the existing `prioritize_extract` path. This
   replaces the scan-time unconditional enqueue in ADR-0013 §1. Rule 4.13:
   a whole-library pass stays available as an explicit opt-in operation
   (Decision 8), never the default.

6. **Progressive extraction: dropped from v1.** ADR-0013's `revision` /
   per-track `readiness` machinery already exists for the growing-`<track>`
   reload experiment it was designed for. At the measured 5.0 s median /
   7.2 s max wall for the population that actually needs standalone
   extraction (Decision 4), progressive delivery buys materially less than
   the case it was designed for: a 7-second wait is not "subtitles arrive
   after the film ends," and the browser polling/swap API surface it
   requires is not worth building for that population. Standalone
   extraction runs as one bounded pass and flips `subtitle_status` from
   `eligible` to `ready` on completion, same as any other extract; no
   `partial` readiness state is reachable from this path. `readiness` and
   `revision` on `SubtitleTrack` are unchanged for tracks that arrive via
   other paths (e.g. a title already fully extracted). `subtitle_progress_ms`
   was never shipped as a column for this path and is not added.

7. **Piggyback on a session that starts near zero.** Add `-map 0:s?`
   outputs to a remux/transcode session's ffmpeg invocation when it is
   already running and the item is `eligible` — free bytes, since the
   process is already open on the file (Decision 4). Opportunistic only:
   ADR-0007 kills the process on seek and ADR-0023 sessions can start at an
   offset, so a piggybacked extract may yield a prefix rather than the
   complete file. A prefix is still useful (the extraction remains
   `eligible` until a later pass, whether piggybacked or standalone,
   completes it) and is never reported as `ready` until it is complete.

8. **Eager whole-library pass: an explicit `nightjar` subcommand, and it
   must be reliable.** **Status: the form is decided, the subcommand is not
   built.** This slice ships 8.1–8.8, the reliability behaviour the pass
   must have; it does not ship an invocation for it. There is no subcommand,
   no config toggle, no environment variable, and nothing in the scanner
   pool sweeps the library today — an operator cannot run this pass at all
   yet. Deciding the *form* now (subcommand, not a toggle) rather than
   leaving it open is what stops a toggle from getting built by default when
   someone eventually wires the trigger up; it is not a claim that the
   trigger exists. Tracked as a residual in
   `nightjar-meta/notes/plan-derived-artifacts-slice-2026-08-06.md`, not
   only here — an ADR noting its own gap and a plan not tracking it is how a
   residual gets forgotten (ADR-0026 §8.4's fate: frozen in an ADR, never
   implemented, untracked until it surfaced months later during ADR-0037).

   Rule 4.13's escape-hatch clause exists for a need the
   on-demand path structurally cannot serve: a household about to lose
   network access (moving, an ISP outage, taking the server off-grid for a
   time) wants subtitles materialised ahead of time, and on-demand
   extraction only fires when a client actually asks for a title — by
   definition after the file has become unreachable, not before. That is a
   real user need with no other answer, which is the shape Rule 4.12 asks
   for, not a preference between eager and lazy that a settings screen would
   invite someone to pick as a matter of taste. (Jellyfin's shipping this as
   a plugin is not that argument — Rule 4.12 does not care what Jellyfin
   ships, and their community's reported multi-day re-extraction on cache
   clear is a reliability cautionary tale, not a justification; kept as
   (2)–(4) below, not as the reason this exists.)

   Because it rescopes work library-wide the way ADR-0037 item 2's region
   change rescopes classification library-wide, it is a `nightjar`
   subcommand on the server, not a route and not a config toggle — the same
   escape-hatch class as ADR-0034 item 12's password reset and ADR-0037
   item 2's region change. A subcommand requires local filesystem/shell
   access, the trust boundary already implied by operating the box; a
   toggle in a settings screen would sit next to on-demand extraction as if
   the two were equally-weighted preferences, when on-demand is the only
   default this ADR permits (Decision 5) and eager is an operator escape
   hatch for a specific, named failure mode. `reset-password` is ADR-0034's
   only subcommand so far; this would be the second, and anything else
   wanting one still argues for itself. Each requirement below traces to a
   measured or observed failure:

   1. **Timeout scales with source size.** Measured throughput on the
      extract path is 55 MB/s. A fixed timeout (the prior 300 s) was an
      unstated ~16 GB ceiling — below the 22–33 GB top of the dogfood
      queue. The eager pass computes and logs a per-file timeout budget
      from size at the measured rate, not a constant.
   2. **Timeout and I/O failure classify as `unavailable`, never `error`**
      (ADR-0014's failure class, reused). `error` means a completed demux
      that produced unusable output; a share timing out mid-read is not
      that.
   3. **Attempt counting and backoff on `unavailable`.** ADR-0014
      re-queues every `unavailable` row on each reachability transition;
      without a cap, a flapping mount plus one unfinishable title is
      permanent load. Reuse ADR-0026 §3's backoff shape (1 day, 7, 30, 90,
      capped at 90) rather than inventing a second schedule.

      **Schema (Rule 4.9, recorded after the fact — migration 018 shipped
      this shape ahead of this paragraph; the columns are correct, this is
      the ADR catching up).** Two columns on `media_items`:
      `subtitle_attempt_count` (`INTEGER NOT NULL DEFAULT 0`) and
      `subtitle_next_retry_at` (`TEXT`, nullable — no deadline until a
      failure is recorded). The schedule itself is not duplicated: both
      columns are driven by `nightjar_db::backoff_days`, the same function
      ADR-0026 §3's `metadata_negative_cache` already calls, so the two
      backoff shapes cannot drift apart under Rule 4.11 (one concept, one
      path — "unavailable and retriable" is one idea whether it is a
      subtitle extract or a metadata lookup, and the schedule is the same
      idea on both sides). Both columns are written by `set_subtitle_status`
      and reset on any non-`unavailable` status write.

      State lives on `media_items` rather than a side table because subtitle
      backoff is 1:1 with the item: one `subtitle_status`, one retry clock,
      same row that already carries `subtitle_status`,
      `subtitle_source_mtime_ms`, and `subtitle_source_size_bytes`. That is
      a different shape from `metadata_negative_cache`, which is keyed on
      `(provider, kind, query_key)` because one item can accumulate several
      failed metadata queries against several providers — a genuine
      one-to-many the negative cache's own table exists to hold. Reusing
      *that* table for subtitle backoff would have meant inventing a
      `query_key` for something that is not a query, to share a table shape
      that does not fit; reusing the *function* both tables call is the
      actual shared part, and that is what Rule 4.11 asks to be shared.
   4. **Per-track partial success.** Write each track to a temp path and
      atomic-rename it individually. Today one bad stream fails the whole
      invocation (Jellyfin has this bug via mis-mapped external MKS
      subtitles); at a measured ~9 text tracks per extract-class file and
      65 in one observed outlier, that is the difference between losing
      one track and losing every track in the file.
   5. **Never delete a good file on a failed pass.** A retry that fails
      after producing zero usable output must not remove a previously
      `ready` track.
   6. **One bulk-reader gate, shared with the keyframe map walk** (ADR-0023
      §2 amendment, same date). A whole-file reader against a library root
      is the same class of load whether it is a subtitle extract or a
      packet walk; one gate serialises both. ADR-0013 §8.4 already treats
      extract as serial and paused for the whole indexing phase; the
      2026-08-06 dogfood log showed extract starting at 00:44:57 while
      polls fired at 00:44:37 — the `begin_index` → `set_scan_job_index_done`
      pause did not cover a poll-initiated pass, which this item closes by
      sharing one gate rather than two independently-serial checks.
   7. **Cancel in flight when a library goes unreachable.** ADR-0014's
      pause gates job *start*; a demux begun before a mount died keeps
      reading a dead share until it hits the timeout in (1). Cancel on the
      same reachability transition that pauses new starts.

      **Amended 2026-08-07 (scanner audit):** this mechanism shipped for
      both subtitle extract and the keyframe-map packet-walk
      (`server/crates/scanner/src/pool.rs`, `keymap/packet_walk.rs`), not
      extract alone — item 6's shared bulk-reader gate always covered both,
      and 8.7's cancel followed the same path. Probe and the directory walk
      do not have it: probe is a single blocking `ffprobe` call with no
      cancellation hook, and the walk checks reachability once before
      starting and not again. Both are common paths, not edge cases. Scope
      for closing that gap is probe and the walk, not "probe/map/artwork/
      walk" — map is done, artwork is not this crate's concern.
   8. **Progress observable:** queue depth, completed count, a rate
      estimate — enough for an operator running the eager pass to know it
      is moving, without a client polling an individual item.

9. **Migration.** Reset the 783 `error` rows to `pending` so a re-probe
   drives them through Decision 2's classifier and lands on `none` /
   `eligible` honestly — the same migration-then-reclassify pattern
   already used for `probe_status = 'error'` in migration 006 (which
   resets to `indexed`, the pre-classification state, not a guessed
   outcome), applied here because `error` under the discovery-based old
   logic recorded environment failure, not a durable fact about the file.
   `SUBTITLE_STATUSES` gains `eligible` as a new value alongside the
   existing `pending | ready | none | error | unavailable`
   (`server/crates/db/src/status.rs`); `pending` keeps its existing
   meaning (not yet probed), `eligible` is Decision 2's new terminal
   classification for "needs extraction, not started." The 23,050
   `pending` rows need a re-probe to populate the new inventory
   table (header reads measured at 217 ms median, roughly 90 minutes
   serial for the library, but that is 24,935 file opens against a share).
   **Default to a deliberate, operator-run ops pass, not a first-start
   backfill** — a backfill that fires itself on upgrade is exactly the
   scan-time surprise this ADR removes; running it silently on every
   install's next boot would reintroduce Rule 4.13's problem one layer up.

10. **Deletions (Rule 4.5).** This slice must delete, not only add:
    - the unconditional scan-time `extract` enqueue (ADR-0013 §1)
    - the fixed 300 s extract timeout constant (Decision 8.1 replaces it)
    - whichever of the two failure-classification call sites currently
      writes `subtitle_status = 'error'` for a transient I/O condition is
      wrong under Decision 8.2 and is removed, not kept alongside the
      correct one (Rule 4.11 — one concept, one path, for "this extract
      did not finish")

## Consequences

- The 23,053 `pending` / 783 `error` rows this ADR inherits are not
  representative of the corrected classification; Decision 2's rule,
  applied after the migration in Decision 9, is expected to move most of
  that population to `none` (image-only, no-subtitle) or leave it
  genuinely `eligible` and now correctly gated by Decisions 3–4 so the
  large majority never triggers a standalone extract at all.
- `SubtitleTrack.readiness` stays in the API shape (ADR-0013 §11) for
  tracks that reach `ready` via any path; the browser polling/swap
  behaviour that would consume a `partial` state is not built for the
  standalone-extraction path (Decision 6). If a future measurement shows a
  population with materially longer extraction walls than this library's
  MP4/`mov_text` case, progressive delivery is a candidate to revisit —
  not assumed dead everywhere, just not worth its API cost for the
  measured v1 population.
- Net queue size falls sharply: the population that ever reaches a
  standalone extract job is ~199 items library-wide (Decision 4), not the
  ~24,800-item sweep ADR-0013 §1 described.
- `media_item_subtitle_tracks` is new derived-adjacent data (it stores
  probe output, not extracted bytes) and follows the same identity
  discipline as other derived artifacts under ADR-0023 §6: keyed on
  `media_item_id`, re-derived on re-probe, no separate content-id column
  needed because a re-probe simply replaces the rows for that item.
- Migration numbering: next free migration after `016_series_identity.sql`
  is `017`; this ADR's writer (Decision 1, 9) lands in that file, not a
  placeholder, per Rule 4.9 (shape decided here, before the writer).

## Out of scope

Session slot scheduling. Burn-in selection and encode graphs (ADR-0018).
`.ass` store shape (ADR-0019, inherited unchanged). `trackId`, store path,
ADR-0025 keys (unchanged from ADR-0013). Subtitle download providers.
Widening `BROWSER_V0` (recorded, not acted on, in the source measurement
brief — see `nightjar-meta/docs/derived-artifacts-slice-brief.md` §5).
QSV / hardware transcode fallback (same brief, same section — unrelated
finding, ops not code).

## Acceptance

Corpus fixtures (Rule 4.3), all small and synthetic — see
`nightjar-meta/docs/derived-artifacts-slice-brief.md` Acceptance for the
authoritative list this ADR's implementation is checked against:

- no subtitle streams → `none`, asserting ffmpeg is never spawned
- image-only → `none`, no extract
- sidecar-only → converted, source never opened
- multi-text-track (≥3) → one demux, all tracks out
- one deliberately unmappable stream among good ones → per-track partial
  success (Decision 8.4)
- an unrecognised subtitle codec → classified `unknown` and counted
  (Decision 1)
- a damaged Matroska that fails EBML parse → `error`, no panic
- a truncated index → usable extent recorded, no crash

Behavioural tests:

- a timeout lands `unavailable`, not `error` (Decision 8.2)
- extract does not run concurrently with a poll-initiated walk (Decision
  8.6)
- an `MPV_V0` session enqueues no extraction; a `BROWSER_V0` direct-play
  session with embedded text does (Decision 3)
- a remux/transcode session produces its subtitle rendition without
  enqueuing a standalone extract (Decision 4)

## References (notes)

- `nightjar-meta/docs/derived-artifacts-slice-brief.md` — source brief,
  2026-08-06, all measured figures in this ADR (n and transport as stated
  there; WiFi over a degraded array, upper bounds, re-measure once the
  array rebuild finishes and the `--sample` flag is dropped from
  `subtitle_inventory_scan.py` / `keyframe_index_probe.py`)
