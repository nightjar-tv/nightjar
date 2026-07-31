# ADR-0004: Async scan jobs and Gate 1 index-pass criterion

- Status: accepted
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
3. Each media item carries `probeStatus`: `indexed` | `probed` | `error`.
   Indexed items are listable before codecs land. Playback decisions treat
   `indexed` as probe-pending (not direct play).
4. Implementation: fast index pass, then a worker pool of about
   `available_parallelism` ffprobe children draining a queue. Batch DB writes
   on the index path.
5. One active job per library. A second POST while a job is active returns the
   existing `jobId` (202), not a second concurrent walk.
6. v0 is not frozen. Replacing the synchronous 200 `ScanResult` body is an
   accepted break within Phase 1 (ADR-0003).

## Consequences

The 10k harness polls `GET /scan-jobs/{id}` and gates on `indexDurationMs`, not
wall-clock to full probe completion.

Clients must poll. The web UI shows progressive counts and the copy-deck
scanning line.

Probe concurrency can stress error paths. Corpus `broken_moov` and rescans must
be rechecked under the pool before Gate 1 is claimed on Pi hardware.

Schema migration `002` adds `scan_jobs` and `media_items.probe_status`
(append-only; irreversible without a new migration).
