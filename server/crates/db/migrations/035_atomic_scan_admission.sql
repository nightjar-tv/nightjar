-- ADR-0059: at most one active scan job per library.
--
-- The admission path read the active row and inserted a new one in two
-- separate statements, so two callers on different connections could both see
-- no active job and both insert. The partial unique index makes the durable
-- invariant a constraint rather than a convention: `scan_jobs` may hold one
-- row in `queued`, `indexing` or `probing` for a library.
--
-- The index covers every `kind`, including `repoint`, because the active-row
-- lookup already treats a queued repoint as active and callers coalesce onto
-- it. The state vocabulary is unchanged.
--
-- `migrate` refuses before this transaction when an install already holds two
-- active rows for one library, naming the library and every colliding job id,
-- so the index is only built over a valid population and version 035 is never
-- recorded for a database that cannot satisfy it. This migration changes no
-- row and deletes nothing.
PRAGMA foreign_keys = ON;

CREATE UNIQUE INDEX idx_scan_jobs_active_library
    ON scan_jobs(library_id)
    WHERE state IN ('queued', 'indexing', 'probing');
