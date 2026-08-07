-- ADR-0023 §6 (amended 2026-08-07, scanner audit): drop the dead
-- `subtitle_source_mtime_ms` / `subtitle_source_size_bytes` invalidation
-- stamps. `subtitle_content_id` is the sole validity stamp for extracted
-- subtitles — the mtime/size pair was the pre-ADR leftover from migration 005
-- that nothing ever read back (Rule 4.11: one invalidation mechanism, not
-- two). Plain `DROP COLUMN` — the same mechanism migrations 006 and 014 use
-- for the removed side of their add-copy-drop-rename column rebuilds; these
-- two columns carry no index, CHECK, view, or trigger reference, so no
-- rebuild or index drop is needed. Migrations 005, 006, and 017, which wrote
-- the pair earlier in the sequence, stay untouched.
PRAGMA foreign_keys = ON;

ALTER TABLE media_items DROP COLUMN subtitle_source_mtime_ms;
ALTER TABLE media_items DROP COLUMN subtitle_source_size_bytes;
