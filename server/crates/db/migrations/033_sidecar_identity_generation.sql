-- ADR-0010 §4 (amended 2026-09-15): durable sidecar identity and generation.
--
-- `content_id` reuses ADR-0023 §6's bounded shape for the sidecar bytes. It is
-- NULL on every existing row: a legacy sidecar is unverified until an explicit
-- reconciliation reads it, so this migration performs no filesystem read and
-- no startup backfill. `sidecar_generation` is likewise NULL until the
-- allocator assigns one.
--
-- The allocator table is the durable "generations previously allocated for
-- this item" counter. Removal deletes membership rows but not this counter, so
-- remove-then-re-add cannot reuse a deleted generation, across restart. The
-- counter starts at 0; allocated generations are positive.
PRAGMA foreign_keys = ON;

ALTER TABLE media_item_sidecars ADD COLUMN content_id TEXT;

ALTER TABLE media_item_sidecars ADD COLUMN sidecar_generation INTEGER
    CHECK (sidecar_generation IS NULL OR sidecar_generation > 0);

CREATE TABLE media_item_sidecar_generations (
    media_item_id INTEGER PRIMARY KEY REFERENCES media_items(id) ON DELETE CASCADE,
    last_generation INTEGER NOT NULL DEFAULT 0 CHECK (last_generation >= 0)
);
