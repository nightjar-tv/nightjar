//! Accounts, profiles and login sessions (ADR-0034, ADR-0040).
//!
//! Storage and session classification only. Password hashing and token minting
//! live in `nightjar-auth`, and this crate deliberately does not depend on it:
//! `argon2` pulls fourteen nodes, and `nightjar-db` is depended on by
//! `scanner`, `transcode` and `metadata`, none of which go near credentials.
//! The caller hands in an already-hashed token.

use rusqlite::{Connection, OptionalExtension, params};

/// Why a presented token is not a usable session.
///
/// Three distinct values rather than one bool, because an operator reading a
/// log needs to tell a stale bookmark from a revoked device (ADR-0034 item 5).
///
/// **Which paths produce which, stated because one of them is not obvious:**
///
/// | rejection | reached by |
/// |---|---|
/// | `Unknown` | a token never issued; **a token whose row was destroyed** — account deleted, *profile deleted*, or swept long after expiry |
/// | `Expired` | the row is present and past `expires_at`, and was never revoked |
/// | `Revoked` | the row is present and `revoked_at` is set: sign out, sign out everywhere, or revoking one device |
///
/// `Revoked` is reachable in B2-1 only through signing out and signing out
/// everywhere. Revoking a *named other* session, which is what `client_label`
/// exists to label, has no route yet: the plan schedules it in no slice, so
/// the column has no reader. Until that route lands, this classifier's third
/// state is exercised by two paths rather than three, and a reader counting
/// call sites will undercount it rather than find it dead.
///
/// The middle row is the one worth reading twice. Migration 020 gives
/// `login_sessions.active_profile_id` `ON DELETE CASCADE`, so deleting a
/// profile **destroys** its sessions rather than marking them revoked, and a
/// token for one of them reads `Unknown`. ADR-0034 item 5 describes that path
/// as revocation, and it is not observable as `Revoked`. All three values are
/// reachable, but not all three are reachable from every path that ends a
/// session, and a reader of this enum should not assume otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRejection {
    Unknown,
    Expired,
    Revoked,
}

/// Classify a session row that has already been found.
///
/// Revocation outranks expiry: a session revoked and then left to expire reads
/// `Revoked`, because revocation is a deliberate act and is the more useful
/// answer to "why is this device signed out". Timestamps compare as strings,
/// which is sound because every one is written by
/// `strftime('%Y-%m-%dT%H:%M:%fZ')`: fixed width, UTC, and therefore
/// lexicographically ordered.
pub fn classify_session(
    expires_at: &str,
    revoked_at: Option<&str>,
    now: &str,
) -> Result<(), SessionRejection> {
    if revoked_at.is_some() {
        return Err(SessionRejection::Revoked);
    }
    if expires_at <= now {
        return Err(SessionRejection::Expired);
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRow {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
    /// One of `owner` | `manager` | `member`; the CHECK is the closed set.
    /// Parsed to `nightjar_core::Role` at the boundary that needs it, the same
    /// way `libraries.kind` is parsed to `LibraryKind` at the route.
    pub role: String,
    pub max_concurrent_sessions: Option<i64>,
    /// Account policy bitrate ceiling; null means no ceiling (ADR-0022 §5 as
    /// amended 2026-09-12). Advisory until trusted-proxy work lands.
    pub max_bitrate_bps: Option<i64>,
    /// Account policy height ceiling; null means no ceiling.
    pub max_height: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRow {
    pub id: i64,
    pub account_id: i64,
    pub profile_ref: String,
    pub name: String,
    pub classification_cap: Option<String>,
    pub simple_interface: bool,
    /// ISO-639-1-shaped lowercase code, or null for no preference
    /// (ADR-0038 item 1 and its 2026-09-12 amendment).
    pub preferred_language: Option<String>,
    /// `auto` | `off` (ADR-0038 item 1).
    pub subtitle_default: String,
}

/// A session resolved from a presented token, with the account's role already
/// joined so an authority check is one read rather than two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub id: i64,
    pub account_id: i64,
    pub role: String,
    pub active_profile_id: Option<i64>,
    pub client_label: String,
    pub expires_at: String,
    pub revoked_at: Option<String>,
}

/// Resolve a presented token to a usable session, or say why not.
///
/// `token_sha256_hex` is the digest, never the token: the plaintext is handed
/// to the client exactly once and is not stored (ADR-0034 item 5).
pub fn session_for_token(
    conn: &Connection,
    token_sha256_hex: &str,
    now: &str,
) -> Result<Result<SessionRow, SessionRejection>, String> {
    let row: Option<SessionRow> = conn
        .query_row(
            "SELECT s.id, s.account_id, a.role, s.active_profile_id, s.client_label,
                    s.expires_at, s.revoked_at
             FROM login_sessions s
             JOIN accounts a ON a.id = s.account_id
             WHERE s.token_sha256 = ?1",
            params![token_sha256_hex],
            |r| {
                Ok(SessionRow {
                    id: r.get(0)?,
                    account_id: r.get(1)?,
                    role: r.get(2)?,
                    active_profile_id: r.get(3)?,
                    client_label: r.get(4)?,
                    expires_at: r.get(5)?,
                    revoked_at: r.get(6)?,
                })
            },
        )
        .optional()
        .map_err(|e| format!("session lookup: {e}"))?;

    let Some(row) = row else {
        return Ok(Err(SessionRejection::Unknown));
    };
    match classify_session(&row.expires_at, row.revoked_at.as_deref(), now) {
        Ok(()) => Ok(Ok(row)),
        Err(rejection) => Ok(Err(rejection)),
    }
}

/// Whether any account exists. The single condition ADR-0034 item 10 gates
/// bootstrap on, and half of the setup readout.
pub fn account_exists(conn: &Connection) -> Result<bool, String> {
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))
        .map_err(|e| format!("account exists: {e}"))?;
    Ok(n > 0)
}

/// Whether any account can administer the server, which is what a setup wizard
/// means by "admin exists" (ADR-0040 item 1: owner or manager).
pub fn admin_exists(conn: &Connection) -> Result<bool, String> {
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM accounts WHERE role IN ('owner', 'manager')",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("admin exists: {e}"))?;
    Ok(n > 0)
}

pub fn library_exists(conn: &Connection) -> Result<bool, String> {
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM libraries", [], |r| r.get(0))
        .map_err(|e| format!("library exists: {e}"))?;
    Ok(n > 0)
}

/// Look an account up by name, without regard to case.
///
/// **`Root` and `root` are one account** (migration 025). The column's own
/// comparison is byte-wise, so this said no to a correctly-typed password
/// whose username differed in case — and ADR-0034 item 5 makes an unknown
/// username and a wrong password deliberately indistinguishable, which is
/// right for enumeration resistance and means a case slip is unreportable.
/// The server cannot say *"wrong case"* without saying *"this user exists"*.
///
/// `COLLATE NOCASE` here matches `idx_accounts_username_nocase`, so the
/// lookup still uses an index. **The stored spelling is returned unchanged**:
/// the row carries whatever case the account was created with, and only the
/// comparison is case-blind.
pub fn account_by_username(
    conn: &Connection,
    username: &str,
) -> Result<Option<AccountRow>, String> {
    conn.query_row(
        "SELECT id, username, password_hash, role, max_concurrent_sessions,
                max_bitrate_bps, max_height
         FROM accounts WHERE username = ?1 COLLATE NOCASE",
        params![username],
        map_account,
    )
    .optional()
    .map_err(|e| format!("account by username: {e}"))
}

pub fn account_by_id(conn: &Connection, id: i64) -> Result<Option<AccountRow>, String> {
    conn.query_row(
        "SELECT id, username, password_hash, role, max_concurrent_sessions,
                max_bitrate_bps, max_height
         FROM accounts WHERE id = ?1",
        params![id],
        map_account,
    )
    .optional()
    .map_err(|e| format!("account by id: {e}"))
}

pub fn list_accounts(conn: &Connection) -> Result<Vec<AccountRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, username, password_hash, role, max_concurrent_sessions,
                    max_bitrate_bps, max_height
             FROM accounts ORDER BY id",
        )
        .map_err(|e| format!("prepare list accounts: {e}"))?;
    let rows = stmt
        .query_map([], map_account)
        .map_err(|e| format!("list accounts: {e}"))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| format!("account row: {e}"))?);
    }
    Ok(out)
}

fn map_account(r: &rusqlite::Row<'_>) -> rusqlite::Result<AccountRow> {
    Ok(AccountRow {
        id: r.get(0)?,
        username: r.get(1)?,
        password_hash: r.get(2)?,
        role: r.get(3)?,
        max_concurrent_sessions: r.get(4)?,
        max_bitrate_bps: r.get(5)?,
        max_height: r.get(6)?,
    })
}

fn map_profile(r: &rusqlite::Row<'_>) -> rusqlite::Result<ProfileRow> {
    let simple: i64 = r.get(5)?;
    Ok(ProfileRow {
        id: r.get(0)?,
        account_id: r.get(1)?,
        profile_ref: r.get(2)?,
        name: r.get(3)?,
        classification_cap: r.get(4)?,
        simple_interface: simple != 0,
        preferred_language: r.get(6)?,
        subtitle_default: r.get(7)?,
    })
}

pub fn profiles_for_account(conn: &Connection, account_id: i64) -> Result<Vec<ProfileRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, account_id, profile_ref, name, classification_cap, simple_interface,
                    preferred_language, subtitle_default
             FROM profiles WHERE account_id = ?1 ORDER BY id",
        )
        .map_err(|e| format!("prepare profiles: {e}"))?;
    let rows = stmt
        .query_map(params![account_id], map_profile)
        .map_err(|e| format!("profiles for account: {e}"))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| format!("profile row: {e}"))?);
    }
    Ok(out)
}

pub fn profile_by_ref(conn: &Connection, profile_ref: &str) -> Result<Option<ProfileRow>, String> {
    conn.query_row(
        "SELECT id, account_id, profile_ref, name, classification_cap, simple_interface,
                preferred_language, subtitle_default
         FROM profiles WHERE profile_ref = ?1",
        params![profile_ref],
        map_profile,
    )
    .optional()
    .map_err(|e| format!("profile by ref: {e}"))
}

pub fn profile_by_id(conn: &Connection, profile_id: i64) -> Result<Option<ProfileRow>, String> {
    conn.query_row(
        "SELECT id, account_id, profile_ref, name, classification_cap, simple_interface,
                preferred_language, subtitle_default
         FROM profiles WHERE id = ?1",
        params![profile_id],
        map_profile,
    )
    .optional()
    .map_err(|e| format!("profile by id: {e}"))
}

/// Create an account and its first profile in one transaction (ADR-0034
/// item 3: an account with no profile has nowhere to write watch state).
pub fn create_account_with_profile(
    conn: &Connection,
    username: &str,
    password_hash: &str,
    role: &str,
    profile_name: &str,
    profile_ref: &str,
) -> Result<(i64, i64), String> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("begin create account: {e}"))?;
    tx.execute(
        "INSERT INTO accounts (username, password_hash, role) VALUES (?1, ?2, ?3)",
        params![username, password_hash, role],
    )
    .map_err(|e| format!("insert account: {e}"))?;
    let account_id = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO profiles (account_id, profile_ref, name) VALUES (?1, ?2, ?3)",
        params![account_id, profile_ref, profile_name],
    )
    .map_err(|e| format!("insert profile: {e}"))?;
    let profile_id = tx.last_insert_rowid();
    tx.commit()
        .map_err(|e| format!("commit create account: {e}"))?;
    Ok((account_id, profile_id))
}

#[allow(clippy::too_many_arguments)]
pub fn create_profile(
    conn: &Connection,
    account_id: i64,
    profile_ref: &str,
    name: &str,
    classification_cap: Option<&str>,
    simple_interface: bool,
    preferred_language: Option<&str>,
    subtitle_default: &str,
) -> Result<i64, String> {
    conn.execute(
        "INSERT INTO profiles
             (account_id, profile_ref, name, classification_cap, simple_interface,
              preferred_language, subtitle_default)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            account_id,
            profile_ref,
            name,
            classification_cap,
            simple_interface as i64,
            preferred_language,
            subtitle_default
        ],
    )
    .map_err(|e| format!("insert profile: {e}"))?;
    Ok(conn.last_insert_rowid())
}

/// Replace a profile's preference fields (ADR-0038 amendment §2). Null
/// language clears the preference; `subtitle_default` is `auto` or `off`.
pub fn update_profile_preferences(
    conn: &Connection,
    profile_id: i64,
    preferred_language: Option<&str>,
    subtitle_default: &str,
) -> Result<(), String> {
    conn.execute(
        "UPDATE profiles SET preferred_language = ?2, subtitle_default = ?3 WHERE id = ?1",
        params![profile_id, preferred_language, subtitle_default],
    )
    .map_err(|e| format!("update profile preferences: {e}"))?;
    Ok(())
}

pub fn create_session(
    conn: &Connection,
    account_id: i64,
    token_sha256: &str,
    client_label: &str,
    expires_at: &str,
) -> Result<i64, String> {
    conn.execute(
        "INSERT INTO login_sessions (account_id, token_sha256, client_label, expires_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![account_id, token_sha256, client_label, expires_at],
    )
    .map_err(|e| format!("insert session: {e}"))?;
    Ok(conn.last_insert_rowid())
}

/// Revoke rather than delete, so the token reads as revoked rather than
/// unknown on its next presentation (ADR-0034 item 5).
pub fn revoke_session(conn: &Connection, session_id: i64, now: &str) -> Result<(), String> {
    conn.execute(
        "UPDATE login_sessions SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL",
        params![session_id, now],
    )
    .map_err(|e| format!("revoke session: {e}"))?;
    Ok(())
}

pub fn revoke_all_for_account(
    conn: &Connection,
    account_id: i64,
    now: &str,
) -> Result<usize, String> {
    conn.execute(
        "UPDATE login_sessions SET revoked_at = ?2
         WHERE account_id = ?1 AND revoked_at IS NULL",
        params![account_id, now],
    )
    .map_err(|e| format!("revoke all: {e}"))
}

/// Narrow to a profile, or widen back with `None` (ADR-0034 item 3).
pub fn set_active_profile(
    conn: &Connection,
    session_id: i64,
    profile_id: Option<i64>,
) -> Result<(), String> {
    conn.execute(
        "UPDATE login_sessions SET active_profile_id = ?2 WHERE id = ?1",
        params![session_id, profile_id],
    )
    .map_err(|e| format!("set active profile: {e}"))?;
    Ok(())
}

pub fn set_role(conn: &Connection, account_id: i64, role: &str) -> Result<(), String> {
    conn.execute(
        "UPDATE accounts SET role = ?2 WHERE id = ?1",
        params![account_id, role],
    )
    .map_err(|e| format!("set role: {e}"))?;
    Ok(())
}

/// Replace an account's three playback-policy ceilings (ADR-0034 item 8,
/// ADR-0022 §5 as amended 2026-09-12). Full replacement: `None` clears the
/// ceiling. Positivity is the migration CHECK; a caller validating shape is
/// the route's job.
pub fn update_account_playback_policy(
    conn: &Connection,
    account_id: i64,
    max_concurrent_sessions: Option<i64>,
    max_bitrate_bps: Option<i64>,
    max_height: Option<i64>,
) -> Result<(), String> {
    conn.execute(
        "UPDATE accounts
            SET max_concurrent_sessions = ?2, max_bitrate_bps = ?3, max_height = ?4
          WHERE id = ?1",
        params![
            account_id,
            max_concurrent_sessions,
            max_bitrate_bps,
            max_height
        ],
    )
    .map_err(|e| format!("update account playback policy: {e}"))?;
    Ok(())
}

/// Transfer ownership in one transaction (ADR-0040 item 3).
///
/// **Demote first, then promote.** The partial unique index permits exactly one
/// `owner` row, so promoting first would violate it mid-transaction. The order
/// is the mechanism, not a style choice, and reversing it fails loudly rather
/// than silently, which is the point of the index.
pub fn transfer_ownership(conn: &Connection, from: i64, to: i64) -> Result<(), String> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("begin transfer: {e}"))?;
    tx.execute(
        "UPDATE accounts SET role = 'manager' WHERE id = ?1",
        params![from],
    )
    .map_err(|e| format!("demote outgoing owner: {e}"))?;
    tx.execute(
        "UPDATE accounts SET role = 'owner' WHERE id = ?1",
        params![to],
    )
    .map_err(|e| format!("promote incoming owner: {e}"))?;
    tx.commit().map_err(|e| format!("commit transfer: {e}"))?;
    Ok(())
}

pub fn delete_account(conn: &Connection, account_id: i64) -> Result<(), String> {
    conn.execute("DELETE FROM accounts WHERE id = ?1", params![account_id])
        .map_err(|e| format!("delete account: {e}"))?;
    Ok(())
}

pub fn delete_profile(conn: &Connection, profile_id: i64) -> Result<(), String> {
    conn.execute("DELETE FROM profiles WHERE id = ?1", params![profile_id])
        .map_err(|e| format!("delete profile: {e}"))?;
    Ok(())
}

pub fn touch_last_seen(conn: &Connection, session_id: i64, now: &str) -> Result<(), String> {
    conn.execute(
        "UPDATE login_sessions SET last_seen_at = ?2 WHERE id = ?1",
        params![session_id, now],
    )
    .map_err(|e| format!("touch last seen: {e}"))?;
    Ok(())
}

/// The server clock, rendered in the one format every timestamp column uses.
///
/// Read from SQLite rather than from the process, so session expiry, revocation
/// and the `DEFAULT` on `issued_at` all come from one clock. ADR-0035 item 3
/// makes the server clock authoritative; this keeps "the server clock" from
/// meaning two slightly different things.
pub fn now_iso(conn: &Connection) -> Result<String, String> {
    conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')", [], |r| {
        r.get(0)
    })
    .map_err(|e| format!("server clock: {e}"))
}

/// `now` plus the absolute session lifetime (ADR-0034 item 5): 90 days, no
/// sliding renewal, no refresh token.
pub fn session_expiry(conn: &Connection) -> Result<String, String> {
    conn.query_row(
        "SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '+90 days')",
        [],
        |r| r.get(0),
    )
    .map_err(|e| format!("session expiry: {e}"))
}

/// Replace a stored hash, used by the rehash-on-login upgrade path.
pub fn set_password_hash(conn: &Connection, account_id: i64, hash: &str) -> Result<(), String> {
    conn.execute(
        "UPDATE accounts SET password_hash = ?2 WHERE id = ?1",
        params![account_id, hash],
    )
    .map_err(|e| format!("set password hash: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate;

    const NOW: &str = "2026-08-10T00:00:00.000Z";
    const LATER: &str = "2099-01-01T00:00:00.000Z";
    const EARLIER: &str = "2020-01-01T00:00:00.000Z";

    #[test]
    fn classify_covers_the_three_states() {
        assert_eq!(classify_session(LATER, None, NOW), Ok(()));
        assert_eq!(
            classify_session(EARLIER, None, NOW),
            Err(SessionRejection::Expired)
        );
        assert_eq!(
            classify_session(LATER, Some(EARLIER), NOW),
            Err(SessionRejection::Revoked)
        );
    }

    /// Revocation outranks expiry, so a device revoked months ago still reads
    /// as revoked rather than decaying into "expired" and losing the fact that
    /// somebody ended it deliberately.
    #[test]
    fn revoked_outranks_expired() {
        assert_eq!(
            classify_session(EARLIER, Some(EARLIER), NOW),
            Err(SessionRejection::Revoked)
        );
    }

    /// Exactly at the expiry instant the session is over. Asserted because
    /// `<=` against `<` is the kind of boundary that is chosen once and then
    /// assumed forever.
    #[test]
    fn expiry_is_inclusive() {
        assert_eq!(
            classify_session(NOW, None, NOW),
            Err(SessionRejection::Expired)
        );
        assert_eq!(
            classify_session("2026-08-10T00:00:00.001Z", None, NOW),
            Ok(())
        );
    }

    /// ADR-0040 item 3: the demote-then-promote order is the mechanism. The
    /// partial unique index permits one owner, so the reverse order violates
    /// it mid-transaction. Both the outcome and the singleton are asserted.
    #[test]
    fn transfer_demotes_then_promotes_and_keeps_one_owner() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO accounts (id, username, password_hash, role)
             VALUES (1, 'a', 'x', 'owner'), (2, 'b', 'x', 'manager');",
        )
        .unwrap();

        transfer_ownership(&conn, 1, 2).unwrap();

        let roles: Vec<(i64, String)> = {
            let mut stmt = conn
                .prepare("SELECT id, role FROM accounts ORDER BY id")
                .unwrap();
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        assert_eq!(
            roles,
            vec![(1, "manager".to_string()), (2, "owner".to_string())]
        );
        let owners: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM accounts WHERE role = 'owner'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(owners, 1, "never ownerless, never two owners");
    }

    /// Promoting without demoting is what the index exists to stop, and it
    /// must fail rather than leave two owners.
    #[test]
    fn promoting_a_second_owner_without_transfer_fails() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO accounts (id, username, password_hash, role)
             VALUES (1, 'a', 'x', 'owner'), (2, 'b', 'x', 'manager');",
        )
        .unwrap();
        assert!(set_role(&conn, 2, "owner").is_err());
        let owners: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM accounts WHERE role = 'owner'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(owners, 1);
    }

    #[test]
    fn revoking_is_idempotent_and_keeps_the_first_timestamp() {
        let conn = seeded();
        let id: i64 = conn
            .query_row(
                "SELECT id FROM login_sessions WHERE token_sha256 = 'live'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        revoke_session(&conn, id, "2026-08-10T00:00:00.000Z").unwrap();
        revoke_session(&conn, id, "2026-08-11T00:00:00.000Z").unwrap();
        let at: String = conn
            .query_row(
                "SELECT revoked_at FROM login_sessions WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            at, "2026-08-10T00:00:00.000Z",
            "a second revoke must not move the record of when it happened"
        );
    }

    #[test]
    fn creating_an_account_creates_its_first_profile() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let (account_id, _) =
            create_account_with_profile(&conn, "a", "hash", "owner", "Main", "ref0").unwrap();
        let profiles = profiles_for_account(&conn, account_id).unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "Main");
        assert_eq!(profiles[0].classification_cap, None, "uncapped by default");
        assert!(!profiles[0].simple_interface);
    }

    /// B2-9 (ADR-0034 item 8, ADR-0022 §5 as amended): the three account
    /// policy ceilings are nullable positive integers. Fresh accounts default
    /// to null, positive values round-trip, null clears, and each column's
    /// CHECK rejects zero and negatives. The negative case per column is the
    /// point: a constraint that only rejected one column would pass a test
    /// that tried one value.
    #[test]
    fn account_playback_policy_defaults_null_and_constrains_positivity() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        migrate(&conn).unwrap();
        create_account_with_profile(&conn, "a", "h", "owner", "P", "r0").unwrap();
        let account = account_by_username(&conn, "a").unwrap().unwrap();

        // Null is the shipped default for all three, not a machine guess.
        assert_eq!(account.max_concurrent_sessions, None);
        assert_eq!(account.max_bitrate_bps, None);
        assert_eq!(account.max_height, None);

        // Positive values round-trip.
        update_account_playback_policy(&conn, account.id, Some(3), Some(8_000_000), Some(1080))
            .unwrap();
        let round = account_by_id(&conn, account.id).unwrap().unwrap();
        assert_eq!(round.max_concurrent_sessions, Some(3));
        assert_eq!(round.max_bitrate_bps, Some(8_000_000));
        assert_eq!(round.max_height, Some(1080));

        // Null clears every ceiling again.
        update_account_playback_policy(&conn, account.id, None, None, None).unwrap();
        let cleared = account_by_id(&conn, account.id).unwrap().unwrap();
        assert_eq!(cleared.max_concurrent_sessions, None);
        assert_eq!(cleared.max_bitrate_bps, None);
        assert_eq!(cleared.max_height, None);

        // Each constraint rejects zero and negatives.
        for column in ["max_concurrent_sessions", "max_bitrate_bps", "max_height"] {
            for bad in [0i64, -1] {
                let sql = format!("UPDATE accounts SET {column} = ?1 WHERE id = ?2");
                assert!(
                    conn.execute(&sql, params![bad, account.id]).is_err(),
                    "{column} = {bad} must be refused by the CHECK"
                );
            }
        }

        // The concurrency column carries the enforcement path's `u32` bound, so
        // a value the boundary would reject cannot be stored either. The
        // maximum itself is accepted, which proves a bound rather than a
        // blanket refusal.
        let set_concurrency = |value: i64| {
            conn.execute(
                "UPDATE accounts SET max_concurrent_sessions = ?1 WHERE id = ?2",
                params![value, account.id],
            )
        };
        assert!(
            set_concurrency(i64::from(u32::MAX)).is_ok(),
            "the u32 maximum is a legal concurrency ceiling"
        );
        assert!(
            set_concurrency(i64::from(u32::MAX) + 1).is_err(),
            "one above the u32 maximum must be refused"
        );
        update_account_playback_policy(&conn, account.id, None, None, None).unwrap();
    }

    #[test]
    fn two_usernames_differing_only_in_case_cannot_both_exist() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        create_account_with_profile(&conn, "Root", "hash", "owner", "Main", "ref0").unwrap();
        for typed in ["root", "ROOT", "rOoT"] {
            assert!(
                create_account_with_profile(&conn, typed, "hash", "member", "Other", "ref1")
                    .is_err(),
                "{typed} must collide with the stored Root"
            );
        }
        assert_eq!(list_accounts(&conn).unwrap().len(), 1);
        let profiles: i64 = conn
            .query_row("SELECT COUNT(*) FROM profiles", [], |r| r.get(0))
            .unwrap();
        assert_eq!(profiles, 1, "no orphan profile from the refused inserts");
    }

    /// The login half. A case slip used to be rejected with the same message a
    /// wrong password gets (ADR-0034 item 5 makes the two indistinguishable on
    /// purpose), so it was unreportable as well as wrong.
    #[test]
    fn a_login_finds_its_account_whatever_case_is_typed() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        create_account_with_profile(&conn, "Root", "hash", "owner", "Main", "ref0").unwrap();
        for typed in ["Root", "root", "ROOT", "rOoT"] {
            let found = account_by_username(&conn, typed)
                .unwrap()
                .unwrap_or_else(|| panic!("{typed} must find the account"));
            // **The stored spelling comes back, not the typed one.** Only the
            // comparison is case-blind; the row is unchanged.
            assert_eq!(found.username, "Root");
        }
        assert!(
            account_by_username(&conn, "rooot").unwrap().is_none(),
            "case-insensitive is not fuzzy"
        );
    }

    /// A name that is not a collision must still be its own account, or the
    /// index would be doing more than it was asked to.
    #[test]
    fn distinct_usernames_are_unaffected() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        create_account_with_profile(&conn, "Root", "hash", "owner", "Main", "ref0").unwrap();
        create_account_with_profile(&conn, "rooter", "hash", "member", "Other", "ref1").unwrap();
        assert_eq!(list_accounts(&conn).unwrap().len(), 2);
    }

    /// profile insert. This asserts the ordering; `a_failed_profile_insert_
    /// rolls_back_the_account` is the one that exercises the rollback.
    #[test]
    fn a_duplicate_username_leaves_nothing_behind() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        create_account_with_profile(&conn, "a", "hash", "owner", "Main", "ref0").unwrap();
        assert!(
            create_account_with_profile(&conn, "a", "hash", "member", "Other", "ref1").is_err()
        );
        assert_eq!(list_accounts(&conn).unwrap().len(), 1);
        let profiles: i64 = conn
            .query_row("SELECT COUNT(*) FROM profiles", [], |r| r.get(0))
            .unwrap();
        assert_eq!(profiles, 1, "no orphan profile from the failed insert");
    }

    /// The transaction's actual job: the account insert succeeds, the profile
    /// insert then fails, and neither survives. Without the rollback this
    /// leaves an account that can never write watch state, which is the state
    /// ADR-0034 item 3 creates the profile in the same transaction to avoid.
    #[test]
    fn a_failed_profile_insert_rolls_back_the_account() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        create_account_with_profile(&conn, "first", "h", "owner", "P", "taken").unwrap();

        // `profile_ref` is UNIQUE, so this fails after the account is inserted.
        let second = create_account_with_profile(&conn, "second", "h", "member", "P", "taken");
        assert!(second.is_err(), "duplicate profile_ref must fail");

        let names: Vec<String> = list_accounts(&conn)
            .unwrap()
            .into_iter()
            .map(|a| a.username)
            .collect();
        assert_eq!(
            names,
            vec!["first".to_string()],
            "the half-created account must not survive its failed profile"
        );
    }

    #[test]
    fn setup_facts_are_independent() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        assert!(!admin_exists(&conn).unwrap());
        assert!(!library_exists(&conn).unwrap());

        // The dogfood state: libraries present, no account.
        conn.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('t', '/t', 'movies');",
        )
        .unwrap();
        assert!(!admin_exists(&conn).unwrap());
        assert!(library_exists(&conn).unwrap());

        // A member is not an admin, so the fact tracks authority not existence.
        create_account_with_profile(&conn, "m", "h", "member", "P", "r0").unwrap();
        assert!(account_exists(&conn).unwrap());
        assert!(!admin_exists(&conn).unwrap());

        create_account_with_profile(&conn, "o", "h", "owner", "P", "r1").unwrap();
        assert!(admin_exists(&conn).unwrap());
    }

    fn seeded() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             INSERT INTO accounts (id, username, password_hash, role)
                  VALUES (1, 'a', 'x', 'owner');
             INSERT INTO profiles (id, account_id, profile_ref, name)
                  VALUES (1, 1, 'aaaa', 'kid');
             INSERT INTO login_sessions
                  (account_id, active_profile_id, token_sha256, client_label, expires_at)
             VALUES (1, 1, 'live', 'tv', '2099-01-01T00:00:00.000Z'),
                    (1, NULL, 'stale', 'old', '2020-01-01T00:00:00.000Z');
             UPDATE login_sessions SET revoked_at = '2026-01-01T00:00:00.000Z'
              WHERE token_sha256 = 'stale';
             INSERT INTO login_sessions
                  (account_id, active_profile_id, token_sha256, client_label, expires_at)
             VALUES (1, NULL, 'old', 'browser', '2020-01-01T00:00:00.000Z');",
        )
        .unwrap();
        conn
    }

    #[test]
    fn each_rejection_is_reachable_through_the_query() {
        let conn = seeded();
        let live = session_for_token(&conn, "live", NOW).unwrap().unwrap();
        assert_eq!(live.role, "owner");
        assert_eq!(live.active_profile_id, Some(1));

        assert_eq!(
            session_for_token(&conn, "stale", NOW).unwrap(),
            Err(SessionRejection::Revoked)
        );
        assert_eq!(
            session_for_token(&conn, "old", NOW).unwrap(),
            Err(SessionRejection::Expired)
        );
        assert_eq!(
            session_for_token(&conn, "never-issued", NOW).unwrap(),
            Err(SessionRejection::Unknown)
        );
    }

    /// The documented consequence of migration 020's `ON DELETE CASCADE`, kept
    /// honest by a test rather than a comment: deleting a profile makes its
    /// session read `Unknown`, not `Revoked`. If that ever becomes `Revoked`
    /// the doc comment on `SessionRejection` is wrong and this fails.
    #[test]
    fn a_deleted_profile_leaves_unknown_not_revoked() {
        let conn = seeded();
        assert!(session_for_token(&conn, "live", NOW).unwrap().is_ok());

        conn.execute("DELETE FROM profiles WHERE id = 1", [])
            .unwrap();

        assert_eq!(
            session_for_token(&conn, "live", NOW).unwrap(),
            Err(SessionRejection::Unknown),
            "cascade destroys the row, so the token is unknown rather than revoked"
        );
        // And the account-scope sessions are untouched by a profile delete.
        assert_eq!(
            session_for_token(&conn, "old", NOW).unwrap(),
            Err(SessionRejection::Expired)
        );
    }
}
