//! File↔item join (ADR-0029 §2). Provider keys only; path keys are derived.

use rusqlite::{Connection, OptionalExtension, Transaction, params};

/// Upsert one provider binding. Does not store path keys.
pub fn upsert_link(
    tx: &Transaction<'_>,
    media_item_id: i64,
    item_key: &str,
    manually_matched: bool,
) -> Result<(), String> {
    tx.execute(
        "INSERT INTO media_item_links (media_item_id, item_key, manually_matched)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(media_item_id, item_key) DO UPDATE SET
            manually_matched = CASE
                WHEN media_item_links.manually_matched = 1 THEN 1
                ELSE excluded.manually_matched
            END",
        params![media_item_id, item_key, manually_matched as i32],
    )
    .map_err(|e| format!("upsert media_item_link: {e}"))?;
    Ok(())
}

/// Replace automatic provider bindings for a file (leaves manually_matched rows).
pub fn replace_auto_link(
    tx: &Transaction<'_>,
    media_item_id: i64,
    item_key: &str,
) -> Result<(), String> {
    replace_auto_links(tx, media_item_id, &[item_key.to_string()])
}

/// Replace automatic bindings with zero or more provider keys (ADR-0025 §2
/// multi-episode file → multiple `item_key`s on one media row).
pub fn replace_auto_links(
    tx: &Transaction<'_>,
    media_item_id: i64,
    item_keys: &[String],
) -> Result<(), String> {
    tx.execute(
        "DELETE FROM media_item_links
         WHERE media_item_id = ?1 AND manually_matched = 0",
        params![media_item_id],
    )
    .map_err(|e| format!("clear auto links: {e}"))?;
    for key in item_keys {
        upsert_link(tx, media_item_id, key, false)?;
    }
    Ok(())
}

pub fn delete_links_for_item_keys(
    tx: &Transaction<'_>,
    item_keys: &[String],
) -> Result<(), String> {
    if item_keys.is_empty() {
        return Ok(());
    }
    let mut stmt = tx
        .prepare("DELETE FROM media_item_links WHERE item_key = ?1")
        .map_err(|e| format!("prepare delete links: {e}"))?;
    for key in item_keys {
        stmt.execute(params![key])
            .map_err(|e| format!("delete link {key}: {e}"))?;
    }
    Ok(())
}

pub fn link_keys_for_item(conn: &Connection, media_item_id: i64) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare("SELECT item_key FROM media_item_links WHERE media_item_id = ?1")
        .map_err(|e| format!("prepare link keys: {e}"))?;
    let rows = stmt
        .query_map(params![media_item_id], |r| r.get(0))
        .map_err(|e| format!("query link keys: {e}"))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| format!("link key row: {e}"))?);
    }
    Ok(out)
}

/// Effective item_key: provider binding if present, else derived path key.
///
/// `relpath` is the library-relative path (ADR-0025 §4). Callers must not
/// pass an absolute filesystem path — there is no root strip here.
pub fn effective_item_key(
    conn: &Connection,
    media_item_id: i64,
    library_id: i64,
    relpath: &str,
) -> Result<String, String> {
    let keyed: Option<String> = conn
        .query_row(
            "SELECT item_key FROM media_item_links
             WHERE media_item_id = ?1
             ORDER BY manually_matched DESC, item_key
             LIMIT 1",
            params![media_item_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("effective item_key: {e}"))?;
    Ok(keyed.unwrap_or_else(|| path_item_key(library_id, relpath)))
}

pub fn path_item_key(library_id: i64, relpath: &str) -> String {
    format!("path:{library_id}:{relpath}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightjar_db::migrate;
    use rusqlite::Connection;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('L', '/L', 'movies');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind)
             VALUES (1, 'a.mkv', 1, 1, 'A', 'movie');",
        )
        .unwrap();
        c
    }

    #[test]
    fn path_key_when_no_binding() {
        let c = mem();
        assert_eq!(
            effective_item_key(&c, 1, 1, "a.mkv").unwrap(),
            "path:1:a.mkv"
        );
    }

    #[test]
    fn provider_binding_wins() {
        let c = mem();
        let tx = c.unchecked_transaction().unwrap();
        upsert_link(&tx, 1, "tmdb:movie:550", false).unwrap();
        tx.commit().unwrap();
        assert_eq!(
            effective_item_key(&c, 1, 1, "a.mkv").unwrap(),
            "tmdb:movie:550"
        );
    }

    #[test]
    fn replace_auto_links_keeps_two_keys() {
        let c = mem();
        let tx = c.unchecked_transaction().unwrap();
        replace_auto_links(&tx, 1, &["tmdb:episode:1".into(), "tmdb:episode:2".into()]).unwrap();
        tx.commit().unwrap();
        let mut keys = link_keys_for_item(&c, 1).unwrap();
        keys.sort();
        assert_eq!(keys, vec!["tmdb:episode:1", "tmdb:episode:2"]);
    }
}
