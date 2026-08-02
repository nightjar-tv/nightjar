-- ADR-0026 §8: enrichment state on the item row; queue is a SELECT over this.

ALTER TABLE media_items ADD COLUMN metadata_status TEXT NOT NULL DEFAULT 'pending'
    CHECK (metadata_status IN ('pending', 'ready', 'unmatched'));

CREATE INDEX idx_media_items_metadata_status ON media_items (metadata_status, id DESC);
