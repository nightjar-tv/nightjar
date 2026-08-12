-- ADR-0039 item 3: a `series` row is written when a show folder forms a
-- resolve group, not when it matches, so `tmdb_show_id` becomes nullable and
-- null means "no entity yet".
--
-- Without this, ADR-0039 item 2's `folder:{library_id}:{relpath}` key exists on
-- paper and nowhere in the database, and the three records that consume
-- `series_key` get a null for exactly the unmatched fraction each was written
-- to support.
--
-- SQLite cannot drop a NOT NULL, so this is the table rebuild, and the row
-- count is checked either side of it in `migrate.rs` (the same guard migration
-- 012 uses). The primary key, the foreign key and the cascade are carried over
-- unchanged; only the nullability moves.

CREATE TABLE series_new (
    library_id INTEGER NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    relpath TEXT NOT NULL,
    -- Null means the folder has formed a group and has no entity yet. It is
    -- not "unknown": a row exists precisely because the folder is known.
    tmdb_show_id INTEGER,
    PRIMARY KEY (library_id, relpath)
);

INSERT INTO series_new (library_id, relpath, tmdb_show_id)
SELECT library_id, relpath, tmdb_show_id FROM series;

DROP TABLE series;

ALTER TABLE series_new RENAME TO series;
