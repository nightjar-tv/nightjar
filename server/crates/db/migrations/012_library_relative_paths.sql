-- ADR-0030: library-relative media paths; counters; scan job kind for repoint.

ALTER TABLE libraries ADD COLUMN paths_unresolved INTEGER NOT NULL DEFAULT 0;
ALTER TABLE libraries ADD COLUMN skipped_outside_root INTEGER NOT NULL DEFAULT 0;

ALTER TABLE scan_jobs ADD COLUMN kind TEXT NOT NULL DEFAULT 'scan';
ALTER TABLE scan_jobs ADD COLUMN candidate_path TEXT;
ALTER TABLE scan_jobs ADD COLUMN skipped_outside_root INTEGER NOT NULL DEFAULT 0;
