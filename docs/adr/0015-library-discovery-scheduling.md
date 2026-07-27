# ADR-0015: Library discovery scheduling

- Status: accepted
- Date: 2026-07-27

## Context

Nightjar learns about adds, changes, and deletes under a library root through
two mechanisms that were not one path: filesystem notify and a periodic poll.
Library creation did not start a scan at all. Dogfood showed a ~40 s wait
after `POST /libraries` for the next 60 s poll tick, and perpetual 60 s polls
even after notify had armed.

Notify over SMB can arm successfully and never deliver creates
([docs/library-change-detection.md](../library-change-detection.md)). Gating
poll on "notify armed" would disable the only mechanism that works on those
shares.

After parallelising directory re-stats (ADR-0013 §8.7), TV Shows warm walk on
the household SMB-over-Wi-Fi link is ~1.7 s at concurrency 8. A 60 s poll is
about a 3% duty cycle. Walk cost no longer justifies adaptive intervals,
confidence scoring, or per-library backoff.

## Decision

1. **One discovery entry point per library.** Every trigger (library
   creation, filesystem notify, manual `POST .../scan`, periodic poll) calls
   the same function (`request_scan`). That function is the only code that
   starts a scan job.

2. **Scan on create.** `POST /libraries` inserts the row, calls
   `request_scan`, and returns **201** with the library and the enqueued
   `jobId`. The walk is async (ADR-0004); the response does not wait for
   index or probe. Cold TV on this link is ~150 s with parallel walk, still
   too long to block the HTTP request.

3. **At most one running scan and one queued follow-up per library.** If a
   trigger arrives while a scan is active, mark the library dirty and return
   the active job id. When that job finishes, start exactly one follow-up if
   dirty. Further triggers during the active job only set the same dirty
   bit. Never two concurrent walks for one library; never a queue of N
   follow-ups.

4. **Notify is a trigger, not a mode.** An FS event calls `request_scan`
   sooner. It never disables polling. There is no gate that stops poll
   because notify armed. Recursive watches may still be deferred until the
   first index finishes so they do not starve SMB metadata IOPS during a
   cold walk (ADR-0013); that is an arming optimisation, not a poll
   replacement.

5. **Fixed poll interval.** Default **60 seconds**. Configurable via
   `NIGHTJAR_POLL_INTERVAL_SECS` (same class of knob as
   `NIGHTJAR_WALK_CONCURRENCY`). Not derived from last walk duration.
   Delete `max(60, 2 × duration_s)`: with parallel warm walks the 2× term
   cannot engage, and a formula that never changes its answer implies
   adaptivity that does not exist. At ~1.7 s TV warm / 60 s interval the
   duty cycle is ~3% on the measured link.

6. **Discovery vs indexing.** No new pipeline. The existing scan job already
   separates an index pass (walk → change list → upsert) from probe/extract
   (ADR-0004). Discovery scheduling only decides when `request_scan` runs;
   indexing remains that job's first phase.

### Explicitly rejected (Rule 4.7)

- **Adaptive poll intervals** based on last walk duration.
- **Confidence scoring** of notify reliability per library.
- **Per-library backoff** on quiet libraries.

At a ~3% duty cycle these add per-library mutable state that depends on
hours of history, is hard to test, and is hard to explain when a user says
a file did not appear. Do not reintroduce them from intuition.

### Latency vs bandwidth (recurring principle)

When per-item cost matches network RTT, the work is **latency-bound** and
concurrency hides wait time (directory stats → parallel walk). When
per-item cost is bytes through one pipe, the work is **bandwidth-bound**
and concurrency hurts (subtitle extract → stay serial). That distinction
separates this stack from the Jellyfin core-count-scaling failure already
noted in ADR-0013.

## Consequences

**Gained.** Creation starts discovery immediately. One code path for every
trigger. Poll remains the guarantee on mute notify. Interval is a readable
constant. Coalescing is one dirty bit, already used for watch-during-scan.

**Lost.** No automatic slowdown on quiet libraries; operators who need a
longer interval set `NIGHTJAR_POLL_INTERVAL_SECS`. Clients that assumed
`POST /libraries` returned only a `Library` must accept `jobId` on 201
(additive field on the create response schema).

**API.** `POST /libraries` 201 body includes `jobId` (OpenAPI
`CreateLibraryResponse`). Spec and implementation land together.

**Tests.** Triggers during a running scan produce one follow-up. Creation
enqueues without waiting for the walk. Notify + poll together do not start
two concurrent walks. Unreachable libraries still dispatch no per-item work
(ADR-0014).
