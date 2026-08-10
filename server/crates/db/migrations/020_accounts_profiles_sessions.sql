-- ADR-0034: accounts, profiles and login sessions. ADR-0040 item 2 supplies
-- the `role` column in place of ADR-0034 item 1's `can_manage_server`, which
-- never reached disk.
--
-- Login sessions are not playback sessions (ADR-0007 / ADR-0011). Those keep
-- the `sessions` name on their own route namespace; these live under
-- `/api/v0/auth/` and are named for what they are.

CREATE TABLE accounts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    username TEXT NOT NULL UNIQUE,
    -- PHC string, so the parameters travel with the hash and raising the
    -- constants later does not lock anyone out (ADR-0034 item 4).
    password_hash TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('owner', 'manager', 'member'))
        DEFAULT 'member',
    -- ADR-0034 item 8: concurrent playback across every profile on this
    -- account. Null means no per-account limit and is the shipped default.
    -- The shape is decided by that item; B2-9 enforces it, and nothing reads
    -- this column before then (Rule 4.9: shape before writer).
    max_concurrent_sessions INTEGER,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

-- ADR-0040 item 2: exactly one owner is a constraint, not a convention. A
-- partial unique index means two owners cannot exist even through a code path
-- nobody reviewed. The guarantee is that the write fails.
CREATE UNIQUE INDEX idx_accounts_one_owner ON accounts (role) WHERE role = 'owner';

CREATE TABLE profiles (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- ADR-0034 item 6: 128 bits of OS randomness as 32 lowercase hex chars.
    -- Not the rowid: SQLite reuses rowids after a delete, so profile 4 deleted
    -- and recreated would inherit the old profile's history.
    profile_ref TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    -- Two fields that stay two fields (ADR-0034 item 1). What a cap means is
    -- B2-D's decision; this migration decides only that both columns exist.
    -- Null cap means uncapped, which is the creation default.
    classification_cap TEXT,
    simple_interface INTEGER NOT NULL DEFAULT 0
        CHECK (simple_interface IN (0, 1)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX idx_profiles_account ON profiles(account_id);

CREATE TABLE login_sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- Null until the session narrows to a profile (ADR-0034 item 3).
    --
    -- ON DELETE CASCADE, deliberately, and it is the one place this migration
    -- chooses where ADR-0034 item 5 rules an option out without naming the
    -- replacement. That item forbids ON DELETE SET NULL, because widening a
    -- capped session back to account scope is a privilege escalation dressed
    -- as a foreign key action. It also says deleting a profile *revokes* those
    -- sessions, and revocation elsewhere means setting `revoked_at` rather
    -- than deleting the row. A revoked row cannot keep a foreign key to a
    -- deleted profile, so those two cannot both hold. Destroying the session
    -- is at least as strong as revoking it; the cost is that presenting that
    -- token afterwards reads as unknown rather than revoked, losing one of
    -- item 5's three distinct errors for this one path.
    active_profile_id INTEGER REFERENCES profiles(id) ON DELETE CASCADE,
    -- SHA-256 of the token as lowercase hex. The token itself is never stored
    -- and is returned to the client exactly once (ADR-0034 item 5).
    token_sha256 TEXT NOT NULL UNIQUE,
    -- Supplied at login so the revoke list can name a device.
    client_label TEXT NOT NULL,
    issued_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    -- Absolute, 90 days, no sliding renewal and no refresh token (Rule 4.7).
    --
    -- The length CHECK is not a format validator and does not pretend to be.
    -- Session classification compares these as strings, which is only sound
    -- while every writer uses `strftime('%Y-%m-%dT%H:%M:%fZ')`: fixed width,
    -- UTC, therefore lexicographically ordered. 24 is that shape's length, so
    -- this catches the class that silently breaks the comparison and extends
    -- or shortens a session, without inventing a guard the rest of the
    -- codebase does not have.
    expires_at TEXT NOT NULL CHECK (length(expires_at) = 24),
    last_seen_at TEXT,
    -- Set rather than deleting the row, so unknown, expired and revoked stay
    -- three distinct typed errors instead of collapsing into one.
    revoked_at TEXT
);

CREATE INDEX idx_login_sessions_account ON login_sessions(account_id);
