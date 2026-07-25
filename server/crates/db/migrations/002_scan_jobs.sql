-- ADR-0004: async scan jobs + item probe status
PRAGMA foreign_keys = ON;

ALTER TABLE media_items ADD COLUMN probe_status TEXT NOT NULL DEFAULT 'probed'
    CHECK (probe_status IN ('indexed', 'probed', 'error'));

CREATE TABLE scan_jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    library_id INTEGER NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    state TEXT NOT NULL CHECK (
        state IN ('queued', 'indexing', 'probing', 'completed', 'failed')
    ),
    added INTEGER NOT NULL DEFAULT 0,
    updated INTEGER NOT NULL DEFAULT 0,
    removed INTEGER NOT NULL DEFAULT 0,
    unchanged INTEGER NOT NULL DEFAULT 0,
    probed INTEGER NOT NULL DEFAULT 0,
    errors INTEGER NOT NULL DEFAULT 0,
    index_duration_ms INTEGER,
    probe_duration_ms INTEGER,
    error_message TEXT,
    started_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    finished_at TEXT
);

CREATE INDEX idx_scan_jobs_library ON scan_jobs(library_id);
CREATE INDEX idx_media_items_probe_status ON media_items(library_id, probe_status);
