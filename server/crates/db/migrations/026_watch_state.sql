-- ADR-0035 item 1: one mutable row per (profile, item_key). There is no
-- series-level row and no play count; the history question belongs to
-- ADR-0036 and a different table.
--
-- `duration_ms` is a snapshot of the file that was playing, not a foreign key
-- to the current file (ADR-0035 item 1). It is NOT NULL because the writer
-- refuses a zero duration, so every stored row carries the duration its
-- thresholds were computed against (ADR-0035 amendment 2026-09-12 item 4).
--
-- `profile_id` cascades on profile deletion (ADR-0034 item 7). `first_played_at`
-- is the server time of the first qualifying write at or above the 2% floor and
-- is preserved; `last_played_at` is the server time of every qualifying write.

CREATE TABLE watch_state (
    profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    item_key TEXT NOT NULL,
    position_ms INTEGER NOT NULL,
    duration_ms INTEGER NOT NULL,
    played INTEGER NOT NULL DEFAULT 0 CHECK (played IN (0, 1)),
    hidden INTEGER NOT NULL DEFAULT 0 CHECK (hidden IN (0, 1)),
    first_played_at TEXT NOT NULL,
    last_played_at TEXT NOT NULL,
    PRIMARY KEY (profile_id, item_key)
);

-- Every read is the rail asking for a profile's recent items in order
-- (ADR-0035 item 1).
CREATE INDEX idx_watch_state_recent ON watch_state(profile_id, last_played_at DESC);
