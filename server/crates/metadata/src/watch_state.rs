//! Durable watch state (ADR-0035). The row is keyed by item identity, so the
//! key grammar and the ADR-0025 §5 key-merge migrator already live in this
//! crate, and the one writer sits beside them.

use crate::item_links::item_key_resolves;
use nightjar_core::{WatchReport, watch_report};
use nightjar_db::{
    WatchStateRow, delete_watch_state, load_watch_state, now_iso, upsert_watch_state, write_tx,
};
use rusqlite::Connection;

/// What one accepted report did to the row.
#[derive(Debug, PartialEq, Eq)]
pub enum WatchWriteOutcome {
    /// Below the resume floor. Any existing row was removed.
    Cleared,
    /// At or above the floor. The stored row.
    Stored(WatchStateRow),
}

#[derive(Debug, PartialEq, Eq)]
pub enum WatchError {
    /// Well formed JSON whose values the policy refuses.
    Invalid(&'static str),
    /// The key names no item through the effective identity layer.
    UnresolvedKey,
    Db(String),
}

/// The one writer (ADR-0035 item 5). Validation, key resolution, threshold
/// derivation, the server clock and the upsert happen in one transaction, so a
/// refused key leaves no row and a failed upsert leaves no partial state.
pub fn write_watch_report(
    conn: &Connection,
    profile_id: i64,
    item_key: &str,
    position_ms: i64,
    duration_ms: i64,
) -> Result<WatchWriteOutcome, WatchError> {
    let report = watch_report(position_ms, duration_ms).ok_or(WatchError::Invalid(
        "positionMs and durationMs must be a position within a positive duration",
    ))?;

    // `write_tx` takes the write lock before the first read. The key check
    // below reads before the upsert writes, so a deferred transaction could
    // take a stale snapshot (see `nightjar_db::write_tx`).
    let tx = write_tx(conn).map_err(WatchError::Db)?;
    if !item_key_resolves(&tx, item_key).map_err(WatchError::Db)? {
        return Err(WatchError::UnresolvedKey);
    }

    let now = now_iso(&tx).map_err(WatchError::Db)?;
    let outcome = match report {
        WatchReport::Clear => {
            delete_watch_state(&tx, profile_id, item_key).map_err(WatchError::Db)?;
            WatchWriteOutcome::Cleared
        }
        WatchReport::Keep { played } => {
            upsert_watch_state(
                &tx,
                profile_id,
                item_key,
                position_ms,
                duration_ms,
                played,
                &now,
            )
            .map_err(WatchError::Db)?;
            let row = load_watch_state(&tx, profile_id, item_key)
                .map_err(WatchError::Db)?
                .ok_or_else(|| WatchError::Db("watch state vanished after upsert".to_string()))?;
            WatchWriteOutcome::Stored(row)
        }
    };
    tx.commit().map_err(|e| WatchError::Db(e.to_string()))?;
    Ok(outcome)
}

/// Read one profile's state for a resolved key.
///
/// `Ok(None)` is a real item with no state, which the API reports distinctly
/// from a key that names nothing (ADR-0035 amendment item 2).
pub fn read_watch_state(
    conn: &Connection,
    profile_id: i64,
    item_key: &str,
) -> Result<Option<WatchStateRow>, WatchError> {
    if !item_key_resolves(conn, item_key).map_err(WatchError::Db)? {
        return Err(WatchError::UnresolvedKey);
    }
    load_watch_state(conn, profile_id, item_key).map_err(WatchError::Db)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightjar_db::{NewLibrary, UpsertItem};

    const KEY: &str = "path:1:a.mkv";

    /// A migrated database with one profile and one real item whose path key
    /// resolves.
    fn fixture() -> nightjar_db::Db {
        let db = nightjar_db::Db::open(std::path::Path::new(":memory:")).unwrap();
        db.with_conn(|conn| {
            conn.execute_batch(
                "INSERT INTO accounts (id, username, password_hash, role)
                     VALUES (1, 'a', 'x', 'owner');
                 INSERT INTO profiles (id, account_id, profile_ref, name)
                     VALUES (1, 1, 'aa', 'P');",
            )
            .map_err(|e| e.to_string())?;
            Ok(())
        })
        .unwrap();
        let library = db
            .create_library(&NewLibrary {
                name: "movies".to_string(),
                path: "/media/movies".to_string(),
                kind: "movies".to_string(),
            })
            .unwrap();
        db.upsert_items_indexed(
            library.id,
            &[UpsertItem {
                path: "a.mkv".to_string(),
                mtime_ms: 0,
                size_bytes: 1,
                title: "A".to_string(),
                kind: "movie".to_string(),
                year: None,
                season: None,
                episode: None,
                content_id: None,
            }],
        )
        .unwrap();
        db
    }

    fn write(db: &nightjar_db::Db, position_ms: i64, duration_ms: i64) -> WatchWriteOutcome {
        db.with_conn(|conn| Ok(write_watch_report(conn, 1, KEY, position_ms, duration_ms)))
            .unwrap()
            .unwrap()
    }

    /// Item 3: last-write-wins, not highest-position-wins. A reverse seek
    /// overwrites the newer higher position.
    #[test]
    fn a_later_lower_position_overwrites_the_higher_one() {
        let db = fixture();
        let WatchWriteOutcome::Stored(high) = write(&db, 8_000, 10_000) else {
            panic!("80% keeps state");
        };
        assert_eq!(high.position_ms, 8_000);

        let WatchWriteOutcome::Stored(low) = write(&db, 1_000, 10_000) else {
            panic!("10% keeps state");
        };
        assert_eq!(low.position_ms, 1_000, "the later report wins");
        assert_eq!(low.duration_ms, 10_000, "duration is the latest snapshot");
    }

    /// The rewatch case: a played item reported below 90% clears `played`,
    /// while `firstPlayedAt` stays where it was.
    #[test]
    fn a_rewatch_below_the_ceiling_clears_played() {
        let db = fixture();
        let WatchWriteOutcome::Stored(played) = write(&db, 9_500, 10_000) else {
            panic!("95% keeps state");
        };
        assert!(played.played);
        let first = played.first_played_at.clone();

        let WatchWriteOutcome::Stored(rewatched) = write(&db, 3_000, 10_000) else {
            panic!("30% keeps state");
        };
        assert!(!rewatched.played, "below 90% clears played");
        assert_eq!(
            rewatched.first_played_at, first,
            "the first qualifying write is preserved"
        );
    }

    /// Below 2% removes state. The positive control is the 2.1% write that
    /// leaves a row, so the test cannot pass by deleting everything.
    #[test]
    fn below_the_floor_clears_state_and_the_floor_keeps_it() {
        let db = fixture();
        write(&db, 5_000, 10_000);
        assert_eq!(write(&db, 199, 10_000), WatchWriteOutcome::Cleared);
        let gone = db.with_conn(|conn| load_watch_state(conn, 1, KEY)).unwrap();
        assert_eq!(gone, None, "an accidental open leaves no resume point");

        assert!(matches!(
            write(&db, 210, 10_000),
            WatchWriteOutcome::Stored(_)
        ));
        let kept = db
            .with_conn(|conn| load_watch_state(conn, 1, KEY))
            .unwrap()
            .expect("2.1% keeps a resume point");
        assert_eq!(kept.position_ms, 210);
    }

    #[test]
    fn invalid_reports_are_refused_and_write_nothing() {
        let db = fixture();
        for (position, duration) in [(1_000, 0), (-1, 10_000), (10_001, 10_000)] {
            let result = db
                .with_conn(|conn| Ok(write_watch_report(conn, 1, KEY, position, duration)))
                .unwrap();
            assert!(
                matches!(result, Err(WatchError::Invalid(_))),
                "{position}/{duration} must be invalid, got {result:?}"
            );
        }
        let count = db
            .with_conn(|conn| {
                conn.query_row("SELECT COUNT(*) FROM watch_state", [], |r| {
                    r.get::<_, i64>(0)
                })
                .map_err(|e| e.to_string())
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    /// An unresolved key is a 404 and writes nothing, so no orphan row exists
    /// for an item the server cannot name.
    #[test]
    fn an_unresolved_key_is_refused_and_writes_nothing() {
        let db = fixture();
        let result = db
            .with_conn(|conn| {
                Ok(write_watch_report(
                    conn,
                    1,
                    "path:1:missing.mkv",
                    1_000,
                    10_000,
                ))
            })
            .unwrap();
        assert_eq!(result, Err(WatchError::UnresolvedKey));

        let result = db
            .with_conn(|conn| Ok(read_watch_state(conn, 1, "path:1:missing.mkv")))
            .unwrap();
        assert_eq!(result, Err(WatchError::UnresolvedKey));

        let count = db
            .with_conn(|conn| {
                conn.query_row("SELECT COUNT(*) FROM watch_state", [], |r| {
                    r.get::<_, i64>(0)
                })
                .map_err(|e| e.to_string())
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    /// A resolved key with no row reads as `Ok(None)`, which the API reports
    /// as `{state: null}` rather than a 404.
    #[test]
    fn a_resolved_key_with_no_state_reads_as_none() {
        let db = fixture();
        let row = db
            .with_conn(|conn| Ok(read_watch_state(conn, 1, KEY)))
            .unwrap()
            .unwrap();
        assert_eq!(row, None);
    }
}
