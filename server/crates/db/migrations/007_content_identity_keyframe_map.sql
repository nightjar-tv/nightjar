-- ADR-0023: media-file identity + keyframe map shapes (Rule 4.9).
-- Columns start NULL / 'pending' until the writers (set_map_status,
-- replace_keyframe_map) fill them.
--
-- MP4 virtual moov': rebuild per session (not cached). No moov artifact table.
-- Cached moov' would need content_id like every other derived artifact.
PRAGMA foreign_keys = ON;

-- Live identity: size_bytes + sha256(first 64 KiB) + sha256(last 64 KiB).
-- Format locked in nightjar_db::content_id. NULL until scan computes it.
ALTER TABLE media_items ADD COLUMN content_id TEXT;

-- Identity each derived artifact was built under (NULL = unknown / pre-migration).
ALTER TABLE media_items ADD COLUMN probed_content_id TEXT;
ALTER TABLE media_items ADD COLUMN subtitle_content_id TEXT;

-- Usable extent from the same pass as map build (DEF-8519 damage signal).
ALTER TABLE media_items ADD COLUMN usable_extent_ms INTEGER;
ALTER TABLE media_items ADD COLUMN usable_extent_content_id TEXT;

-- Map job status on the item; entries live in keyframe_map_entries.
-- Values validated in Rust: pending | ready | error | unavailable.
ALTER TABLE media_items ADD COLUMN map_status TEXT NOT NULL DEFAULT 'pending';
ALTER TABLE media_items ADD COLUMN map_content_id TEXT;

CREATE TABLE keyframe_map_entries (
    media_item_id INTEGER NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
    content_id TEXT NOT NULL,
    container_kind TEXT NOT NULL,
    pts_ms INTEGER NOT NULL,
    byte_offset INTEGER NOT NULL,
    PRIMARY KEY (media_item_id, pts_ms)
);

CREATE INDEX idx_keyframe_map_item ON keyframe_map_entries(media_item_id);
CREATE INDEX idx_media_items_map_status ON media_items(map_status);
