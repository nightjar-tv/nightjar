//! Key migrators: ADR-0025 §5 for `item_key`, ADR-0039 item 7 for
//! `series_key`.
//!
//! Both no-op while their tables do not exist yet (Block 2). Every caller runs
//! them anyway so the path is one (Rule 4.11) when those tables land. Two
//! functions rather than one because they rewrite different tables — a movie
//! assign runs both over the same value, which is exactly the case that would
//! make merging them look reasonable and would then hide which table each rule
//! applies to.

use rusqlite::{Connection, OptionalExtension, params};

/// Rewrite `series_key` on every table keyed by it (ADR-0039 item 7).
///
/// **The tables this rewrites do not exist yet, and the list is here rather
/// than at each future call site so there is one place to add them.**
/// `profile_track_choice` arrives with B2-8 and the kids override table with
/// B2-6. Until then this is reached, finds nothing, and reports that it found
/// nothing — which is different from not being called.
///
/// **Merge on collision, which is the common case rather than the exotic one.**
/// Two folders can bind the same show (a split library, or extras in a sibling
/// directory), so two keys arrive at one and collide on
/// `(profile_id, series_key)`:
///
/// - `profile_track_choice`: newer `updated_at` wins. There is no ratio to
///   compare the way ADR-0025 §5 compares position, and a preference has no
///   partial state.
/// - Kids overrides: the **more restrictive** entry survives. That is ADR-0037
///   item 6's precedence — blocked, then allowed, then the ladder — applied to
///   a merge rather than a second ordering invented here.
///
/// **Unbind does not call this.** ADR-0028 clear-match returns a folder to a
/// `folder:` key and the rows stay on `tmdb:show:{id}`, because rewriting them
/// would be wrong while a second folder is still bound to that show, and
/// re-binding the same folder reattaches them for free.
pub fn migrate_series_keys(
    conn: &Connection,
    old_key: &str,
    new_key: &str,
) -> Result<SeriesMigrateReport, String> {
    if old_key == new_key {
        return Ok(SeriesMigrateReport::default());
    }
    let present: Vec<&str> = SERIES_KEYED_TABLES
        .iter()
        .copied()
        .filter(|t| table_exists(conn, t).unwrap_or(false))
        .collect();
    if let Some(table) = present.first() {
        // Unreachable until B2-6 and B2-8 add the tables, and it fails loudly
        // rather than silently doing nothing. The merge rules above are what a
        // writer implements here; writing them now, against a schema that does
        // not exist, would be inventing the schema (Rule 4.9).
        return Err(format!(
            "series_key migration for {table} is unimplemented: the table exists, \
             so the merge rule in this function's documentation now has to be written"
        ));
    }
    Ok(SeriesMigrateReport {
        tables_present: present.len(),
    })
}

/// Every table keyed on `series_key`. Add here, not at a call site.
const SERIES_KEYED_TABLES: &[&str] = &["profile_track_choice", "kids_overrides"];

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SeriesMigrateReport {
    /// How many of [`SERIES_KEYED_TABLES`] were present. Zero is the expected
    /// value today and is reported rather than assumed, so a caller can tell
    /// "nothing to move" from "never ran".
    pub tables_present: usize,
}

/// Rewrite `item_key` on watch/playback tables from `old_keys` → `new_key`.
///
/// Merge when both old and new already have a watch row (ADR-0025 §5):
/// higher relative position wins, then played, then newer `last_played_at`.
pub fn migrate_item_keys(
    conn: &Connection,
    old_keys: &[String],
    new_key: &str,
) -> Result<MigrateReport, String> {
    if old_keys.is_empty() {
        return Ok(MigrateReport::default());
    }
    if !table_exists(conn, "watch_state")? {
        return Ok(MigrateReport {
            tables_present: false,
            ..Default::default()
        });
    }
    let mut report = MigrateReport {
        tables_present: true,
        ..Default::default()
    };
    for old in old_keys {
        if old == new_key {
            continue;
        }
        report.watch_rewrites += migrate_watch_row(conn, old, new_key)?;
        if table_exists(conn, "playback_events")? {
            report.event_rewrites += migrate_events(conn, old, new_key)?;
        }
    }
    Ok(report)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MigrateReport {
    pub tables_present: bool,
    pub watch_rewrites: usize,
    pub event_rewrites: usize,
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool, String> {
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![name],
            |r| r.get(0),
        )
        .map_err(|e| format!("table_exists {name}: {e}"))?;
    Ok(n > 0)
}

fn migrate_watch_row(conn: &Connection, old_key: &str, new_key: &str) -> Result<usize, String> {
    let old_row = load_watch(conn, old_key)?;
    let Some(old) = old_row else {
        return Ok(0);
    };
    let new_row = load_watch(conn, new_key)?;
    match new_row {
        None => {
            conn.execute(
                "UPDATE watch_state SET item_key = ?1 WHERE item_key = ?2",
                params![new_key, old_key],
            )
            .map_err(|e| format!("rewrite watch_state: {e}"))?;
            Ok(1)
        }
        Some(new) => {
            let keep_old = prefer_old(&old, &new);
            if keep_old {
                conn.execute(
                    "DELETE FROM watch_state WHERE item_key = ?1",
                    params![new_key],
                )
                .map_err(|e| format!("delete new watch: {e}"))?;
                conn.execute(
                    "UPDATE watch_state SET item_key = ?1 WHERE item_key = ?2",
                    params![new_key, old_key],
                )
                .map_err(|e| format!("promote old watch: {e}"))?;
            } else {
                conn.execute(
                    "DELETE FROM watch_state WHERE item_key = ?1",
                    params![old_key],
                )
                .map_err(|e| format!("drop old watch: {e}"))?;
            }
            Ok(1)
        }
    }
}

#[derive(Debug)]
struct WatchRow {
    position_ms: i64,
    duration_ms: Option<i64>,
    played: bool,
    last_played_at: String,
}

fn load_watch(conn: &Connection, key: &str) -> Result<Option<WatchRow>, String> {
    // Schema may use different column names once Block 2 lands; tolerate
    // missing columns by probing pragma.
    if !columns_include(conn, "watch_state", "item_key")? {
        return Ok(None);
    }
    let has_duration = columns_include(conn, "watch_state", "duration_ms")?;
    let has_played = columns_include(conn, "watch_state", "played")?;
    let has_pos = columns_include(conn, "watch_state", "position_ms")?;
    let has_last = columns_include(conn, "watch_state", "last_played_at")?;
    if !has_pos || !has_last {
        // Minimal rewrite only.
        let exists: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM watch_state WHERE item_key = ?1 LIMIT 1",
                params![key],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| format!("probe watch: {e}"))?;
        if exists.is_some() {
            return Ok(Some(WatchRow {
                position_ms: 0,
                duration_ms: None,
                played: false,
                last_played_at: String::new(),
            }));
        }
        return Ok(None);
    }
    let sql = format!(
        "SELECT position_ms, {}, {}, last_played_at FROM watch_state WHERE item_key = ?1 LIMIT 1",
        if has_duration { "duration_ms" } else { "NULL" },
        if has_played { "played" } else { "0" }
    );
    conn.query_row(&sql, params![key], |r| {
        Ok(WatchRow {
            position_ms: r.get(0)?,
            duration_ms: r.get(1)?,
            played: r.get::<_, i64>(2).unwrap_or(0) != 0,
            last_played_at: r.get(3)?,
        })
    })
    .optional()
    .map_err(|e| format!("load watch {key}: {e}"))
}

fn columns_include(conn: &Connection, table: &str, col: &str) -> Result<bool, String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| format!("pragma table_info: {e}"))?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .map_err(|e| format!("pragma cols: {e}"))?;
    for row in rows {
        if row.map_err(|e| e.to_string())? == col {
            return Ok(true);
        }
    }
    Ok(false)
}

fn prefer_old(old: &WatchRow, new: &WatchRow) -> bool {
    let old_frac = relative_position(old);
    let new_frac = relative_position(new);
    match old_frac.partial_cmp(&new_frac) {
        Some(std::cmp::Ordering::Greater) => true,
        Some(std::cmp::Ordering::Less) => false,
        _ => {
            if old.played != new.played {
                old.played
            } else {
                old.last_played_at >= new.last_played_at
            }
        }
    }
}

fn relative_position(w: &WatchRow) -> f64 {
    match w.duration_ms.filter(|d| *d > 0) {
        Some(d) => w.position_ms as f64 / d as f64,
        None => w.position_ms as f64,
    }
}

fn migrate_events(conn: &Connection, old_key: &str, new_key: &str) -> Result<usize, String> {
    if !columns_include(conn, "playback_events", "item_key")? {
        return Ok(0);
    }
    let n = conn
        .execute(
            "UPDATE playback_events SET item_key = ?1 WHERE item_key = ?2",
            params![new_key, old_key],
        )
        .map_err(|e| format!("rewrite playback_events: {e}"))?;
    Ok(n)
}

#[cfg(test)]
mod tests {

    /// ADR-0039 item 7. The tables land with B2-6 and B2-8, so today this
    /// finds nothing — and reports that it found nothing, which is a different
    /// fact from never having been called. Without the count, a future reader
    /// cannot tell a working migrator from an unwired one.
    #[test]
    fn series_key_migration_reports_that_no_keyed_table_exists_yet() {
        let c = Connection::open_in_memory().unwrap();
        let r = migrate_series_keys(&c, "folder:1:Alpha", "tmdb:show:55").unwrap();
        assert_eq!(r.tables_present, 0);
    }

    /// A bind that does not change the key is not a migration. This is the
    /// common case on a re-match to the same show, and it must not be counted
    /// or logged as a rewrite.
    #[test]
    fn series_key_migration_is_a_no_op_when_the_key_is_unchanged() {
        let c = Connection::open_in_memory().unwrap();
        let r = migrate_series_keys(&c, "tmdb:show:55", "tmdb:show:55").unwrap();
        assert_eq!(r, SeriesMigrateReport::default());
    }
    use super::*;
    use nightjar_db::migrate;

    #[test]
    fn no_op_without_watch_table() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        let r = migrate_item_keys(&c, &["path:1:a.mkv".into()], "tmdb:movie:1").unwrap();
        assert!(!r.tables_present);
    }

    #[test]
    fn rewrites_simple_watch_row() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "CREATE TABLE watch_state (
                profile_id INTEGER NOT NULL,
                item_key TEXT NOT NULL,
                position_ms INTEGER NOT NULL,
                duration_ms INTEGER,
                played INTEGER NOT NULL DEFAULT 0,
                last_played_at TEXT NOT NULL,
                PRIMARY KEY (profile_id, item_key)
             );
             INSERT INTO watch_state VALUES (1, 'path:1:a.mkv', 1000, 10000, 0, '2026-01-01T00:00:00Z');",
        )
        .unwrap();
        let r = migrate_item_keys(&c, &["path:1:a.mkv".into()], "tmdb:movie:9").unwrap();
        assert!(r.tables_present);
        assert_eq!(r.watch_rewrites, 1);
        let key: String = c
            .query_row("SELECT item_key FROM watch_state", [], |row| row.get(0))
            .unwrap();
        assert_eq!(key, "tmdb:movie:9");
    }

    #[test]
    fn merge_keeps_higher_fraction() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "CREATE TABLE watch_state (
                profile_id INTEGER NOT NULL,
                item_key TEXT NOT NULL,
                position_ms INTEGER NOT NULL,
                duration_ms INTEGER,
                played INTEGER NOT NULL DEFAULT 0,
                last_played_at TEXT NOT NULL,
                PRIMARY KEY (profile_id, item_key)
             );
             INSERT INTO watch_state VALUES
               (1, 'old', 9000, 10000, 0, '2026-01-01T00:00:00Z'),
               (1, 'new', 1000, 10000, 0, '2026-02-01T00:00:00Z');",
        )
        .unwrap();
        migrate_item_keys(&c, &["old".into()], "new").unwrap();
        let pos: i64 = c
            .query_row(
                "SELECT position_ms FROM watch_state WHERE item_key = 'new'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pos, 9000);
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM watch_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }
}
