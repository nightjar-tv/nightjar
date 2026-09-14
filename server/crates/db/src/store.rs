use crate::migrate;
use crate::paths::{
    fold_path, is_absolute_stored, require_library_root, require_relpath, resolve_media_path,
    to_relpath,
};
use crate::status::{backoff_days, parse_map_status, parse_probe_status, parse_subtitle_status};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

pub struct Db {
    conn: Mutex<Connection>,
}

/// How many times [`with_write_tx`] re-runs a transaction that could not take
/// the write lock. Contention here is another connection's commit, which is
/// short; if three attempts do not clear it the caller sees the error rather
/// than the request hanging.
const WRITE_TX_ATTEMPTS: u32 = 3;

/// Begin a write transaction up front — `BEGIN IMMEDIATE`.
///
/// `Connection::unchecked_transaction` is `BEGIN DEFERRED`. A deferred
/// transaction that SELECTs before it writes takes a read snapshot and then
/// tries to upgrade it to a write. In WAL, if any other connection committed
/// in the interim, SQLite fails that upgrade with `SQLITE_BUSY_SNAPSHOT`
/// **immediately, without consulting `busy_timeout`** — the snapshot is stale
/// and no amount of waiting can make it current. Taking the write lock before
/// the first read means there is nothing to upgrade.
///
/// Use this for any transaction that reads before it writes. A transaction
/// whose first statement is a write already acquires the lock at that
/// statement and does not need it.
pub fn write_tx(conn: &Connection) -> Result<Transaction<'_>, String> {
    Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|e| format!("begin write transaction: {e}"))
}

/// True for the two lock failures worth another attempt: `SQLITE_BUSY` (the
/// write lock was held past `busy_timeout`) and `SQLITE_BUSY_SNAPSHOT` (a
/// stale read snapshot, which `write_tx` is meant to prevent but which a
/// caller still holding a deferred transaction elsewhere could produce).
fn is_busy_error(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(e, _)
            if e.code == rusqlite::ErrorCode::DatabaseBusy
                || e.code == rusqlite::ErrorCode::DatabaseLocked
    )
}

/// Run a read-then-write transaction, retrying while the write lock is busy.
///
/// `f` must be safe to run more than once: it is re-invoked from the start on
/// a retry, against a fresh transaction. Anything it returns from an earlier
/// attempt is discarded.
///
/// `BEGIN IMMEDIATE` honours `busy_timeout`, so a retry here is for the case
/// where that timeout is itself exhausted. It exists because the alternative
/// at the sidecar call site was a WARN and silent data loss: nothing revisited
/// the item, so an external subtitle simply never got associated.
pub fn with_write_tx<T, F>(conn: &Connection, mut f: F) -> Result<T, String>
where
    F: FnMut(&Transaction<'_>) -> Result<T, String>,
{
    let mut last = String::new();
    for attempt in 1..=WRITE_TX_ATTEMPTS {
        let tx = match Transaction::new_unchecked(conn, TransactionBehavior::Immediate) {
            Ok(tx) => tx,
            Err(e) if is_busy_error(&e) => {
                last = format!("begin write transaction: {e}");
                tracing::debug!(attempt, error = %last, "write transaction busy; retrying");
                continue;
            }
            Err(e) => return Err(format!("begin write transaction: {e}")),
        };
        let value = f(&tx)?;
        match tx.commit() {
            Ok(()) => return Ok(value),
            Err(e) if is_busy_error(&e) => {
                last = format!("commit write transaction: {e}");
                tracing::debug!(attempt, error = %last, "write transaction busy; retrying");
            }
            Err(e) => return Err(format!("commit write transaction: {e}")),
        }
    }
    Err(last)
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
    /// Source frame rate as a rational (ADR-0052); NULL until probed.
    pub video_frame_rate_num: Option<i64>,
    pub video_frame_rate_den: Option<i64>,
    /// Source HDR: `none` | `hdr10` | `dolby_vision` | `dolby_vision_p5` (ADR-0022).
    pub hdr: Option<String>,
    pub probe_status: String,
    pub scan_error: Option<String>,
    pub subtitle_status: String,
    /// ADR-0023 live media-file fingerprint (NULL until scan computes it).
    pub content_id: Option<String>,
    pub probed_content_id: Option<String>,
    pub subtitle_content_id: Option<String>,
    pub usable_extent_ms: Option<i64>,
    pub usable_extent_content_id: Option<String>,
    pub map_status: String,
    pub map_content_id: Option<String>,
    /// Metadata pipeline state: pending | matched | ready | unmatched (ADR-0026).
    pub metadata_status: String,
    /// ADR-0058: moves when the scanner accepts a change to the observed
    /// bytes, the source path, or the library-root binding. Always >= 1.
    pub media_revision: i64,
    /// ADR-0058: counts accepted publications; independent of media identity.
    pub probe_revision: i64,
    /// ADR-0058: the media revision the last publication was certified against,
    /// or NULL when no revisioned publication has landed.
    pub probed_media_revision: Option<i64>,
    /// ADR-0058: the selected absolute video stream index, or NULL.
    pub video_stream_index: Option<i64>,
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
    /// Source frame rate as a rational (ADR-0052).
    pub video_frame_rate_num: Option<i64>,
    pub video_frame_rate_den: Option<i64>,
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

/// Live item-state counts behind a library's scan progress display.
///
/// `probe_queued` is a queue depth and the job row's `probed` is a cumulative
/// total; they are never added together as if they were the same measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanProgressCounts {
    pub found: i64,
    pub probe_queued: i64,
    pub probe_errors: i64,
    pub metadata_pending: i64,
    pub metadata_ready: i64,
    pub metadata_unmatched: i64,
}

/// One row for fold-aware index matching (ADR-0030 §2).
#[derive(Debug, Clone)]
pub struct ItemPathRow {
    pub id: i64,
    pub path: String,
    pub mtime_ms: i64,
    pub size_bytes: i64,
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

/// One persisted subtitle stream row in `media_item_subtitle_tracks`
/// (ADR-0041 Decision 1). A re-probe replaces the rows.
///
/// The publisher writes `probe_revision` itself from the expectation, so the
/// `media_item_id` here is not read: the row belongs to the item under CAS
/// (ADR-0058).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleTrackRow {
    pub media_item_id: i64,
    pub stream_index: i64,
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub forced: bool,
    pub sdh: bool,
    /// `text` | `ass` | `image` | `unknown` (migration 017 CHECK).
    pub kind: String,
}

/// One audio stream row the publisher writes to `media_item_audio_tracks`
/// (ADR-0058). Like [`SubtitleTrackRow`], the item comes from the expectation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioTrackRow {
    pub stream_index: i64,
    pub codec: String,
    pub language: Option<String>,
    pub channels: Option<i64>,
    pub channel_layout: Option<String>,
    pub title: Option<String>,
    pub is_default: bool,
}

/// The captured expectation one probe result must still match to publish
/// (ADR-0058).
///
/// The worker records every field before it probes. A publication commits only
/// while the row still matches all of them, so a result can never describe
/// bytes, a path, a library-root binding, or a media revision other than the
/// one it observed.
#[derive(Debug, Clone)]
pub struct ProbeExpectation {
    pub item_id: i64,
    pub library_id: i64,
    /// Current `libraries.path` the item is bound to.
    pub library_root: String,
    /// Stored `media_items.path`.
    pub path: String,
    pub media_revision: i64,
    pub probe_revision: i64,
    /// ADR-0023 identity observed before probing. `None` or empty cannot
    /// certify a success.
    pub content_id: Option<String>,
    pub mtime_ms: i64,
    pub size_bytes: i64,
}

/// The complete technical result of one successful probe (ADR-0058).
///
/// Every scalar and both inventories travel together: a partial success is not
/// a publication.
#[derive(Debug, Clone)]
pub struct ProbeSnapshot {
    pub duration_ms: Option<i64>,
    pub container: Option<String>,
    pub video_codec: Option<String>,
    /// Selected absolute video stream index.
    pub video_stream_index: Option<i64>,
    pub audio_codec: Option<String>,
    pub audio_channels: Option<i64>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub video_bitrate_bps: Option<i64>,
    pub video_frame_rate_num: Option<i64>,
    pub video_frame_rate_den: Option<i64>,
    pub hdr: Option<String>,
    /// Complete audio inventory, ordered by absolute stream index.
    pub audio_tracks: Vec<AudioTrackRow>,
    /// Complete subtitle inventory, ordered by absolute stream index.
    pub subtitle_tracks: Vec<SubtitleTrackRow>,
    /// Derived by [`crate::status::classify_subtitle_status`] (ADR-0041
    /// Decision 2).
    pub subtitle_status: String,
}

/// One probe run, ready to publish against its expectation (ADR-0058).
#[derive(Debug, Clone)]
pub enum ProbeOutcome {
    Success(Box<ProbeSnapshot>),
    Failure {
        /// `error` or `unavailable`; `probed` is not a failure.
        probe_status: String,
        scan_error: String,
    },
}

/// What a publication did (ADR-0058).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbePublication {
    Published {
        media_revision: i64,
        probe_revision: i64,
    },
    FailureRecorded,
    /// The captured expectation no longer matches, so nothing was written.
    Stale,
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

    /// Run a short critical section against the shared connection.
    /// Do not hold this across network I/O (metadata drain uses its own conn).
    pub fn with_conn<F, T>(&self, f: F) -> Result<T, String>
    where
        F: FnOnce(&Connection) -> Result<T, String>,
    {
        let conn = self.lock()?;
        f(&conn)
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
        // Reads `libraries.path` before it writes, so take the write lock up
        // front rather than upgrading a deferred read snapshot (see `write_tx`).
        let tx = write_tx(&conn)?;
        // ADR-0058: the library-root binding is part of the media identity a
        // probe snapshot is captured against, so a root that actually moves
        // invalidates every item's snapshot exactly once. A no-op write (same
        // normalized root) must not move a revision.
        let moved: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM libraries WHERE id = ?1 AND path <> ?2",
                params![library_id, root],
                |r| r.get(0),
            )
            .map_err(|e| format!("check library path change: {e}"))?;
        tx.execute(
            "UPDATE libraries SET path = ?2 WHERE id = ?1",
            params![library_id, root],
        )
        .map_err(|e| format!("update library path: {e}"))?;
        if moved > 0 {
            tx.execute(
                "UPDATE media_items SET
                    media_revision = CASE
                        WHEN media_revision = 9223372036854775807 THEN -1
                        ELSE media_revision + 1
                    END,
                    probed_media_revision = NULL,
                    probed_content_id = NULL
                 WHERE library_id = ?1",
                params![library_id],
            )
            .map_err(|e| format!("bump media revision for library {library_id}: {e}"))?;
        }
        tx.commit()
            .map_err(|e| format!("commit library path update: {e}"))?;
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
                    // ADR-0058: the stored source path is part of the media
                    // identity a snapshot is captured against, so rewriting an
                    // absolute legacy path to a library-relative one moves the
                    // revision and clears both validity stamps.
                    conn.execute(
                        "UPDATE media_items SET path = ?2,
                            media_revision = CASE
                                WHEN media_revision = 9223372036854775807 THEN -1
                                ELSE media_revision + 1
                            END,
                            probed_media_revision = NULL,
                            probed_content_id = NULL
                         WHERE id = ?1",
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
                "SELECT id, path, mtime_ms, size_bytes, probe_status FROM media_items
                 WHERE library_id = ?1",
            )
            .map_err(|e| format!("prepare item paths: {e}"))?;
        let rows = stmt
            .query_map(params![library_id], |r| {
                Ok(ItemPathRow {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    mtime_ms: r.get(2)?,
                    size_bytes: r.get(3)?,
                    probe_status: r.get(4)?,
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

    /// Record a frame rate resolved outside the probe (ADR-0052 decision 4).
    /// Does not touch `probe_status`: this is one field filled in, not a probe.
    pub fn set_item_frame_rate(&self, id: i64, num: i64, den: i64) -> Result<(), String> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE media_items
                SET video_frame_rate_num = ?2, video_frame_rate_den = ?3
              WHERE id = ?1",
            params![id, num, den],
        )
        .map_err(|e| format!("set frame rate for item {id}: {e}"))?;
        Ok(())
    }

    pub fn list_items(&self, library_id: i64) -> Result<Vec<MediaItemRow>, String> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, library_id, path, mtime_ms, size_bytes, title, kind,
                        year, season, episode, duration_ms, container, video_codec,
                        audio_codec, audio_channels, width, height, video_bitrate_bps,
                        video_frame_rate_num, video_frame_rate_den, hdr,
                        probe_status, scan_error, subtitle_status,
                        content_id, probed_content_id,
                        subtitle_content_id, usable_extent_ms, usable_extent_content_id,
                        map_status, map_content_id, metadata_status,
                        media_revision, probe_revision, probed_media_revision,
                        video_stream_index
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
                    audio_codec, audio_channels, width, height, video_bitrate_bps,
                    video_frame_rate_num, video_frame_rate_den, hdr,
                    probe_status, scan_error, subtitle_status,
                    content_id, probed_content_id,
                    subtitle_content_id, usable_extent_ms, usable_extent_content_id,
                    map_status, map_content_id, metadata_status,
                    media_revision, probe_revision, probed_media_revision,
                    video_stream_index
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
                        probe_status = 'indexed',
                        scan_error = NULL,
                        probed_at = NULL,
                        subtitle_status = 'pending',
                        content_id = excluded.content_id,
                        subtitle_content_id = NULL,
                        usable_extent_ms = NULL,
                        usable_extent_content_id = NULL,
                        map_status = 'pending',
                        map_content_id = NULL,
                        media_revision = CASE
                            WHEN excluded.content_id IS NOT NULL
                             AND media_items.content_id IS NOT NULL
                             AND excluded.content_id <> media_items.content_id
                            THEN CASE
                                WHEN media_revision = 9223372036854775807 THEN -1
                                ELSE media_revision + 1
                            END
                            ELSE media_revision
                        END,
                        probed_media_revision = CASE
                            WHEN excluded.content_id IS NOT NULL
                             AND media_items.content_id IS NOT NULL
                             AND excluded.content_id <> media_items.content_id
                            THEN NULL
                            ELSE probed_media_revision
                        END,
                        probed_content_id = CASE
                            WHEN excluded.content_id IS NOT NULL
                             AND media_items.content_id IS NOT NULL
                             AND excluded.content_id <> media_items.content_id
                            THEN NULL
                            ELSE probed_content_id
                        END",
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

    /// Write technical scalars and a probe status without certifying a
    /// snapshot (ADR-0058).
    ///
    /// **Legacy fact-only writer, narrowed.** It does not touch
    /// `probed_content_id`, `probed_media_revision`, or `probe_revision`, so a
    /// `probed` status written here leaves the row uncertified. Only
    /// [`Db::publish_probe`] may certify a snapshot. The scanner's probe path
    /// uses the publisher; this remains for tests that seed a probed item.
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
                video_frame_rate_num = ?13,
                video_frame_rate_den = ?14,
                hdr = ?10,
                probe_status = ?11,
                scan_error = ?12,
                probed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
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
                update.video_frame_rate_num,
                update.video_frame_rate_den,
            ],
        )
        .map_err(|e| format!("apply probe for item {}: {e}", update.item_id))?;
        Ok(())
    }

    /// Publish one probe result atomically against its captured expectation
    /// (ADR-0058).
    ///
    /// The transaction takes the write lock up front (`BEGIN IMMEDIATE`) and
    /// CASes every captured field — item id, library id and current
    /// library-root binding, stored path, media revision, expected probe
    /// revision, content identity, mtime, and size. A mismatch writes nothing
    /// and returns [`ProbePublication::Stale`]. A success writes every
    /// scalar, replaces both complete inventories at `probe_revision + 1`, and
    /// sets the validity stamps in that one transaction; a failure records the
    /// status and error, clears both validity stamps, and leaves the prior
    /// facts and inventories in place. Any child or scalar write failure rolls
    /// the whole publication back and returns `Err`.
    pub fn publish_probe(
        &self,
        expectation: &ProbeExpectation,
        outcome: &ProbeOutcome,
    ) -> Result<ProbePublication, String> {
        let conn = self.lock()?;
        // Reads before it writes, so take the write lock up front; `with_write_tx`
        // retries the lock rather than losing the publication to a busy peer.
        with_write_tx(&conn, |tx| publish_probe_tx(tx, expectation, outcome))
    }

    pub fn set_subtitle_status(&self, item_id: i64, status: &str) -> Result<(), String> {
        let status = parse_subtitle_status(status)?;
        let conn = self.lock()?;
        if status == "unavailable" {
            // ADR-0041 Decision 8.3: every availability failure increments the
            // attempt count and pushes the re-queue deadline out on the
            // ADR-0026 §3 schedule (1d/7d/30d/90d cap), so a flapping mount
            // cannot re-drain an unfinishable title on every reachability
            // transition. `requeue_unavailable_for_library` gates on
            // `subtitle_next_retry_at`.
            let attempts: i64 = conn
                .query_row(
                    "SELECT subtitle_attempt_count FROM media_items WHERE id = ?1",
                    params![item_id],
                    |r| r.get(0),
                )
                .map_err(|e| format!("read subtitle attempts for item {item_id}: {e}"))?;
            let days = backoff_days(attempts.saturating_add(1));
            conn.execute(
                "UPDATE media_items SET
                    subtitle_status = 'unavailable',
                    subtitle_content_id = NULL,
                    subtitle_attempt_count = subtitle_attempt_count + 1,
                    subtitle_next_retry_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?2)
                 WHERE id = ?1",
                params![item_id, format!("+{days} days")],
            )
            .map_err(|e| format!("set subtitle unavailable for item {item_id}: {e}"))?;
            return Ok(());
        }
        conn.execute(
            "UPDATE media_items SET
                subtitle_status = ?2,
                subtitle_content_id = CASE
                    WHEN ?2 IN ('ready', 'none') THEN content_id
                    ELSE NULL
                END,
                subtitle_attempt_count = 0,
                subtitle_next_retry_at = NULL
             WHERE id = ?1",
            params![item_id, status],
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
                "UPDATE media_items SET subtitle_status = 'pending'
                 WHERE library_id = ?1 AND subtitle_status = 'unavailable'
                   AND (subtitle_next_retry_at IS NULL
                        OR subtitle_next_retry_at <= strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
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
        // Reads before it writes, so it takes the write lock up front. As a
        // deferred transaction this SELECT took a read snapshot that the
        // metadata drain's next commit invalidated, and the DELETE below then
        // failed instantly with SQLITE_BUSY_SNAPSHOT — 285 times on the
        // 2026-08-07 cold scan, median 93 µs apart, each one a WARN with no
        // retry and nothing to revisit the item. The external subtitle was
        // simply never associated.
        let conn = self.lock()?;

        // Nothing found beside the file and nothing stored: there is nothing to
        // reconcile, so do not open a write transaction at all.
        //
        // The index pass calls this for *every* item it upserts, and on a
        // typical library most items have no sidecar. Without this the pass
        // pays one `BEGIN IMMEDIATE` and one no-op `DELETE` per item —
        // ~25,000 of them on a cold scan of the dogfood library — each holding
        // the write lock across its own SELECT. Taking the lock up front is
        // what makes the read-then-write path correct, so this is the other
        // half of that change: keep the lock for the items that need it and
        // stop taking it for the ones that do not.
        //
        // It is deliberately **not** enough that nothing was found. No
        // sidecars on disk with rows still stored is the sidecar-was-deleted
        // case, and those rows have to go — that path must still reach the
        // transaction below.
        //
        // The existence check runs under `self.lock()` on the shared
        // connection, and this function is the only writer of
        // `media_item_sidecars`, so no row can appear between the check and
        // the return. It is an index lookup
        // (`idx_media_item_sidecars_item`), not a scan.
        if sidecars.is_empty() && !has_stored_sidecars(&conn, media_item_id)? {
            return Ok(false);
        }

        with_write_tx(&conn, |tx| {
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
            Ok(changed)
        })
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

    /// Replace all subtitle-stream inventory rows for one media item
    /// (ADR-0041 Decision 1; re-probe replaces the rows).
    ///
    /// **Legacy revision-0 writer, superseded in production.** The scanner's
    /// probe publishes its inventory through [`Db::publish_probe`], which
    /// stamps the rows with the new probe revision in the same transaction as
    /// the scalars. Rows written here carry probe revision 0, so they cannot
    /// certify a publication. This remains for tests that need a stored
    /// inventory without a full snapshot.
    pub fn replace_item_subtitle_tracks(
        &self,
        media_item_id: i64,
        tracks: &[SubtitleTrackRow],
    ) -> Result<(), String> {
        let conn = self.lock()?;
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("begin subtitle track replace: {e}"))?;
        tx.execute(
            "DELETE FROM media_item_subtitle_tracks WHERE media_item_id = ?1",
            [media_item_id],
        )
        .map_err(|e| format!("clear subtitle tracks for item {media_item_id}: {e}"))?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO media_item_subtitle_tracks (
                        media_item_id, stream_index, codec, language, title,
                        forced, sdh, kind
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )
                .map_err(|e| format!("prepare subtitle track insert: {e}"))?;
            for t in tracks {
                stmt.execute(params![
                    media_item_id,
                    t.stream_index,
                    t.codec,
                    t.language,
                    t.title,
                    t.forced as i64,
                    t.sdh as i64,
                    t.kind,
                ])
                .map_err(|e| format!("insert subtitle track {}: {e}", t.stream_index))?;
            }
        }
        tx.commit()
            .map_err(|e| format!("commit subtitle track replace: {e}"))?;
        Ok(())
    }

    pub fn list_item_subtitle_tracks(
        &self,
        media_item_id: i64,
    ) -> Result<Vec<SubtitleTrackRow>, String> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT media_item_id, stream_index, codec, language, title,
                        forced, sdh, kind
                 FROM media_item_subtitle_tracks
                 WHERE media_item_id = ?1
                 ORDER BY stream_index",
            )
            .map_err(|e| format!("prepare list subtitle tracks: {e}"))?;
        let rows = stmt
            .query_map([media_item_id], map_subtitle_track)
            .map_err(|e| format!("list subtitle tracks for item {media_item_id}: {e}"))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| format!("map subtitle track: {e}"))?);
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

    /// Most recent scan job for a library, whatever its state.
    pub fn latest_scan_job(&self, library_id: i64) -> Result<Option<ScanJobRow>, String> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, library_id, state, added, updated, removed, unchanged,
                    probed, errors, index_duration_ms, probe_duration_ms,
                    error_message, started_at, finished_at, kind, candidate_path,
                    skipped_outside_root, deferred_remove
             FROM scan_jobs WHERE library_id = ?1 ORDER BY id DESC LIMIT 1",
            [library_id],
            map_scan_job,
        )
        .optional()
        .map_err(|e| format!("latest scan job for library {library_id}: {e}"))
    }

    /// Live counts behind a library's progress display.
    ///
    /// `probe_status` and `metadata_status` are read as named counts rather
    /// than returned as histograms, because `indexed` is a queue depth and
    /// `probed` on the job row is a cumulative total, and a caller handed both
    /// in one shape has to know which is which to avoid dividing one by the
    /// other.
    pub fn scan_progress_counts(&self, library_id: i64) -> Result<ScanProgressCounts, String> {
        let conn = self.lock()?;
        let found: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM media_items WHERE library_id = ?1",
                [library_id],
                |r| r.get(0),
            )
            .map_err(|e| format!("progress found count: {e}"))?;
        let probe_queued: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM media_items
                 WHERE library_id = ?1 AND probe_status = 'indexed'",
                [library_id],
                |r| r.get(0),
            )
            .map_err(|e| format!("progress probe queue depth: {e}"))?;
        let probe_errors: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM media_items
                 WHERE library_id = ?1 AND probe_status = 'error'",
                [library_id],
                |r| r.get(0),
            )
            .map_err(|e| format!("progress probe errors: {e}"))?;
        // 'matched' is identity-found-detail-pending (ADR-0026 §8.1), which is
        // still draining, so it counts as pending here for the same reason the
        // drain's own remaining query counts it.
        let metadata_pending: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM media_items
                 WHERE library_id = ?1 AND metadata_status IN ('pending', 'matched')",
                [library_id],
                |r| r.get(0),
            )
            .map_err(|e| format!("progress metadata pending: {e}"))?;
        let metadata_ready: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM media_items
                 WHERE library_id = ?1 AND metadata_status = 'ready'",
                [library_id],
                |r| r.get(0),
            )
            .map_err(|e| format!("progress metadata ready: {e}"))?;
        let metadata_unmatched: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM media_items
                 WHERE library_id = ?1 AND metadata_status = 'unmatched'",
                [library_id],
                |r| r.get(0),
            )
            .map_err(|e| format!("progress metadata unmatched: {e}"))?;
        Ok(ScanProgressCounts {
            found,
            probe_queued,
            probe_errors,
            metadata_pending,
            metadata_ready,
            metadata_unmatched,
        })
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
        video_frame_rate_num: r.get(18)?,
        video_frame_rate_den: r.get(19)?,
        hdr: r.get(20)?,
        probe_status: r.get(21)?,
        scan_error: r.get(22)?,
        subtitle_status: r.get(23)?,
        content_id: r.get(24)?,
        probed_content_id: r.get(25)?,
        subtitle_content_id: r.get(26)?,
        usable_extent_ms: r.get(27)?,
        usable_extent_content_id: r.get(28)?,
        map_status: r.get(29)?,
        map_content_id: r.get(30)?,
        metadata_status: r.get(31)?,
        media_revision: r.get(32)?,
        probe_revision: r.get(33)?,
        probed_media_revision: r.get(34)?,
        video_stream_index: r.get(35)?,
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

/// Whether any sidecar row is stored for this item.
///
/// Index lookup on `idx_media_item_sidecars_item` (migration 003), not a scan.
/// Used to skip the reconcile transaction entirely when nothing was found on
/// disk and nothing is stored.
fn has_stored_sidecars(conn: &Connection, media_item_id: i64) -> Result<bool, String> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM media_item_sidecars WHERE media_item_id = ?1)",
        [media_item_id],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n != 0)
    .map_err(|e| format!("check stored sidecars for item {media_item_id}: {e}"))
}

fn map_subtitle_track(r: &rusqlite::Row<'_>) -> rusqlite::Result<SubtitleTrackRow> {
    let forced: i64 = r.get(5)?;
    let sdh: i64 = r.get(6)?;
    Ok(SubtitleTrackRow {
        media_item_id: r.get(0)?,
        stream_index: r.get(1)?,
        codec: r.get(2)?,
        language: r.get(3)?,
        title: r.get(4)?,
        forced: forced != 0,
        sdh: sdh != 0,
        kind: r.get(7)?,
    })
}

/// The body of [`Db::publish_probe`], run inside one `BEGIN IMMEDIATE`
/// transaction (ADR-0058).
///
/// It is a free function so the transaction and the CAS stay together and the
/// whole publication is one closure body: either every write in it commits or
/// none does.
fn publish_probe_tx(
    tx: &Transaction<'_>,
    expectation: &ProbeExpectation,
    outcome: &ProbeOutcome,
) -> Result<ProbePublication, String> {
    // CAS every captured field. `m.content_id IS ?7` is the NULL-safe form:
    // a captured NULL identity matches a NULL column and nothing else.
    let matched: Option<(i64, i64)> = tx
        .query_row(
            "SELECT m.media_revision, m.probe_revision
               FROM media_items m
               JOIN libraries l ON l.id = m.library_id
              WHERE m.id = ?1
                AND m.library_id = ?2
                AND l.path = ?3
                AND m.path = ?4
                AND m.media_revision = ?5
                AND m.probe_revision = ?6
                AND m.content_id IS ?7
                AND m.mtime_ms = ?8
                AND m.size_bytes = ?9",
            params![
                expectation.item_id,
                expectation.library_id,
                expectation.library_root,
                expectation.path,
                expectation.media_revision,
                expectation.probe_revision,
                expectation.content_id,
                expectation.mtime_ms,
                expectation.size_bytes,
            ],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(|e| format!("probe CAS for item {}: {e}", expectation.item_id))?;
    let Some((media_revision, probe_revision)) = matched else {
        return Ok(ProbePublication::Stale);
    };

    match outcome {
        ProbeOutcome::Failure {
            probe_status,
            scan_error,
        } => {
            let status = parse_probe_status(probe_status)?;
            if !matches!(status, "error" | "unavailable") {
                return Err(format!(
                    "probe failure for item {} must be 'error' or 'unavailable', got '{status}'",
                    expectation.item_id
                ));
            }
            // Diagnostics land; both validity stamps clear; technical facts,
            // inventories, and `probe_revision` are left as they were.
            tx.execute(
                "UPDATE media_items SET
                    probe_status = ?2,
                    scan_error = ?3,
                    probed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                    probed_media_revision = NULL,
                    probed_content_id = NULL
                 WHERE id = ?1",
                params![expectation.item_id, status, scan_error],
            )
            .map_err(|e| format!("record probe failure for item {}: {e}", expectation.item_id))?;
            Ok(ProbePublication::FailureRecorded)
        }
        ProbeOutcome::Success(snapshot) => {
            // NULL or empty identity cannot certify a success (ADR-0058).
            let Some(content_id) = expectation
                .content_id
                .as_deref()
                .filter(|id| !id.is_empty())
            else {
                return Ok(ProbePublication::Stale);
            };
            let subtitle_status = parse_subtitle_status(&snapshot.subtitle_status)?;
            let next = probe_revision.checked_add(1).ok_or_else(|| {
                format!("probe revision overflow for item {}", expectation.item_id)
            })?;

            tx.execute(
                "UPDATE media_items SET
                    duration_ms = ?2,
                    container = ?3,
                    video_codec = ?4,
                    audio_codec = ?5,
                    audio_channels = ?6,
                    width = ?7,
                    height = ?8,
                    video_bitrate_bps = ?9,
                    video_frame_rate_num = ?10,
                    video_frame_rate_den = ?11,
                    hdr = ?12,
                    video_stream_index = ?13,
                    probe_status = 'probed',
                    scan_error = NULL,
                    probed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                    subtitle_status = ?14,
                    subtitle_content_id = CASE
                        WHEN ?14 IN ('ready', 'none') THEN ?17 ELSE NULL END,
                    subtitle_attempt_count = 0,
                    subtitle_next_retry_at = NULL,
                    probe_revision = ?15,
                    probed_media_revision = ?16,
                    probed_content_id = ?17
                 WHERE id = ?1",
                params![
                    expectation.item_id,
                    snapshot.duration_ms,
                    snapshot.container,
                    snapshot.video_codec,
                    snapshot.audio_codec,
                    snapshot.audio_channels,
                    snapshot.width,
                    snapshot.height,
                    snapshot.video_bitrate_bps,
                    snapshot.video_frame_rate_num,
                    snapshot.video_frame_rate_den,
                    snapshot.hdr,
                    snapshot.video_stream_index,
                    subtitle_status,
                    next,
                    media_revision,
                    content_id,
                ],
            )
            .map_err(|e| format!("publish probe facts for item {}: {e}", expectation.item_id))?;

            // Complete replacement, stamped with the new revision. An empty
            // inventory deletes the old rows and inserts nothing.
            tx.execute(
                "DELETE FROM media_item_audio_tracks WHERE media_item_id = ?1",
                [expectation.item_id],
            )
            .map_err(|e| {
                format!(
                    "clear audio inventory for item {}: {e}",
                    expectation.item_id
                )
            })?;
            {
                let mut stmt = tx
                    .prepare(
                        "INSERT INTO media_item_audio_tracks (
                            media_item_id, probe_revision, stream_index, codec, language,
                            channels, channel_layout, title, is_default
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    )
                    .map_err(|e| format!("prepare audio track insert: {e}"))?;
                for track in &snapshot.audio_tracks {
                    stmt.execute(params![
                        expectation.item_id,
                        next,
                        track.stream_index,
                        track.codec,
                        track.language,
                        track.channels,
                        track.channel_layout,
                        track.title,
                        track.is_default as i64,
                    ])
                    .map_err(|e| {
                        format!(
                            "insert audio track {} for item {}: {e}",
                            track.stream_index, expectation.item_id
                        )
                    })?;
                }
            }
            tx.execute(
                "DELETE FROM media_item_subtitle_tracks WHERE media_item_id = ?1",
                [expectation.item_id],
            )
            .map_err(|e| {
                format!(
                    "clear subtitle inventory for item {}: {e}",
                    expectation.item_id
                )
            })?;
            {
                let mut stmt = tx
                    .prepare(
                        "INSERT INTO media_item_subtitle_tracks (
                            media_item_id, stream_index, codec, language, title,
                            forced, sdh, kind, probe_revision
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    )
                    .map_err(|e| format!("prepare subtitle track insert: {e}"))?;
                for track in &snapshot.subtitle_tracks {
                    stmt.execute(params![
                        expectation.item_id,
                        track.stream_index,
                        track.codec,
                        track.language,
                        track.title,
                        track.forced as i64,
                        track.sdh as i64,
                        track.kind,
                        next,
                    ])
                    .map_err(|e| {
                        format!(
                            "insert subtitle track {} for item {}: {e}",
                            track.stream_index, expectation.item_id
                        )
                    })?;
                }
            }

            Ok(ProbePublication::Published {
                media_revision,
                probe_revision: next,
            })
        }
    }
}

#[cfg(test)]
mod write_tx_tests {
    use super::*;

    /// Two connections on one WAL database, as the process actually runs: the
    /// store's shared connection and the metadata drain's private one.
    fn two_conns() -> (tempfile::TempDir, Connection, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let open = || {
            let c = Connection::open(&path).unwrap();
            // Short timeout: these tests deliberately collide, and the
            // production 5,000 ms would just make them slow.
            c.execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA busy_timeout=50;",
            )
            .unwrap();
            c
        };
        let a = open();
        a.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER NOT NULL)")
            .unwrap();
        let b = open();
        (dir, a, b)
    }

    #[test]
    fn deferred_read_then_write_loses_its_snapshot_to_another_connection() {
        // The defect, reproduced: this is what `replace_item_sidecars` did 285
        // times on the 2026-08-07 cold scan. It is here so the fix below is
        // measured against a failure that actually happens.
        let (_dir, a, b) = two_conns();
        let tx = a.unchecked_transaction().unwrap();
        let _: i64 = tx
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap();
        b.execute("INSERT INTO t (v) VALUES (1)", []).unwrap();

        let err = tx.execute("INSERT INTO t (v) VALUES (2)", []).unwrap_err();
        assert!(
            is_busy_error(&err),
            "expected a busy/snapshot failure upgrading a stale read, got {err:?}"
        );
    }

    #[test]
    fn write_tx_survives_a_commit_from_another_connection() {
        let (_dir, a, b) = two_conns();
        let wrote = with_write_tx(&a, |tx| {
            let n: i64 = tx
                .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
                .map_err(|e| e.to_string())?;
            // The interleaving that broke the deferred version. Taking the
            // write lock first means this connection cannot get in here at
            // all, so there is no stale snapshot to upgrade.
            assert!(b.execute("INSERT INTO t (v) VALUES (1)", []).is_err());
            tx.execute("INSERT INTO t (v) VALUES (2)", [])
                .map_err(|e| e.to_string())?;
            Ok(n)
        })
        .expect("write transaction should commit");
        assert_eq!(wrote, 0, "read saw the pre-write state");

        let total: i64 = a
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 1, "only the transaction's own insert landed");
    }

    #[test]
    fn with_write_tx_returns_the_closure_error_unchanged() {
        let (_dir, a, _b) = two_conns();
        let err = with_write_tx(&a, |_tx| Err::<(), _>("nope".to_string())).unwrap_err();
        assert_eq!(
            err, "nope",
            "a caller error must not be retried or reworded"
        );
        assert!(
            a.is_autocommit(),
            "a failed attempt must roll back, not leave the connection in a transaction"
        );
        with_write_tx(&a, |tx| {
            tx.execute("INSERT INTO t (v) VALUES (1)", [])
                .map_err(|e| e.to_string())?;
            Ok(())
        })
        .expect("connection is usable after a rolled-back attempt");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One indexed item, no keyframe map, for the sidecar reconcile tests.
    fn item_for_sidecars(db: &Db) -> i64 {
        let lib = db
            .create_library(&NewLibrary {
                name: "films".into(),
                path: "/films".into(),
                kind: "movies".into(),
            })
            .unwrap();
        db.upsert_items_indexed(
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
                content_id: None,
            }],
        )
        .unwrap()[0]
    }

    fn sidecar(item_id: i64, track_id: &str) -> SidecarRow {
        SidecarRow {
            media_item_id: item_id,
            track_id: track_id.into(),
            path: "clip.en.srt".into(),
            mtime_ms: 10,
            size_bytes: 20,
            format: "srt".into(),
            language: Some("en".into()),
            forced: false,
            sdh: false,
        }
    }

    /// Nothing found and nothing stored must not open a write transaction.
    ///
    /// Proved by behaviour rather than by inspection: another connection holds
    /// the write lock for the whole call. A path that opens `BEGIN IMMEDIATE`
    /// blocks on it and fails when `busy_timeout` expires; the fast path never
    /// asks for the lock and returns immediately.
    ///
    /// The index pass calls this once per upserted item, so before this the
    /// cost was one write transaction and one no-op DELETE per item on every
    /// cold scan — and it is independent of `INDEX_BATCH`, so no flush-size
    /// change reaches it.
    #[test]
    fn nothing_found_and_nothing_stored_takes_no_write_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let db = Db::open(&path).unwrap();
        let item_id = item_for_sidecars(&db);

        // Second connection, as the metadata drain is, holding the write lock.
        let other = Connection::open(&path).unwrap();
        other
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=50;")
            .unwrap();
        let blocker = Transaction::new_unchecked(&other, TransactionBehavior::Immediate).unwrap();
        blocker
            .execute(
                "UPDATE media_items SET title = 'held' WHERE id = ?1",
                [item_id],
            )
            .unwrap();

        let changed = db
            .replace_item_sidecars(item_id, &[])
            .expect("must not need the write lock when there is nothing to reconcile");
        assert!(!changed, "nothing to reconcile is not a change");

        drop(blocker);
    }

    /// The case the fast path must not swallow: the sidecar file was deleted,
    /// so nothing is found, but stored rows still have to go. "Nothing found"
    /// alone is not the condition — "nothing found and nothing stored" is.
    #[test]
    fn nothing_found_still_deletes_stored_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let item_id = item_for_sidecars(&db);

        assert!(
            db.replace_item_sidecars(item_id, &[sidecar(item_id, "s1")])
                .unwrap(),
            "storing the first sidecar is a change"
        );
        assert_eq!(db.list_item_sidecars(item_id).unwrap().len(), 1);

        // Sidecar file removed from disk: discovery finds nothing.
        assert!(
            db.replace_item_sidecars(item_id, &[]).unwrap(),
            "removing the last sidecar is a change"
        );
        assert!(
            db.list_item_sidecars(item_id).unwrap().is_empty(),
            "stored rows must be deleted when the sidecar is gone"
        );

        // And now that both sides are empty, the fast path applies.
        assert!(!db.replace_item_sidecars(item_id, &[]).unwrap());
    }

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

    /// ADR-0023 §6/§8: a re-index of a changed file clears the stale map
    /// rows; the item is left unmapped so the §9 demand trigger (playbackInfo
    /// / session create) rebuilds it. The scan path never queues the whole
    /// library, but the replace invalidation still holds.
    #[test]
    fn reindex_upsert_clears_stale_map_for_replaced_file() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("nightjar.db")).unwrap();
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
                    content_id: Some("1-aaa-bbb".into()),
                }],
            )
            .unwrap();
        let item_id = ids[0];
        db.replace_keyframe_map(item_id, "1-aaa-bbb", "matroska", &[(0, 100)], None)
            .unwrap();
        assert!(db.keyframe_map(item_id).unwrap().is_some());

        // File replaced under the path: mtime and size moved.
        db.upsert_items_indexed(
            lib.id,
            &[UpsertItem {
                path: "clip.mkv".into(),
                mtime_ms: 2,
                size_bytes: 3,
                title: "clip".into(),
                kind: "movie".into(),
                year: None,
                season: None,
                episode: None,
                content_id: Some("2-ccc-ddd".into()),
            }],
        )
        .unwrap();
        assert!(
            db.keyframe_map(item_id).unwrap().is_none(),
            "stale byte offsets must not survive a replace"
        );
        let row = db.get_item(item_id).unwrap().unwrap();
        assert_eq!(
            row.map_status, "pending",
            "replaced item is unmapped until a consumer asks (ADR-0023 §9)"
        );
        assert!(row.map_content_id.is_none());
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

    /// ADR-0041 Decision 1: probe-time inventory replace is delete+insert, so
    /// a re-probe can never leave stale rows, and kind values survive the
    /// migration-017 CHECK round-trip.
    #[test]
    fn subtitle_tracks_replace_and_list_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("nightjar.db")).unwrap();
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: "/t".into(),
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
                    content_id: None,
                }],
            )
            .unwrap();
        let item_id = ids[0];
        let first = vec![
            SubtitleTrackRow {
                media_item_id: item_id,
                stream_index: 2,
                codec: "subrip".into(),
                language: Some("eng".into()),
                title: None,
                forced: false,
                sdh: true,
                kind: "text".into(),
            },
            SubtitleTrackRow {
                media_item_id: item_id,
                stream_index: 3,
                codec: "hdmv_pgs_subtitle".into(),
                language: None,
                title: None,
                forced: false,
                sdh: false,
                kind: "image".into(),
            },
        ];
        db.replace_item_subtitle_tracks(item_id, &first).unwrap();
        assert_eq!(db.list_item_subtitle_tracks(item_id).unwrap(), first);

        // A re-probe replaces, never appends: drop the image track, add an
        // unknown-codec one.
        let second = vec![SubtitleTrackRow {
            media_item_id: item_id,
            stream_index: 4,
            codec: "".into(),
            language: None,
            title: None,
            forced: false,
            sdh: false,
            kind: "unknown".into(),
        }];
        db.replace_item_subtitle_tracks(item_id, &second).unwrap();
        assert_eq!(db.list_item_subtitle_tracks(item_id).unwrap(), second);
    }

    /// ADR-0041 Decision 8.3: an `unavailable` write records the attempt and
    /// sets a re-queue deadline on the ADR-0026 §3 schedule; a later
    /// reachability re-queue skips items still inside their backoff window and
    /// requeues only those past it. Any non-`unavailable` write resets the
    /// retry state.
    #[test]
    fn subtitle_unavailable_backs_off_across_requeue() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("nightjar.db")).unwrap();
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: "/t".into(),
                kind: "movies".into(),
            })
            .unwrap();
        let ids = db
            .upsert_items_indexed(
                lib.id,
                &[
                    UpsertItem {
                        path: "a.mkv".into(),
                        mtime_ms: 1,
                        size_bytes: 2,
                        title: "a".into(),
                        kind: "movie".into(),
                        year: None,
                        season: None,
                        episode: None,
                        content_id: None,
                    },
                    UpsertItem {
                        path: "b.mkv".into(),
                        mtime_ms: 1,
                        size_bytes: 2,
                        title: "b".into(),
                        kind: "movie".into(),
                        year: None,
                        season: None,
                        episode: None,
                        content_id: None,
                    },
                    UpsertItem {
                        path: "c.mkv".into(),
                        mtime_ms: 1,
                        size_bytes: 2,
                        title: "c".into(),
                        kind: "movie".into(),
                        year: None,
                        season: None,
                        episode: None,
                        content_id: None,
                    },
                ],
            )
            .unwrap();
        let (a, b, c) = (ids[0], ids[1], ids[2]);

        // First failure: attempt 1, deadline one day out (ADR-0026 §3).
        db.set_subtitle_status(a, "unavailable").unwrap();
        db.set_subtitle_status(b, "unavailable").unwrap();
        let retry_state = |id: i64| -> (i64, Option<String>) {
            db.lock()
                .unwrap()
                .query_row(
                    "SELECT subtitle_attempt_count, subtitle_next_retry_at
                     FROM media_items WHERE id = ?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap()
        };
        assert_eq!(retry_state(a).0, 1);
        let (_, retry_a) = retry_state(a);
        assert!(retry_a.is_some(), "first failure must set a retry deadline");
        let (_, retry_b) = retry_state(b);
        assert!(retry_b.is_some());

        // A second failure escalates to 7 days.
        db.set_subtitle_status(a, "unavailable").unwrap();
        let (attempts_a2, retry_a2) = retry_state(a);
        assert_eq!(attempts_a2, 2);
        assert!(retry_a2.as_deref().unwrap() > retry_a.as_deref().unwrap());

        // Requeue gate: all three items are unavailable, but only the one
        // whose deadline has passed (c, expired by hand) is requeued. a and b
        // stay unavailable until their deadlines expire.
        db.set_subtitle_status(c, "unavailable").unwrap();
        db.lock()
            .unwrap()
            .execute(
                "UPDATE media_items SET subtitle_next_retry_at = '2000-01-01T00:00:00.000Z'
                 WHERE id = ?1",
                [c],
            )
            .unwrap();
        let (probes, extracts, maps) = db.requeue_unavailable_for_library(lib.id).unwrap();
        assert_eq!(probes, 0);
        assert_eq!(extracts, 1, "only c (deadline passed) may requeue now");
        assert_eq!(maps, 0);
        assert_eq!(
            db.get_item(a).unwrap().unwrap().subtitle_status,
            "unavailable",
            "a stays inside its backoff window"
        );
        assert_eq!(
            db.get_item(c).unwrap().unwrap().subtitle_status,
            "pending",
            "c requeued past its deadline"
        );

        // Any non-unavailable write resets the retry state.
        db.set_subtitle_status(a, "eligible").unwrap();
        let (attempts_a3, retry_a3) = retry_state(a);
        assert_eq!(attempts_a3, 0);
        assert_eq!(retry_a3, None);
    }

    /// Progress reads name their measures rather than handing back a
    /// histogram: `probe_queued` is a live queue depth over `probe_status`,
    /// while the job row's `probed` is a cumulative count for that job. Seeded
    /// rather than observed from a scan, so it is reproducible.
    #[test]
    fn scan_progress_counts_are_named_not_a_histogram() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = db
            .create_library(&NewLibrary {
                name: "tv".into(),
                path: "/tv".into(),
                kind: "shows".into(),
            })
            .unwrap();
        let other = db
            .create_library(&NewLibrary {
                name: "films".into(),
                path: "/films".into(),
                kind: "movies".into(),
            })
            .unwrap();
        db.with_conn(|c| {
            c.execute(
                "INSERT INTO media_items
                    (library_id, path, mtime_ms, size_bytes, title, kind,
                     probe_status, metadata_status)
                 VALUES (?1, 'a.mkv', 1, 1, 'a', 'episode', 'indexed', 'pending'),
                        (?1, 'b.mkv', 1, 1, 'b', 'episode', 'indexed', 'matched'),
                        (?1, 'c.mkv', 1, 1, 'c', 'episode', 'probed', 'ready'),
                        (?1, 'd.mkv', 1, 1, 'd', 'episode', 'error', 'unmatched'),
                        (?2, 'e.mkv', 1, 1, 'e', 'movie', 'indexed', 'pending')",
                params![lib.id, other.id],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        })
        .unwrap();

        let counts = db.scan_progress_counts(lib.id).unwrap();
        assert_eq!(counts.found, 4, "the other library's row is not counted");
        assert_eq!(counts.probe_queued, 2);
        assert_eq!(counts.probe_errors, 1);
        // ADR-0026 §8.1: `matched` is identity found and detail still pending,
        // which is work in flight, so it counts as pending here.
        assert_eq!(counts.metadata_pending, 2);
        assert_eq!(counts.metadata_ready, 1);
        assert_eq!(counts.metadata_unmatched, 1);
    }

    #[test]
    fn latest_scan_job_is_the_newest_for_that_library() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = db
            .create_library(&NewLibrary {
                name: "tv".into(),
                path: "/tv".into(),
                kind: "shows".into(),
            })
            .unwrap();
        assert!(db.latest_scan_job(lib.id).unwrap().is_none());

        let first = db.create_scan_job(lib.id).unwrap();
        db.complete_scan_job(first, 10).unwrap();
        let second = db.create_scan_job(lib.id).unwrap();
        db.set_scan_job_state(second, "indexing").unwrap();

        let job = db.latest_scan_job(lib.id).unwrap().unwrap();
        assert_eq!(job.id, second);
        assert_eq!(job.state, "indexing");
    }

    // ------------------------------------------------------------------
    // ADR-0058 revision reconciliation
    // ------------------------------------------------------------------

    fn revision_library(db: &Db, root: &str) -> i64 {
        db.create_library(&NewLibrary {
            name: "films".into(),
            path: root.into(),
            kind: "movies".into(),
        })
        .unwrap()
        .id
    }

    fn upsert_observed(
        db: &Db,
        library_id: i64,
        path: &str,
        mtime_ms: i64,
        content_id: Option<&str>,
    ) -> i64 {
        db.upsert_items_indexed(
            library_id,
            &[UpsertItem {
                path: path.into(),
                mtime_ms,
                size_bytes: 2,
                title: "clip".into(),
                kind: "movie".into(),
                year: None,
                season: None,
                episode: None,
                content_id: content_id.map(str::to_string),
            }],
        )
        .unwrap()[0]
    }

    /// The expectation the publisher CASes, taken from the stored row exactly
    /// as the scanner captures it before probing (ADR-0058).
    fn expectation_of(db: &Db, item_id: i64) -> ProbeExpectation {
        let row = db.get_item(item_id).unwrap().unwrap();
        let lib = db.get_library(row.library_id).unwrap().unwrap();
        ProbeExpectation {
            item_id: row.id,
            library_id: row.library_id,
            library_root: lib.path,
            path: row.path,
            media_revision: row.media_revision,
            probe_revision: row.probe_revision,
            content_id: row.content_id,
            mtime_ms: row.mtime_ms,
            size_bytes: row.size_bytes,
        }
    }

    /// One complete successful snapshot, with distinct values in every field
    /// so a partial write is visible.
    fn complete_snapshot(item_id: i64) -> ProbeSnapshot {
        ProbeSnapshot {
            duration_ms: Some(1000),
            container: Some("matroska".into()),
            video_codec: Some("h264".into()),
            video_stream_index: Some(0),
            audio_codec: Some("aac".into()),
            audio_channels: Some(2),
            width: Some(1920),
            height: Some(1080),
            video_bitrate_bps: Some(5_000_000),
            video_frame_rate_num: Some(24),
            video_frame_rate_den: Some(1),
            hdr: Some("none".into()),
            audio_tracks: vec![AudioTrackRow {
                stream_index: 1,
                codec: "aac".into(),
                language: Some("eng".into()),
                channels: Some(2),
                channel_layout: Some("stereo".into()),
                title: Some("Main".into()),
                is_default: true,
            }],
            subtitle_tracks: vec![SubtitleTrackRow {
                media_item_id: item_id,
                stream_index: 2,
                codec: "subrip".into(),
                language: Some("eng".into()),
                title: None,
                forced: false,
                sdh: false,
                kind: "text".into(),
            }],
            subtitle_status: "eligible".into(),
        }
    }

    fn publish_complete_success(db: &Db, item_id: i64) -> ProbePublication {
        db.publish_probe(
            &expectation_of(db, item_id),
            &ProbeOutcome::Success(Box::new(complete_snapshot(item_id))),
        )
        .unwrap()
    }

    fn raw_media_revision(db: &Db, item_id: i64) -> i64 {
        db.with_conn(|c| {
            c.query_row(
                "SELECT media_revision FROM media_items WHERE id = ?1",
                params![item_id],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())
        })
        .unwrap()
    }

    /// `(probe_revision, probed_at, probe_status, scan_error)` straight from
    /// the row, so a "complete no-op" can be asserted on columns the typed row
    /// does not carry.
    fn probe_stamps(db: &Db, item_id: i64) -> (i64, Option<String>, String, Option<String>) {
        db.with_conn(|c| {
            c.query_row(
                "SELECT probe_revision, probed_at, probe_status, scan_error
                   FROM media_items WHERE id = ?1",
                params![item_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .map_err(|e| e.to_string())
        })
        .unwrap()
    }

    /// Raw audio inventory as `(probe_revision, stream_index, codec, is_default)`.
    fn audio_inventory(db: &Db, item_id: i64) -> Vec<(i64, i64, String, i64)> {
        db.with_conn(|c| {
            c.prepare(
                "SELECT probe_revision, stream_index, codec, is_default
                   FROM media_item_audio_tracks
                  WHERE media_item_id = ?1 ORDER BY stream_index",
            )
            .map_err(|e| e.to_string())?
            .query_map([item_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
        })
        .unwrap()
    }

    /// Raw subtitle `probe_revision`s ordered by absolute stream index.
    fn subtitle_revisions(db: &Db, item_id: i64) -> Vec<i64> {
        db.with_conn(|c| {
            c.prepare(
                "SELECT probe_revision FROM media_item_subtitle_tracks
                  WHERE media_item_id = ?1 ORDER BY stream_index",
            )
            .map_err(|e| e.to_string())?
            .query_map([item_id], |r| r.get(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<i64>, _>>()
            .map_err(|e| e.to_string())
        })
        .unwrap()
    }

    #[test]
    fn new_item_starts_at_revision_one_and_unprobed() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));

        let row = db.get_item(id).unwrap().unwrap();
        assert_eq!(row.media_revision, 1);
        assert_eq!(row.probe_revision, 0);
        assert_eq!(row.probed_media_revision, None);
        assert_eq!(row.probed_content_id, None);
        assert_eq!(row.video_stream_index, None);
    }

    /// The observation is unchanged when the content identity is: a touch must
    /// not move the revision or clear the validity stamps.
    #[test]
    fn unchanged_observation_does_not_move_the_revision() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));
        assert_eq!(
            publish_complete_success(&db, id),
            ProbePublication::Published {
                media_revision: 1,
                probe_revision: 1,
            }
        );

        upsert_observed(&db, lib, "clip.mkv", 2, Some("1-aaa-bbb"));

        let row = db.get_item(id).unwrap().unwrap();
        assert_eq!(row.media_revision, 1, "a touch is not a media change");
        assert_eq!(row.probed_media_revision, Some(1));
        assert_eq!(row.probed_content_id.as_deref(), Some("1-aaa-bbb"));
    }

    /// A different content identity moves the revision exactly once and clears
    /// both validity stamps. Repeating the same observation moves nothing.
    #[test]
    fn changed_content_identity_moves_the_revision_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));
        assert!(matches!(
            publish_complete_success(&db, id),
            ProbePublication::Published { .. }
        ));

        upsert_observed(&db, lib, "clip.mkv", 2, Some("2-ccc-ddd"));

        let row = db.get_item(id).unwrap().unwrap();
        assert_eq!(row.media_revision, 2, "one accepted change, one increment");
        assert_eq!(row.probed_media_revision, None);
        assert_eq!(row.probed_content_id, None);
        assert_eq!(
            row.probe_revision, 1,
            "identity change is not a publication"
        );

        upsert_observed(&db, lib, "clip.mkv", 3, Some("2-ccc-ddd"));
        assert_eq!(
            raw_media_revision(&db, id),
            2,
            "the same identity again is not a second change"
        );
    }

    /// ADR-0058: an invalid validity stamp makes the prior facts
    /// diagnostic-only; the facts themselves are not cleared.
    #[test]
    fn identity_change_keeps_legacy_technical_facts() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));
        assert!(matches!(
            publish_complete_success(&db, id),
            ProbePublication::Published { .. }
        ));

        upsert_observed(&db, lib, "clip.mkv", 2, Some("2-ccc-ddd"));

        let row = db.get_item(id).unwrap().unwrap();
        assert_eq!(row.duration_ms, Some(1000));
        assert_eq!(row.container.as_deref(), Some("matroska"));
        assert_eq!(row.video_codec.as_deref(), Some("h264"));
        assert_eq!(row.width, Some(1920));
        assert_eq!(row.probed_content_id, None, "stamp cleared");
        assert_eq!(row.media_revision, 2);
    }

    /// A library-root move rebinds every item, so it moves each revision once
    /// and clears the stamps. Re-stating the same root is a no-op.
    #[test]
    fn library_root_change_moves_the_revision_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));
        assert!(matches!(
            publish_complete_success(&db, id),
            ProbePublication::Published { .. }
        ));

        db.update_library_path(lib, "/films2").unwrap();

        let row = db.get_item(id).unwrap().unwrap();
        assert_eq!(row.media_revision, 2);
        assert_eq!(row.probed_media_revision, None);
        assert_eq!(row.probed_content_id, None);

        db.update_library_path(lib, "/films2").unwrap();
        assert_eq!(
            raw_media_revision(&db, id),
            2,
            "the same root is not a second change"
        );

        db.update_library_path(lib, "/films3").unwrap();
        assert_eq!(raw_media_revision(&db, id), 3);
    }

    /// A legacy absolute stored path is a different source path from the
    /// relative one the repair writes, so the repair moves the revision once.
    #[test]
    fn legacy_path_repair_moves_the_revision_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = db
            .with_conn(|c| {
                c.execute(
                    "INSERT INTO media_items
                        (library_id, path, mtime_ms, size_bytes, title, kind,
                         content_id, probed_content_id, media_revision, probe_revision)
                     VALUES (?1, '/films/clip.mkv', 1, 2, 'clip', 'movie',
                             '1-aaa-bbb', '1-aaa-bbb', 1, 0)",
                    params![lib],
                )
                .map_err(|e| e.to_string())?;
                Ok(c.last_insert_rowid())
            })
            .unwrap();

        let unresolved = db.repair_library_paths(lib).unwrap();

        assert_eq!(unresolved, 0);
        let row = db.get_item(id).unwrap().unwrap();
        assert_eq!(row.path, "clip.mkv");
        assert_eq!(row.media_revision, 2);
        assert_eq!(row.probed_media_revision, None);
        assert_eq!(row.probed_content_id, None);
    }

    /// Sidecars belong to the item but not to its media identity, so replacing
    /// them moves neither revision.
    #[test]
    fn sidecar_only_change_moves_neither_revision() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));
        assert!(matches!(
            publish_complete_success(&db, id),
            ProbePublication::Published { .. }
        ));

        assert!(
            db.replace_item_sidecars(id, &[sidecar(id, "s-en")])
                .unwrap()
        );

        let row = db.get_item(id).unwrap().unwrap();
        assert_eq!(row.media_revision, 1);
        assert_eq!(row.probe_revision, 1);
        assert_eq!(row.probed_media_revision, Some(1));
        assert_eq!(row.probed_content_id.as_deref(), Some("1-aaa-bbb"));
    }

    /// The revision is bounded, and the increment refuses to wrap: at the
    /// maximum the write aborts and the row is left as it was.
    #[test]
    fn media_revision_overflow_aborts() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));
        db.with_conn(|c| {
            c.execute(
                "UPDATE media_items SET media_revision = ?2 WHERE id = ?1",
                params![id, i64::MAX],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        })
        .unwrap();

        let err = db
            .upsert_items_indexed(
                lib,
                &[UpsertItem {
                    path: "clip.mkv".into(),
                    mtime_ms: 2,
                    size_bytes: 2,
                    title: "clip".into(),
                    kind: "movie".into(),
                    year: None,
                    season: None,
                    episode: None,
                    content_id: Some("2-ccc-ddd".into()),
                }],
            )
            .unwrap_err();
        assert!(
            err.to_lowercase().contains("check") || err.to_lowercase().contains("constraint"),
            "overflow must abort on the revision CHECK, got: {err}"
        );

        let row = db.get_item(id).unwrap().unwrap();
        assert_eq!(row.media_revision, i64::MAX, "the abort rolls back");
        assert_eq!(row.content_id.as_deref(), Some("1-aaa-bbb"));
    }

    // ------------------------------------------------------------------
    // ADR-0058 atomic probe publication
    // ------------------------------------------------------------------

    /// One transaction writes every scalar and both inventories at one new
    /// revision, and certifies the captured identity.
    #[test]
    fn success_publishes_all_scalars_and_children_at_one_revision() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));

        assert_eq!(
            publish_complete_success(&db, id),
            ProbePublication::Published {
                media_revision: 1,
                probe_revision: 1,
            }
        );

        let row = db.get_item(id).unwrap().unwrap();
        assert_eq!(row.probe_status, "probed");
        assert_eq!(row.probe_revision, 1);
        assert_eq!(row.probed_media_revision, Some(1));
        assert_eq!(row.probed_content_id.as_deref(), Some("1-aaa-bbb"));
        assert_eq!(row.video_stream_index, Some(0));
        assert_eq!(row.duration_ms, Some(1000));
        assert_eq!(row.container.as_deref(), Some("matroska"));
        assert_eq!(row.video_codec.as_deref(), Some("h264"));
        assert_eq!(row.audio_codec.as_deref(), Some("aac"));
        assert_eq!(row.audio_channels, Some(2));
        assert_eq!(row.width, Some(1920));
        assert_eq!(row.height, Some(1080));
        assert_eq!(row.video_bitrate_bps, Some(5_000_000));
        assert_eq!(row.video_frame_rate_num, Some(24));
        assert_eq!(row.video_frame_rate_den, Some(1));
        assert_eq!(row.hdr.as_deref(), Some("none"));
        assert_eq!(row.subtitle_status, "eligible");
        assert_eq!(row.scan_error, None);

        // Both children carry the same new revision as the item.
        assert_eq!(
            audio_inventory(&db, id),
            vec![(1, 1, "aac".to_string(), 1)],
            "audio inventory is written at the publication revision"
        );
        assert_eq!(
            subtitle_revisions(&db, id),
            vec![1],
            "subtitle inventory is written at the publication revision"
        );
        let tracks = db.list_item_subtitle_tracks(id).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].stream_index, 2);
        assert_eq!(tracks[0].kind, "text");
    }

    /// A held expectation must not overwrite a newer publication, nor cross a
    /// media-identity change: both are CAS misses that write nothing.
    #[test]
    fn held_expectation_cannot_overwrite_a_newer_revision() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));

        // A is captured, then B publishes against the same media revision.
        let held_a = expectation_of(&db, id);
        assert_eq!(held_a.probe_revision, 0);
        assert!(matches!(
            publish_complete_success(&db, id),
            ProbePublication::Published {
                probe_revision: 1,
                ..
            }
        ));

        let stale = db
            .publish_probe(
                &held_a,
                &ProbeOutcome::Success(Box::new(complete_snapshot(id))),
            )
            .unwrap();
        assert_eq!(stale, ProbePublication::Stale, "A names an old revision");
        let row = db.get_item(id).unwrap().unwrap();
        assert_eq!(row.probe_revision, 1);
        assert_eq!(row.probed_content_id.as_deref(), Some("1-aaa-bbb"));

        // A held expectation also cannot cross a media-identity change.
        let held_b = expectation_of(&db, id);
        assert_eq!(held_b.media_revision, 1);
        upsert_observed(&db, lib, "clip.mkv", 2, Some("2-ccc-ddd"));
        assert_eq!(raw_media_revision(&db, id), 2);

        let stale = db
            .publish_probe(
                &held_b,
                &ProbeOutcome::Failure {
                    probe_status: "error".into(),
                    scan_error: "late failure".into(),
                },
            )
            .unwrap();
        assert_eq!(stale, ProbePublication::Stale);
        let row = db.get_item(id).unwrap().unwrap();
        assert_eq!(
            row.probe_status, "indexed",
            "the stale failure wrote nothing"
        );
        assert_eq!(row.scan_error, None);
    }

    /// A stale success is a complete no-op: no scalar, child, stamp, or
    /// timestamp moves.
    #[test]
    fn stale_success_is_a_complete_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));
        assert!(matches!(
            publish_complete_success(&db, id),
            ProbePublication::Published { .. }
        ));

        let before = db.get_item(id).unwrap().unwrap();
        let stamps = probe_stamps(&db, id);
        let audio_before = audio_inventory(&db, id);
        let subtitles_before = subtitle_revisions(&db, id);

        // A held expectation, then one CAS field moves under it.
        let held = expectation_of(&db, id);
        db.with_conn(|c| {
            c.execute(
                "UPDATE media_items SET mtime_ms = mtime_ms + 1 WHERE id = ?1",
                [id],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        })
        .unwrap();

        let stale = db
            .publish_probe(
                &held,
                &ProbeOutcome::Success(Box::new(complete_snapshot(id))),
            )
            .unwrap();
        assert_eq!(stale, ProbePublication::Stale);

        let after = db.get_item(id).unwrap().unwrap();
        assert_eq!(after.duration_ms, before.duration_ms);
        assert_eq!(after.container, before.container);
        assert_eq!(after.video_stream_index, before.video_stream_index);
        assert_eq!(after.subtitle_status, before.subtitle_status);
        assert_eq!(after.probed_content_id, before.probed_content_id);
        assert_eq!(
            probe_stamps(&db, id),
            stamps,
            "probe_revision, probed_at, status and error must not move"
        );
        assert_eq!(audio_inventory(&db, id), audio_before);
        assert_eq!(subtitle_revisions(&db, id), subtitles_before);
    }

    /// A stale failure is likewise a strict no-op: the diagnostics are not
    /// written and the validity stamps are not cleared.
    #[test]
    fn stale_failure_is_a_complete_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));
        assert!(matches!(
            publish_complete_success(&db, id),
            ProbePublication::Published { .. }
        ));

        let stamps = probe_stamps(&db, id);
        let held = expectation_of(&db, id);
        upsert_observed(&db, lib, "clip.mkv", 2, Some("2-ccc-ddd"));

        let stale = db
            .publish_probe(
                &held,
                &ProbeOutcome::Failure {
                    probe_status: "error".into(),
                    scan_error: "stale".into(),
                },
            )
            .unwrap();
        assert_eq!(stale, ProbePublication::Stale);

        let row = db.get_item(id).unwrap().unwrap();
        assert_eq!(row.probe_status, "indexed");
        assert_eq!(row.scan_error, None);
        assert_eq!(row.probed_content_id, None);
        assert_eq!(probe_stamps(&db, id).0, 1, "probe_revision is unchanged");
        assert_eq!(stamps.2, "probed", "the prior status was 'probed'");
        assert_eq!(
            subtitle_revisions(&db, id),
            vec![1],
            "the inventory is untouched"
        );
    }

    /// An empty inventory deletes the prior rows: a publication is a complete
    /// replacement, never an append.
    #[test]
    fn empty_inventory_replacement_deletes_old_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));
        assert!(matches!(
            publish_complete_success(&db, id),
            ProbePublication::Published { .. }
        ));
        assert_eq!(audio_inventory(&db, id).len(), 1);
        assert_eq!(subtitle_revisions(&db, id).len(), 1);

        let mut empty = complete_snapshot(id);
        empty.audio_tracks.clear();
        empty.subtitle_tracks.clear();
        empty.subtitle_status = "none".into();
        let published = db
            .publish_probe(
                &expectation_of(&db, id),
                &ProbeOutcome::Success(Box::new(empty)),
            )
            .unwrap();
        assert_eq!(
            published,
            ProbePublication::Published {
                media_revision: 1,
                probe_revision: 2,
            }
        );

        assert!(audio_inventory(&db, id).is_empty());
        assert!(subtitle_revisions(&db, id).is_empty());
        assert_eq!(db.get_item(id).unwrap().unwrap().subtitle_status, "none");
    }

    /// A failure keeps the prior facts and inventories for diagnostics, clears
    /// both validity stamps, and leaves `probe_revision` alone.
    #[test]
    fn failure_retains_diagnostics_and_clears_validity() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));
        assert!(matches!(
            publish_complete_success(&db, id),
            ProbePublication::Published { .. }
        ));

        let recorded = db
            .publish_probe(
                &expectation_of(&db, id),
                &ProbeOutcome::Failure {
                    probe_status: "unavailable".into(),
                    scan_error: "unavailable: mount gone".into(),
                },
            )
            .unwrap();
        assert_eq!(recorded, ProbePublication::FailureRecorded);

        let row = db.get_item(id).unwrap().unwrap();
        assert_eq!(row.probe_status, "unavailable");
        assert_eq!(row.scan_error.as_deref(), Some("unavailable: mount gone"));
        assert_eq!(row.probed_media_revision, None, "validity stamp cleared");
        assert_eq!(row.probed_content_id, None, "validity stamp cleared");
        assert_eq!(row.probe_revision, 1, "a failure is not a publication");
        assert_eq!(row.duration_ms, Some(1000), "facts retained");
        assert_eq!(row.video_stream_index, Some(0));
        assert_eq!(audio_inventory(&db, id).len(), 1, "inventory retained");
        assert_eq!(subtitle_revisions(&db, id), vec![1]);
    }

    /// A failure outcome may carry only `error` or `unavailable`. Any other
    /// status — including the valid-but-non-failure `indexed` and `probed`, and
    /// the unknown `other` — is rejected before any write.
    #[test]
    fn failure_rejects_non_failure_statuses() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));

        for status in ["indexed", "probed", "other"] {
            let result = db.publish_probe(
                &expectation_of(&db, id),
                &ProbeOutcome::Failure {
                    probe_status: status.into(),
                    scan_error: format!("failure with status {status}"),
                },
            );
            assert!(result.is_err(), "status '{status}' must be rejected");

            let row = db.get_item(id).unwrap().unwrap();
            assert_eq!(row.probe_status, "indexed", "status '{status}'");
            assert_eq!(row.scan_error, None, "status '{status}'");
            assert_eq!(probe_stamps(&db, id).0, 0, "status '{status}'");
            assert!(audio_inventory(&db, id).is_empty(), "status '{status}'");
            assert!(subtitle_revisions(&db, id).is_empty(), "status '{status}'");
        }
    }

    /// At the maximum probe revision the increment cannot wrap: the
    /// publication aborts and writes nothing.
    #[test]
    fn probe_revision_overflow_aborts_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));
        db.with_conn(|c| {
            c.execute(
                "UPDATE media_items SET probe_revision = ?2 WHERE id = ?1",
                params![id, i64::MAX],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        })
        .unwrap();

        let err = db
            .publish_probe(
                &expectation_of(&db, id),
                &ProbeOutcome::Success(Box::new(complete_snapshot(id))),
            )
            .unwrap_err();
        assert!(err.contains("overflow"), "{err}");

        let row = db.get_item(id).unwrap().unwrap();
        assert_eq!(row.probe_revision, i64::MAX);
        assert_eq!(row.probe_status, "indexed");
        assert_eq!(row.duration_ms, None, "no scalar was written");
        assert_eq!(row.probed_content_id, None);
        assert!(audio_inventory(&db, id).is_empty());
        assert!(subtitle_revisions(&db, id).is_empty());
    }

    /// A child-row constraint fault rolls the whole publication back, scalars
    /// included.
    #[test]
    fn child_constraint_fault_rolls_back_the_whole_publication() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));

        // PRIMARY KEY (media_item_id, stream_index): the second row collides.
        let mut snapshot = complete_snapshot(id);
        snapshot.audio_tracks = vec![
            AudioTrackRow {
                stream_index: 1,
                codec: "aac".into(),
                language: None,
                channels: Some(2),
                channel_layout: None,
                title: None,
                is_default: true,
            },
            AudioTrackRow {
                stream_index: 1,
                codec: "ac3".into(),
                language: None,
                channels: Some(6),
                channel_layout: None,
                title: None,
                is_default: false,
            },
        ];

        let err = db
            .publish_probe(
                &expectation_of(&db, id),
                &ProbeOutcome::Success(Box::new(snapshot)),
            )
            .unwrap_err();
        assert!(err.contains("insert audio track"), "{err}");

        let row = db.get_item(id).unwrap().unwrap();
        assert_eq!(row.probe_status, "indexed");
        assert_eq!(row.probe_revision, 0);
        assert_eq!(row.duration_ms, None, "the scalar write rolled back too");
        assert_eq!(row.video_stream_index, None);
        assert_eq!(row.probed_content_id, None);
        assert!(audio_inventory(&db, id).is_empty());
        assert!(subtitle_revisions(&db, id).is_empty());
    }

    /// NULL or empty identity cannot certify a success; the result is stale and
    /// nothing is written.
    #[test]
    fn success_without_content_identity_does_not_publish() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, None);

        let result = db
            .publish_probe(
                &expectation_of(&db, id),
                &ProbeOutcome::Success(Box::new(complete_snapshot(id))),
            )
            .unwrap();
        assert_eq!(result, ProbePublication::Stale, "NULL identity");
        let row = db.get_item(id).unwrap().unwrap();
        assert_eq!(row.probe_status, "indexed");
        assert_eq!(row.probe_revision, 0);
        assert_eq!(row.duration_ms, None);

        db.with_conn(|c| {
            c.execute("UPDATE media_items SET content_id = '' WHERE id = ?1", [id])
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
        .unwrap();
        let result = db
            .publish_probe(
                &expectation_of(&db, id),
                &ProbeOutcome::Success(Box::new(complete_snapshot(id))),
            )
            .unwrap();
        assert_eq!(result, ProbePublication::Stale, "empty identity");
        assert_eq!(db.get_item(id).unwrap().unwrap().probe_status, "indexed");
    }

    /// A failure does not need an identity to be worth recording.
    #[test]
    fn failure_without_content_identity_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, None);

        let recorded = db
            .publish_probe(
                &expectation_of(&db, id),
                &ProbeOutcome::Failure {
                    probe_status: "error".into(),
                    scan_error: "no identity".into(),
                },
            )
            .unwrap();
        assert_eq!(recorded, ProbePublication::FailureRecorded);
        let row = db.get_item(id).unwrap().unwrap();
        assert_eq!(row.probe_status, "error");
        assert_eq!(row.scan_error.as_deref(), Some("no identity"));
        assert_eq!(row.probe_revision, 0);
    }

    /// A database write fault stays an error and leaves the row untouched.
    #[test]
    fn failure_write_fault_returns_err_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));
        let expectation = expectation_of(&db, id);

        // Reads still work; every write on this connection now fails.
        db.with_conn(|c| {
            c.execute_batch("PRAGMA query_only = ON")
                .map_err(|e| e.to_string())
        })
        .unwrap();

        let err = db
            .publish_probe(
                &expectation,
                &ProbeOutcome::Failure {
                    probe_status: "error".into(),
                    scan_error: "fault".into(),
                },
            )
            .unwrap_err();
        assert!(!err.is_empty());

        // The read side still shows the untouched row.
        let row = db.get_item(id).unwrap().unwrap();
        assert_eq!(row.probe_status, "indexed");
        assert_eq!(row.scan_error, None);
        assert_eq!(row.probe_revision, 0);
    }

    /// The legacy fact-only writer can no longer certify a snapshot.
    #[test]
    fn legacy_fact_writer_does_not_certify_a_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));

        db.apply_probe_update(&ProbeUpdate {
            item_id: id,
            duration_ms: Some(1000),
            container: Some("matroska".into()),
            video_codec: Some("h264".into()),
            audio_codec: Some("aac".into()),
            audio_channels: Some(2),
            width: Some(1920),
            height: Some(1080),
            video_bitrate_bps: Some(5_000_000),
            video_frame_rate_num: Some(24),
            video_frame_rate_den: Some(1),
            hdr: Some("none".into()),
            probe_status: "probed".into(),
            scan_error: None,
        })
        .unwrap();

        let row = db.get_item(id).unwrap().unwrap();
        assert_eq!(row.probe_status, "probed");
        assert_eq!(row.duration_ms, Some(1000));
        assert_eq!(row.probed_content_id, None, "no certification");
        assert_eq!(row.probed_media_revision, None);
        assert_eq!(row.probe_revision, 0);
    }
}
