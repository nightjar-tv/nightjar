-- ADR-0041: persist the subtitle stream inventory at probe time (Decision 1)
-- and add the `eligible` classification (Decision 2). ffprobe already returns
-- the subtitle stream list; the parser dropped it. One row per stream, keyed
-- on media_item_id; a re-probe replaces the rows for that item (ADR-0025
-- identity discipline, no separate content-id column).
-- Also reset the opaque `error` rows to `pending` so a re-probe drives them
-- through Decision 2's classifier (Decision 9) — the same migration-then-
-- reclassify pattern migration 006 used for `subtitle_status = 'error'`.
-- `pending` keeps its existing meaning (not yet probed); `eligible` is
-- Decision 2's new terminal classification for "needs extraction, not
-- started" and is validated in Rust (status.rs), not by a SQL CHECK —
-- migration 006 dropped those (Rust validates values).
PRAGMA foreign_keys = ON;

CREATE TABLE media_item_subtitle_tracks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    media_item_id INTEGER NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
    stream_index INTEGER NOT NULL,
    codec TEXT NOT NULL,
    language TEXT,
    title TEXT,
    forced INTEGER NOT NULL DEFAULT 0,
    sdh INTEGER NOT NULL DEFAULT 0,
    kind TEXT NOT NULL CHECK (kind IN ('text', 'ass', 'image', 'unknown'))
);

CREATE INDEX idx_media_item_subtitle_tracks_item ON media_item_subtitle_tracks(media_item_id);

-- Opaque prior errors cannot be classified; re-derive (ADR-0041 Decision 9).
UPDATE media_items SET subtitle_status = 'pending',
    subtitle_source_mtime_ms = NULL,
    subtitle_source_size_bytes = NULL
 WHERE subtitle_status = 'error';
