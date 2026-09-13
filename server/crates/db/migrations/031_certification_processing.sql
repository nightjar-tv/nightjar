-- ADR-0037 item 8 (B2-6, Astra correction): the processed-state marker for the
-- certification projection.
--
-- `certifications_projection_version` is the policy version that produced
-- `certifications_json`. Version 0 means unprocessed or legacy; the current
-- policy starts at 1 and increments whenever the reduction semantics change, so
-- the bounded back-fill can tell a stale row from a finished one without
-- hashing in playback.
--
-- `certifications_source_sha256` is SHA-256 over the exact stored raw payload
-- bytes the projection read. A no-payload row legitimately keeps NULL, which is
-- why the candidate predicate is state-based rather than NULL-hash based; every
-- raw-payload write in this codebase is paired with a projection write in the
-- same transaction, so a non-NULL version already means the hash matches.

ALTER TABLE metadata_canonical
    ADD COLUMN certifications_projection_version INTEGER NOT NULL DEFAULT 0;

ALTER TABLE metadata_canonical
    ADD COLUMN certifications_source_sha256 TEXT;
