-- `Root` and `root` were two accounts, and the constraint meant to prevent
-- that did not (OPEN-DEFECTS entry 14).
--
-- `username TEXT NOT NULL UNIQUE` in migration 020 carries no collation, so
-- SQLite compares it byte for byte. Both halves follow from that one fact:
-- a login typed in the wrong case is rejected with the same message a wrong
-- password gets, and two accounts differing only in case can both exist.
--
-- **The uniqueness half is the worse one.** Nobody has hit it because these
-- installs have one account, but nothing stops it, and there is no good
-- answer once two such rows exist.
--
-- **A unique index, not a rebuilt column.** Adding `COLLATE NOCASE` to the
-- column itself needs SQLite's twelve-step table rebuild, and that requires
-- `PRAGMA foreign_keys=OFF` *outside* a transaction — the pragma is a no-op
-- inside one, and every migration here runs in a transaction. `profiles` and
-- `login_sessions` both reference `accounts(id) ON DELETE CASCADE`, so a
-- rebuild with foreign keys live would cascade them away. An index over
-- `username COLLATE NOCASE` gives the identical uniqueness guarantee with no
-- rebuild and no cascade. The column's own `UNIQUE` stays and is subsumed by
-- it: anything the byte-wise constraint refuses, this one refuses too.
--
-- The lookup in `accounts.rs` compares `COLLATE NOCASE` and uses this index.
-- **A colliding install is refused before this file runs**, by
-- `refuse_colliding_usernames` in `migrate.rs`, which names the rows rather
-- than choosing a winner. Merging two accounts silently would move one
-- person's profiles and watch state onto another person's login.

CREATE UNIQUE INDEX idx_accounts_username_nocase
    ON accounts (username COLLATE NOCASE);
