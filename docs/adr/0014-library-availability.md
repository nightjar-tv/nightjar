# ADR-0014: Library availability and failure classification

- Status: accepted
- Date: 2026-07-27

## Context

A NAS unmount during a background extraction pass burned through roughly a
thousand items at about 30 ms each, marking every one `error`. ffprobe failures
arrived with an empty error body (`-v quiet`), and sidecar reads failed with
`No such file or directory`. Those look identical to corrupt files and deleted
sidecars. Opaque `error` is never re-queued while mtime is unchanged, so
subtitles and probes stay failed forever after the mount returns.

Dogfood (`~/nightjar-data`, 2026-07-27): 3 `probe_status=error` (empty stderr)
and 1054 `subtitle_status=error`. None would auto-retry without this change.

Worse: a hung or half-dead SMB mount can still present as a directory and
return an empty listing. Today's index pass would run `delete_missing` against
that walk, wipe every item, and cascade into derived subtitle data. That is
data loss, not a support nuisance.

## Decision

1. **Library reachability gates all work.** Before dispatching any probe,
   extract, or scan walk for a library, the root must be reachable.
   Unreachable → mark the library unavailable, pause that library's queues,
   retry the root on a slow interval. One log line on transition, never one
   per item.

2. **Empty walk must never delete.** The index pass calls `delete_missing`
   only when reachability is positively confirmed for that pass (root
   reachable at start and the walk did not complete under doubt). If
   reachability is in doubt at any point, skip `delete_missing`, leave rows
   alone, mark the library unavailable, and end without item deletions.

3. **Same failure vocabulary on probe and extract.** Status value
   `unavailable` on both `probe_status` and `subtitle_status`. `error` means
   permanent unreadable/corrupt/parse. `unavailable` means mount/IO absence
   and is always retriable. Mount-gone ENOENT on a sidecar is not "user
   deleted the sidecar."

4. **On reachable again:** one log line; re-queue `probe_status=unavailable`
   → `indexed` and `subtitle_status=unavailable` → `pending`. Never re-queue
   `error`.

5. **Schema shape (Rule 4.9).** Do not rebuild `media_items` solely to widen
   CHECK constraints — a half-completed copy of a 24k-row dogfood table is
   unacceptable. Migration `006` adds `libraries.reachable`, drops the
   `probe_status` / `subtitle_status` CHECKs via SQLite `DROP COLUMN` /
   `ADD COLUMN` (atomic per statement, inside the migration transaction),
   and validates allowed values in Rust at write time. Uglier than CHECK;
   safer than a full row-payload rebuild. Future rebuilds, if unavoidable,
   must assert `COUNT(*)` before == after inside the same transaction.

6. **Upgrade (ADR-0012 pattern, second application).** Prior `error` rows do
   not record why. Migration one-time resets `probe_status='error'` →
   `indexed` (clear `scan_error`) and `subtitle_status='error'` → `pending`
   (clear source stamps). Startup drains re-derive. Genuine corrupt files
   fail once more as permanent `error`.

7. **ffprobe diagnostics.** Spawn failures use a distinct `spawn ffprobe…`
   message (no stderr expected). Process non-zero exits use `-v error`,
   include the exit status, and a truncated stderr tail so
   `ffprobe failed for path: ` with nothing after the colon does not recur.

8. **Reachability tick.** Interval about 15–30 s, non-overlapping (skip if a
   tick is in flight), one `is_dir` per library root. The check runs with a
   hard timeout; timeout means unreachable/doubt so a hung mount cannot
   wedge the ticker.

9. **API.** `Library.reachable` (boolean). v0 additive. Copy deck string:
   "The folder {path} isn't reachable. Check that the drive is mounted, then
   rescan."

10. **Doctor (plan only).** Phase 4 `nightjar doctor` reports library
    reachability first; an unreachable library dominates the output. Not
    implemented in this slice.

## Consequences

**Gained.** Mount flaps stop permanently failing the library. Empty/doubtful
walks cannot wipe items. Probe and extract agree on failure meaning
(Rule 4.11). Beta reports of ffprobe failures carry stderr or at least an
exit code.

**Lost.** SQLite no longer CHECKs probe/subtitle status strings; Rust writers
must stay honest. One-time upgrade re-probes/re-extracts every prior `error`
row, including any genuinely corrupt files (cheap; rare).

**Corpus / tests.** Empty-walk-under-doubt must not delete. Unavailable root
dispatches no per-item work. Recovery re-queues `unavailable` only.
`broken_moov` stays permanent `error`.
