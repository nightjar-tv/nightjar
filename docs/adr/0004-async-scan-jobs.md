# ADR-0004: Async scan jobs and Gate 1 index-pass criterion

- Status: accepted; amended 2026-08-08 (§2.2, §2.4 and a new §3 — in place,
  Rule 6.4)
- Date: 2026-07-25

## Context

Gate 1 required a 10,000-file library scan under 60 seconds on a Raspberry Pi 4.
That criterion was written when "scan" meant one synchronous walk, ffprobe, and
upsert. Serial ffprobe of 10k files exceeds a 90-second HTTP socket timeout on
desktop hardware. A release-mode binary cannot fix a sequential I/O bound.

The copy deck already promises progressive availability: "Scanning your library
— you can start watching as items appear." The gate was measuring a different
meaning of "scan" than that promise.

Splitting index from probe is an API-shape decision (Rule 6.1). Quietly
reinterpreting the gate without an ADR would violate Rule 6.4.

## Decision

Scan is two phases. The Gate 1 blocker is the index pass only. Probe throughput
is a floored metric, not a gate. The API returns a job id immediately and
clients poll for status.

### 1. Gate 1 scan criterion (Rule 6.4 amend of the plan gate)

The index pass (walk, filename parse, upsert) must make the library browsable
with items appearing in under 60 seconds for 10k files on a Pi 4. Unchanged
rescan index pass remains under 5 seconds.

Probe throughput from a bounded ffprobe worker pool is tracked. The 10k harness
reports files/sec and fails below `PROBE_FLOOR_FPS` (default 50 files/sec;
measured about 156 on an M-series MacBook, 2026-07). The floor is deliberately
loose. It exists to catch a change that halves probe speed, not to benchmark
hardware. CI may set a lower value (e.g. 40) when shared runners sit near the
default. Record a Pi-calibrated floor when the Gate 1 hardware run happens.

This aligns the gate with the copy deck promise.

### 2. Async scan jobs (API)

1. `POST /api/v0/libraries/{libraryId}/scan` returns 202 with a `jobId`
   immediately. It does not wait for probes.
2. `GET /api/v0/scan-jobs/{jobId}` returns job `state` and counts:
   `queued` → `indexing` → `probing` → `completed` | `failed`, plus
   `indexDurationMs` once the index pass finishes.

   *Amended 2026-08-08.* `indexDurationMs` and `probeDurationMs` describe
   **overlapping** spans and do not sum to job wall time. Probing now starts
   during the index pass (§2.4), and `probeDurationMs` is measured from the
   first probe enqueued rather than from the end of the walk, so it keeps
   meaning "how long probing took". `probeDurationMs` has also never covered
   the fs-notify or `drain_pending_probes` enqueue paths — those carry no
   batch — so it measures one scan job's probes, not all probing on the
   server. That scope, not a timing fault, is the likely explanation for
   probing observed continuing past `completed` on the 2026-08-07 run.
3. Each media item carries `probeStatus`: `indexed` | `probed` | `error`.
   Indexed items are listable before codecs land. Playback decisions treat
   `indexed` as probe-pending (not direct play).
4. Implementation: an index pass that **enqueues each probe as it discovers
   it**, drained by a worker pool of about `available_parallelism` ffprobe
   children. Batch DB writes on the index path. One completion barrier per
   scan job still defines job end: its counter starts at zero and rises with
   the walk, and the job waits on it once the pass is done.

   *Amended 2026-08-08 (Rule 6.4).* This previously read "fast index pass,
   **then** a worker pool ... draining a queue", and the code matched: probes
   accumulated in a `Vec` through the whole pass and were handed over exactly
   once at the end. Nothing was queued to probe until the walk finished, so on
   the 2026-08-07 dogfood run not one of 23,283 TV items carried a `probed_at`
   before the pass completed, and **time to first playable item was the whole
   index pass — 78 minutes**. No phase reordering or hiding rule could change
   that, because nothing was queued. The barrier was never an ordering or dedup
   mechanism — only a counter and a condvar — and `drain_pending_probes`
   already enqueued probes individually, so only the counter arithmetic had to
   move.

   Two limits on what this buys, both measured rather than assumed. The readdir
   walk still runs to completion before the upsert loop begins, so the first
   probe lands at readdir plus one index batch, not at the start of the pass;
   `walk_ms` and `upsert_ms` are logged separately so that split is
   attributable rather than inferred. And probe throughput is unchanged — four
   concurrent ffprobe children either way. What changes is that probing
   overlaps the walk instead of following it.

5. Walk order is not sorted. Sorting to surface recently-added titles first
   would require the walk to finish before ordering could be applied, which
   defeats the purpose of enqueueing as we go. Ordering matters less than it
   appears, for the reason in §3.
6. One active job per library. A second POST while a job is active returns the
   existing `jobId` (202), not a second concurrent walk.
7. v0 is not frozen. Replacing the synchronous 200 `ScanResult` body is an
   accepted break within Phase 1 (ADR-0003).

### 3. Unprobed items are shown, and promoted on demand (added 2026-08-08)

An item without metadata still works — filename title, no poster, plays fine.
An item without a probe **cannot play at all**, because `decide_playback` has
no codecs. "No metadata" is ugly; "no probe" is broken. So probe is gated and
metadata is not, and the two are deliberately not treated alike.

That leaves what to do with an item that is indexed but not yet probed.

1. **Show it. Do not hide it.** Hiding unprobed items and promoting them on
   demand are mutually exclusive designs — a hidden item cannot be demanded —
   and hiding is the worse of the two. It makes walk order the only thing
   determining what appears, so a user watches their library fill
   alphabetically with no way to influence it, and **search returns nothing for
   a title they know they own**, which reads as missing rather than pending.
   Showing everything costs only that the grid contains items needing a moment
   before they play. With §2.4 the gap between indexed and probed is queue
   depth rather than the length of the walk, so that moment is short.
2. **Promote on demand.** Anything a user reaches jumps to the front of the
   probe queue. Demand then routes around walk order, which is why §2.5 can
   leave the walk unsorted.
3. **Shape, when built:** `GET /items/{id}/playbackInfo` is the demand trigger
   and promotes the probe; `POST /items/{id}/sessions` waits, bounded, for it
   rather than refusing outright. This mirrors the keyframe map exactly
   (ADR-0023 §9.1 trigger, §9.3 bounded wait) and reuses that mechanism rather
   than inventing a second one.
4. **The UI needs a not-ready signal**, and its wording is a copy-deck question
   as much as a technical one. The dev UI's `probing…` badge is a placeholder,
   not the answer.

Decided 2026-08-08. Item 1 is already the behaviour — item listing has never
filtered on probe status. Items 2 and 3 are a later slice; nothing in this ADR
is contingent on them.

## Consequences

The 10k harness polls `GET /scan-jobs/{id}` and gates on `indexDurationMs`, not
wall-clock to full probe completion.

Clients must poll. The web UI shows progressive counts and the copy-deck
scanning line.

Probe concurrency can stress error paths. Corpus `broken_moov` and rescans must
be rechecked under the pool before Gate 1 is claimed on Pi hardware.

Schema migration `002` adds `scan_jobs` and `media_items.probe_status`
(append-only; irreversible without a new migration).

Probing overlapping the walk means ffprobe children now read from the share
while the walk stats it, where before the two phases were strictly sequential.
On a bandwidth-bound mount that could slow the walk. It is not gated
speculatively: the walk/probe split in the index-pass log and the FUSE queue
sampler are there to answer it with a measurement, on healthy hardware, rather
than a guess.
