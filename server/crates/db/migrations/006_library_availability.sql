-- ADR-0014: library reachability + unavailable failure class.
-- Do not rebuild media_items row payload. Drop status CHECKs by replacing
-- columns (SQLite DROP COLUMN is atomic per statement). Rust validates values.
PRAGMA foreign_keys = ON;

ALTER TABLE libraries ADD COLUMN reachable INTEGER NOT NULL DEFAULT 1;

DROP INDEX IF EXISTS idx_media_items_probe_status;
DROP INDEX IF EXISTS idx_media_items_subtitle_status;

ALTER TABLE media_items ADD COLUMN probe_status_new TEXT NOT NULL DEFAULT 'probed';
UPDATE media_items SET probe_status_new = probe_status;
ALTER TABLE media_items DROP COLUMN probe_status;
ALTER TABLE media_items RENAME COLUMN probe_status_new TO probe_status;

ALTER TABLE media_items ADD COLUMN subtitle_status_new TEXT NOT NULL DEFAULT 'pending';
UPDATE media_items SET subtitle_status_new = subtitle_status;
ALTER TABLE media_items DROP COLUMN subtitle_status;
ALTER TABLE media_items RENAME COLUMN subtitle_status_new TO subtitle_status;

CREATE INDEX IF NOT EXISTS idx_media_items_probe_status ON media_items(library_id, probe_status);
CREATE INDEX IF NOT EXISTS idx_media_items_subtitle_status ON media_items(subtitle_status);

-- Opaque prior errors cannot be classified; re-derive (ADR-0014 §6).
UPDATE media_items SET probe_status = 'indexed', scan_error = NULL
 WHERE probe_status = 'error';
UPDATE media_items SET subtitle_status = 'pending',
    subtitle_source_mtime_ms = NULL,
    subtitle_source_size_bytes = NULL
 WHERE subtitle_status = 'error';
