-- ADR-0043 §2, built, and extended with the route half.
--
-- Two nullable TEXT columns recording **why the matcher decided what it
-- decided**: one for a failure, one for a success and by which route. Same
-- column family, same write site — `set_metadata_status` already updates
-- `metadata_status` per item, and both columns are set in that statement, so
-- this adds no write and no round trip.
--
-- **Diagnostic, never control flow** (ADR-0043 §2). Nothing reads either column
-- to decide what to do next; they record why a decision already taken came out
-- as it did. A consumer that branched on one would make the token sets an API
-- and freeze them.
--
-- **Not a CHECK constraint and not a foreign key to a token table.** Both sets
-- grow as the matcher distinguishes more causes — ADR-0043's own list gained a
-- token and lost one between being written and being built — and a CHECK would
-- make every addition a migration.
--
-- **No backfill.** Existing rows read NULL, which honestly means "decided
-- before this migration" and is distinguishable from every real token.
--
-- Precedent: `metadata_negative_cache.reason` (migration 009) is already a
-- closed-token reason column on the negative path. This extends an established
-- pattern to the path that succeeds (Rule 4.11).

ALTER TABLE media_items ADD COLUMN metadata_unmatched_reason TEXT;
ALTER TABLE media_items ADD COLUMN metadata_match_method TEXT;
