use rusqlite::{Connection, params};
use std::path::Path;

use crate::paths::{normalize_library_root, to_relpath};

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

        tx.execute(
            "INSERT INTO schema_migrations (version) VALUES (?1)",
            [version],
        )
        .map_err(|e| format!("record migration {version}: {e}"))?;
        tx.commit()
            .map_err(|e| format!("commit migration {version}: {e}"))?;
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

fn count_table(conn: &Connection, table: &str) -> Result<i64, String> {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .map_err(|e| format!("count {table}: {e}"))
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
        assert_eq!(v, 13);
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
        let has_reachable: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('libraries') WHERE name = 'reachable'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_reachable, 1);
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
        assert_eq!(version_after, 13);
    }
}
