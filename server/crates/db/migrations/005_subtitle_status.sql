-- ADR-0013: scan-time subtitle extraction status and validity stamps
PRAGMA foreign_keys = ON;

ALTER TABLE media_items ADD COLUMN subtitle_status TEXT NOT NULL DEFAULT 'pending'
    CHECK (subtitle_status IN ('pending', 'ready', 'none', 'error'));

ALTER TABLE media_items ADD COLUMN subtitle_source_mtime_ms INTEGER;
ALTER TABLE media_items ADD COLUMN subtitle_source_size_bytes INTEGER;

CREATE INDEX idx_media_items_subtitle_status ON media_items(subtitle_status);
