-- ADR-0013 §13.2/§13.4 (amended 2026-09-15): immutable, generation-addressed
-- subtitle artifacts and the committed per-track publication reference.
--
-- One row per `(media_item_id, track_id, token)`. It is the *only* thing that
-- makes an immutable generation artifact servable: a file on disk is never
-- enough, and an item-level `subtitle_status = 'ready'` is the coarse
-- preparing/lifecycle field, not the serving gate.
--
-- Every row is written by the D2B.2 source compare-and-swap, which re-reads the
-- certified source in the same transaction. The captured identity is recorded
-- on the row so serving can reject a reference whose certification, revisions
-- or sidecar generation no longer match, even when cleanup has not run.
--
-- The table starts empty. No backfill: a pre-existing mutable
-- `subs/{itemId}/{trackId}.vtt` has no generation token, so it is an orphan and
-- is never served through this contract (ADR-0013 §13.2).
PRAGMA foreign_keys = ON;

-- ADR-0013 §13.2: the per-item allocator for `artifactRevision`. Allocation
-- reserves a candidate identity; it does not publish readiness. A reserved
-- revision that is never committed leaves a gap, which is harmless because
-- serving resolves the filename from the committed publication row only.
ALTER TABLE media_items
    ADD COLUMN subtitle_artifact_sequence INTEGER NOT NULL DEFAULT 0
    CHECK (subtitle_artifact_sequence >= 0);

CREATE TABLE subtitle_publications (
    media_item_id INTEGER NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
    track_id TEXT NOT NULL,
    token TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('partial', 'complete')),
    -- ADR-0013 §13.2: the immutable artifact revision this reference names. It
    -- is positive and allocated monotonically per item. Serving builds the
    -- filename from this value and never from the request.
    artifact_revision INTEGER NOT NULL CHECK (artifact_revision > 0),
    -- ADR-0013 §11: the server-declared per-track revision. Every committed
    -- publication for one `(item, track, token)` bumps it, so a client reload
    -- never depends on process-local state that a restart would reset.
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision >= 1),
    media_revision INTEGER NOT NULL,
    probe_revision INTEGER NOT NULL,
    sidecar_generation INTEGER
        CHECK (sidecar_generation IS NULL OR sidecar_generation > 0),
    -- The captured certification stamp. Equal to `media_items.content_id` at
    -- publication; a later certification that moved no revision still changes
    -- it, so serving compares it against the current certified source.
    subtitle_content_id TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (media_item_id, track_id, token)
);

CREATE INDEX idx_subtitle_publications_item
    ON subtitle_publications (media_item_id);
