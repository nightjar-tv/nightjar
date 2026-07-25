-- Phase 1 schema (ADR-0003)
PRAGMA foreign_keys = ON;

CREATE TABLE libraries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    path TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL CHECK (kind IN ('movies', 'shows')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE media_items (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    library_id INTEGER NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    mtime_ms INTEGER NOT NULL,
    size_bytes INTEGER NOT NULL,
    title TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('movie', 'episode', 'unknown')),
    year INTEGER,
    season INTEGER,
    episode INTEGER,
    duration_ms INTEGER,
    container TEXT,
    video_codec TEXT,
    audio_codec TEXT,
    width INTEGER,
    height INTEGER,
    scan_error TEXT,
    probed_at TEXT,
    UNIQUE (library_id, path)
);

CREATE INDEX idx_media_items_library ON media_items(library_id);
CREATE INDEX idx_media_items_title ON media_items(title);
