-- ADR-0026 §8.1: two-tier status. 'matched' = search (or NFO with TMDB id)
-- accepted >= 0.80; sparse canonical + art path refs written; enrich pending.
-- SQLite cannot alter a CHECK constraint, so rebuild the column via
-- add-copy-drop-rename. Existing rows keep their values ('ready' stays
-- 'ready'; historical rows are not rewritten to 'matched').

DROP INDEX idx_media_items_metadata_status;

ALTER TABLE media_items ADD COLUMN metadata_status_new TEXT NOT NULL DEFAULT 'pending'
    CHECK (metadata_status_new IN ('pending', 'matched', 'ready', 'unmatched'));

UPDATE media_items SET metadata_status_new = metadata_status;

ALTER TABLE media_items DROP COLUMN metadata_status;
ALTER TABLE media_items RENAME COLUMN metadata_status_new TO metadata_status;

CREATE INDEX idx_media_items_metadata_status ON media_items (metadata_status, id DESC);
