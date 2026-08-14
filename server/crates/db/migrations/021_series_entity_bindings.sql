-- ADR-0046 item 2: a show folder may bind more than one provider entity, and
-- stays exactly one browse unit while doing it. `series` keeps `tmdb_show_id`
-- as the primary binding (ADR-0033, migration 016) and this table records
-- every entity the folder binds to, keyed on the same `(library_id, relpath)`
-- pair so migration and runtime cannot disagree about the key.
--
-- A nullable second column on `series` was rejected: Monster is already three
-- entities on the day this ships, and a column named for a cardinality is a
-- shape you replace rather than extend. A per-item provider id on
-- `media_items` was rejected under Rule 4.11 — `media_item_links` is that
-- record already.
--
-- The primary binding is a row here too, so there is one shape rather than a
-- special case for the first entity.
--
-- **Season ranges: NULL means unbounded, not unknown.** A primary row backfilled
-- from an existing `series` row is NULL on all four columns, which reads as
-- "covers every folder season, numbering unchanged" — precisely today's
-- behaviour, so a folder that never spans keeps behaving exactly as it does
-- now. A second entity carries a bounded range: Will & Grace's revival is
-- folder seasons 9–11 mapping to entity seasons 1–3.
--
-- The mapping is a stored range and **not an offset** (ADR-0046 item 3b). An
-- offset does not generalise: an anthology has no offset — Monster's entities
-- both start at season 1 and the folder's number is an ordinal over them — and
-- a sequel miniseries may be a season, a special, or unnumbered.

CREATE TABLE series_entity_bindings (
    library_id INTEGER NOT NULL,
    relpath TEXT NOT NULL,
    tmdb_show_id INTEGER NOT NULL,
    is_primary INTEGER NOT NULL DEFAULT 0 CHECK (is_primary IN (0, 1)),
    folder_season_start INTEGER,
    folder_season_end INTEGER,
    entity_season_start INTEGER,
    entity_season_end INTEGER,
    PRIMARY KEY (library_id, relpath, tmdb_show_id),
    FOREIGN KEY (library_id, relpath)
        REFERENCES series (library_id, relpath) ON DELETE CASCADE,
    CHECK (folder_season_start IS NULL OR folder_season_end IS NULL
           OR folder_season_start <= folder_season_end),
    CHECK (entity_season_start IS NULL OR entity_season_end IS NULL
           OR entity_season_start <= entity_season_end)
);

-- One primary per folder. A partial index rather than a CHECK, because the
-- constraint is across rows and SQLite CHECK is per row.
CREATE UNIQUE INDEX series_entity_bindings_one_primary
    ON series_entity_bindings (library_id, relpath)
    WHERE is_primary = 1;

-- Backfill by copy, not re-derivation (ADR-0046 item 2, "Existing rows").
-- Every folder that binds today gets exactly one row, primary, unbounded on
-- both sides. `INSERT OR IGNORE` against the primary key makes this a no-op if
-- it ever runs twice; the migration framework already guarantees it runs once.
INSERT OR IGNORE INTO series_entity_bindings
    (library_id, relpath, tmdb_show_id, is_primary,
     folder_season_start, folder_season_end,
     entity_season_start, entity_season_end)
SELECT library_id, relpath, tmdb_show_id, 1, NULL, NULL, NULL, NULL
FROM series;
