# ADR-0059: Atomic active-scan admission

**Status:** Accepted (2026-09-19)

## Context

A library may run at most one scan job at a time. The admission path did not
enforce that: `request_scan` read the active job with `active_scan_job` and
then, in a separate step, inserted a new row with `create_scan_job`. Two
callers on different connections could both see no active job and both insert.
The same two-step shape let a poll read the repoint-delete holdoff and then
insert after the holdoff was armed, leaving a row that should never have
existed.

`scan_jobs` carried no database constraint on active rows, so nothing caught
the duplicate. The window is small but real: the poll scheduler, the manual
scan route, and library create all reach the same admission path.

## Decision

### Durable invariant and schema

`scan_jobs` may have at most one active row for a library. An active row is
`state` in (`queued`, `indexing`, `probing`). Migration `035` adds a partial
unique index that enforces the invariant for existing and new rows:

```sql
CREATE UNIQUE INDEX idx_scan_jobs_active_library
    ON scan_jobs(library_id)
    WHERE state IN ('queued', 'indexing', 'probing');
```

The index covers every `kind`, including `repoint`, because
`active_scan_job` already treats a queued repoint as active and callers
coalesce onto it. The vocabulary of states does not change.

### Atomic admission

`Db::admit_scan_job(library_id, holdoff_check)` performs the active-row lookup
and any queued insert in one `BEGIN IMMEDIATE` transaction. `holdoff_check` is
`Option<impl FnOnce() -> bool>`: `Some` for a poll, `None` for every other
trigger.

The transaction order is fixed:

1. Read the active row. If one exists, return `Existing(job_id)`.
2. Otherwise, when `holdoff_check` is `Some`, evaluate it now and return
   `Skipped` when it reports the holdoff active.
3. Otherwise insert the `queued` scan row, commit, and return `Created(job_id)`.

The result is ownership- and outcome-tagged:

- `Created(job_id)` — this caller inserted the row and owns the worker launch;
- `Existing(job_id)` — an active row already exists; the caller coalesces;
- `Skipped` — a live holdoff was observed; no row was inserted and nothing
  starts.

The lookup precedes the check and the check precedes the insert. A holdoff
observed after the lookup therefore never leaves an inserted row. A failed
transaction rolls back the insert and leaves no active orphan. Only `Created`
may spawn the worker.

`holdoff_check` is not a precomputed boolean. The caller closes over the
pool's synchronized holdoff state, so the decision reads the state at the
moment the transaction runs, not a snapshot taken before it.

### Caller ordering

`scanner::request_scan` keeps its current order inside the atomic operation:
reachability check, then active-row lookup, then the live poll repoint-holdoff
decision only when no active job was found. Therefore:

- a poll that meets an already-active job returns that job id, and a new poll
  can return `0` for holdoff with no row and no worker;
- `Poll` and `FollowUp` coalesce without setting the dirty bit;
- `Manual` and `Create` coalesce and set the existing dirty bit;
- a newly `Created` job does not set the dirty bit.

No queue redesign, polling-policy change, or probe behavior change is part of
this decision.

### Unique-conflict fallback

A unique-conflict fallback is not a second admission path. If one is ever
needed for multi-connection races, it must be the error-handling branch of the
same single transaction-shaped API and must return the committed active id.
The implementation relies on `BEGIN IMMEDIATE` plus the index and keeps the
result mapping in `Db`; no fallback exists.

### Rollback and upgrade behavior

Migration `035` is transactional. Before it opens its transaction, `migrate`
checks for existing active duplicates and refuses with a message that names
the library and every colliding job id, in the same shape migration `025`
refuses colliding usernames. An invalid populated database therefore aborts
without recording version `035`, and boot stops so an operator decides.

Fresh and populated databases retain every `scan_jobs` row. The index is
created only after the duplicate check passes. The migration performs no
backfill, no state change, and no deletion.

## Scope

This decision fixes admission only. Repoint admission, queue redesign,
polling freshness, probe generation, demand-probe UX, scan-state vocabulary,
and API response shapes are separate slices.

## Verification

Tests must prove:

- migration `035` creates the index on a fresh database, preserves every row
  on a populated one, and refuses a duplicate-active fixture without recording
  version `035`;
- two synchronized admissions through independent connections to one database,
  while the admitted row is held active, produce one `Created`, one
  `Existing`, one active row, and equal returned ids;
- the shared-`Db` path returns `Created` then `Existing` for the same library;
- the active-row lookup runs before the holdoff check (an existing row is
  returned without evaluating the check) and the check runs before the insert
  (a live holdoff returns `Skipped` with no row);
- an insert failure leaves no active orphan;
- the trigger semantics above hold for poll, follow-up, manual, and create,
  and the existing follow-up tests stay green.
