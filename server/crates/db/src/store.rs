use crate::migrate;
use crate::paths::{
    fold_path, is_absolute_stored, require_library_root, require_relpath, resolve_media_path,
    to_relpath,
};
use crate::status::{parse_map_status, parse_probe_status, parse_subtitle_status};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

pub struct Db {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct NewLibrary {
    pub name: String,
    pub path: String,
    pub kind: String,
}

#[derive(Debug, Clone)]
pub struct LibraryRow {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub kind: String,
    pub item_count: i64,
    /// ADR-0014: false when the library root is not reachable.
    pub reachable: bool,
    /// ADR-0030: rows still absolute after migration / pending repair.
    pub paths_unresolved: i64,
    /// ADR-0030: last index pass skipped (outside root) count.
    pub skipped_outside_root: i64,
}

#[derive(Debug, Clone)]
pub struct MediaItemRow {
    pub id: i64,
    pub library_id: i64,
    pub path: String,
    pub mtime_ms: i64,
    pub size_bytes: i64,
    pub title: String,
    pub kind: String,
    pub year: Option<i32>,
    pub season: Option<i32>,
    pub episode: Option<i32>,
    pub duration_ms: Option<i64>,
    pub container: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub audio_channels: Option<i64>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    /// Video stream bitrate from ffprobe (ADR-0022); NULL until probed.
    pub video_bitrate_bps: Option<i64>,
    /// Source HDR: `none` | `hdr10` | `dolby_vision` | `dolby_vision_p5` (ADR-0022).
    pub hdr: Option<String>,
    pub probe_status: String,
    pub scan_error: Option<String>,
    pub subtitle_status: String,
    pub subtitle_source_mtime_ms: Option<i64>,
    pub subtitle_source_size_bytes: Option<i64>,
    /// ADR-0023 live media-file fingerprint (NULL until scan computes it).
    pub content_id: Option<String>,
    pub probed_content_id: Option<String>,
    pub subtitle_content_id: Option<String>,
    pub usable_extent_ms: Option<i64>,
    pub usable_extent_content_id: Option<String>,
    pub map_status: String,
    pub map_content_id: Option<String>,
}

/// A stored keyframe map (ADR-0023 §7) whose stamps match live identity.
#[derive(Debug, Clone)]
pub struct KeyframeMapRows {
    pub container_kind: String,
    pub content_id: String,
    /// `(pts_ms, byte_offset)` ordered by `pts_ms`.
    pub entries: Vec<(i64, i64)>,
}

/// Index-pass upsert: codecs left null, probe_status = indexed.
#[derive(Debug, Clone)]
pub struct UpsertItem {
    pub path: String,
    pub mtime_ms: i64,
    pub size_bytes: i64,
    pub title: String,
    pub kind: String,
    pub year: Option<i32>,
    pub season: Option<i32>,
    pub episode: Option<i32>,
    /// ADR-0023 fingerprint; None only when the read failed (row stays NULL).
    pub content_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProbeUpdate {
    pub item_id: i64,
    pub duration_ms: Option<i64>,
    pub container: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub audio_channels: Option<i64>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub video_bitrate_bps: Option<i64>,
    pub hdr: Option<String>,
    pub probe_status: String,
    pub scan_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ScanJobRow {
    pub id: i64,
    pub library_id: i64,
    pub state: String,
    pub added: i64,
    pub updated: i64,
    pub removed: i64,
    pub unchanged: i64,
    pub probed: i64,
    pub errors: i64,
    pub index_duration_ms: Option<i64>,
    pub probe_duration_ms: Option<i64>,
    pub error_message: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
    /// `scan` | `repoint` (ADR-0030).
    pub kind: String,
    pub candidate_path: Option<String>,
    pub skipped_outside_root: i64,
    /// Rows that would have been deleted on a repoint's first index; still present.
    pub deferred_remove: i64,
}

/// One row for fold-aware index matching (ADR-0030 §2).
#[derive(Debug, Clone)]
pub struct ItemPathRow {
    pub id: i64,
    pub path: String,
    pub mtime_ms: i64,
    pub probe_status: String,
}

/// Filesystem subtitle sidecar stored at index time (ADR-0010).
#[derive(Debug, Clone)]
pub struct SidecarRow {
    pub media_item_id: i64,
    pub track_id: String,
    pub path: String,
    pub mtime_ms: i64,
    pub size_bytes: i64,
    pub format: String,
    pub language: Option<String>,
    pub forced: bool,
    pub sdh: bool,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;
             PRAGMA busy_timeout=5000;",
        )
        .map_err(|e| format!("pragma setup: {e}"))?;
        migrate::migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, String> {
        self.conn
            .lock()
            .map_err(|_| "database lock poisoned".to_string())
    }

    pub fn create_library(&self, lib: &NewLibrary) -> Result<LibraryRow, String> {
        let root = require_library_root(&lib.path)?;
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO libraries (name, path, kind) VALUES (?1, ?2, ?3)",
            params![lib.name, root, lib.kind],
        )
        .map_err(|e| format!("insert library: {e}"))?;
        let id = conn.last_insert_rowid();
        Ok(LibraryRow {
            id,
            name: lib.name.clone(),
            path: root,
            kind: lib.kind.clone(),
            item_count: 0,
            reachable: true,
            paths_unresolved: 0,
            skipped_outside_root: 0,
        })
    }

    pub fn list_libraries(&self) -> Result<Vec<LibraryRow>, String> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT l.id, l.name, l.path, l.kind,
                        (SELECT COUNT(*) FROM media_items m WHERE m.library_id = l.id),
                        l.reachable, l.paths_unresolved, l.skipped_outside_root
                 FROM libraries l
                 ORDER BY l.name COLLATE NOCASE",
            )
            .map_err(|e| format!("prepare list libraries: {e}"))?;
        let rows = stmt
            .query_map([], map_library)
            .map_err(|e| format!("query libraries: {e}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("read libraries: {e}"))
    }

    pub fn get_library(&self, id: i64) -> Result<Option<LibraryRow>, String> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT l.id, l.name, l.path, l.kind,
                    (SELECT COUNT(*) FROM media_items m WHERE m.library_id = l.id),
                    l.reachable, l.paths_unresolved, l.skipped_outside_root
             FROM libraries l WHERE l.id = ?1",
            [id],
            map_library,
        )
        .optional()
        .map_err(|e| format!("get library {id}: {e}"))
    }

    pub fn update_library_name(&self, library_id: i64, name: &str) -> Result<(), String> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE libraries SET name = ?2 WHERE id = ?1",
            params![library_id, name],
        )
        .map_err(|e| format!("update library name: {e}"))?;
        Ok(())
    }

    pub fn update_library_path(&self, library_id: i64, path: &str) -> Result<(), String> {
        let root = require_library_root(path)?;
        let conn = self.lock()?;
        conn.execute(
            "UPDATE libraries SET path = ?2 WHERE id = ?1",
            params![library_id, root],
        )
        .map_err(|e| format!("update library path: {e}"))?;
        Ok(())
    }

    pub fn set_library_path_counters(
        &self,
        library_id: i64,
        paths_unresolved: i64,
        skipped_outside_root: i64,
    ) -> Result<(), String> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE libraries SET paths_unresolved = ?2, skipped_outside_root = ?3
             WHERE id = ?1",
            params![library_id, paths_unresolved, skipped_outside_root],
        )
        .map_err(|e| format!("set library path counters: {e}"))?;
        Ok(())
    }

    /// Strip remaining absolute rows that now match `library.path` (ADR-0030 §5).
    pub fn repair_library_paths(&self, library_id: i64) -> Result<i64, String> {
        let lib = self
            .get_library(library_id)?
            .ok_or_else(|| format!("library {library_id} not found"))?;
        let conn = self.lock()?;
        let items: Vec<(i64, String)> = {
            let mut stmt = conn
                .prepare("SELECT id, path FROM media_items WHERE library_id = ?1")
                .map_err(|e| format!("repair prepare items: {e}"))?;
            let rows = stmt
                .query_map(params![library_id], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(|e| format!("repair query items: {e}"))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("repair read items: {e}"))?
        };
        let mut unresolved = 0i64;
        for (id, path) in items {
            if !is_absolute_stored(&path) {
                continue;
            }
            match to_relpath(&lib.path, Path::new(&path)) {
                Some(rel) => {
                    conn.execute(
                        "UPDATE media_items SET path = ?2 WHERE id = ?1",
                        params![id, rel],
                    )
                    .map_err(|e| format!("repair item {id}: {e}"))?;
                }
                None => unresolved += 1,
            }
        }
        let sidecars: Vec<(i64, String, String)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT s.media_item_id, s.track_id, s.path FROM media_item_sidecars s
                     JOIN media_items m ON m.id = s.media_item_id
                     WHERE m.library_id = ?1",
                )
                .map_err(|e| format!("repair prepare sidecars: {e}"))?;
            let rows = stmt
                .query_map(params![library_id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .map_err(|e| format!("repair query sidecars: {e}"))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("repair read sidecars: {e}"))?
        };
        for (media_item_id, track_id, path) in sidecars {
            if !is_absolute_stored(&path) {
                continue;
            }
            match to_relpath(&lib.path, Path::new(&path)) {
                Some(rel) => {
                    conn.execute(
                        "UPDATE media_item_sidecars SET path = ?3
                         WHERE media_item_id = ?1 AND track_id = ?2",
                        params![media_item_id, track_id, rel],
                    )
                    .map_err(|e| format!("repair sidecar: {e}"))?;
                }
                None => unresolved += 1,
            }
        }
        conn.execute(
            "UPDATE libraries SET paths_unresolved = ?2 WHERE id = ?1",
            params![library_id, unresolved],
        )
        .map_err(|e| format!("repair set unresolved: {e}"))?;
        Ok(unresolved)
    }

    pub fn list_item_paths(&self, library_id: i64) -> Result<Vec<ItemPathRow>, String> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, path, mtime_ms, probe_status FROM media_items
                 WHERE library_id = ?1",
            )
            .map_err(|e| format!("prepare item paths: {e}"))?;
        let rows = stmt
            .query_map(params![library_id], |r| {
                Ok(ItemPathRow {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    mtime_ms: r.get(2)?,
                    probe_status: r.get(3)?,
                })
            })
            .map_err(|e| format!("query item paths: {e}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("read item paths: {e}"))
    }

    /// Absolute filesystem path for an item (mixed absolute/relpath column).
    pub fn absolute_item_path(&self, library_id: i64, stored_path: &str) -> Result<String, String> {
        let lib = self
            .get_library(library_id)?
            .ok_or_else(|| format!("library {library_id} not found"))?;
        Ok(resolve_media_path(&lib.path, stored_path)
            .to_string_lossy()
            .into_owned())
    }

    pub fn set_library_reachable(&self, library_id: i64, reachable: bool) -> Result<(), String> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE libraries SET reachable = ?2 WHERE id = ?1",
            params![library_id, reachable as i64],
        )
        .map_err(|e| format!("set library {library_id} reachable: {e}"))?;
        Ok(())
    }

    pub fn count_items(&self, library_id: i64) -> Result<i64, String> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT COUNT(*) FROM media_items WHERE library_id = ?1",
            [library_id],
            |r| r.get(0),
        )
        .map_err(|e| format!("count items for library {library_id}: {e}"))
    }

    pub fn list_items(&self, library_id: i64) -> Result<Vec<MediaItemRow>, String> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, library_id, path, mtime_ms, size_bytes, title, kind,
                        year, season, episode, duration_ms, container, video_codec,
                        audio_codec, audio_channels, width, height, video_bitrate_bps, hdr,
                        probe_status, scan_error, subtitle_status, subtitle_source_mtime_ms,
                        subtitle_source_size_bytes, content_id, probed_content_id,
                        subtitle_content_id, usable_extent_ms, usable_extent_content_id,
                        map_status, map_content_id
                 FROM media_items
                 WHERE library_id = ?1
                 ORDER BY title COLLATE NOCASE, season, episode",
            )
            .map_err(|e| format!("prepare list items: {e}"))?;
        let rows = stmt
            .query_map([library_id], map_item)
            .map_err(|e| format!("query items: {e}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("read items: {e}"))
    }

    pub fn get_item(&self, id: i64) -> Result<Option<MediaItemRow>, String> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, library_id, path, mtime_ms, size_bytes, title, kind,
                    year, season, episode, duration_ms, container, video_codec,
                    audio_codec, audio_channels, width, height, video_bitrate_bps, hdr,
                    probe_status, scan_error, subtitle_status, subtitle_source_mtime_ms,
                    subtitle_source_size_bytes, content_id, probed_content_id,
                    subtitle_content_id, usable_extent_ms, usable_extent_content_id,
                    map_status, map_content_id
             FROM media_items WHERE id = ?1",
            [id],
            map_item,
        )
        .optional()
        .map_err(|e| format!("get item {id}: {e}"))
    }

    pub fn item_mtime(&self, library_id: i64, path: &str) -> Result<Option<(i64, i64)>, String> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, mtime_ms FROM media_items WHERE library_id = ?1 AND path = ?2",
            params![library_id, path],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(|e| format!("item mtime: {e}"))
    }

    /// id, mtime_ms, probe_status for one library path.
    pub fn item_index_row(
        &self,
        library_id: i64,
        path: &str,
    ) -> Result<Option<(i64, i64, String)>, String> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, mtime_ms, probe_status FROM media_items
             WHERE library_id = ?1 AND path = ?2",
            params![library_id, path],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(|e| format!("item index row: {e}"))
    }

    /// Upsert index-pass rows in one transaction. Returns item ids in input order.
    pub fn upsert_items_indexed(
        &self,
        library_id: i64,
        items: &[UpsertItem],
    ) -> Result<Vec<i64>, String> {
        let conn = self.lock()?;
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("begin index upsert: {e}"))?;
        let mut ids = Vec::with_capacity(items.len());
        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO media_items (
                        library_id, path, mtime_ms, size_bytes, title, kind,
                        year, season, episode, duration_ms, container, video_codec,
                        audio_codec, audio_channels, width, height, probe_status,
                        scan_error, probed_at, content_id
                     ) VALUES (
                        ?1, ?2, ?3, ?4, ?5, ?6,
                        ?7, ?8, ?9, NULL, NULL, NULL,
                        NULL, NULL, NULL, NULL, 'indexed',
                        NULL, NULL, ?10
                     )
                     ON CONFLICT(library_id, path) DO UPDATE SET
                        mtime_ms = excluded.mtime_ms,
                        size_bytes = excluded.size_bytes,
                        title = excluded.title,
                        kind = excluded.kind,
                        year = excluded.year,
                        season = excluded.season,
                        episode = excluded.episode,
                        duration_ms = NULL,
                        container = NULL,
                        video_codec = NULL,
                        audio_codec = NULL,
                        audio_channels = NULL,
                        width = NULL,
                        height = NULL,
                        video_bitrate_bps = NULL,
                        hdr = NULL,
                        probe_status = 'indexed',
                        scan_error = NULL,
                        probed_at = NULL,
                        subtitle_status = 'pending',
                        subtitle_source_mtime_ms = NULL,
                        subtitle_source_size_bytes = NULL,
                        content_id = excluded.content_id,
                        probed_content_id = NULL,
                        subtitle_content_id = NULL,
                        usable_extent_ms = NULL,
                        usable_extent_content_id = NULL,
                        map_status = 'pending',
                        map_content_id = NULL",
                )
                .map_err(|e| format!("prepare index upsert: {e}"))?;
            for item in items {
                let path = require_relpath(&item.path)?;
                stmt.execute(params![
                    library_id,
                    path,
                    item.mtime_ms,
                    item.size_bytes,
                    item.title,
                    item.kind,
                    item.year,
                    item.season,
                    item.episode,
                    item.content_id,
                ])
                .map_err(|e| format!("upsert item {path}: {e}"))?;
            }
        }
        {
            let mut stmt = tx
                .prepare("SELECT id FROM media_items WHERE library_id = ?1 AND path = ?2")
                .map_err(|e| format!("prepare fetch id: {e}"))?;
            for item in items {
                let id: i64 = stmt
                    .query_row(params![library_id, item.path], |r| r.get(0))
                    .map_err(|e| format!("fetch upserted id: {e}"))?;
                ids.push(id);
            }
        }
        {
            // Stale byte offsets are worse than no map (ADR-0023 §6).
            let mut del = tx
                .prepare("DELETE FROM keyframe_map_entries WHERE media_item_id = ?1")
                .map_err(|e| format!("prepare map clear: {e}"))?;
            for &id in &ids {
                del.execute([id])
                    .map_err(|e| format!("clear map entries for item {id}: {e}"))?;
            }
        }
        tx.commit()
            .map_err(|e| format!("commit index upsert: {e}"))?;
        Ok(ids)
    }

    pub fn apply_probe_update(&self, update: &ProbeUpdate) -> Result<(), String> {
        let status = parse_probe_status(&update.probe_status)?;
        let conn = self.lock()?;
        conn.execute(
            "UPDATE media_items SET
                duration_ms = ?2,
                container = ?3,
                video_codec = ?4,
                audio_codec = ?5,
                audio_channels = ?6,
                width = ?7,
                height = ?8,
                video_bitrate_bps = ?9,
                hdr = ?10,
                probe_status = ?11,
                scan_error = ?12,
                probed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                probed_content_id = content_id
             WHERE id = ?1",
            params![
                update.item_id,
                update.duration_ms,
                update.container,
                update.video_codec,
                update.audio_codec,
                update.audio_channels,
                update.width,
                update.height,
                update.video_bitrate_bps,
                update.hdr,
                status,
                update.scan_error,
            ],
        )
        .map_err(|e| format!("apply probe for item {}: {e}", update.item_id))?;
        Ok(())
    }

    pub fn set_subtitle_status(
        &self,
        item_id: i64,
        status: &str,
        source_mtime_ms: Option<i64>,
        source_size_bytes: Option<i64>,
    ) -> Result<(), String> {
        let status = parse_subtitle_status(status)?;
        let conn = self.lock()?;
        conn.execute(
            "UPDATE media_items SET
                subtitle_status = ?2,
                subtitle_source_mtime_ms = ?3,
                subtitle_source_size_bytes = ?4,
                subtitle_content_id = CASE
                    WHEN ?2 IN ('ready', 'none') THEN content_id
                    ELSE NULL
                END
             WHERE id = ?1",
            params![item_id, status, source_mtime_ms, source_size_bytes],
        )
        .map_err(|e| format!("set subtitle status for item {item_id}: {e}"))?;
        Ok(())
    }

    /// Persist a built keyframe map under `content_id` (ADR-0023).
    pub fn replace_keyframe_map(
        &self,
        media_item_id: i64,
        content_id: &str,
        container_kind: &str,
        entries: &[(i64, i64)],
        usable_extent_ms: Option<i64>,
    ) -> Result<(), String> {
        let kind = crate::status::parse_map_container_kind(container_kind)?;
        let conn = self.lock()?;
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("begin keyframe map replace: {e}"))?;
        tx.execute(
            "DELETE FROM keyframe_map_entries WHERE media_item_id = ?1",
            [media_item_id],
        )
        .map_err(|e| format!("clear keyframe map for item {media_item_id}: {e}"))?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO keyframe_map_entries (
                        media_item_id, content_id, container_kind, pts_ms, byte_offset
                     ) VALUES (?1, ?2, ?3, ?4, ?5)",
                )
                .map_err(|e| format!("prepare keyframe map insert: {e}"))?;
            for &(pts_ms, byte_offset) in entries {
                stmt.execute(params![
                    media_item_id,
                    content_id,
                    kind,
                    pts_ms,
                    byte_offset
                ])
                .map_err(|e| format!("insert keyframe map entry: {e}"))?;
            }
        }
        tx.execute(
            "UPDATE media_items SET
                map_status = 'ready',
                map_content_id = ?2,
                usable_extent_ms = ?3,
                usable_extent_content_id = CASE WHEN ?3 IS NULL THEN NULL ELSE ?2 END
             WHERE id = ?1",
            params![media_item_id, content_id, usable_extent_ms],
        )
        .map_err(|e| format!("set map ready for item {media_item_id}: {e}"))?;
        tx.commit()
            .map_err(|e| format!("commit keyframe map replace: {e}"))?;
        Ok(())
    }

    pub fn set_map_status(&self, item_id: i64, status: &str) -> Result<(), String> {
        let status = parse_map_status(status)?;
        let conn = self.lock()?;
        conn.execute(
            "UPDATE media_items SET map_status = ?2 WHERE id = ?1",
            params![item_id, status],
        )
        .map_err(|e| format!("set map status for item {item_id}: {e}"))?;
        Ok(())
    }

    pub fn set_content_id(&self, item_id: i64, content_id: &str) -> Result<(), String> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE media_items SET content_id = ?2 WHERE id = ?1",
            params![item_id, content_id],
        )
        .map_err(|e| format!("set content_id for item {item_id}: {e}"))?;
        Ok(())
    }

    /// Mark map pending and clear entries (session fallback / explicit rebuild).
    pub fn mark_map_pending(&self, item_id: i64) -> Result<(), String> {
        let conn = self.lock()?;
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("begin map pending: {e}"))?;
        tx.execute(
            "DELETE FROM keyframe_map_entries WHERE media_item_id = ?1",
            [item_id],
        )
        .map_err(|e| format!("clear map for pending item {item_id}: {e}"))?;
        tx.execute(
            "UPDATE media_items SET
                map_status = 'pending',
                map_content_id = NULL,
                usable_extent_ms = NULL,
                usable_extent_content_id = NULL
             WHERE id = ?1",
            [item_id],
        )
        .map_err(|e| format!("mark map pending for item {item_id}: {e}"))?;
        tx.commit()
            .map_err(|e| format!("commit map pending: {e}"))?;
        Ok(())
    }

    /// Returns (item_id, path, library_id) for maps that need building.
    pub fn list_pending_map_items(&self) -> Result<Vec<(i64, String, i64)>, String> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, path, library_id FROM media_items
                 WHERE map_status = 'pending'
                    OR (map_status = 'ready' AND (
                        map_content_id IS NULL
                        OR content_id IS NULL
                        OR map_content_id IS NOT content_id
                    ))
                 ORDER BY id",
            )
            .map_err(|e| format!("prepare pending map items: {e}"))?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(|e| format!("list pending map items: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("read pending map items: {e}"))
    }

    /// Keyframe map for a session start, or None when there is nothing
    /// usable: no ready map, or stamps that no longer match live identity.
    ///
    /// The whole map is read at session create so later seeks in that
    /// session snap without another query. Bind-time revalidation against
    /// the file on disk is the caller's (ADR-0023 §4).
    pub fn keyframe_map(&self, media_item_id: i64) -> Result<Option<KeyframeMapRows>, String> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT e.pts_ms, e.byte_offset, e.container_kind, e.content_id
                 FROM keyframe_map_entries e
                 JOIN media_items m ON m.id = e.media_item_id
                 WHERE e.media_item_id = ?1
                   AND m.map_status = 'ready'
                   AND m.content_id IS NOT NULL
                   AND m.map_content_id = m.content_id
                   AND e.content_id = m.content_id
                 ORDER BY e.pts_ms",
            )
            .map_err(|e| format!("prepare keyframe map for item {media_item_id}: {e}"))?;
        let rows = stmt
            .query_map(params![media_item_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .map_err(|e| format!("keyframe map for item {media_item_id}: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("read keyframe map for item {media_item_id}: {e}"))?;

        let Some((_, _, container_kind, content_id)) = rows.first() else {
            return Ok(None);
        };
        Ok(Some(KeyframeMapRows {
            container_kind: container_kind.clone(),
            content_id: content_id.clone(),
            entries: rows
                .iter()
                .map(|&(pts, offset, ..)| (pts, offset))
                .collect(),
        }))
    }

    /// Reset availability failures so the pool can re-drain them (ADR-0014).
    pub fn requeue_unavailable_for_library(
        &self,
        library_id: i64,
    ) -> Result<(usize, usize, usize), String> {
        let conn = self.lock()?;
        let probes = conn
            .execute(
                "UPDATE media_items SET probe_status = 'indexed', scan_error = NULL
                 WHERE library_id = ?1 AND probe_status = 'unavailable'",
                [library_id],
            )
            .map_err(|e| format!("requeue unavailable probes: {e}"))?;
        let extracts = conn
            .execute(
                "UPDATE media_items SET subtitle_status = 'pending',
                    subtitle_source_mtime_ms = NULL, subtitle_source_size_bytes = NULL
                 WHERE library_id = ?1 AND subtitle_status = 'unavailable'",
                [library_id],
            )
            .map_err(|e| format!("requeue unavailable extracts: {e}"))?;
        let maps = conn
            .execute(
                "UPDATE media_items SET map_status = 'pending', map_content_id = NULL
                 WHERE library_id = ?1 AND map_status = 'unavailable'",
                [library_id],
            )
            .map_err(|e| format!("requeue unavailable maps: {e}"))?;
        Ok((probes, extracts, maps))
    }

    /// Items that never finished probing (e.g. process restart mid-scan).
    /// Returns (item_id, path, library_id).
    pub fn list_indexed_unprobed(&self) -> Result<Vec<(i64, String, i64)>, String> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, path, library_id FROM media_items
                 WHERE probe_status = 'indexed'
                 ORDER BY id",
            )
            .map_err(|e| format!("prepare indexed unprobed: {e}"))?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(|e| format!("list indexed unprobed: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("read indexed unprobed: {e}"))
    }

    /// Returns (item_id, path, mtime_ms, size_bytes, library_id).
    #[allow(clippy::type_complexity)]
    pub fn list_pending_subtitle_items(&self) -> Result<Vec<(i64, String, i64, i64, i64)>, String> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, path, mtime_ms, size_bytes, library_id FROM media_items
                 WHERE subtitle_status = 'pending'
                    OR (subtitle_status = 'ready' AND (
                        subtitle_source_mtime_ms IS NOT mtime_ms
                        OR subtitle_source_size_bytes IS NOT size_bytes
                    ))",
            )
            .map_err(|e| format!("prepare pending subtitle items: {e}"))?;
        stmt.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .map_err(|e| format!("list pending subtitle items: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read pending subtitle items: {e}"))
    }

    pub fn list_all_item_ids(&self) -> Result<Vec<i64>, String> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT id FROM media_items")
            .map_err(|e| format!("prepare item ids: {e}"))?;
        stmt.query_map([], |r| r.get(0))
            .map_err(|e| format!("list item ids: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("read item ids: {e}"))
    }

    pub fn mark_items_subtitle_pending(&self, ids: &[i64]) -> Result<(), String> {
        if ids.is_empty() {
            return Ok(());
        }
        let conn = self.lock()?;
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("begin subtitle pending update: {e}"))?;
        let mut stmt = tx
            .prepare(
                "UPDATE media_items SET subtitle_status = 'pending',
                    subtitle_source_mtime_ms = NULL, subtitle_source_size_bytes = NULL
                 WHERE id = ?1",
            )
            .map_err(|e| format!("prepare subtitle pending update: {e}"))?;
        for id in ids {
            stmt.execute([id])
                .map_err(|e| format!("mark subtitle pending for item {id}: {e}"))?;
        }
        drop(stmt);
        tx.commit()
            .map_err(|e| format!("commit subtitle pending update: {e}"))?;
        Ok(())
    }

    /// Delete items whose path fold is not in `keep_folds`. Never deletes
    /// unresolved absolute rows (ADR-0030 §5). `keep_folds` are
    /// [`fold_path`] keys of walked relpaths.
    pub fn delete_missing_fold(
        &self,
        library_id: i64,
        keep_folds: &HashSet<String>,
    ) -> Result<Vec<i64>, String> {
        let rows = self.list_item_paths(library_id)?;
        let mut to_delete = Vec::new();
        for row in &rows {
            if is_absolute_stored(&row.path) {
                continue;
            }
            if keep_folds.contains(&fold_path(&row.path)) {
                continue;
            }
            to_delete.push(row.id);
        }
        if to_delete.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.lock()?;
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("begin delete_missing: {e}"))?;
        {
            let mut stmt = tx
                .prepare("DELETE FROM media_items WHERE id = ?1")
                .map_err(|e| format!("prepare delete item: {e}"))?;
            for id in &to_delete {
                stmt.execute(params![id])
                    .map_err(|e| format!("delete item {id}: {e}"))?;
            }
        }
        tx.commit()
            .map_err(|e| format!("commit delete_missing: {e}"))?;
        Ok(to_delete)
    }

    /// Legacy exact-path delete (tests). Prefer [`Self::delete_missing_fold`].
    pub fn delete_missing(
        &self,
        library_id: i64,
        keep_paths: &[String],
    ) -> Result<Vec<i64>, String> {
        let folds: HashSet<String> = keep_paths.iter().map(|p| fold_path(p)).collect();
        self.delete_missing_fold(library_id, &folds)
    }

    /// Replace all sidecar rows for one media item (index-pass association).
    pub fn replace_item_sidecars(
        &self,
        media_item_id: i64,
        sidecars: &[SidecarRow],
    ) -> Result<bool, String> {
        let conn = self.lock()?;
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("begin sidecar replace: {e}"))?;
        let existing: Vec<SidecarRow> = {
            let mut stmt = tx
                .prepare(
                    "SELECT media_item_id, track_id, path, mtime_ms, size_bytes,
                            format, language, forced, sdh
                     FROM media_item_sidecars WHERE media_item_id = ?1 ORDER BY track_id",
                )
                .map_err(|e| format!("prepare existing sidecars: {e}"))?;
            stmt.query_map([media_item_id], map_sidecar)
                .map_err(|e| format!("list existing sidecars: {e}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("read existing sidecars: {e}"))?
        };
        let changed = existing.len() != sidecars.len()
            || existing.iter().zip(sidecars).any(|(a, b)| {
                a.track_id != b.track_id
                    || a.path != b.path
                    || a.mtime_ms != b.mtime_ms
                    || a.size_bytes != b.size_bytes
                    || a.format != b.format
                    || a.language != b.language
                    || a.forced != b.forced
                    || a.sdh != b.sdh
            });
        tx.execute(
            "DELETE FROM media_item_sidecars WHERE media_item_id = ?1",
            [media_item_id],
        )
        .map_err(|e| format!("clear sidecars for item {media_item_id}: {e}"))?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO media_item_sidecars (
                        media_item_id, track_id, path, mtime_ms, size_bytes,
                        format, language, forced, sdh
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )
                .map_err(|e| format!("prepare sidecar insert: {e}"))?;
            for s in sidecars {
                let path = require_relpath(&s.path)?;
                stmt.execute(params![
                    media_item_id,
                    s.track_id,
                    path,
                    s.mtime_ms,
                    s.size_bytes,
                    s.format,
                    s.language,
                    s.forced as i64,
                    s.sdh as i64,
                ])
                .map_err(|e| format!("insert sidecar {}: {e}", s.track_id))?;
            }
        }
        tx.commit()
            .map_err(|e| format!("commit sidecar replace: {e}"))?;
        Ok(changed)
    }

    pub fn list_item_sidecars(&self, media_item_id: i64) -> Result<Vec<SidecarRow>, String> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT media_item_id, track_id, path, mtime_ms, size_bytes,
                        format, language, forced, sdh
                 FROM media_item_sidecars
                 WHERE media_item_id = ?1
                 ORDER BY track_id",
            )
            .map_err(|e| format!("prepare list sidecars: {e}"))?;
        let rows = stmt
            .query_map([media_item_id], map_sidecar)
            .map_err(|e| format!("list sidecars for item {media_item_id}: {e}"))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| format!("map sidecar: {e}"))?);
        }
        Ok(out)
    }

    pub fn get_item_sidecar(
        &self,
        media_item_id: i64,
        track_id: &str,
    ) -> Result<Option<SidecarRow>, String> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT media_item_id, track_id, path, mtime_ms, size_bytes,
                    format, language, forced, sdh
             FROM media_item_sidecars
             WHERE media_item_id = ?1 AND track_id = ?2",
            params![media_item_id, track_id],
            map_sidecar,
        )
        .optional()
        .map_err(|e| format!("get sidecar {track_id} for item {media_item_id}: {e}"))
    }

    pub fn create_scan_job(&self, library_id: i64) -> Result<i64, String> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO scan_jobs (library_id, state, kind) VALUES (?1, 'queued', 'scan')",
            [library_id],
        )
        .map_err(|e| format!("insert scan job: {e}"))?;
        Ok(conn.last_insert_rowid())
    }

    pub fn create_repoint_job(&self, library_id: i64, candidate_path: &str) -> Result<i64, String> {
        let candidate = require_library_root(candidate_path)?;
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO scan_jobs (library_id, state, kind, candidate_path)
             VALUES (?1, 'queued', 'repoint', ?2)",
            params![library_id, candidate],
        )
        .map_err(|e| format!("insert repoint job: {e}"))?;
        Ok(conn.last_insert_rowid())
    }

    pub fn active_scan_job(&self, library_id: i64) -> Result<Option<i64>, String> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id FROM scan_jobs
             WHERE library_id = ?1
               AND state IN ('queued', 'indexing', 'probing')
             ORDER BY id DESC
             LIMIT 1",
            [library_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("active scan job: {e}"))
    }

    /// Mark in-flight scan jobs failed. A process exit leaves rows in
    /// queued/indexing/probing with no worker; reusing them blocks new scans.
    pub fn fail_stale_scan_jobs(&self) -> Result<usize, String> {
        let conn = self.lock()?;
        let n = conn
            .execute(
                "UPDATE scan_jobs SET
                    state = 'failed',
                    error_message = 'scan interrupted by process restart',
                    finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                 WHERE state IN ('queued', 'indexing', 'probing')",
                [],
            )
            .map_err(|e| format!("fail stale scan jobs: {e}"))?;
        Ok(n)
    }

    pub fn get_scan_job(&self, job_id: i64) -> Result<Option<ScanJobRow>, String> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, library_id, state, added, updated, removed, unchanged,
                    probed, errors, index_duration_ms, probe_duration_ms,
                    error_message, started_at, finished_at, kind, candidate_path,
                    skipped_outside_root, deferred_remove
             FROM scan_jobs WHERE id = ?1",
            [job_id],
            map_scan_job,
        )
        .optional()
        .map_err(|e| format!("get scan job {job_id}: {e}"))
    }

    pub fn set_scan_job_skipped_outside_root(
        &self,
        job_id: i64,
        skipped: i64,
    ) -> Result<(), String> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE scan_jobs SET skipped_outside_root = ?2 WHERE id = ?1",
            params![job_id, skipped],
        )
        .map_err(|e| format!("set scan job skipped_outside_root: {e}"))?;
        Ok(())
    }

    pub fn set_scan_job_deferred_remove(
        &self,
        job_id: i64,
        deferred_remove: i64,
    ) -> Result<(), String> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE scan_jobs SET deferred_remove = ?2 WHERE id = ?1",
            params![job_id, deferred_remove],
        )
        .map_err(|e| format!("set scan job deferred_remove: {e}"))?;
        Ok(())
    }

    /// Rows that [`Self::delete_missing_fold`] would remove (relpath rows only).
    pub fn count_missing_fold(
        &self,
        library_id: i64,
        keep_folds: &HashSet<String>,
    ) -> Result<i64, String> {
        let rows = self.list_item_paths(library_id)?;
        let n = rows
            .iter()
            .filter(|r| !is_absolute_stored(&r.path))
            .filter(|r| !keep_folds.contains(&fold_path(&r.path)))
            .count();
        Ok(n as i64)
    }

    pub fn set_scan_job_state(&self, job_id: i64, state: &str) -> Result<(), String> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE scan_jobs SET state = ?2 WHERE id = ?1",
            params![job_id, state],
        )
        .map_err(|e| format!("set scan job state: {e}"))?;
        Ok(())
    }

    pub fn set_scan_job_index_done(
        &self,
        job_id: i64,
        added: u32,
        updated: u32,
        removed: u32,
        unchanged: u32,
        index_duration_ms: u64,
    ) -> Result<(), String> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE scan_jobs SET
                state = 'probing',
                added = ?2,
                updated = ?3,
                removed = ?4,
                unchanged = ?5,
                index_duration_ms = ?6
             WHERE id = ?1",
            params![
                job_id,
                added,
                updated,
                removed,
                unchanged,
                index_duration_ms as i64
            ],
        )
        .map_err(|e| format!("set scan job index done: {e}"))?;
        Ok(())
    }

    pub fn bump_scan_job_probe(&self, job_id: i64, error: bool) -> Result<(), String> {
        let conn = self.lock()?;
        if error {
            conn.execute(
                "UPDATE scan_jobs SET probed = probed + 1, errors = errors + 1 WHERE id = ?1",
                [job_id],
            )
        } else {
            conn.execute(
                "UPDATE scan_jobs SET probed = probed + 1 WHERE id = ?1",
                [job_id],
            )
        }
        .map_err(|e| format!("bump scan job probe: {e}"))?;
        Ok(())
    }

    pub fn complete_scan_job(&self, job_id: i64, probe_duration_ms: u64) -> Result<(), String> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE scan_jobs SET
                state = 'completed',
                probe_duration_ms = ?2,
                finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
            params![job_id, probe_duration_ms as i64],
        )
        .map_err(|e| format!("complete scan job: {e}"))?;
        Ok(())
    }

    pub fn fail_scan_job(&self, job_id: i64, message: &str) -> Result<(), String> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE scan_jobs SET
                state = 'failed',
                error_message = ?2,
                finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
            params![job_id, message],
        )
        .map_err(|e| format!("fail scan job: {e}"))?;
        Ok(())
    }
}

fn map_library(r: &rusqlite::Row<'_>) -> rusqlite::Result<LibraryRow> {
    let reachable_i: i64 = r.get(5)?;
    Ok(LibraryRow {
        id: r.get(0)?,
        name: r.get(1)?,
        path: r.get(2)?,
        kind: r.get(3)?,
        item_count: r.get(4)?,
        reachable: reachable_i != 0,
        paths_unresolved: r.get(6)?,
        skipped_outside_root: r.get(7)?,
    })
}

fn map_item(r: &rusqlite::Row<'_>) -> rusqlite::Result<MediaItemRow> {
    Ok(MediaItemRow {
        id: r.get(0)?,
        library_id: r.get(1)?,
        path: r.get(2)?,
        mtime_ms: r.get(3)?,
        size_bytes: r.get(4)?,
        title: r.get(5)?,
        kind: r.get(6)?,
        year: r.get(7)?,
        season: r.get(8)?,
        episode: r.get(9)?,
        duration_ms: r.get(10)?,
        container: r.get(11)?,
        video_codec: r.get(12)?,
        audio_codec: r.get(13)?,
        audio_channels: r.get(14)?,
        width: r.get(15)?,
        height: r.get(16)?,
        video_bitrate_bps: r.get(17)?,
        hdr: r.get(18)?,
        probe_status: r.get(19)?,
        scan_error: r.get(20)?,
        subtitle_status: r.get(21)?,
        subtitle_source_mtime_ms: r.get(22)?,
        subtitle_source_size_bytes: r.get(23)?,
        content_id: r.get(24)?,
        probed_content_id: r.get(25)?,
        subtitle_content_id: r.get(26)?,
        usable_extent_ms: r.get(27)?,
        usable_extent_content_id: r.get(28)?,
        map_status: r.get(29)?,
        map_content_id: r.get(30)?,
    })
}

fn map_scan_job(r: &rusqlite::Row<'_>) -> rusqlite::Result<ScanJobRow> {
    Ok(ScanJobRow {
        id: r.get(0)?,
        library_id: r.get(1)?,
        state: r.get(2)?,
        added: r.get(3)?,
        updated: r.get(4)?,
        removed: r.get(5)?,
        unchanged: r.get(6)?,
        probed: r.get(7)?,
        errors: r.get(8)?,
        index_duration_ms: r.get(9)?,
        probe_duration_ms: r.get(10)?,
        error_message: r.get(11)?,
        started_at: r.get(12)?,
        finished_at: r.get(13)?,
        kind: r.get(14)?,
        candidate_path: r.get(15)?,
        skipped_outside_root: r.get(16)?,
        deferred_remove: r.get(17)?,
    })
}

fn map_sidecar(r: &rusqlite::Row<'_>) -> rusqlite::Result<SidecarRow> {
    let forced: i64 = r.get(7)?;
    let sdh: i64 = r.get(8)?;
    Ok(SidecarRow {
        media_item_id: r.get(0)?,
        track_id: r.get(1)?,
        path: r.get(2)?,
        mtime_ms: r.get(3)?,
        size_bytes: r.get(4)?,
        format: r.get(5)?,
        language: r.get(6)?,
        forced: forced != 0,
        sdh: sdh != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One indexed item with a ready map, both stamped with `content_id`.
    fn mapped_item(db: &Db, content_id: &str) -> i64 {
        let lib = db
            .create_library(&NewLibrary {
                name: "films".into(),
                path: "/films".into(),
                kind: "movies".into(),
            })
            .unwrap();
        let ids = db
            .upsert_items_indexed(
                lib.id,
                &[UpsertItem {
                    path: "clip.mkv".into(),
                    mtime_ms: 1,
                    size_bytes: 2,
                    title: "clip".into(),
                    kind: "movie".into(),
                    year: None,
                    season: None,
                    episode: None,
                    content_id: Some(content_id.into()),
                }],
            )
            .unwrap();
        let item_id = ids[0];
        db.replace_keyframe_map(
            item_id,
            content_id,
            "matroska",
            &[(0, 100), (2000, 900)],
            None,
        )
        .unwrap();
        item_id
    }

    /// ADR-0023 §4: a map is only usable while its stamp still matches the
    /// item's identity, so a replaced file reads as no map at all.
    #[test]
    fn keyframe_map_is_withheld_once_identity_moves() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("nightjar.db")).unwrap();
        let item_id = mapped_item(&db, "1-aaa-bbb");

        let map = db.keyframe_map(item_id).unwrap().expect("map is usable");
        assert_eq!(map.container_kind, "matroska");
        assert_eq!(map.content_id, "1-aaa-bbb");
        assert_eq!(map.entries, vec![(0, 100), (2000, 900)]);

        db.set_content_id(item_id, "2-ccc-ddd").unwrap();
        assert!(db.keyframe_map(item_id).unwrap().is_none());
    }
}
