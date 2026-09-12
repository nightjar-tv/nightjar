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
    // ADR-0025 §5 merges "if both old and new keys already have a watch row
    // for a profile". Watch state is keyed per profile, so the rewrite and the
    // merge are per profile too; a single-row load would move one profile's
    // row and leave the rest under the old key.
    let mut moved = 0;
    for profile_id in profiles_with_watch_key(conn, old_key)? {
        let Some(old) = load_watch(conn, profile_id, old_key)? else {
            continue;
        };
        match load_watch(conn, profile_id, new_key)? {
            None => {
                conn.execute(
                    "UPDATE watch_state SET item_key = ?1 WHERE profile_id = ?2 AND item_key = ?3",
                    params![new_key, profile_id, old_key],
                )
                .map_err(|e| format!("rewrite watch_state: {e}"))?;
            }
            Some(new) => {
                let keep_old = prefer_old(&old, &new);
                if keep_old {
                    conn.execute(
                        "DELETE FROM watch_state WHERE profile_id = ?1 AND item_key = ?2",
                        params![profile_id, new_key],
                    )
                    .map_err(|e| format!("delete new watch: {e}"))?;
                    conn.execute(
                        "UPDATE watch_state SET item_key = ?1 WHERE profile_id = ?2 AND item_key = ?3",
                        params![new_key, profile_id, old_key],
                    )
                    .map_err(|e| format!("promote old watch: {e}"))?;
                } else {
                    conn.execute(
                        "DELETE FROM watch_state WHERE profile_id = ?1 AND item_key = ?2",
                        params![profile_id, old_key],
                    )
                    .map_err(|e| format!("drop old watch: {e}"))?;
                }
            }
        }
        moved += 1;
    }
    Ok(moved)
}

/// The profiles holding a row under `key`. Empty when the table has no
/// `profile_id` column, which is the shape guard the rest of this module uses.
fn profiles_with_watch_key(conn: &Connection, key: &str) -> Result<Vec<i64>, String> {
    if !columns_include(conn, "watch_state", "profile_id")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn
        .prepare("SELECT profile_id FROM watch_state WHERE item_key = ?1")
        .map_err(|e| format!("prepare watch profiles: {e}"))?;
    let rows = stmt
        .query_map(params![key], |r| r.get(0))
        .map_err(|e| format!("query watch profiles: {e}"))?;
    rows.collect::<Result<Vec<i64>, _>>()
        .map_err(|e| format!("watch profile row: {e}"))
}

#[derive(Debug)]
struct WatchRow {
    position_ms: i64,
    duration_ms: Option<i64>,
    played: bool,
    last_played_at: String,
}

fn load_watch(conn: &Connection, profile_id: i64, key: &str) -> Result<Option<WatchRow>, String> {
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
                "SELECT 1 FROM watch_state WHERE profile_id = ?1 AND item_key = ?2 LIMIT 1",
                params![profile_id, key],
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
        "SELECT position_ms, {}, {}, last_played_at FROM watch_state
         WHERE profile_id = ?1 AND item_key = ?2 LIMIT 1",
        if has_duration { "duration_ms" } else { "NULL" },
        if has_played { "played" } else { "0" }
    );
    conn.query_row(&sql, params![profile_id, key], |r| {
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
    use super::*;

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

    /// The other half of the contract: a declared table that exists without its
    /// merge rule is an error, not a silent no-op. A migrator that returned
    /// `Ok` here would leave a future writer's rows unrewritten and say
    /// nothing.
    #[test]
    fn series_key_migration_refuses_a_present_table_with_no_merge_rule() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE profile_track_choice (profile_id INTEGER, series_key TEXT)")
            .unwrap();
        let err = migrate_series_keys(&c, "folder:1:Alpha", "tmdb:show:55").unwrap_err();
        assert!(err.contains("profile_track_choice"), "{err}");
    }

    /// ADR-0039 item 7: a movie assign changes the item key and the series key
    /// at once, so both migrators run over the same value. They rewrite
    /// different tables and must compose without either seeing the other's
    /// rows.
    #[test]
    fn item_and_series_migrators_compose_over_one_value() {
        let c = Connection::open_in_memory().unwrap();
        nightjar_db::migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO accounts (id, username, password_hash, role)
                 VALUES (1, 'a', 'x', 'owner');
             INSERT INTO profiles (id, account_id, profile_ref, name)
                 VALUES (1, 1, 'aa', 'P');
             INSERT INTO watch_state
                 (profile_id, item_key, position_ms, duration_ms, played, hidden,
                  first_played_at, last_played_at)
             VALUES
                 (1, 'path:1:a.mkv', 9000, 10000, 0, 0,
                  '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z');",
        )
        .unwrap();

        let item = migrate_item_keys(&c, &["path:1:a.mkv".into()], "tmdb:movie:9").unwrap();
        let series = migrate_series_keys(&c, "path:1:a.mkv", "tmdb:movie:9").unwrap();
        assert!(item.tables_present, "the live watch-state migrator ran");
        assert_eq!(item.watch_rewrites, 1);
        assert_eq!(
            series.tables_present, 0,
            "no series-keyed table exists yet, and it says so"
        );
        let key: String = c
            .query_row("SELECT item_key FROM watch_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(key, "tmdb:movie:9");
    }

    /// The guard for a database that predates migration 026. It no longer
    /// needs a migrated connection: an empty one is exactly that database.
    #[test]
    fn no_op_without_watch_table() {
        let c = Connection::open_in_memory().unwrap();
        let r = migrate_item_keys(&c, &["path:1:a.mkv".into()], "tmdb:movie:1").unwrap();
        assert!(!r.tables_present);
    }

    /// The real table, not a local stand-in. The migrator stopped being dead
    /// code when migration 026 landed (ADR-0035 Consequences), and this is the
    /// shape it meets from now on.
    #[test]
    fn composes_with_the_real_watch_state_table() {
        let c = Connection::open_in_memory().unwrap();
        nightjar_db::migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO accounts (id, username, password_hash, role)
                 VALUES (1, 'a', 'x', 'owner');
             INSERT INTO profiles (id, account_id, profile_ref, name)
                 VALUES (1, 1, 'aa', 'P');
             INSERT INTO watch_state
                 (profile_id, item_key, position_ms, duration_ms, played, hidden,
                  first_played_at, last_played_at)
             VALUES
                 (1, 'path:1:a.mkv', 9000, 10000, 0, 0,
                  '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z');",
        )
        .unwrap();

        let report = migrate_item_keys(&c, &["path:1:a.mkv".into()], "tmdb:movie:9").unwrap();
        assert!(report.tables_present);
        assert_eq!(report.watch_rewrites, 1);
        let key: String = c
            .query_row("SELECT item_key FROM watch_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(key, "tmdb:movie:9");
    }

    /// ADR-0025 §5 merges "for a profile". Two profiles under the old key both
    /// move, and a profile that already holds the new key keeps its own winner
    /// rather than inheriting the other profile's row.
    #[test]
    fn rewrites_and_merges_each_profile_independently() {
        let c = Connection::open_in_memory().unwrap();
        nightjar_db::migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO accounts (id, username, password_hash, role)
                 VALUES (1, 'a', 'x', 'owner');
             INSERT INTO profiles (id, account_id, profile_ref, name)
                 VALUES (1, 1, 'aa', 'P1'), (2, 1, 'bb', 'P2');
             INSERT INTO watch_state
                 (profile_id, item_key, position_ms, duration_ms, played, hidden,
                  first_played_at, last_played_at)
             VALUES
                 -- profile 1: the old key is further along, so it wins.
                 (1, 'old', 9000, 10000, 0, 0,
                  '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z'),
                 (1, 'new', 1000, 10000, 0, 0,
                  '2026-01-01T00:00:00.000Z', '2026-02-01T00:00:00.000Z'),
                 -- profile 2: only the old key, so it moves without a merge.
                 (2, 'old', 2000, 10000, 0, 0,
                  '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z');",
        )
        .unwrap();

        let report = migrate_item_keys(&c, &["old".into()], "new").unwrap();
        assert_eq!(report.watch_rewrites, 2);
        let rows: Vec<(i64, i64)> = {
            let mut stmt = c
                .prepare(
                    "SELECT profile_id, position_ms FROM watch_state
                     WHERE item_key = 'new' ORDER BY profile_id",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(rows, vec![(1, 9000), (2, 2000)]);
        let stale: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM watch_state WHERE item_key = 'old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stale, 0);
    }
}
