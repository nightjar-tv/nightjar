use rusqlite::{Connection, params};
use std::path::Path;

use crate::paths::{normalize_library_root, show_folder_relpath, to_relpath};

const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../migrations/001_init.sql")),
    (2, include_str!("../migrations/002_scan_jobs.sql")),
    (3, include_str!("../migrations/003_subtitle_sidecars.sql")),
    (4, include_str!("../migrations/004_audio_channels.sql")),
    (5, include_str!("../migrations/005_subtitle_status.sql")),
    (
        6,
        include_str!("../migrations/006_library_availability.sql"),
    ),
    (
        7,
        include_str!("../migrations/007_content_identity_keyframe_map.sql"),
    ),
    (8, include_str!("../migrations/008_probe_bitrate_hdr.sql")),
    (
        9,
        include_str!("../migrations/009_metadata_cache_payloads.sql"),
    ),
    (10, include_str!("../migrations/010_metadata_status.sql")),
    (
        11,
        include_str!("../migrations/011_canonical_metadata_item_links.sql"),
    ),
    (
        12,
        include_str!("../migrations/012_library_relative_paths.sql"),
    ),
    (13, include_str!("../migrations/013_cleaner_version.sql")),
    (
        14,
        include_str!("../migrations/014_metadata_status_matched.sql"),
    ),
    (
        15,
        include_str!("../migrations/015_metadata_status_repairs.sql"),
    ),
    (16, include_str!("../migrations/016_series_identity.sql")),
    (
        17,
        include_str!("../migrations/017_subtitle_track_inventory.sql"),
    ),
    (
        18,
        include_str!("../migrations/018_subtitle_extract_backoff.sql"),
    ),
    (
        19,
        include_str!("../migrations/019_drop_subtitle_source_stamps.sql"),
    ),
    (
        20,
        include_str!("../migrations/020_accounts_profiles_sessions.sql"),
    ),
    (
        21,
        include_str!("../migrations/021_series_entity_bindings.sql"),
    ),
    (
        22,
        include_str!("../migrations/022_metadata_decision_reasons.sql"),
    ),
    (
        23,
        include_str!("../migrations/023_confirmed_by_episode_title.sql"),
    ),
    (24, include_str!("../migrations/024_probe_frame_rate.sql")),
    (
        25,
        include_str!("../migrations/025_accounts_username_case_insensitive.sql"),
    ),
    (26, include_str!("../migrations/026_watch_state.sql")),
    (
        27,
        include_str!("../migrations/027_series_binding_without_entity.sql"),
    ),
    (
        28,
        include_str!("../migrations/028_track_selection_persistence.sql"),
    ),
    (
        29,
        include_str!("../migrations/029_account_playback_policy.sql"),
    ),
];

pub fn migrate(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY NOT NULL,
            applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
        );",
    )
    .map_err(|e| format!("ensure schema_migrations: {e}"))?;

    let current: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("read migration version: {e}"))?;

    for &(version, sql) in MIGRATIONS {
        if version <= current {
            continue;
        }
        let before_items = if version == 6 || version == 12 {
            count_table(conn, "media_items")?
        } else {
            0
        };
        let before_sidecars = if version == 6 || version == 12 {
            count_table(conn, "media_item_sidecars")?
        } else {
            0
        };
        // 027 rebuilds `series`. A rebuild is the one migration shape that can
        // silently lose rows, so both it and the `series_entity_bindings` rows
        // that cascade from it are counted either side, the same guard 6 and 12
        // carry.
        let before_series = if version == 27 {
            count_table(conn, "series")?
        } else {
            0
        };
        let before_bindings = if version == 27 {
            count_table(conn, "series_entity_bindings")?
        } else {
            0
        };
        // 029 rebuilds `accounts`. `profiles` and `login_sessions` cascade
        // from it, so all three are counted either side of the copy.
        let before_accounts = if version == 29 {
            count_table(conn, "accounts")?
        } else {
            0
        };
        let before_account_profiles = if version == 29 {
            count_table(conn, "profiles")?
        } else {
            0
        };
        let before_account_sessions = if version == 29 {
            count_table(conn, "login_sessions")?
        } else {
            0
        };
        // Migration 25 adds a case-insensitive unique index over `username`.
        // If an install already holds two accounts differing only in case the
        // index cannot be built, and SQLite would say so as
        // `UNIQUE constraint failed: accounts.username`, which names neither
        // row. Refuse first and name them.
        if version == 25 {
            refuse_colliding_usernames(conn)?;
        }

        // Migration 27 rebuilds `series`, which `series_entity_bindings`
        // references ON DELETE CASCADE. Migration 29 rebuilds `accounts`,
        // which `profiles` and `login_sessions` reference ON DELETE CASCADE.
        // With foreign keys live, the rebuild's `DROP TABLE` performs an
        // implicit delete and takes every child row with it. SQLite's
        // table-rebuild procedure turns the pragma off *outside* the
        // transaction — it is a no-op inside one, which is the constraint
        // migration 025's comment names — so it is turned off here and
        // restored after the commit.
        let rebuilds_parent = version == 27 || version == 29;
        let restore_foreign_keys = if rebuilds_parent && foreign_keys_enabled(conn)? {
            conn.execute_batch("PRAGMA foreign_keys=OFF;")
                .map_err(|e| format!("disable foreign keys for migration {version}: {e}"))?;
            true
        } else {
            false
        };

        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("begin migration {version}: {e}"))?;
        tx.execute_batch(sql)
            .map_err(|e| format!("apply migration {version}: {e}"))?;

        if version == 6 {
            let after_items = count_table(&tx, "media_items")?;
            let after_sidecars = count_table(&tx, "media_item_sidecars")?;
            if after_items != before_items {
                return Err(format!(
                    "migration 6 aborted: media_items count {before_items} -> {after_items}"
                ));
            }
            if after_sidecars != before_sidecars {
                return Err(format!(
                    "migration 6 aborted: media_item_sidecars count {before_sidecars} -> {after_sidecars}"
                ));
            }
        }

        if version == 12 {
            strip_paths_to_relpath(&tx)?;
            let after_items = count_table(&tx, "media_items")?;
            let after_sidecars = count_table(&tx, "media_item_sidecars")?;
            if after_items != before_items {
                return Err(format!(
                    "migration 12 aborted: media_items count {before_items} -> {after_items}"
                ));
            }
            if after_sidecars != before_sidecars {
                return Err(format!(
                    "migration 12 aborted: media_item_sidecars count {before_sidecars} -> {after_sidecars}"
                ));
            }
        }

        if version == 16 {
            derive_series_rows(&tx)?;
        }

        if version == 27 {
            let after_series = count_table(&tx, "series")?;
            let after_bindings = count_table(&tx, "series_entity_bindings")?;
            if after_series != before_series {
                return Err(format!(
                    "migration 27 aborted: series count {before_series} -> {after_series}"
                ));
            }
            if after_bindings != before_bindings {
                return Err(format!(
                    "migration 27 aborted: series_entity_bindings count {before_bindings} -> {after_bindings}"
                ));
            }
        }

        if version == 29 {
            let after_accounts = count_table(&tx, "accounts")?;
            let after_profiles = count_table(&tx, "profiles")?;
            let after_sessions = count_table(&tx, "login_sessions")?;
            if after_accounts != before_accounts {
                return Err(format!(
                    "migration 29 aborted: accounts count {before_accounts} -> {after_accounts}"
                ));
            }
            if after_profiles != before_account_profiles {
                return Err(format!(
                    "migration 29 aborted: profiles count {before_account_profiles} -> {after_profiles}"
                ));
            }
            if after_sessions != before_account_sessions {
                return Err(format!(
                    "migration 29 aborted: login_sessions count {before_account_sessions} -> {after_sessions}"
                ));
            }
        }

        tx.execute(
            "INSERT INTO schema_migrations (version) VALUES (?1)",
            [version],
        )
        .map_err(|e| format!("record migration {version}: {e}"))?;
        tx.commit()
            .map_err(|e| format!("commit migration {version}: {e}"))?;
        if restore_foreign_keys {
            conn.execute_batch("PRAGMA foreign_keys=ON;")
                .map_err(|e| format!("restore foreign keys after migration {version}: {e}"))?;
        }
        tracing::info!(version, "applied database migration");
    }
    Ok(())
}

/// ADR-0030 §5: strip clean prefixes; leave non-stripping rows absolute and
/// count them on the library. Never abort boot.
fn strip_paths_to_relpath(tx: &rusqlite::Transaction<'_>) -> Result<(), String> {
    let libs: Vec<(i64, String)> = {
        let mut stmt = tx
            .prepare("SELECT id, path FROM libraries")
            .map_err(|e| format!("migration 12 list libraries: {e}"))?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| format!("migration 12 query libraries: {e}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("migration 12 read libraries: {e}"))?
    };

    for (lib_id, root_raw) in libs {
        let root = normalize_library_root(&root_raw);
        if root != root_raw {
            tx.execute(
                "UPDATE libraries SET path = ?2 WHERE id = ?1",
                params![lib_id, root],
            )
            .map_err(|e| format!("migration 12 normalize root {lib_id}: {e}"))?;
        }

        let items: Vec<(i64, String)> = {
            let mut stmt = tx
                .prepare("SELECT id, path FROM media_items WHERE library_id = ?1")
                .map_err(|e| format!("migration 12 prepare items: {e}"))?;
            let rows = stmt
                .query_map(params![lib_id], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(|e| format!("migration 12 query items: {e}"))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("migration 12 read items: {e}"))?
        };

        let mut unresolved = 0i64;
        for (item_id, path) in items {
            match to_relpath(&root, Path::new(&path)) {
                Some(rel) if rel != path => {
                    tx.execute(
                        "UPDATE media_items SET path = ?2 WHERE id = ?1",
                        params![item_id, rel],
                    )
                    .map_err(|e| format!("migration 12 item {item_id}: {e}"))?;
                }
                Some(_) => {}
                None => unresolved += 1,
            }
        }

        let sidecars: Vec<(i64, String, String)> = {
            let mut stmt = tx
                .prepare(
                    "SELECT s.media_item_id, s.track_id, s.path FROM media_item_sidecars s
                     JOIN media_items m ON m.id = s.media_item_id
                     WHERE m.library_id = ?1",
                )
                .map_err(|e| format!("migration 12 prepare sidecars: {e}"))?;
            let rows = stmt
                .query_map(params![lib_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map_err(|e| format!("migration 12 query sidecars: {e}"))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("migration 12 read sidecars: {e}"))?
        };
        for (media_item_id, track_id, path) in sidecars {
            match to_relpath(&root, Path::new(&path)) {
                Some(rel) if rel != path => {
                    tx.execute(
                        "UPDATE media_item_sidecars SET path = ?3
                         WHERE media_item_id = ?1 AND track_id = ?2",
                        params![media_item_id, track_id, rel],
                    )
                    .map_err(|e| format!("migration 12 sidecar {media_item_id}/{track_id}: {e}"))?;
                }
                Some(_) => {}
                None => unresolved += 1,
            }
        }

        tx.execute(
            "UPDATE libraries SET paths_unresolved = ?2 WHERE id = ?1",
            params![lib_id, unresolved],
        )
        .map_err(|e| format!("migration 12 set unresolved {lib_id}: {e}"))?;
    }
    Ok(())
}

/// ADR-0033 Q5: one-shot retro-derive of folder-keyed series rows from the
/// existing `ready` episode population. A path walk, not a re-match: those
/// rows already carry `tmdb:episode:` links, so folder grouping needs no
/// provider call. Idempotent at the SQL level (INSERT OR IGNORE); nothing
/// inside `drain_pending` re-derives series rows (plan Decision 6).
fn derive_series_rows(tx: &rusqlite::Transaction<'_>) -> Result<(), String> {
    let rows: Vec<(i64, String, String, i64)> = {
        let mut stmt = tx
            .prepare(
                "SELECT m.library_id, m.path, l.path, c.tmdb_show
                 FROM media_items m
                 JOIN libraries l ON l.id = m.library_id
                 JOIN media_item_links il ON il.media_item_id = m.id
                 JOIN metadata_canonical c
                   ON c.provider = 'tmdb' AND c.entity_kind = 'episode'
                   AND il.item_key = 'tmdb:episode:' || c.provider_id
                 WHERE m.metadata_status = 'ready'
                   AND m.kind = 'episode'
                   AND c.tmdb_show IS NOT NULL
                 ORDER BY m.id",
            )
            .map_err(|e| format!("migration 16 prepare derive: {e}"))?;
        let q = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map_err(|e| format!("migration 16 query derive: {e}"))?;
        let mut out = Vec::new();
        for row in q {
            out.push(row.map_err(|e| format!("migration 16 derive row: {e}"))?);
        }
        out
    };
    for (library_id, path, root, tmdb_show) in rows {
        let folder = show_folder_relpath(&path, &root);
        tx.execute(
            "INSERT OR IGNORE INTO series (library_id, relpath, tmdb_show_id)
             VALUES (?1, ?2, ?3)",
            params![library_id, folder, tmdb_show],
        )
        .map_err(|e| format!("migration 16 insert series row: {e}"))?;
    }
    Ok(())
}

/// Refuse migration 25 when an install already holds usernames that differ
/// only in case, and say which ones (OPEN-DEFECTS entry 14).
///
/// **Merging or dropping one of them is not this migration's call.** The rows
/// are two people's logins: `profiles`, watch state and `login_sessions` all
/// hang off `accounts(id)`, so picking a winner moves one person's history
/// onto the other's account, silently and without a way back. Boot stops
/// instead, with both usernames and both ids in the message, and an operator
/// decides.
///
/// **Nothing on disk today can hit this.** Every install these migrations have
/// seen has one account. That is the reason to write the check rather than a
/// reason to skip it: the case it guards is the one nobody will be ready for.
fn refuse_colliding_usernames(conn: &Connection) -> Result<(), String> {
    let mut stmt = conn
        .prepare(
            "SELECT group_concat(id || ' ' || quote(username), ', ')
             FROM accounts
             GROUP BY username COLLATE NOCASE
             HAVING COUNT(*) > 1",
        )
        .map_err(|e| format!("prepare username collision check: {e}"))?;
    let groups: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| format!("username collision check: {e}"))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("username collision row: {e}"))?;
    if groups.is_empty() {
        return Ok(());
    }
    Err(format!(
        "migration 25 refused: {} username(s) differ only in case, and this \
         migration will not choose which account keeps the name. Rename one \
         side of each group, then start again. Colliding rows (id username): {}",
        groups.len(),
        groups.join("; ")
    ))
}

fn count_table(conn: &Connection, table: &str) -> Result<i64, String> {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .map_err(|e| format!("count {table}: {e}"))
}

/// Whether foreign keys are live on this connection. The migration 27 rebuild
/// reads it so it restores the pragma to what it found rather than assuming.
fn foreign_keys_enabled(conn: &Connection) -> Result<bool, String> {
    let on: i64 = conn
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .map_err(|e| format!("read foreign_keys pragma: {e}"))?;
    Ok(on != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn migrates_fresh_db() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM libraries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
        let v: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(v, 29);
        // 026 (ADR-0035 item 1): the table, its primary key and the recent
        // index exist, and the two boolean columns reject a value outside 0/1.
        let has_watch_state: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'watch_state'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_watch_state, 1);
        let has_watch_state_index: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'index' AND name = 'idx_watch_state_recent'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_watch_state_index, 1);
        let has_series: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'series'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_series, 1);
        let has_series_bindings: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'series_entity_bindings'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_series_bindings, 1);
        // 028 (ADR-0038 amendment): the profile defaults and the per-series
        // override table exist, with the three-valued subtitle mode.
        let has_track_choice: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'profile_track_choice'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_track_choice, 1);
        for col in ["preferred_language", "subtitle_default"] {
            let present: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('profiles') WHERE name = ?1",
                    [col],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(present, 1, "{col}");
        }
        // 022: both decision columns exist and are nullable, so an existing
        // row reads NULL — "decided before this migration", distinguishable
        // from every real token.
        for col in ["metadata_unmatched_reason", "metadata_match_method"] {
            let present: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('media_items') WHERE name = ?1",
                    [col],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(present, 1, "{col}");
        }
        let has_subtitle_tracks: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'media_item_subtitle_tracks'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_subtitle_tracks, 1);
        let subtitle_track_kind_check: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('media_item_subtitle_tracks')
                 WHERE name = 'kind'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(subtitle_track_kind_check, 1);
        let has_subtitle_tracks_index: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'index' AND name = 'idx_media_item_subtitle_tracks_item'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_subtitle_tracks_index, 1);
        let has_neg: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'metadata_negative_cache'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_neg, 1);
        let has_cleaner: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('metadata_negative_cache')
                 WHERE name = 'cleaner_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_cleaner, 1);
        let has_raw: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'metadata_raw_payloads'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_raw, 1);
        let has_canonical: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'metadata_canonical'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_canonical, 1);
        let has_links: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'media_item_links'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_links, 1);
        let has_meta_status: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('media_items') WHERE name = 'metadata_status'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_meta_status, 1);
        // 014: two-tier 'matched' is admitted; unknown values still rejected.
        conn.execute(
            "INSERT INTO libraries (name, path, kind) VALUES ('l', '/tmp/l', 'movies')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO media_items
                (library_id, path, mtime_ms, size_bytes, title, kind, metadata_status)
             VALUES (1, '/tmp/014.mkv', 1, 1, 'T', 'movie', 'matched')",
            [],
        )
        .unwrap();
        let rejected = conn.execute(
            "INSERT INTO media_items
                (library_id, path, mtime_ms, size_bytes, title, kind, metadata_status)
             VALUES (1, '/tmp/014b.mkv', 1, 1, 'T', 'movie', 'bogus')",
            [],
        );
        assert!(rejected.is_err(), "014: unknown status must be rejected");
        let has_reachable: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('libraries') WHERE name = 'reachable'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_reachable, 1);
        // 018 (ADR-0041 Decision 8.3): subtitle retry state columns exist.
        let has_attempt_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('media_items') WHERE name = 'subtitle_attempt_count'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_attempt_count, 1);
        let has_next_retry_at: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('media_items') WHERE name = 'subtitle_next_retry_at'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_next_retry_at, 1);
        let has_content_id: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('media_items') WHERE name = 'content_id'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_content_id, 1);
        let has_bitrate: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('media_items') WHERE name = 'video_bitrate_bps'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_bitrate, 1);
        let has_hdr: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('media_items') WHERE name = 'hdr'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_hdr, 1);
        let has_map: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'keyframe_map_entries'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_map, 1);
        migrate(&conn).unwrap(); // idempotent
    }

    #[test]
    fn upgrades_a_populated_db_in_place() {
        let conn = Connection::open_in_memory().unwrap();
        for &(version, sql) in MIGRATIONS.iter().take(3) {
            conn.execute_batch(sql).unwrap();
            conn.execute_batch(&format!(
                "CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY NOT NULL,
                     applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')));
                 INSERT INTO schema_migrations (version) VALUES ({version});"
            ))
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('t', '/tmp/t', 'movies');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind)
             VALUES (1, '/tmp/t/a.mkv', 1, 2, 'A', 'movie');",
        )
        .unwrap();

        migrate(&conn).unwrap();

        let channels: Option<i64> = conn
            .query_row("SELECT audio_channels FROM media_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(channels, None, "existing rows carry a null channel count");
        let status: String = conn
            .query_row("SELECT subtitle_status FROM media_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "pending");
        let reachable: i64 = conn
            .query_row("SELECT reachable FROM libraries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(reachable, 1);
        let map_status: String = conn
            .query_row("SELECT map_status FROM media_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(map_status, "pending");
        let content_id: Option<String> = conn
            .query_row("SELECT content_id FROM media_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(content_id, None);
    }

    #[test]
    fn migration_6_resets_opaque_errors_and_keeps_row_count() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY NOT NULL,
                applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );",
        )
        .unwrap();
        for &(version, sql) in MIGRATIONS.iter().take(5) {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations (version) VALUES (?1)",
                [version],
            )
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('t', '/tmp/t', 'movies');
             INSERT INTO media_items (
                library_id, path, mtime_ms, size_bytes, title, kind, probe_status, scan_error, subtitle_status
             ) VALUES
               (1, '/tmp/t/a.mkv', 1, 2, 'A', 'movie', 'error', 'ffprobe failed for /tmp/t/a.mkv: ', 'error'),
               (1, '/tmp/t/b.mkv', 1, 2, 'B', 'movie', 'probed', NULL, 'ready');
             INSERT INTO media_item_sidecars (
                media_item_id, track_id, path, mtime_ms, size_bytes, format
             ) VALUES (1, 's-en', '/tmp/t/a.en.srt', 1, 2, 'srt');",
        )
        .unwrap();

        migrate(&conn).unwrap();

        let items: i64 = conn
            .query_row("SELECT COUNT(*) FROM media_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(items, 2);
        let sidecars: i64 = conn
            .query_row("SELECT COUNT(*) FROM media_item_sidecars", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sidecars, 1);
        let probe: String = conn
            .query_row(
                "SELECT probe_status FROM media_items WHERE path LIKE '%a.mkv'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(probe, "indexed");
        let sub: String = conn
            .query_row(
                "SELECT subtitle_status FROM media_items WHERE path LIKE '%a.mkv'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sub, "pending");
        conn.execute(
            "UPDATE media_items SET probe_status = 'unavailable', subtitle_status = 'unavailable'
             WHERE path LIKE '%a.mkv'",
            [],
        )
        .unwrap();
    }

    #[test]
    fn migration_7_adds_identity_and_map_without_dropping_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY NOT NULL,
                applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );",
        )
        .unwrap();
        for &(version, sql) in MIGRATIONS.iter().take(6) {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations (version) VALUES (?1)",
                [version],
            )
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('t', '/tmp/t', 'movies');
             INSERT INTO media_items (
                library_id, path, mtime_ms, size_bytes, title, kind, probe_status, subtitle_status
             ) VALUES (1, '/tmp/t/a.mkv', 1, 2, 'A', 'movie', 'probed', 'ready');",
        )
        .unwrap();

        migrate(&conn).unwrap();

        let items: i64 = conn
            .query_row("SELECT COUNT(*) FROM media_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(items, 1);
        let map_status: String = conn
            .query_row("SELECT map_status FROM media_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(map_status, "pending");
        // No moov cache table — per-session rebuild (ADR-0023 §3c).
        let moov_tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE '%moov%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(moov_tables, 0);
    }

    /// 015 (RC4): one E1 row (episode link + canonical episode row, status
    /// `unmatched`) becomes `ready`; one E2 row (`tmdb:show:{episode_id}`
    /// mis-prefix) loses the link and falls back to `pending`. The harness
    /// applies by version, so idempotence is asserted at the SQL level by
    /// running the migration text twice.
    #[test]
    fn migration_15_repairs_e1_ready_and_e2_pending() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY NOT NULL,
                applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );",
        )
        .unwrap();
        for &(version, sql) in MIGRATIONS.iter().take(14) {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations (version) VALUES (?1)",
                [version],
            )
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO media_items (
                library_id, path, mtime_ms, size_bytes, title, kind, season, episode, metadata_status
             ) VALUES
                (1, 'Alpha/S01E01.mkv', 1, 1, 'Alpha', 'episode', 1, 1, 'unmatched'),
                (1, 'Stick/S01E10.mkv', 1, 1, 'Stick', 'episode', 1, 10, 'unmatched');
             INSERT INTO metadata_canonical (
                provider, entity_kind, provider_id, title, ids_json, tmdb_show, projected_at
             ) VALUES
                ('tmdb', 'episode', '1001', 'Pilot', '{\"tmdb\":1001}', 55, '2026-01-01T00:00:00Z'),
                ('tmdb', 'episode', '5995804', 'Deja Vu All Over Again', '{\"tmdb\":5995804}', 66, '2026-01-01T00:00:00Z');
             INSERT INTO media_item_links (media_item_id, item_key, manually_matched)
             VALUES
                (1, 'tmdb:episode:1001', 0),
                (2, 'tmdb:show:5995804', 0);",
        )
        .unwrap();

        let migration_15 = MIGRATIONS
            .iter()
            .find(|(version, _)| *version == 15)
            .map(|(_, sql)| *sql)
            .unwrap();
        conn.execute_batch(migration_15).unwrap();

        let e1_status: String = conn
            .query_row(
                "SELECT metadata_status FROM media_items WHERE path = 'Alpha/S01E01.mkv'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(e1_status, "ready", "E1: full episode identity means ready");
        let e2_status: String = conn
            .query_row(
                "SELECT metadata_status FROM media_items WHERE path = 'Stick/S01E10.mkv'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(e2_status, "pending", "E2: mis-prefix falls back to pending");
        let e1_link: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM media_item_links
                 WHERE media_item_id = 1 AND item_key = 'tmdb:episode:1001'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(e1_link, 1, "E1 episode link must survive");
        let e2_link: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM media_item_links
                 WHERE media_item_id = 2 AND item_key LIKE 'tmdb:show:%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(e2_link, 0, "E2 mis-prefixed show link must be deleted");

        // Idempotence at the SQL level: a second run changes nothing.
        conn.execute_batch(migration_15).unwrap();
        let e1_again: String = conn
            .query_row(
                "SELECT metadata_status FROM media_items WHERE path = 'Alpha/S01E01.mkv'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(e1_again, "ready");
        let e2_again: String = conn
            .query_row(
                "SELECT metadata_status FROM media_items WHERE path = 'Stick/S01E10.mkv'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(e2_again, "pending");
        let link_count_again: i64 = conn
            .query_row("SELECT COUNT(*) FROM media_item_links", [], |r| r.get(0))
            .unwrap();
        assert_eq!(link_count_again, 1);
    }

    /// 016 (RC8): one-shot series-row derive (ADR-0033 Q5). Ready episodes
    /// with episode links produce one row per show folder — `Season N/` and
    /// `Specials/` inherit the folder. Re-running the derive is a no-op.
    #[test]
    fn migration_16_derives_series_rows_and_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY NOT NULL,
                applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );",
        )
        .unwrap();
        for &(version, sql) in MIGRATIONS.iter().take(15) {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations (version) VALUES (?1)",
                [version],
            )
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/media/S', 'shows');
             INSERT INTO media_items (
                library_id, path, mtime_ms, size_bytes, title, kind, season, episode, metadata_status
             ) VALUES
                (1, 'Alpha/Season 1/Alpha.S01E01.mkv', 1, 1, 'Alpha', 'episode', 1, 1, 'ready'),
                (1, 'Alpha/Season 2/Alpha.S02E01.mkv', 1, 1, 'Alpha', 'episode', 2, 1, 'ready'),
                (1, 'Alpha/Specials/Alpha.S00E01.mkv', 1, 1, 'Alpha', 'episode', 0, 1, 'ready'),
                (1, 'Beta/Season 1/Beta.S01E01.mkv', 1, 1, 'Beta', 'episode', 1, 1, 'ready'),
                (1, 'Gamma/pending.mkv', 1, 1, 'Gamma', 'episode', 1, 1, 'pending');
             INSERT INTO metadata_canonical (
                provider, entity_kind, provider_id, title, ids_json, tmdb_show, projected_at
             ) VALUES
                ('tmdb', 'episode', '1001', 'One', '{\"tmdb\":1001}', 55, '2026-01-01T00:00:00Z'),
                ('tmdb', 'episode', '2001', 'Two', '{\"tmdb\":2001}', 55, '2026-01-01T00:00:00Z'),
                ('tmdb', 'episode', '9001', 'Sp', '{\"tmdb\":9001}', 55, '2026-01-01T00:00:00Z'),
                ('tmdb', 'episode', '3001', 'B1', '{\"tmdb\":3001}', 66, '2026-01-01T00:00:00Z');
             INSERT INTO media_item_links (media_item_id, item_key, manually_matched)
             VALUES
                (1, 'tmdb:episode:1001', 0),
                (2, 'tmdb:episode:2001', 0),
                (3, 'tmdb:episode:9001', 0),
                (4, 'tmdb:episode:3001', 0);",
        )
        .unwrap();

        let migration_16 = MIGRATIONS
            .iter()
            .find(|(version, _)| *version == 16)
            .map(|(_, sql)| *sql)
            .unwrap();
        conn.execute_batch(migration_16).unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        derive_series_rows(&tx).unwrap();
        tx.commit().unwrap();

        let mut stmt = conn
            .prepare("SELECT library_id, relpath, tmdb_show_id FROM series ORDER BY relpath")
            .unwrap();
        let rows: Vec<(i64, String, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![(1, "Alpha".to_string(), 55), (1, "Beta".to_string(), 66),],
            "one row per show folder; Season 1/2 and Specials inherit Alpha"
        );

        // Idempotent at the SQL level: a second derive changes nothing.
        let tx2 = conn.unchecked_transaction().unwrap();
        derive_series_rows(&tx2).unwrap();
        tx2.commit().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM series", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }

    /// 017 (ADR-0041): the inventory table exists and `subtitle_status =
    /// 'error'` rows reset to `pending` with their source stamps cleared, so a
    /// re-probe re-derives them through Decision 2's classifier (Decision 9,
    /// same pattern as migration 006's subtitle reset). `ready` rows and rows
    /// with other statuses are untouched.
    #[test]
    fn migration_17_adds_inventory_and_resets_errors_to_pending() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY NOT NULL,
                applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );",
        )
        .unwrap();
        for &(version, sql) in MIGRATIONS.iter().take(16) {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations (version) VALUES (?1)",
                [version],
            )
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('t', '/tmp/t', 'movies');
             INSERT INTO media_items (
                library_id, path, mtime_ms, size_bytes, title, kind, probe_status, subtitle_status,
                subtitle_source_mtime_ms, subtitle_source_size_bytes
             ) VALUES
                (1, '/tmp/t/a.mkv', 1, 2, 'A', 'movie', 'probed', 'error', 7, 9),
                (1, '/tmp/t/b.mkv', 1, 2, 'B', 'movie', 'probed', 'ready', 7, 9),
                (1, '/tmp/t/c.mkv', 1, 2, 'C', 'movie', 'probed', 'unavailable', NULL, NULL);",
        )
        .unwrap();

        let migration_17 = MIGRATIONS
            .iter()
            .find(|(version, _)| *version == 17)
            .map(|(_, sql)| *sql)
            .unwrap();
        conn.execute_batch(migration_17).unwrap();

        let a_status: String = conn
            .query_row(
                "SELECT subtitle_status FROM media_items WHERE path = '/tmp/t/a.mkv'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(a_status, "pending", "opaque error rows reset to pending");
        let a_mtime: Option<i64> = conn
            .query_row(
                "SELECT subtitle_source_mtime_ms FROM media_items WHERE path = '/tmp/t/a.mkv'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(a_mtime, None, "source stamps cleared like migration 006");
        let b_status: String = conn
            .query_row(
                "SELECT subtitle_status FROM media_items WHERE path = '/tmp/t/b.mkv'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(b_status, "ready", "non-error rows untouched");
        let c_status: String = conn
            .query_row(
                "SELECT subtitle_status FROM media_items WHERE path = '/tmp/t/c.mkv'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(c_status, "unavailable", "unavailable rows untouched");
        let error_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM media_items WHERE subtitle_status = 'error'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(error_count, 0);

        let has_tracks: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'media_item_subtitle_tracks'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_tracks, 1);
        // The new CHECK admits exactly the four kinds the classifier consumes.
        let rejected = conn.execute(
            "INSERT INTO media_item_subtitle_tracks
                (media_item_id, stream_index, codec, kind)
             VALUES (1, 0, 'bogus', 'bogus')",
            [],
        );
        assert!(rejected.is_err(), "unknown kind must be rejected");
        let accepted = conn.execute(
            "INSERT INTO media_item_subtitle_tracks
                (media_item_id, stream_index, codec, kind)
             VALUES (1, 0, 'subrip', 'text')",
            [],
        );
        assert!(accepted.is_ok(), "the four admitted kinds insert cleanly");
    }

    /// 018 (ADR-0041 Decision 8.3): the subtitle retry-state columns exist
    /// with sane defaults and existing rows keep their status. Pure ADD
    /// COLUMN, so idempotence is asserted at the SQL level by re-applying.
    #[test]
    fn migration_18_adds_subtitle_retry_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY NOT NULL,
                applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );",
        )
        .unwrap();
        for &(version, sql) in MIGRATIONS.iter().take(17) {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations (version) VALUES (?1)",
                [version],
            )
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('t', '/tmp/t', 'movies');
             INSERT INTO media_items (
                library_id, path, mtime_ms, size_bytes, title, kind, subtitle_status
             ) VALUES
                (1, '/tmp/t/a.mkv', 1, 2, 'A', 'movie', 'unavailable'),
                (1, '/tmp/t/b.mkv', 1, 2, 'B', 'movie', 'ready');",
        )
        .unwrap();

        let migration_18 = MIGRATIONS
            .iter()
            .find(|(version, _)| *version == 18)
            .map(|(_, sql)| *sql)
            .unwrap();
        conn.execute_batch(migration_18).unwrap();

        let status_a: String = conn
            .query_row(
                "SELECT subtitle_status FROM media_items WHERE path = '/tmp/t/a.mkv'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status_a, "unavailable", "existing status untouched");
        let attempts_a: i64 = conn
            .query_row(
                "SELECT subtitle_attempt_count FROM media_items WHERE path = '/tmp/t/a.mkv'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(attempts_a, 0, "retry count defaults to 0");
        let retry_a: Option<String> = conn
            .query_row(
                "SELECT subtitle_next_retry_at FROM media_items WHERE path = '/tmp/t/a.mkv'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            retry_a, None,
            "no retry deadline until a failure is recorded"
        );
        let status_b: String = conn
            .query_row(
                "SELECT subtitle_status FROM media_items WHERE path = '/tmp/t/b.mkv'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status_b, "ready");
    }

    /// ADR-0034 items 1, 5 and 6 plus ADR-0040 item 2. Guard shape follows
    /// migrations 6 and 12: an existing populated DB migrates without moving
    /// the row counts the rest of the system depends on.
    #[test]
    fn migration_20_adds_accounts_without_touching_media() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY NOT NULL,
                applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );",
        )
        .unwrap();
        for &(version, sql) in MIGRATIONS.iter().take(19) {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations (version) VALUES (?1)",
                [version],
            )
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('t', '/tmp/t', 'shows');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind)
             VALUES (1, '/tmp/t/a.mkv', 1, 2, 'A', 'episode'),
                    (1, '/tmp/t/b.mkv', 1, 2, 'B', 'episode');
             INSERT INTO media_item_links (media_item_id, item_key)
             VALUES (1, 'tmdb:episode:1');",
        )
        .unwrap();
        let items_before = count(&conn, "media_items");
        let links_before = count(&conn, "media_item_links");

        let migration_20 = MIGRATIONS
            .iter()
            .find(|(version, _)| *version == 20)
            .map(|(_, sql)| *sql)
            .unwrap();
        conn.execute_batch(migration_20).unwrap();

        assert_eq!(count(&conn, "media_items"), items_before);
        assert_eq!(count(&conn, "media_item_links"), links_before);
        assert_eq!(count(&conn, "accounts"), 0);
        assert_eq!(count(&conn, "profiles"), 0);
        assert_eq!(count(&conn, "login_sessions"), 0);
    }

    /// ADR-0040 item 2: exactly one owner is a constraint, not a convention.
    /// The guarantee is that the second write fails, not that a reviewer
    /// notices, so the second write is what this asserts.
    #[test]
    fn migration_20_refuses_a_second_owner() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO accounts (username, password_hash, role)
             VALUES ('a', 'x', 'owner');",
        )
        .unwrap();

        let second = conn.execute_batch(
            "INSERT INTO accounts (username, password_hash, role)
             VALUES ('b', 'x', 'owner');",
        );
        assert!(second.is_err(), "a second owner must not be insertable");

        // Managers and members are unconstrained in number, so the index is
        // the owner singleton and not a general uniqueness rule on `role`.
        conn.execute_batch(
            "INSERT INTO accounts (username, password_hash, role)
             VALUES ('c', 'x', 'manager'), ('d', 'x', 'manager'),
                    ('e', 'x', 'member'), ('f', 'x', 'member');",
        )
        .unwrap();
        assert_eq!(count(&conn, "accounts"), 5);

        // Promoting a second account to owner is the same violation by a
        // different route, and the partial index catches it there too.
        let promote =
            conn.execute_batch("UPDATE accounts SET role = 'owner' WHERE username = 'c';");
        assert!(promote.is_err(), "promotion cannot create a second owner");
    }

    /// The CHECK constraint is the closed set, so a value outside ADR-0040
    /// item 1's three cannot reach disk however it is spelled.
    #[test]
    fn migration_20_role_is_a_closed_set() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        for bad in ["admin", "Owner", "", "superuser"] {
            let r = conn.execute(
                "INSERT INTO accounts (username, password_hash, role) VALUES (?1, 'x', ?2)",
                rusqlite::params![bad, bad],
            );
            assert!(r.is_err(), "role {bad:?} must be refused");
        }
        // The default is the least authority, so an insert that forgets the
        // column cannot silently create an administrator.
        conn.execute_batch("INSERT INTO accounts (username, password_hash) VALUES ('a', 'x');")
            .unwrap();
        let role: String = conn
            .query_row("SELECT role FROM accounts WHERE username = 'a'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(role, "member");
    }

    /// The `expires_at` length CHECK. Not a format validator: it catches the
    /// class that breaks the lexicographic comparison session classification
    /// depends on, which is a timestamp of the wrong shape reaching disk.
    #[test]
    fn migration_20_refuses_a_misshapen_expiry() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO accounts (id, username, password_hash, role)
                  VALUES (1, 'a', 'x', 'owner');",
        )
        .unwrap();

        for bad in [
            "2026-08-10",                    // date only, sorts before every full stamp
            "2026-08-10T00:00:00Z",          // no milliseconds, one char short
            "2026-08-10T00:00:00.000+10:00", // offset rather than UTC
            "",
        ] {
            let r = conn.execute(
                "INSERT INTO login_sessions
                     (account_id, token_sha256, client_label, expires_at)
                 VALUES (1, ?1, 'tv', ?2)",
                rusqlite::params![bad, bad],
            );
            assert!(r.is_err(), "expiry {bad:?} must be refused");
        }

        conn.execute(
            "INSERT INTO login_sessions
                 (account_id, token_sha256, client_label, expires_at)
             VALUES (1, 'ok', 'tv', '2099-01-01T00:00:00.000Z')",
            [],
        )
        .unwrap();
    }

    /// ADR-0034 item 5: deleting a profile must not leave a live session that
    /// has silently widened to account scope. The migration's answer is
    /// ON DELETE CASCADE, so the session is destroyed rather than widened.
    #[test]
    fn migration_20_profile_delete_takes_its_sessions() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             INSERT INTO accounts (id, username, password_hash, role)
                  VALUES (1, 'a', 'x', 'owner');
             INSERT INTO profiles (id, account_id, profile_ref, name)
                  VALUES (1, 1, 'aa', 'kid'), (2, 1, 'bb', 'adult');
             INSERT INTO login_sessions
                  (account_id, active_profile_id, token_sha256, client_label, expires_at)
             VALUES (1, 1, 'h1', 'tv', '2099-01-01T00:00:00.000Z'),
                    (1, 2, 'h2', 'phone', '2099-01-01T00:00:00.000Z'),
                    (1, NULL, 'h3', 'browser', '2099-01-01T00:00:00.000Z');",
        )
        .unwrap();

        conn.execute("DELETE FROM profiles WHERE id = 1", [])
            .unwrap();

        let remaining: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT token_sha256 FROM login_sessions ORDER BY token_sha256")
                .unwrap();
            let rows = stmt.query_map([], |r| r.get(0)).unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        assert_eq!(
            remaining,
            vec!["h2".to_string(), "h3".to_string()],
            "the capped session goes with its profile; the account-scope one stays"
        );
        // And nothing was widened: no surviving row points at the dead profile.
        assert_eq!(
            count_where(&conn, "login_sessions", "active_profile_id = 1"),
            0
        );
    }

    /// Deleting an account takes its profiles and its sessions (ADR-0034
    /// item 7).
    #[test]
    fn migration_20_account_delete_cascades() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             INSERT INTO accounts (id, username, password_hash, role)
                  VALUES (1, 'a', 'x', 'owner'), (2, 'b', 'x', 'member');
             INSERT INTO profiles (id, account_id, profile_ref, name)
                  VALUES (1, 2, 'aa', 'p');
             INSERT INTO login_sessions
                  (account_id, active_profile_id, token_sha256, client_label, expires_at)
             VALUES (2, 1, 'h1', 'tv', '2099-01-01T00:00:00.000Z');",
        )
        .unwrap();

        conn.execute("DELETE FROM accounts WHERE id = 2", [])
            .unwrap();
        assert_eq!(count(&conn, "profiles"), 0);
        assert_eq!(count(&conn, "login_sessions"), 0);
        assert_eq!(count(&conn, "accounts"), 1);
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    fn count_where(conn: &Connection, table: &str, predicate: &str) -> i64 {
        conn.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE {predicate}"),
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// Copy a real dogfood DB, migrate through 012, print strip leftovers.
    /// Run: `NIGHTJAR_MIGRATE_COPY=/path/to/copy.db cargo test -p nightjar-db \
    ///   migrate_copy_through_012 -- --ignored --nocapture`
    #[test]
    #[ignore = "manual: needs NIGHTJAR_MIGRATE_COPY pointing at a disposable DB copy"]
    fn migrate_copy_through_012() {
        let path = std::env::var("NIGHTJAR_MIGRATE_COPY")
            .expect("NIGHTJAR_MIGRATE_COPY must point at a disposable .db copy");
        let conn = Connection::open(&path).unwrap();
        let version_before: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let items_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM media_items", [], |r| r.get(0))
            .unwrap();
        let sidecars_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM media_item_sidecars", [], |r| r.get(0))
            .unwrap();
        eprintln!(
            "before: schema={version_before} items={items_before} sidecars={sidecars_before}"
        );
        migrate(&conn).unwrap();
        let version_after: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
                r.get(0)
            })
            .unwrap();
        let items_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM media_items", [], |r| r.get(0))
            .unwrap();
        let sidecars_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM media_item_sidecars", [], |r| r.get(0))
            .unwrap();
        let unresolved: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(paths_unresolved), 0) FROM libraries",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let abs_items: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM media_items WHERE path LIKE '/%' OR substr(path, 2, 1) = ':'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let abs_sidecars: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM media_item_sidecars WHERE path LIKE '/%' OR substr(path, 2, 1) = ':'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        eprintln!(
            "after: schema={version_after} items={items_after} sidecars={sidecars_after} \
             paths_unresolved_sum={unresolved} abs_shaped_items={abs_items} abs_shaped_sidecars={abs_sidecars}"
        );
        assert_eq!(items_before, items_after, "media_items COUNT must hold");
        assert_eq!(sidecars_before, sidecars_after, "sidecars COUNT must hold");
        assert_eq!(version_after, 19);
    }

    fn status_histogram(conn: &Connection, column: &str) -> Vec<(String, i64)> {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {column}, COUNT(*) FROM media_items GROUP BY {column} ORDER BY {column}"
            ))
            .unwrap();
        stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    /// Copy a real dogfood DB, migrate through 018, confirm the 783-row
    /// `subtitle_status = 'error'` -> `pending` reset (migration 017) moved
    /// exactly those rows and nothing else (ADR-0014 §5's before-equals-after
    /// discipline, applied in spirit — 017 is a CREATE TABLE plus a filtered
    /// UPDATE, not the ADD/DROP/RENAME copy dance §5 was written for, but the
    /// same class of "opaque error reset touching real rows at scale" migration
    /// 006 and 012 got this treatment for). Migrates through the latest schema
    /// version (19 since migration 019 landed).
    /// Run: `NIGHTJAR_MIGRATE_COPY=/path/to/copy.db cargo test -p nightjar-db \
    ///   migrate_copy_through_017 -- --ignored --nocapture`
    #[test]
    #[ignore = "manual: needs NIGHTJAR_MIGRATE_COPY pointing at a disposable DB copy"]
    fn migrate_copy_through_017() {
        let path = std::env::var("NIGHTJAR_MIGRATE_COPY")
            .expect("NIGHTJAR_MIGRATE_COPY must point at a disposable .db copy");
        let conn = Connection::open(&path).unwrap();

        let version_before: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let items_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM media_items", [], |r| r.get(0))
            .unwrap();
        let subtitle_before = status_histogram(&conn, "subtitle_status");
        let metadata_before = status_histogram(&conn, "metadata_status");
        let map_before = status_histogram(&conn, "map_status");
        eprintln!(
            "before: schema={version_before} items={items_before}\n  \
             subtitle_status={subtitle_before:?}\n  metadata_status={metadata_before:?}\n  \
             map_status={map_before:?}"
        );

        let t0 = std::time::Instant::now();
        migrate(&conn).unwrap();
        let wall = t0.elapsed();

        let version_after: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
                r.get(0)
            })
            .unwrap();
        let items_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM media_items", [], |r| r.get(0))
            .unwrap();
        let subtitle_after = status_histogram(&conn, "subtitle_status");
        let metadata_after = status_histogram(&conn, "metadata_status");
        let map_after = status_histogram(&conn, "map_status");
        let tracks_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM media_item_subtitle_tracks", [], |r| {
                r.get(0)
            })
            .unwrap();
        eprintln!(
            "after: schema={version_after} items={items_after} wall_ms={} \n  \
             subtitle_status={subtitle_after:?}\n  metadata_status={metadata_after:?}\n  \
             map_status={map_after:?}\n  subtitle_tracks_rows={tracks_after}",
            wall.as_millis()
        );

        assert_eq!(items_before, items_after, "media_items COUNT must hold");
        assert_eq!(version_after, 19);
        assert_eq!(
            metadata_before, metadata_after,
            "metadata_status must be untouched"
        );
        assert_eq!(map_before, map_after, "map_status must be untouched");
        assert_eq!(
            tracks_after, 0,
            "table created empty; probe populates it, not the migration"
        );
    }

    /// Copy a real dogfood DB, migrate through 019, confirm the two dropped
    /// subtitle source-stamp columns are gone and that `subtitle_status` and
    /// `subtitle_content_id` are unchanged for every row (ADR-0023 §6
    /// amendment: `subtitle_content_id` is the sole validity stamp for
    /// extracted subtitles, so a migration that only removes two now-unread
    /// columns must not alter any subtitle classification outcome). A
    /// column-drop migration (SQLite `ALTER TABLE ... DROP COLUMN`, same
    /// mechanism as migrations 006/014) is exactly where a `NOT NULL` or a
    /// default could quietly shift on a surviving column, so the
    /// before/after bar is broader than the two named columns: the
    /// `metadata_status`, `map_status`, and `probe_status` histograms, the
    /// `media_item_subtitle_tracks` COUNT, and the non-NULL counts of
    /// `content_id` / `map_content_id` are all asserted unchanged. The copy
    /// is made at schema 16, so migrations 17 and 18 are applied first to
    /// reach the pre-019 state; 017's `error` → `pending` reset is a real
    /// `subtitle_status` change by design and would false-positive the
    /// "nothing else changed" bar if the whole `migrate()` ran between the
    /// two snapshots. 17 and 18 are pure SQL (no Rust hook in `migrate`).
    /// Run: `NIGHTJAR_MIGRATE_COPY=/path/to/copy.db cargo test -p nightjar-db \
    ///   migrate_copy_through_019 -- --ignored --nocapture`
    #[test]
    #[ignore = "manual: needs NIGHTJAR_MIGRATE_COPY pointing at a disposable DB copy"]
    fn migrate_copy_through_019() {
        let path = std::env::var("NIGHTJAR_MIGRATE_COPY")
            .expect("NIGHTJAR_MIGRATE_COPY must point at a disposable .db copy");
        let conn = Connection::open(&path).unwrap();

        let version_before: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            version_before, 16,
            "the dogfood copy is expected to be at schema 16 (pre-017)"
        );
        for (version, sql) in MIGRATIONS.iter().filter(|(v, _)| *v == 17 || *v == 18) {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations (version) VALUES (?1)",
                [*version],
            )
            .unwrap();
        }
        let items_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM media_items", [], |r| r.get(0))
            .unwrap();
        let subtitle_before = subtitle_rows(&conn);
        let metadata_before = status_histogram(&conn, "metadata_status");
        let map_before = status_histogram(&conn, "map_status");
        let probe_before = status_histogram(&conn, "probe_status");
        let tracks_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM media_item_subtitle_tracks", [], |r| {
                r.get(0)
            })
            .unwrap();
        let content_id_nonnull_before = non_null_count(&conn, "content_id");
        let map_content_id_nonnull_before = non_null_count(&conn, "map_content_id");
        eprintln!(
            "before-019: schema=18 items={items_before}\n  \
             subtitle_rows={}\n  metadata_status={metadata_before:?}\n  \
             map_status={map_before:?}\n  probe_status={probe_before:?}\n  \
             subtitle_tracks={tracks_before} content_id_nonnull={content_id_nonnull_before} \
             map_content_id_nonnull={map_content_id_nonnull_before}",
            subtitle_before.len()
        );

        let migration_19 = MIGRATIONS
            .iter()
            .find(|(version, _)| *version == 19)
            .map(|(_, sql)| *sql)
            .unwrap();
        let t0 = std::time::Instant::now();
        conn.execute_batch(migration_19).unwrap();
        conn.execute("INSERT INTO schema_migrations (version) VALUES (19)", [])
            .unwrap();
        let wall = t0.elapsed();

        let version_after: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
                r.get(0)
            })
            .unwrap();
        let items_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM media_items", [], |r| r.get(0))
            .unwrap();
        let subtitle_after = subtitle_rows(&conn);
        let metadata_after = status_histogram(&conn, "metadata_status");
        let map_after = status_histogram(&conn, "map_status");
        let probe_after = status_histogram(&conn, "probe_status");
        let tracks_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM media_item_subtitle_tracks", [], |r| {
                r.get(0)
            })
            .unwrap();
        let content_id_nonnull_after = non_null_count(&conn, "content_id");
        let map_content_id_nonnull_after = non_null_count(&conn, "map_content_id");
        let stamp_columns_left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('media_items')
                 WHERE name IN ('subtitle_source_mtime_ms', 'subtitle_source_size_bytes')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        eprintln!(
            "after-019: schema={version_after} items={items_after} wall_ms={} \n  \
             subtitle_rows={} stamp_columns_left={}\n  \
             metadata_status={metadata_after:?}\n  map_status={map_after:?}\n  \
             probe_status={probe_after:?}\n  subtitle_tracks={tracks_after} \
             content_id_nonnull={content_id_nonnull_after} \
             map_content_id_nonnull={map_content_id_nonnull_after}",
            wall.as_millis(),
            subtitle_after.len(),
            stamp_columns_left
        );

        assert_eq!(items_before, items_after, "media_items COUNT must hold");
        assert_eq!(version_after, 19);
        assert_eq!(
            stamp_columns_left, 0,
            "both subtitle source-stamp columns are gone from media_items"
        );
        assert_eq!(
            subtitle_before, subtitle_after,
            "subtitle_status / subtitle_content_id unchanged for every row"
        );
        assert_eq!(
            metadata_before, metadata_after,
            "metadata_status histogram must not move on a column-drop rebuild"
        );
        assert_eq!(
            map_before, map_after,
            "map_status histogram must not move on a column-drop rebuild"
        );
        assert_eq!(
            probe_before, probe_after,
            "probe_status histogram must not move on a column-drop rebuild"
        );
        assert_eq!(
            tracks_before, tracks_after,
            "media_item_subtitle_tracks must be untouched by migration 019"
        );
        assert_eq!(
            content_id_nonnull_before, content_id_nonnull_after,
            "content_id nullability must not shift on the rebuild"
        );
        assert_eq!(
            map_content_id_nonnull_before, map_content_id_nonnull_after,
            "map_content_id nullability must not shift on the rebuild"
        );
    }

    /// Non-NULL row count for a `media_items` column — the nullability probe
    /// for column-drop migrations, where a rebuild can quietly add a default
    /// or flip a `NOT NULL` on a surviving column.
    fn non_null_count(conn: &Connection, column: &str) -> i64 {
        conn.query_row(
            &format!("SELECT COUNT(*) FROM media_items WHERE {column} IS NOT NULL"),
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// (id, subtitle_status, subtitle_content_id) for every row — the per-row
    /// "nothing else changed" bar for migrations that must not shift subtitle
    /// classification outcomes.
    fn subtitle_rows(conn: &Connection) -> Vec<(i64, String, String)> {
        let mut stmt = conn
            .prepare(
                "SELECT id, subtitle_status, COALESCE(subtitle_content_id, '')
                 FROM media_items ORDER BY id",
            )
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    /// Bring an in-memory DB up through every migration, with `foreign_keys`
    /// ON the way `Db::open` sets it, so cascade behaviour is the real one.
    fn migrated_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        migrate(&conn).unwrap();
        conn
    }

    /// Apply every migration up to and including `through`, without the
    /// version-specific code steps. This is how a populated pre-027 database is
    /// built: rows are inserted after the DDL exists and before 027 runs, which
    /// is exactly the upgrade shape 027 has to survive.
    fn migrate_through(conn: &Connection, through: i64) {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY NOT NULL,
                applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );
            PRAGMA foreign_keys=ON;",
        )
        .unwrap();
        for &(version, sql) in MIGRATIONS {
            if version > through {
                break;
            }
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations (version) VALUES (?1)",
                [version],
            )
            .unwrap();
        }
    }

    /// One row of `series_entity_bindings`, named rather than an 8-tuple so
    /// the assertions below read as the schema they are checking.
    #[derive(Debug, PartialEq, Eq)]
    struct BindingRow {
        library_id: i64,
        relpath: String,
        tmdb_show_id: i64,
        is_primary: i64,
        folder_seasons: (Option<i64>, Option<i64>),
        entity_seasons: (Option<i64>, Option<i64>),
    }

    fn binding_rows(conn: &Connection) -> Vec<BindingRow> {
        let mut stmt = conn
            .prepare(
                "SELECT library_id, relpath, tmdb_show_id, is_primary,
                        folder_season_start, folder_season_end,
                        entity_season_start, entity_season_end
                 FROM series_entity_bindings
                 ORDER BY relpath, tmdb_show_id",
            )
            .unwrap();
        stmt.query_map([], |r| {
            Ok(BindingRow {
                library_id: r.get(0)?,
                relpath: r.get(1)?,
                tmdb_show_id: r.get(2)?,
                is_primary: r.get(3)?,
                folder_seasons: (r.get(4)?, r.get(5)?),
                entity_seasons: (r.get(6)?, r.get(7)?),
            })
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }

    /// 021 (ADR-0046 item 2): every existing `series` row becomes exactly one
    /// primary binding, unbounded on both sides. Unbounded is the point — a
    /// backfilled row reads as "covers every folder season, numbering
    /// unchanged", which is what the folder does today, so a folder that never
    /// spans keeps behaving exactly as it does now.
    #[test]
    fn migration_21_backfills_one_unbounded_primary_per_series_row() {
        let conn = migrated_conn();
        conn.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/media/S', 'shows');
             INSERT INTO series (library_id, relpath, tmdb_show_id) VALUES
                (1, 'Will & Grace', 4454),
                (1, 'Beta', 66);",
        )
        .unwrap();

        // The backfill ran before these rows existed, so re-run its statement
        // the way a fresh migration on a populated DB would see them.
        conn.execute_batch(
            "INSERT OR IGNORE INTO series_entity_bindings
                (library_id, relpath, tmdb_show_id, is_primary,
                 folder_season_start, folder_season_end,
                 entity_season_start, entity_season_end)
             SELECT library_id, relpath, tmdb_show_id, 1, NULL, NULL, NULL, NULL
             FROM series;",
        )
        .unwrap();

        let series_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM series", [], |r| r.get(0))
            .unwrap();
        let rows = binding_rows(&conn);
        assert_eq!(
            rows.len() as i64,
            series_count,
            "exactly one binding row per series row"
        );
        assert!(
            rows.iter().all(|r| r.is_primary == 1),
            "every backfilled row is the primary binding"
        );
        assert!(
            rows.iter()
                .all(|r| r.folder_seasons == (None, None) && r.entity_seasons == (None, None)),
            "backfilled ranges are unbounded, not computed from the library"
        );
    }

    /// The backfill is a copy, so running it twice changes nothing. The
    /// migration framework already guarantees one run; this guards the
    /// statement itself, because a backfill that is not idempotent is a
    /// re-run away from duplicating every folder.
    #[test]
    fn migration_21_backfill_is_idempotent() {
        let conn = migrated_conn();
        conn.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/media/S', 'shows');
             INSERT INTO series (library_id, relpath, tmdb_show_id) VALUES (1, 'Alpha', 55);",
        )
        .unwrap();
        let backfill = "INSERT OR IGNORE INTO series_entity_bindings
                (library_id, relpath, tmdb_show_id, is_primary,
                 folder_season_start, folder_season_end,
                 entity_season_start, entity_season_end)
             SELECT library_id, relpath, tmdb_show_id, 1, NULL, NULL, NULL, NULL
             FROM series;";
        conn.execute_batch(backfill).unwrap();
        let first = binding_rows(&conn);
        conn.execute_batch(backfill).unwrap();
        let second = binding_rows(&conn);
        assert_eq!(first, second, "a second backfill changes nothing");
        assert_eq!(first.len(), 1);
    }

    /// A second entity is representable: the folder's seasons and the entity's
    /// differ, and the row round-trips unchanged. Nothing writes a row like
    /// this in production yet — slice B does — so this is where the shape is
    /// proved able to hold Will & Grace before any code depends on it.
    #[test]
    fn migration_21_holds_a_renumbered_second_entity() {
        let conn = migrated_conn();
        conn.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/media/S', 'shows');
             INSERT INTO series (library_id, relpath, tmdb_show_id)
             VALUES (1, 'Will & Grace', 4454);
             INSERT INTO series_entity_bindings
                (library_id, relpath, tmdb_show_id, is_primary,
                 folder_season_start, folder_season_end,
                 entity_season_start, entity_season_end)
             VALUES
                (1, 'Will & Grace', 4454, 1, NULL, NULL, NULL, NULL),
                (1, 'Will & Grace', 74321, 0, 9, 11, 1, 3);",
        )
        .unwrap();
        let rows = binding_rows(&conn);
        assert_eq!(
            rows,
            vec![
                BindingRow {
                    library_id: 1,
                    relpath: "Will & Grace".to_string(),
                    tmdb_show_id: 4454,
                    is_primary: 1,
                    folder_seasons: (None, None),
                    entity_seasons: (None, None),
                },
                BindingRow {
                    library_id: 1,
                    relpath: "Will & Grace".to_string(),
                    tmdb_show_id: 74321,
                    is_primary: 0,
                    folder_seasons: (Some(9), Some(11)),
                    entity_seasons: (Some(1), Some(3)),
                },
            ],
            "folder seasons 9-11 map to entity seasons 1-3, stored as a range \
             and not as an offset"
        );
    }

    /// One primary per folder, enforced by the partial index rather than by
    /// convention. Two primaries would make "which entity names the unit"
    /// ambiguous at the storage layer, which is the question ADR-0046 item 5
    /// leaves open — open at the product level, not undecidable at the schema.
    #[test]
    fn migration_21_rejects_a_second_primary_for_one_folder() {
        let conn = migrated_conn();
        conn.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/media/S', 'shows');
             INSERT INTO series (library_id, relpath, tmdb_show_id) VALUES (1, 'Alpha', 55);
             INSERT INTO series_entity_bindings
                (library_id, relpath, tmdb_show_id, is_primary)
             VALUES (1, 'Alpha', 55, 1);",
        )
        .unwrap();
        let err = conn
            .execute_batch(
                "INSERT INTO series_entity_bindings
                    (library_id, relpath, tmdb_show_id, is_primary)
                 VALUES (1, 'Alpha', 999, 1);",
            )
            .unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("unique"),
            "second primary must be rejected, got: {err}"
        );

        // A second *non*-primary binding for the same folder is the whole
        // point of the table and must still be allowed.
        conn.execute_batch(
            "INSERT INTO series_entity_bindings
                (library_id, relpath, tmdb_show_id, is_primary,
                 folder_season_start, folder_season_end,
                 entity_season_start, entity_season_end)
             VALUES (1, 'Alpha', 999, 0, 9, 11, 1, 3);",
        )
        .unwrap();
        assert_eq!(binding_rows(&conn).len(), 2);
    }

    /// Deleting a library cascades through `series` into its bindings. Without
    /// this the table accumulates rows pointing at folders that no longer
    /// exist, and a later folder at the same relpath inherits them.
    #[test]
    fn migration_21_bindings_cascade_when_the_library_goes() {
        let conn = migrated_conn();
        conn.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/media/S', 'shows');
             INSERT INTO series (library_id, relpath, tmdb_show_id) VALUES (1, 'Alpha', 55);
             INSERT INTO series_entity_bindings
                (library_id, relpath, tmdb_show_id, is_primary)
             VALUES (1, 'Alpha', 55, 1);",
        )
        .unwrap();
        assert_eq!(binding_rows(&conn).len(), 1);
        conn.execute_batch("DELETE FROM libraries WHERE id = 1;")
            .unwrap();
        assert_eq!(
            binding_rows(&conn).len(),
            0,
            "bindings go with the series rows they belong to"
        );
    }

    /// A binding must belong to a `series` row. The composite foreign key is
    /// what makes `(library_id, relpath)` mean the same thing in both tables.
    #[test]
    fn migration_21_rejects_a_binding_with_no_series_row() {
        let conn = migrated_conn();
        conn.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/media/S', 'shows');",
        )
        .unwrap();
        let err = conn
            .execute_batch(
                "INSERT INTO series_entity_bindings
                    (library_id, relpath, tmdb_show_id, is_primary)
                 VALUES (1, 'Nonexistent', 55, 1);",
            )
            .unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("foreign key"),
            "expected a foreign key violation, got: {err}"
        );
    }

    /// ADR-0039 item 3. 027 is the rebuild that drops a NOT NULL, and a rebuild
    /// is the one migration shape that can lose rows silently — here with a
    /// second table cascading from it. This is the dogfood-shaped upgrade: rows
    /// already present, bound, some with a second entity, and every one of them
    /// must survive the copy.
    #[test]
    fn migration_27_keeps_every_series_row_binding_and_allows_a_null() {
        let c = Connection::open_in_memory().unwrap();
        migrate_through(&c, 26);
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/S', 'shows');
             INSERT INTO series (library_id, relpath, tmdb_show_id) VALUES
               (1, 'Shameless (US) (2011)', 34343),
               (1, 'Shameless (UK) (2004)', 20610),
               (1, 'Alpha', 55);
             INSERT INTO series_entity_bindings
                (library_id, relpath, tmdb_show_id, is_primary,
                 folder_season_start, folder_season_end,
                 entity_season_start, entity_season_end)
             VALUES
                (1, 'Alpha', 55, 1, NULL, NULL, NULL, NULL),
                (1, 'Alpha', 77, 0, 9, 11, 1, 3);",
        )
        .unwrap();
        let before_series = count_table(&c, "series").unwrap();
        let before_bindings = binding_rows(&c);

        migrate(&c).unwrap();

        assert_eq!(
            count_table(&c, "series").unwrap(),
            before_series,
            "no series row lost"
        );
        assert_eq!(
            binding_rows(&c),
            before_bindings,
            "no binding lost or rewritten by the parent rebuild"
        );

        // The point of the rebuild: a folder can now have a row and no entity.
        c.execute(
            "INSERT INTO series (library_id, relpath, tmdb_show_id) VALUES (1, 'Unbound', NULL)",
            [],
        )
        .unwrap();
        let unbound: Option<i64> = c
            .query_row(
                "SELECT tmdb_show_id FROM series WHERE relpath = 'Unbound'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unbound, None);

        // The child foreign key survived the parent rebuild: a binding with no
        // series row is still refused.
        let err = c
            .execute(
                "INSERT INTO series_entity_bindings
                    (library_id, relpath, tmdb_show_id, is_primary)
                 VALUES (1, 'Nonexistent', 99, 1)",
                [],
            )
            .unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("foreign key"),
            "expected a foreign key violation, got: {err}"
        );
    }

    /// The primary key and the library cascade are carried over by the rebuild,
    /// not re-stated by luck. Both would be easy to drop while copying the DDL,
    /// and the cascade now runs through `series` into its bindings.
    #[test]
    fn migration_27_keeps_the_primary_key_and_the_cascade() {
        let c = Connection::open_in_memory().unwrap();
        migrate_through(&c, 26);
        migrate(&c).unwrap();
        c.execute_batch(
            "PRAGMA foreign_keys = ON;
             INSERT INTO libraries (name, path, kind) VALUES ('S', '/S', 'shows');
             INSERT INTO series (library_id, relpath, tmdb_show_id) VALUES (1, 'Alpha', 55);
             INSERT INTO series_entity_bindings
                (library_id, relpath, tmdb_show_id, is_primary)
             VALUES (1, 'Alpha', 55, 1);",
        )
        .unwrap();
        let dup = c.execute(
            "INSERT INTO series (library_id, relpath, tmdb_show_id) VALUES (1, 'Alpha', 99)",
            [],
        );
        assert!(dup.is_err(), "(library_id, relpath) is still unique");

        c.execute("DELETE FROM libraries WHERE id = 1", []).unwrap();
        assert_eq!(
            count_table(&c, "series").unwrap(),
            0,
            "the row cascades with its library"
        );
        assert_eq!(
            count_table(&c, "series_entity_bindings").unwrap(),
            0,
            "and its bindings cascade through it"
        );
    }

    /// A fresh install reaches 027 with no rows, and the rebuilt table takes a
    /// null binding immediately. `migrates_fresh_db` pins the version; this is
    /// the shape a brand-new library will actually write.
    #[test]
    fn migration_27_on_a_fresh_install_allows_a_null_binding() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/S', 'shows');
             INSERT INTO series (library_id, relpath, tmdb_show_id) VALUES (1, 'Alpha', NULL);",
        )
        .unwrap();
        let nulls: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM series WHERE tmdb_show_id IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(nulls, 1);
    }

    /// 029 rebuilds `accounts` to give all three policy columns the same
    /// positivity CHECK. The rebuild must not lose accounts, profiles or login
    /// sessions, and AUTOINCREMENT must continue past the copied ids.
    #[test]
    fn migration_29_rebuilds_accounts_without_losing_children() {
        let conn = Connection::open_in_memory().unwrap();
        migrate_through(&conn, 28);
        conn.execute_batch(
            "INSERT INTO accounts (id, username, password_hash, role, max_concurrent_sessions)
                 VALUES (1, 'a', 'h', 'owner', 2), (2, 'b', 'h', 'member', NULL);
             INSERT INTO profiles (id, account_id, profile_ref, name)
                 VALUES (1, 1, 'r1', 'P1');
             INSERT INTO login_sessions
                 (account_id, active_profile_id, token_sha256, client_label, expires_at)
                 VALUES (1, 1, 'tok', 'tv', '2099-01-01T00:00:00.000Z');",
        )
        .unwrap();

        migrate(&conn).unwrap();

        let accounts: i64 = conn
            .query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))
            .unwrap();
        let profiles: i64 = conn
            .query_row("SELECT COUNT(*) FROM profiles", [], |r| r.get(0))
            .unwrap();
        let sessions: i64 = conn
            .query_row("SELECT COUNT(*) FROM login_sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!((accounts, profiles, sessions), (2, 1, 1));
        // The existing concurrency value survived the rebuild.
        let kept: Option<i64> = conn
            .query_row(
                "SELECT max_concurrent_sessions FROM accounts WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, Some(2));

        // AUTOINCREMENT continues past the copied ids rather than reusing one.
        conn.execute(
            "INSERT INTO accounts (username, password_hash, role) VALUES ('c', 'h', 'member')",
            [],
        )
        .unwrap();
        let new_id: i64 = conn
            .query_row("SELECT id FROM accounts WHERE username = 'c'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            new_id > 2,
            "AUTOINCREMENT continues past the copy: {new_id}"
        );
    }

    /// Rewind an already-migrated database to just before migration 25, the
    /// way a real install upgrading into it looks. Migrations 26 through 29
    /// and their schema are removed with it, so `migrate` sees a database at
    /// version 24 and applies all five in order.
    ///
    /// Migration 28 adds columns to `profiles`, so the rewind rebuilds that
    /// table rather than dropping the columns: SQLite refuses to drop a column
    /// a CHECK constraint names, and `subtitle_default` has one.
    fn rewind_to_24(conn: &Connection) {
        conn.execute_batch(
            "DROP INDEX IF EXISTS idx_accounts_username_nocase;
             DROP INDEX IF EXISTS idx_watch_state_recent;
             DROP TABLE IF EXISTS watch_state;
             DROP TABLE IF EXISTS profile_track_choice;
             CREATE TABLE profiles_rewind (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                 profile_ref TEXT NOT NULL UNIQUE,
                 name TEXT NOT NULL,
                 classification_cap TEXT,
                 simple_interface INTEGER NOT NULL DEFAULT 0
                     CHECK (simple_interface IN (0, 1)),
                 created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             );
             INSERT INTO profiles_rewind
                 (id, account_id, profile_ref, name, classification_cap,
                  simple_interface, created_at)
             SELECT id, account_id, profile_ref, name, classification_cap,
                    simple_interface, created_at FROM profiles;
             DROP TABLE profiles;
             ALTER TABLE profiles_rewind RENAME TO profiles;
             CREATE INDEX idx_profiles_account ON profiles(account_id);
             DELETE FROM schema_migrations WHERE version IN (25, 26, 27, 28, 29);",
        )
        .unwrap();
    }

    /// OPEN-DEFECTS entry 14. An install that already holds `Root` and `root`
    /// cannot have the index built over it, and this migration will not pick a
    /// winner: the rows are two people's logins, with profiles and watch state
    /// hanging off each id.
    #[test]
    fn migration_25_refuses_an_install_whose_usernames_already_collide() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        rewind_to_24(&conn);
        conn.execute_batch(
            "INSERT INTO accounts (id, username, password_hash, role)
                 VALUES (1, 'Root', 'h', 'owner'), (2, 'root', 'h', 'member');",
        )
        .unwrap();

        let err = migrate(&conn).expect_err("a colliding install must refuse");
        assert!(err.contains("migration 25 refused"), "{err}");
        // **Both rows named, with their ids.** An operator cannot act on
        // "a collision exists".
        assert!(err.contains("1 'Root'"), "{err}");
        assert!(err.contains("2 'root'"), "{err}");
        // Refused, not half-applied.
        let applied: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version = 25",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(applied, 0, "the refused migration must not record itself");
    }

    /// The control for the check above: the same rewind, the same two
    /// accounts, names that do not collide — and the migration applies.
    /// Without this, a check that refused everything would read as correct.
    #[test]
    fn migration_25_applies_when_no_usernames_collide() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        rewind_to_24(&conn);
        conn.execute_batch(
            "INSERT INTO accounts (id, username, password_hash, role)
                 VALUES (1, 'Root', 'h', 'owner'), (2, 'rooter', 'h', 'member');",
        )
        .unwrap();

        migrate(&conn).expect("no collision, so the migration applies");
        let applied: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version = 25",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(applied, 1);
        // And the index it added is doing its job afterwards.
        let dup = conn.execute_batch(
            "INSERT INTO accounts (username, password_hash, role)
                 VALUES ('ROOT', 'h', 'member');",
        );
        assert!(dup.is_err(), "the new index must refuse a case-variant");
    }

    /// 026 (ADR-0035 item 1). An install that already holds accounts and
    /// profiles upgrades in place: the table arrives empty, `(profile_id,
    /// item_key)` is the primary key, the boolean flags are closed, and
    /// deleting a profile takes its watch state with it.
    #[test]
    fn migration_26_adds_watch_state_and_cascades_on_profile_delete() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY NOT NULL,
                applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );",
        )
        .unwrap();
        for &(version, sql) in MIGRATIONS.iter().take(25) {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations (version) VALUES (?1)",
                [version],
            )
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO accounts (id, username, password_hash, role)
                 VALUES (1, 'a', 'x', 'owner');
             INSERT INTO profiles (id, account_id, profile_ref, name)
                 VALUES (1, 1, 'aa', 'P');
             INSERT INTO libraries (name, path, kind) VALUES ('t', '/tmp/t', 'movies');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind)
                 VALUES (1, 'a.mkv', 1, 1, 'A', 'movie');",
        )
        .unwrap();

        migrate(&conn).unwrap();

        assert_eq!(
            count(&conn, "watch_state"),
            0,
            "the new table arrives empty"
        );
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute(
            "INSERT INTO watch_state
                (profile_id, item_key, position_ms, duration_ms, played, hidden,
                 first_played_at, last_played_at)
             VALUES (1, 'path:1:a.mkv', 1000, 10000, 0, 0,
                     '2026-09-12T00:00:00.000Z', '2026-09-12T00:00:00.000Z')",
            [],
        )
        .unwrap();
        let duplicate = conn.execute(
            "INSERT INTO watch_state
                (profile_id, item_key, position_ms, duration_ms, played, hidden,
                 first_played_at, last_played_at)
             VALUES (1, 'path:1:a.mkv', 2000, 10000, 0, 0,
                     '2026-09-12T00:00:00.000Z', '2026-09-12T00:00:00.000Z')",
            [],
        );
        assert!(
            duplicate.is_err(),
            "(profile_id, item_key) is the primary key"
        );
        let bad_played = conn.execute(
            "INSERT INTO watch_state
                (profile_id, item_key, position_ms, duration_ms, played, hidden,
                 first_played_at, last_played_at)
             VALUES (1, 'path:1:b.mkv', 2000, 10000, 2, 0,
                     '2026-09-12T00:00:00.000Z', '2026-09-12T00:00:00.000Z')",
            [],
        );
        assert!(bad_played.is_err(), "played is a 0/1 flag");

        conn.execute("DELETE FROM profiles WHERE id = 1", [])
            .unwrap();
        assert_eq!(
            count(&conn, "watch_state"),
            0,
            "profile deletion takes its watch state"
        );
    }
}
