# ADR-0015: Library discovery scheduling

- Status: accepted
- Date: 2026-07-27
- Amended: 2026-08-03 (global index epoch; poll default 300 s;
  path-hinted notify ingest); 2026-08-04 (notify/dirty coalesce — hint
  without full scan; poll-while-active is not dirty)

## Context

Nightjar learns about adds, changes, and deletes under a library root through
two mechanisms that were not one path: filesystem notify and a periodic poll.
Library creation did not start a scan at all. Dogfood showed a ~40 s wait
after `POST /libraries` for the next 60 s poll tick, and perpetual 60 s polls
even after notify had armed.

Notify over SMB can arm successfully and never deliver creates
(`nightjar-meta/docs/library-change-detection.md`, moved from this repo's
`docs/` in the 2026-08-07 product/meta split — pre-decision research, not a
binding contributor doc). Gating poll on "notify armed" would disable the
only mechanism that works on those shares.

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

1. **Full-walk entry points.** Library create, manual `POST .../scan`,
   periodic poll, and internal manual follow-up call `request_scan` (with a
   `ScanTrigger`). That is the only code that **starts a full-library scan
   job**. Notify media creates use `hint_ingest` and do **not** call
   `request_scan` (decision 5).

2. **Scan on create.** `POST /libraries` inserts the row, calls
   `request_scan` (`Create`), and returns **201** with the library and the
   enqueued `jobId`. The walk is async (ADR-0004); the response does not wait
   for index or probe. Cold TV on this link is ~150 s with parallel walk,
   still too long to block the HTTP request.

3. **At most one running scan per library; follow-up only for manual
   coalesce.** Never two concurrent walks for one library; never a queue of
   N follow-ups.

   | Mid-walk trigger | Dirty bit | Skip `delete_missing`? | Auto follow-up? |
   |---|---|---|---|
   | **Hint / `dirty_add`** (path-hint upsert while active) | `dirty_add` | **Yes** (row may be outside keep-set) | **No** — next poll heals deletes |
   | **Poll** while active | **No** (no-op) | **No** — this walk **is** the poll | **No** |
   | **Manual POST** while active | `scan_dirty` | Yes until follow-up | **Yes** — exactly one |

   Treating poll-while-active as dirty that suppresses delete would starve
   `delete_missing` on every library whose walk exceeds the poll interval
   (N150 TV warm ~minutes, poll 300 s). Poll-while-active is therefore a
   dirty no-op.

4. **One process-wide index/walk epoch.** At most one library may run a tree
   walk or index upsert pass at a time (`LibraryPool::enter_index_epoch`).
   Other libraries’ scan jobs wait for the epoch, then run. Probe, extract,
   and map continue under the existing pool rules after the epoch releases
   (extract/map still pause while any epoch is held unless play-priority —
   ADR-0013). Repoint holds one epoch across dry-run walk and the commit
   index so another library cannot interleave a cold walk on the same share.

   Per-library coalescing (decision 3) remains. The epoch adds the missing
   cross-library bound.

5. **Notify accelerates creates; it does not mandate a full walk.** An FS
   event never disables polling. There is no gate that stops poll because
   notify armed, and there is **no notify-works detection** by design.

   **Path-hinted ingest (notify media paths).** When the debounced event
   carries a concrete path that is a media file under a library root, the
   watcher runs `hint_ingest` only: one stat + fold-aware upsert + probe
   enqueue (same match rules as the index loop). That path **never** calls
   `delete_missing` and **never** calls `request_scan`. Deletes wait for
   the next poll (or manual scan), ≤ one poll interval. Non-media,
   directories, sidecars, missing paths, and zero-size files are ignored
   (copy-in-progress / debounce miss — no stability sampler in v1). Hint
   does **not** take the index epoch; concurrent with an in-flight walk is
   intentional.

   If a full scan is active when the hint **upserts**, set `dirty_add` so
   that job skips `delete_missing` (keep-set race). Do **not** schedule a
   follow-up full walk for the hint alone; the next poll may delete.

   What notify is for: on a library where FS events fire, a new media file
   can appear in the index within seconds without a full tree walk. On
   shares where notify is mute, poll remains the guarantee. Notify is an
   accelerator for **adds**, never a gate and never the delete path.

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
  the table rots (see `nightjar-meta/docs/library-change-detection.md`).

Path-hinted **single-file** ingest on notify is decision 5 above. Jellyfin-style
**subtree** refresh (whole season/dir, still without `delete_missing`) remains
a later design; full poll / full walk owns deletes under ADR-0014.

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
