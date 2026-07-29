-- Filesystem subtitle sidecars discovered at index time (ADR-0010).
PRAGMA foreign_keys = ON;

CREATE TABLE media_item_sidecars (
    media_item_id INTEGER NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
    track_id TEXT NOT NULL,
    path TEXT NOT NULL,
    mtime_ms INTEGER NOT NULL,
    size_bytes INTEGER NOT NULL,
    format TEXT NOT NULL,
    language TEXT,
    forced INTEGER NOT NULL DEFAULT 0,
    sdh INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (media_item_id, track_id)
);

CREATE INDEX idx_media_item_sidecars_item ON media_item_sidecars(media_item_id);
