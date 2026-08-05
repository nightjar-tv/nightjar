-- ADR-0033: durable folder-keyed series identity. One row per show folder
-- under one library; two folders never merge by fold collision (Q2/Q3).
-- The show folder is a path walk (season dirs inherit), so the one-shot
-- retro-derive of existing `ready` rows lives as a code step beside this DDL
-- in migrate.rs (Q5) — the same show_folder_relpath the queue group
-- formation uses, so migration and runtime always agree on the key.
-- Nothing inside drain_pending re-derives series rows (plan Decision 6).

CREATE TABLE series (
    library_id INTEGER NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    relpath TEXT NOT NULL,
    tmdb_show_id INTEGER NOT NULL,
    PRIMARY KEY (library_id, relpath)
);
