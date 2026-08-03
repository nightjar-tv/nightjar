-- ADR-0026 §3: stamp negative-cache rows so cleaner fold changes re-search.

ALTER TABLE metadata_negative_cache
    ADD COLUMN cleaner_version INTEGER NOT NULL DEFAULT 1;
