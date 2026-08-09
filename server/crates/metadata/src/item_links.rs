//! File↔item join (ADR-0029 §2). Provider keys only; path keys are derived.

use rusqlite::{Connection, Transaction, params};
use std::collections::{HashMap, HashSet};

/// The `item_key` / `series_key` prefixes (ADR-0025 §1, ADR-0039 item 2).
///
/// Declared here because this module owns the key grammar. Readers elsewhere
/// import them rather than spelling the literals again, so there is one place
/// the grammar is written down (Rule 4.11). The grammar stays opaque on the
/// wire either way: these are for the server's own use, not a parse contract.
pub const EPISODE_KEY_PREFIX: &str = "tmdb:episode:";
pub const MOVIE_KEY_PREFIX: &str = "tmdb:movie:";
pub const SHOW_KEY_PREFIX: &str = "tmdb:show:";
pub const FOLDER_KEY_PREFIX: &str = "folder:";
pub const PATH_KEY_PREFIX: &str = "path:";

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

/// Drop every provider binding for a media file (clear-match / reassign prep).
pub fn clear_all_links_for_media_item(
    tx: &Transaction<'_>,
    media_item_id: i64,
) -> Result<(), String> {
    tx.execute(
        "DELETE FROM media_item_links WHERE media_item_id = ?1",
        params![media_item_id],
    )
    .map_err(|e| format!("clear all links: {e}"))?;
    Ok(())
}

/// Mark all links for a media file as manually matched (ADR-0028).
pub fn set_manually_matched(tx: &Transaction<'_>, media_item_id: i64) -> Result<(), String> {
    tx.execute(
        "UPDATE media_item_links SET manually_matched = 1 WHERE media_item_id = ?1",
        params![media_item_id],
    )
    .map_err(|e| format!("set manually_matched: {e}"))?;
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

/// Watch-state keys only (ADR-0025). Provisional enrich handles such as
/// `tmdb:show:{id}` (two-tier matched TV before season bind) are stored as
/// links for id recovery but must not own watch history.
pub fn is_watch_item_key(key: &str) -> bool {
    key.starts_with("tmdb:movie:")
        || key.starts_with("tmdb:episode:")
        || key.starts_with("path:")
        || key.starts_with("tvdb:")
}

/// Effective item_key: watch-shaped provider binding if present, else path key.
///
/// `relpath` is the library-relative path (ADR-0025 §4). Callers must not
/// pass an absolute filesystem path — there is no root strip here.
pub fn effective_item_key(
    conn: &Connection,
    media_item_id: i64,
    library_id: i64,
    relpath: &str,
) -> Result<String, String> {
    let mut stmt = conn
        .prepare(
            "SELECT item_key FROM media_item_links
             WHERE media_item_id = ?1
             ORDER BY manually_matched DESC, item_key",
        )
        .map_err(|e| format!("prepare effective item_key: {e}"))?;
    let rows = stmt
        .query_map(params![media_item_id], |r| r.get::<_, String>(0))
        .map_err(|e| format!("query effective item_key: {e}"))?;
    for row in rows {
        let k = row.map_err(|e| format!("item_key row: {e}"))?;
        if is_watch_item_key(&k) {
            return Ok(k);
        }
    }
    Ok(path_item_key(library_id, relpath))
}

pub fn path_item_key(library_id: i64, relpath: &str) -> String {
    format!("path:{library_id}:{relpath}")
}

/// Bulk [`effective_item_key`] for a whole library, keyed by media item id.
///
/// Same rule as the single-item form: the first watch-shaped provider binding
/// in `(manually_matched DESC, item_key)` order, else the path key. The two
/// share [`is_watch_item_key`] and that ordering deliberately, and
/// `bulk_agrees_with_single` pins them together, because a listing that keyed
/// items differently from the rest of the server would be a second answer to
/// "what is this item" (Rule 4.11).
pub fn effective_item_keys_for_library(
    conn: &Connection,
    library_id: i64,
) -> Result<HashMap<i64, String>, String> {
    let mut keys = HashMap::new();
    let mut items = conn
        .prepare("SELECT id, path FROM media_items WHERE library_id = ?1")
        .map_err(|e| format!("prepare library item keys: {e}"))?;
    let rows = items
        .query_map(params![library_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(|e| format!("query library item keys: {e}"))?;
    for row in rows {
        let (id, path) = row.map_err(|e| format!("library item key row: {e}"))?;
        keys.insert(id, path_item_key(library_id, &path));
    }

    let mut links = conn
        .prepare(
            "SELECT l.media_item_id, l.item_key
             FROM media_item_links l
             JOIN media_items m ON m.id = l.media_item_id
             WHERE m.library_id = ?1
             ORDER BY l.media_item_id, l.manually_matched DESC, l.item_key",
        )
        .map_err(|e| format!("prepare library links: {e}"))?;
    let rows = links
        .query_map(params![library_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(|e| format!("query library links: {e}"))?;
    let mut bound: HashSet<i64> = HashSet::new();
    for row in rows {
        let (id, key) = row.map_err(|e| format!("library link row: {e}"))?;
        if is_watch_item_key(&key) && bound.insert(id) {
            keys.insert(id, key);
        }
    }
    Ok(keys)
}

/// ADR-0039 item 2: the series key of a show folder.
///
/// The grammar is documented for debugging and is not a parse contract; the
/// key is opaque on the wire, permanently. It is derived from the folder's
/// `series` row rather than stored, so a folder that binds later does not
/// leave a stale key behind (ADR-0039 item 5).
pub fn series_key_for_show_folder(
    library_id: i64,
    show_folder: &str,
    tmdb_show_id: Option<i64>,
) -> String {
    match tmdb_show_id {
        Some(id) => format!("tmdb:show:{id}"),
        None => format!("folder:{library_id}:{show_folder}"),
    }
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

    #[test]
    fn bulk_agrees_with_single() {
        let c = mem();
        c.execute_batch(
            "INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind)
             VALUES (1, 'b.mkv', 1, 1, 'B', 'movie'),
                    (1, 'c.mkv', 1, 1, 'C', 'movie');",
        )
        .unwrap();
        let tx = c.unchecked_transaction().unwrap();
        // A provider binding, a manual binding that must outrank an automatic
        // one, and a non-watch `tmdb:show:` link that must lose to the path key.
        upsert_link(&tx, 1, "tmdb:movie:550", false).unwrap();
        upsert_link(&tx, 2, "tmdb:movie:11", false).unwrap();
        upsert_link(&tx, 2, "tmdb:movie:99", true).unwrap();
        upsert_link(&tx, 3, "tmdb:show:1396", false).unwrap();
        tx.commit().unwrap();

        let bulk = effective_item_keys_for_library(&c, 1).unwrap();
        for (id, relpath) in [(1, "a.mkv"), (2, "b.mkv"), (3, "c.mkv")] {
            assert_eq!(
                bulk.get(&id).map(String::as_str),
                Some(effective_item_key(&c, id, 1, relpath).unwrap().as_str()),
                "item {id}"
            );
        }
        assert_eq!(bulk[&2], "tmdb:movie:99", "manual binding wins");
        assert_eq!(bulk[&3], "path:1:c.mkv", "tmdb:show: is not a watch key");
    }

    /// ADR-0039 item 2: bound folders key on the entity, unbound ones on the
    /// folder, and an unbound folder at the library root keys on the empty
    /// relpath rather than dropping out of the grammar.
    #[test]
    fn series_key_grammar() {
        assert_eq!(
            series_key_for_show_folder(2, "Shameless (US)", Some(1396)),
            "tmdb:show:1396"
        );
        assert_eq!(
            series_key_for_show_folder(2, "Shameless (US)", None),
            "folder:2:Shameless (US)"
        );
        assert_eq!(series_key_for_show_folder(2, "", None), "folder:2:");
        // The D2 class: two fold-colliding folders are two keys.
        assert_ne!(
            series_key_for_show_folder(2, "Shameless (US)", None),
            series_key_for_show_folder(2, "Shameless (UK)", None)
        );
    }

    #[test]
    fn show_key_is_not_a_watch_key() {
        // Provisional `tmdb:show:` links (ADR-0026 §8.4) recover ids on the
        // art path only; they must never own watch state.
        assert!(!is_watch_item_key("tmdb:show:1396"));
        assert!(is_watch_item_key("tmdb:movie:550"));
        assert!(is_watch_item_key("tmdb:episode:1"));
        assert!(is_watch_item_key("path:1:a.mkv"));
        assert!(is_watch_item_key("tvdb:123"));
    }
}
