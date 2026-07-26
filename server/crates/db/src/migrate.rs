use rusqlite::Connection;

const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../migrations/001_init.sql")),
    (2, include_str!("../migrations/002_scan_jobs.sql")),
    (3, include_str!("../migrations/003_subtitle_sidecars.sql")),
    (4, include_str!("../migrations/004_audio_channels.sql")),
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
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("begin migration {version}: {e}"))?;
        tx.execute_batch(sql)
            .map_err(|e| format!("apply migration {version}: {e}"))?;
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
        assert_eq!(v, 4);
        migrate(&conn).unwrap(); // idempotent
    }

    /// Append-only migrations run against libraries that already have rows,
    /// so an added column must not disturb them.
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
    }
}
