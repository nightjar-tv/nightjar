# ADR-0015: Library discovery scheduling

- Status: accepted
- Date: 2026-07-27
- Amended: 2026-08-03 (global index epoch; poll default 300 s)

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
about a 3% duty cycle **for one library alone**. Dogfood after ADR-0030 remount
(2026-08-03) showed the real failure mode: poll fires `request_scan` for
**every** library at once; each walk uses up to eight directory workers; several
libraries on one share pile up until even a tiny local library’s index takes
~11 minutes while `unchanged` matches a quiet warm pass. Per-library
coalescing (decision 3) does not stop cross-library walk thrash.

Jellyfin’s shape (optional `FileSystemWatcher` + ~12 h scheduled full refresh)
gets new files quickly only when realtime monitoring works; mute shares wait
for the slow task or a manual scan. Nightjar keeps a stronger promise: poll
always bounds mute-share detection latency. Notify stays an accelerator for
local disks (the common non-SMB install), never a gate.

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

4. **One process-wide index/walk epoch.** At most one library may run a tree
   walk or index upsert pass at a time (`LibraryPool::enter_index_epoch`).
   Other libraries’ scan jobs wait for the epoch, then run. Probe, extract,
   and map continue under the existing pool rules after the epoch releases
   (extract/map still pause while any epoch is held unless play-priority —
   ADR-0013). Repoint holds one epoch across dry-run walk and the commit
   index so another library cannot interleave a cold walk on the same share.

   Per-library coalescing (decision 3) remains. The epoch adds the missing
   cross-library bound.

5. **Notify is a trigger, not a mode.** An FS event calls `request_scan`
   sooner. It never disables polling. There is no gate that stops poll
   because notify armed, and there is **no notify-works detection**
   anywhere in the system by design.

   What notify is for now: on a **local** library it shortens detection
   from up to one poll interval down to a couple of seconds. On **network
   shares** where notify arms but never fires (SMB and similar), nothing
   breaks because poll is the guarantee. Notify is an accelerator, never
   a gate.

   Recursive watches are still **deferred until the first index pass
   finishes**. Originally that avoided starving a cold SMB walk of
   metadata IOPS: with serial walks, arming recursive watches during the
   cold TV pass pushed walks past 15–20 minutes (ADR-0013), and serial
   cold TV after remount measured 687–729 s. Parallel directory re-stats
   brought cold TV to ~150 s on the same link (ADR-0013 §8.7), so that
   starvation justification is **stale**. The deferral is left in place
   because it is harmless (poll covers the window before notify arms) and
   can be revisited; it must not be mistaken for a load-bearing
   correctness rule or removed/kept out of superstition.

6. **Fixed poll interval.** Default **300 seconds**. Configurable via
   `NIGHTJAR_POLL_INTERVAL_SECS` (same class of knob as
   `NIGHTJAR_WALK_CONCURRENCY`), clamp 5..=3600. Not derived from last walk
   duration. Mute-share detection latency is bounded by this interval plus
   one warm (or cold) walk under the global epoch. Operators who want faster
   pickup on quiet local disks may lower the env; the epoch still prevents
   multi-library pile-up.

7. **Discovery vs indexing.** No new pipeline. The existing scan job already
   separates an index pass (walk → change list → upsert) from probe/extract
   (ADR-0004). Discovery scheduling only decides when `request_scan` runs;
   indexing remains that job's first phase.

### Explicitly rejected (Rule 4.7)

- **Adaptive poll intervals** based on last walk duration.
- **Confidence scoring** of notify reliability per library.
- **Per-library backoff** on quiet libraries.
- **Notify-works verification** (tempfile probe or “saw an event ⇒ stop
  polling”). Writes into library trees, fails on read-only mounts, and
  recreates silent miss when SMB fires sometimes then goes mute.
- **Deleting notify** in favour of poll-only. Punishes local-disk installs
  where notify is the right accelerator.
- **Mount-type classification** (fstype / “network vs local”) as a poll
  gate. iSCSI, mergerfs/Unraid user shares, and Docker Desktop look wrong;
  the table rots (see library-change-detection brief).

Path-hinted refresh that skips `delete_missing` (Jellyfin-style subtree
refresh) remains a later design; full poll owns deletes under ADR-0014 until
that ADR exists.

### Latency vs bandwidth (recurring principle)

When per-item cost matches network RTT, the work is **latency-bound** and
concurrency hides wait time (directory stats → parallel walk). When
per-item cost is bytes through one pipe, the work is **bandwidth-bound**
and concurrency hurts (subtitle extract → stay serial). That distinction
separates this stack from the Jellyfin core-count-scaling failure already
noted in ADR-0013. Cross-library walk concurrency is the same class of
mistake as parallel extract: more metadata IOPS on one share make every
walker slower.

## Consequences

**Gained.** Creation starts discovery immediately. One code path for every
trigger. Poll remains the guarantee on mute notify. Interval is a readable
constant. Coalescing is one dirty bit, already used for watch-during-scan.
Multi-library installs no longer overrun one mount with N parallel walks.
Notify still accelerates local creates without becoming a reliability gate.

**Lost.** No automatic slowdown on quiet libraries; operators who need a
shorter or longer interval set `NIGHTJAR_POLL_INTERVAL_SECS`. A library whose
scan starts while another is indexing waits for the epoch (job exists;
walk deferred). Clients that assumed `POST /libraries` returned only a
`Library` must accept `jobId` on 201 (additive field on the create response
schema).

**API.** `POST /libraries` 201 body includes `jobId` (OpenAPI
`CreateLibraryResponse`). Spec and implementation land together.

**Tests.** Triggers during a running scan produce one follow-up. Creation
enqueues without waiting for the walk. Notify + poll together do not start
two concurrent walks for one library. Index epoch is exclusive across
holders. Unreachable libraries still dispatch no per-item work (ADR-0014).
