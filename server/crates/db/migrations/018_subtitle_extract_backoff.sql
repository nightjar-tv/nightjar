-- ADR-0041 Decision 8.3: attempt counting and backoff on subtitle
-- `unavailable`. ADR-0014 re-queues every `unavailable` row on each
-- reachability transition; without a cap a flapping mount plus one
-- unfinishable title is permanent load. The schedule (1d, 7d, 30d, 90d cap)
-- is ADR-0026 §3's, shared via `backoff_days` in status.rs so the metadata
-- negative cache and this gate cannot drift (Rule 4.11). `subtitle_next_retry_at`
-- gates the reachability re-queue; `subtitle_attempt_count` drives the
-- schedule. Both are written by `set_subtitle_status`, reset on any
-- non-`unavailable` status write.
PRAGMA foreign_keys = ON;

ALTER TABLE media_items ADD COLUMN subtitle_attempt_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE media_items ADD COLUMN subtitle_next_retry_at TEXT;
