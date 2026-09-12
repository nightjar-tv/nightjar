//! Durable watch state (ADR-0035 item 1). One mutable row per
//! `(profile_id, item_key)`.
//!
//! These are the table's read and write primitives. The one writer that
//! combines them with validation, thresholds and the server clock lives in
//! `nightjar-metadata`, beside the item-key grammar the row is keyed by.

use rusqlite::{Connection, OptionalExtension, params};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchStateRow {
    pub profile_id: i64,
    pub item_key: String,
    pub position_ms: i64,
    pub duration_ms: i64,
    pub played: bool,
    pub hidden: bool,
    pub first_played_at: String,
    pub last_played_at: String,
}

pub fn load_watch_state(
    conn: &Connection,
    profile_id: i64,
    item_key: &str,
) -> Result<Option<WatchStateRow>, String> {
    conn.query_row(
        "SELECT profile_id, item_key, position_ms, duration_ms, played, hidden,
                first_played_at, last_played_at
         FROM watch_state WHERE profile_id = ?1 AND item_key = ?2",
        params![profile_id, item_key],
        |r| {
            Ok(WatchStateRow {
                profile_id: r.get(0)?,
                item_key: r.get(1)?,
                position_ms: r.get(2)?,
                duration_ms: r.get(3)?,
                played: r.get::<_, i64>(4)? != 0,
                hidden: r.get::<_, i64>(5)? != 0,
                first_played_at: r.get(6)?,
                last_played_at: r.get(7)?,
            })
        },
    )
    .optional()
    .map_err(|e| format!("load watch state: {e}"))
}

/// Insert or overwrite one row.
///
/// `first_played_at` is set on insert and preserved on update, because the
/// amendment makes it the time of the first qualifying write. `hidden` is not
/// in the update list either: this route does not write it, and clearing a
/// value a later block owns would be a silent data loss (ADR-0035 amendment
/// items 1 and 5).
pub fn upsert_watch_state(
    conn: &Connection,
    profile_id: i64,
    item_key: &str,
    position_ms: i64,
    duration_ms: i64,
    played: bool,
    now: &str,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO watch_state
            (profile_id, item_key, position_ms, duration_ms, played, hidden,
             first_played_at, last_played_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?6)
         ON CONFLICT(profile_id, item_key) DO UPDATE SET
            position_ms = excluded.position_ms,
            duration_ms = excluded.duration_ms,
            played = excluded.played,
            last_played_at = excluded.last_played_at",
        params![
            profile_id,
            item_key,
            position_ms,
            duration_ms,
            played as i32,
            now
        ],
    )
    .map_err(|e| format!("upsert watch state: {e}"))?;
    Ok(())
}

pub fn delete_watch_state(
    conn: &Connection,
    profile_id: i64,
    item_key: &str,
) -> Result<(), String> {
    conn.execute(
        "DELETE FROM watch_state WHERE profile_id = ?1 AND item_key = ?2",
        params![profile_id, item_key],
    )
    .map_err(|e| format!("delete watch state: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate;

    const NOW: &str = "2026-09-12T00:00:00.000Z";
    const LATER: &str = "2026-09-12T00:10:00.000Z";

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO accounts (id, username, password_hash, role)
                 VALUES (1, 'a', 'x', 'owner');
             INSERT INTO profiles (id, account_id, profile_ref, name)
                 VALUES (1, 1, 'aa', 'P'), (2, 1, 'bb', 'Q');",
        )
        .unwrap();
        c
    }

    #[test]
    fn insert_then_update_preserves_the_first_play_time() {
        let c = conn();
        upsert_watch_state(&c, 1, "tmdb:movie:1", 1_000, 10_000, false, NOW).unwrap();
        let first = load_watch_state(&c, 1, "tmdb:movie:1").unwrap().unwrap();
        assert_eq!(first.first_played_at, NOW);
        assert_eq!(first.last_played_at, NOW);
        assert!(!first.played);
        assert!(!first.hidden);

        upsert_watch_state(&c, 1, "tmdb:movie:1", 9_500, 10_000, true, LATER).unwrap();
        let updated = load_watch_state(&c, 1, "tmdb:movie:1").unwrap().unwrap();
        assert_eq!(updated.first_played_at, NOW, "first play time is preserved");
        assert_eq!(updated.last_played_at, LATER);
        assert_eq!(updated.position_ms, 9_500);
        assert!(updated.played);
    }

    /// `hidden` belongs to a later block. An update from this route must not
    /// clear a value that block sets.
    #[test]
    fn update_preserves_a_hidden_flag() {
        let c = conn();
        upsert_watch_state(&c, 1, "tmdb:movie:1", 1_000, 10_000, false, NOW).unwrap();
        c.execute(
            "UPDATE watch_state SET hidden = 1 WHERE profile_id = 1 AND item_key = 'tmdb:movie:1'",
            [],
        )
        .unwrap();

        upsert_watch_state(&c, 1, "tmdb:movie:1", 5_000, 10_000, false, LATER).unwrap();
        let row = load_watch_state(&c, 1, "tmdb:movie:1").unwrap().unwrap();
        assert!(row.hidden, "the update must not clear hidden");
    }

    #[test]
    fn delete_removes_only_the_named_profile_row() {
        let c = conn();
        upsert_watch_state(&c, 1, "tmdb:movie:1", 1_000, 10_000, false, NOW).unwrap();
        upsert_watch_state(&c, 2, "tmdb:movie:1", 1_000, 10_000, false, NOW).unwrap();

        delete_watch_state(&c, 1, "tmdb:movie:1").unwrap();
        assert_eq!(load_watch_state(&c, 1, "tmdb:movie:1").unwrap(), None);
        assert!(
            load_watch_state(&c, 2, "tmdb:movie:1").unwrap().is_some(),
            "the sibling profile keeps its own row"
        );
    }

    /// The Gate 3 claim: state survives a server restart, which is a reopen of
    /// the on-disk database rather than an in-memory one.
    #[test]
    fn watch_state_survives_reopening_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nightjar.db");
        {
            let db = crate::Db::open(&path).unwrap();
            db.with_conn(|conn| {
                conn.execute_batch(
                    "INSERT INTO accounts (id, username, password_hash, role)
                         VALUES (1, 'a', 'x', 'owner');
                     INSERT INTO profiles (id, account_id, profile_ref, name)
                         VALUES (1, 1, 'aa', 'P');",
                )
                .map_err(|e| e.to_string())?;
                upsert_watch_state(conn, 1, "tmdb:movie:1", 4_200, 10_000, false, NOW)
            })
            .unwrap();
        }

        let reopened = crate::Db::open(&path).unwrap();
        let row = reopened
            .with_conn(|conn| load_watch_state(conn, 1, "tmdb:movie:1"))
            .unwrap()
            .expect("the row survives the reopen");
        assert_eq!(row.position_ms, 4_200);
        assert_eq!(row.duration_ms, 10_000);
        assert_eq!(row.first_played_at, NOW);
    }
}
