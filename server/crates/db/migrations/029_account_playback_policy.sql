-- ADR-0034 item 8 as amended 2026-09-12, ADR-0022 §5 as amended 2026-09-12
-- (B2-9): the account carries the policy half of the playback ceilings.
--
-- `max_bitrate_bps` and `max_height` are new and nullable. Null means no
-- account policy ceiling and is the shipped default for all three columns; a
-- finite default is never inferred from machine capacity (ADR-0034 item 8).
--
-- `max_concurrent_sessions` already exists from migration 020 and had no
-- positivity constraint. The plan's contract is "nullable positive integer"
-- for each of the three, so this rebuilds `accounts` to give the existing
-- column the same CHECK the two new ones get. SQLite cannot add a constraint
-- to an existing column, so this is the table rebuild.
--
-- `profiles` and `login_sessions` reference `accounts(id) ON DELETE CASCADE`.
-- `DROP TABLE accounts` with foreign keys live would cascade both away, and
-- `PRAGMA foreign_keys=OFF` is a no-op inside the migration transaction, so
-- `migrate.rs` turns it off for this version and restores it after the
-- commit. It counts `accounts`, `profiles` and `login_sessions` either side of
-- the copy so a silent cascade fails the migration instead of shipping.
--
-- The primary key, the username UNIQUE, the role CHECK and the two indexes
-- are carried over unchanged. `profiles` and `login_sessions` are not
-- touched, and their foreign keys keep resolving to the rebuilt table by name.

CREATE TABLE accounts_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    username TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('owner', 'manager', 'member'))
        DEFAULT 'member',
    -- Concurrent playback across every profile on this account. Null means no
    -- per-account limit and is the shipped default. Counted at playback
    -- session creation only (ADR-0034 item 8). The upper bound matches the
    -- `u32` the enforcement path reads, so storage and enforcement agree.
    max_concurrent_sessions INTEGER
        CHECK (
            max_concurrent_sessions IS NULL
            OR (max_concurrent_sessions > 0 AND max_concurrent_sessions <= 4294967295)
        ),
    -- Account policy bitrate ceiling. Advisory until trusted-proxy work lands:
    -- surfaced on the account response and playback-info, not byte-enforced
    -- (ADR-0022 §5 as amended).
    max_bitrate_bps INTEGER
        CHECK (max_bitrate_bps IS NULL OR max_bitrate_bps > 0),
    -- Account policy height ceiling. Same advisory posture as the bitrate.
    max_height INTEGER
        CHECK (max_height IS NULL OR max_height > 0),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

INSERT INTO accounts_new
    (id, username, password_hash, role, max_concurrent_sessions, created_at)
SELECT id, username, password_hash, role, max_concurrent_sessions, created_at
FROM accounts;

DROP TABLE accounts;

ALTER TABLE accounts_new RENAME TO accounts;

-- ADR-0040 item 2: exactly one owner is a constraint, not a convention.
CREATE UNIQUE INDEX idx_accounts_one_owner ON accounts (role) WHERE role = 'owner';

-- Migration 025: `Root` and `root` are one account.
CREATE UNIQUE INDEX idx_accounts_username_nocase
    ON accounts (username COLLATE NOCASE);
