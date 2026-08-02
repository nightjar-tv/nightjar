-- ADR-0029: entity-keyed canonical projection + file↔item join.

CREATE TABLE metadata_canonical (
    provider TEXT NOT NULL,
    entity_kind TEXT NOT NULL CHECK (entity_kind IN ('movie', 'tv', 'episode')),
    provider_id TEXT NOT NULL,
    title TEXT NOT NULL,
    original_title TEXT,
    year INTEGER,
    air_date TEXT,
    plot TEXT,
    season INTEGER,
    episode INTEGER,
    runtime_minutes INTEGER,
    -- Kind-sparse: episode rows leave genres/cast NULL (inherit via tv).
    genres_json TEXT,
    cast_json TEXT,
    ratings_json TEXT,
    artwork_json TEXT,
    ids_json TEXT NOT NULL,
    collection_id INTEGER,
    collection_name TEXT,
    -- Episode parent for season-scoped re-project delete (ADR-0029 §1.6).
    tmdb_show INTEGER,
    projected_at TEXT NOT NULL,
    PRIMARY KEY (provider, entity_kind, provider_id)
);

CREATE INDEX idx_metadata_canonical_episode_show_season
    ON metadata_canonical (provider, tmdb_show, season)
    WHERE entity_kind = 'episode';

-- Provider (and NFO-upgrade) keys only. Path keys are derived (ADR-0029 §2.2).
CREATE TABLE media_item_links (
    media_item_id INTEGER NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
    item_key TEXT NOT NULL,
    manually_matched INTEGER NOT NULL DEFAULT 0
        CHECK (manually_matched IN (0, 1)),
    PRIMARY KEY (media_item_id, item_key)
);

CREATE INDEX idx_media_item_links_item_key ON media_item_links (item_key);
