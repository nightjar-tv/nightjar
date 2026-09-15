use crate::migrate;
use crate::paths::{
    fold_path, is_absolute_stored, require_library_root, require_relpath, resolve_media_path,
    to_relpath,
};
use crate::status::{backoff_days, parse_map_status, parse_probe_status, parse_subtitle_status};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::collections::{HashMap, HashSet};
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// ADR-0010 §4 / ADR-0023 §6 bounded identity of the sidecar bytes. `None`
    /// is a legacy row an explicit reconciliation has not verified yet.
    pub content_id: Option<String>,
    /// DB-allocated generation, positive once assigned. `None` is unverified.
    pub sidecar_generation: Option<i64>,
}

/// One sidecar a successful directory discovery observed (ADR-0010 §4).
///
/// The scanner computes `content_id` from the sidecar's own bounded windows;
/// the DB never reads the filesystem. An identity read failure is an error at
/// the call site rather than a `None` here, so a row cannot be reconciled
/// without an observed identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedSidecar {
    pub track_id: String,
    pub path: String,
    pub mtime_ms: i64,
    pub size_bytes: i64,
    pub format: String,
    pub language: Option<String>,
    pub forced: bool,
    pub sdh: bool,
    pub content_id: String,
}

/// One row a reconciliation rewrote: the stored row before, the stored row
/// after (ADR-0010 §4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarChange {
    pub before: SidecarRow,
    pub after: SidecarRow,
}

/// The exact set delta one committed reconciliation produced (ADR-0010 §4).
///
/// Later artifact handling consumes this rather than inferring the delta from
/// a second read. An empty delta is an unchanged set, not a missing one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SidecarDelta {
    pub added: Vec<SidecarRow>,
    pub changed: Vec<SidecarChange>,
    pub removed: Vec<SidecarRow>,
}

impl SidecarDelta {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }
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
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// The certified result of one coherent probe read (ADR-0058 Reads).
///
/// It carries the stored [`ProbeSnapshot`] together with the revisions and
/// identity that certify it, so a consumer never has to re-check the stamps
/// itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertifiedProbeSnapshot {
    pub item_id: i64,
    pub library_id: i64,
    /// Stored `media_items.path`, so a consumer never re-reads the row.
    pub path: String,
    /// The media revision the snapshot was certified against; equal to the
    /// row's `probed_media_revision`.
    pub media_revision: i64,
    /// The positive publication counter every scalar and child row bears.
    pub probe_revision: i64,
    /// The nonempty identity the snapshot was certified against.
    pub content_id: String,
    /// The source mtime the snapshot was captured against. D2B.2 rechecks it
    /// before extraction and before final publication (ADR-0013 §13.3).
    pub mtime_ms: i64,
    /// The source size the snapshot was captured against.
    pub size_bytes: i64,
    pub snapshot: ProbeSnapshot,
}

/// One coherent probe read for a media item (ADR-0058 Reads).
///
/// [`Db::coherent_probe_read`] returns `Ok(None)` when no such item exists, so
/// an unknown item is never conflated with an existing item that is not
/// certified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoherentProbeRead {
    /// The stored facts are certified against the current media revision and
    /// identity, and every child row carries the same positive probe revision.
    Ready(Box<CertifiedProbeSnapshot>),
    /// The item exists but its stored facts are not certified.
    Unverified,
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

/// How one finished physical probe counts toward the scan job that demanded it
/// (ADR-0058 terminal classes).
///
/// One physical probe can serve many logical demands. It is classified once and
/// this same class is applied to every waiter, so joined demands can never
/// disagree about what the one child did. `ErrorOnly` exists because a
/// publisher database error is an error with no probe behind it: it must not
/// inflate `probed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeAccounting {
    /// A certified publication: one probe, no error.
    Published,
    /// A recorded failure carrying a terminal media error: one probe, one error.
    Error,
    /// A recorded failure without a terminal error (`unavailable`): one probe.
    Unavailable,
    /// No result could be written: a publisher database error, or a failed
    /// snapshot build after a successful ffprobe. One error, no probe.
    ErrorOnly,
    /// Stale, superseded, or cancelled work: no count.
    None,
}

/// One serveable sidecar member with its durable generation (ADR-0010 §4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertifiedSidecar {
    pub track_id: String,
    pub path: String,
    pub format: String,
    pub mtime_ms: i64,
    pub size_bytes: i64,
    pub generation: i64,
}

/// State of one committed per-track subtitle publication (ADR-0013 §13.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleArtifactState {
    /// Growing bytes are committed and serveable, but the run is not finished.
    Partial,
    /// The complete immutable artifact is committed and serveable.
    Complete,
}

impl SubtitleArtifactState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Partial => "partial",
            Self::Complete => "complete",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "partial" => Ok(Self::Partial),
            "complete" => Ok(Self::Complete),
            other => Err(format!("unknown subtitle publication state {other}")),
        }
    }
}

/// One committed per-track subtitle publication reference (ADR-0013 §13.4).
///
/// It is keyed by `(item_id, track_id, token)` and records the captured
/// certification, revisions and sidecar generation for that one track, plus
/// the certification `content_id` and the partial/complete state. A row is
/// written only by the source compare-and-swap, and serving requires a row
/// whose recorded identity still matches the current certified source. It is
/// never inferred from a file on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleArtifact {
    pub track_id: String,
    pub token: String,
    pub state: SubtitleArtifactState,
    /// ADR-0013 §13.2 immutable artifact revision. It names the artifact file
    /// inside the generation directory; serving resolves the filename from this
    /// value and never constructs it.
    pub artifact_revision: u64,
    /// ADR-0013 §11 server-declared per-track revision. Every committed
    /// publication for this `(item, track, token)` bumps it by one.
    pub revision: u64,
    pub media_revision: i64,
    pub probe_revision: i64,
    /// The track's own ADR-0010 §4 generation; `None` for an embedded track.
    pub sidecar_generation: Option<i64>,
    /// The captured certification stamp (ADR-0058 `content_id`). Serving
    /// compares it against the current certified source, so a certification
    /// change that moved no revision still invalidates the reference.
    pub subtitle_content_id: String,
}

/// One per-track publication the source compare-and-swap commits
/// (ADR-0013 §13.3.4, §13.5).
///
/// The caller reserves `artifact_revision` from
/// [`Db::reserve_subtitle_artifact_revision`], finalizes that exact revision's
/// bytes on disk, and only then asks the database to commit this reference. A
/// mismatch preserves the previous reference and bytes: the finalized candidate
/// stays unreferenced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleArtifactPublication {
    pub track_id: String,
    /// The bounded per-track token of the captured source (ADR-0013 §13.1).
    pub token: String,
    /// The reserved `artifact_revision` of the bytes just finalized.
    pub artifact_revision: u64,
    pub state: SubtitleArtifactState,
}

/// One coherent read of an item's subtitle source for playback-info listing and
/// demand (ADR-0013 §13.4).
///
/// Certification, the durable sidecar membership, the committed per-track
/// publications and the coarse lifecycle fields all come from one transaction,
/// so a listing never combines an observation of one generation with the
/// publication state of another. Unlike [`CertifiedSubtitleSource`] it does not
/// require certification: an item whose probe snapshot is not certified still
/// lists its durable sidecar rows (without delivery).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleListingSource {
    pub item_id: i64,
    /// The certified probe snapshot, when the item has one. `None` means the
    /// embedded inventory is unknown for this item, so it is not listed.
    pub snapshot: Option<CertifiedProbeSnapshot>,
    /// Every durable sidecar row, ordered by `track_id`.
    pub sidecars: Vec<SidecarRow>,
    /// Committed per-track publication references, ordered by
    /// `(track_id, token)`.
    pub artifacts: Vec<SubtitleArtifact>,
    /// Coarse item-level lifecycle fields (ADR-0013 §6). Not the serving gate.
    pub subtitle_status: String,
    pub subtitle_next_retry_at: Option<String>,
}

impl SubtitleListingSource {
    /// The certified view of this read, or `None` when the item's probe
    /// snapshot is not certified or a serveable sidecar lacks a durable
    /// generation (ADR-0010 §4). Serving and publication use this view, so the
    /// certification rule has one implementation.
    pub fn certified(&self) -> Option<CertifiedSubtitleSource> {
        let snapshot = self.snapshot.clone()?;
        let mut sidecars = Vec::new();
        for row in &self.sidecars {
            if !sidecar_format_is_serveable(&row.format) {
                continue;
            }
            let generation = row.sidecar_generation?;
            sidecars.push(CertifiedSidecar {
                track_id: row.track_id.clone(),
                path: row.path.clone(),
                format: row.format.clone(),
                mtime_ms: row.mtime_ms,
                size_bytes: row.size_bytes,
                generation,
            });
        }
        Some(CertifiedSubtitleSource {
            snapshot,
            sidecars,
            artifacts: self.artifacts.clone(),
        })
    }

    /// Whether a standalone extract is still needed for this item's content
    /// (ADR-0013 §13.4): some serveable member of the current certified source
    /// has no complete committed publication.
    ///
    /// This is a content signal only. It does not depend on the coarse
    /// item-level `subtitle_status`, so a formerly `ready` item whose sidecar
    /// was edited, or a formerly `none` item that just gained its first
    /// sidecar, still reports demand. The caller applies the eligibility and
    /// backoff rules.
    pub fn needs_publication(&self) -> bool {
        self.certified()
            .is_some_and(|source| source.needs_publication())
    }
}

/// The certified source of one standalone subtitle artifact (D2B.2).
///
/// It composes the ADR-0058 certified probe snapshot with the ADR-0010 §4
/// serveable sidecar membership and generations and the committed per-track
/// publication references. Every part is read in one transaction, so the source
/// describes exactly one consistent observation: serving can never combine an
/// item-level observation with a newer certified source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertifiedSubtitleSource {
    pub snapshot: CertifiedProbeSnapshot,
    /// Serveable sidecar members, ordered by `track_id`. Empty when the item
    /// has no serveable sidecar.
    pub sidecars: Vec<CertifiedSidecar>,
    /// Committed per-track publication references, ordered by
    /// `(track_id, token)`.
    pub artifacts: Vec<SubtitleArtifact>,
}

impl CertifiedSubtitleSource {
    pub fn item_id(&self) -> i64 {
        self.snapshot.item_id
    }

    /// The committed publication reference that makes `track_id` serveable, or
    /// `None` when there is none (ADR-0013 §13.4).
    ///
    /// A reference counts only when it is keyed by the token this source
    /// currently mints for the track *and* its recorded certification still
    /// equals the source's. The token carries the media/probe revisions and the
    /// track's own sidecar generation, so a reference from another generation
    /// can never match; the explicit `content_id` comparison catches a
    /// certification change that moved no revision.
    pub fn artifact_for(&self, track_id: &str) -> Option<&SubtitleArtifact> {
        let token = self.token_for_track(track_id)?;
        self.artifacts.iter().find(|a| {
            a.track_id == track_id
                && a.token == token
                && a.subtitle_content_id == self.snapshot.content_id
        })
    }

    /// Whether `track_id`'s complete immutable artifact is already committed.
    /// A run skips such a track: complete bytes are never overwritten
    /// (ADR-0013 §13.2, §13.4).
    pub fn is_complete(&self, track_id: &str) -> bool {
        self.artifact_for(track_id)
            .is_some_and(|a| a.state == SubtitleArtifactState::Complete)
    }

    /// Whether some serveable member still lacks a complete committed
    /// publication (ADR-0013 §13.4). This is the demand signal for a standalone
    /// extract.
    pub fn needs_publication(&self) -> bool {
        self.members()
            .iter()
            .any(|track_id| !self.is_complete(track_id))
    }

    /// Whether every serveable member of this source has a complete committed
    /// publication, counting `pending` as the row a publication is about to
    /// commit. Only then does the coarse item-level lifecycle become `ready`
    /// (ADR-0013 §6).
    fn all_members_complete(&self, pending: &SubtitleArtifactPublication) -> bool {
        self.members().iter().all(|track_id| {
            (pending.track_id == *track_id && pending.state == SubtitleArtifactState::Complete)
                || self.is_complete(track_id)
        })
    }

    /// Every serveable track of this source, ordered by track id. Embedded
    /// text streams and serveable sidecars are both members.
    pub fn members(&self) -> Vec<String> {
        let mut members: Vec<String> = Vec::new();
        for track in &self.snapshot.snapshot.subtitle_tracks {
            if track.kind != "text" {
                continue;
            }
            if let Ok(index) = u32::try_from(track.stream_index) {
                members.push(format!("e{index}"));
            }
        }
        for sidecar in &self.sidecars {
            members.push(sidecar.track_id.clone());
        }
        members.sort();
        members.dedup();
        members
    }

    /// The opaque, bounded, versioned generation token for one track
    /// (ADR-0013 §13.1):
    ///
    /// ```text
    /// token := "v1" "-m" media_rev "-p" probe_rev [ "-s" sidecar_gen ]
    /// rev   := "0" | [1-9][0-9]*     (unsigned decimal, no leading zero)
    /// ```
    ///
    /// The media and probe revisions are the certified snapshot's; `-s` is the
    /// track's own ADR-0010 §4 durable generation and appears only for a
    /// sidecar member. No other track id enters the token, and the complete
    /// sidecar set is never concatenated. Returns `None` for a track that is
    /// not a serveable member of this source.
    ///
    /// The token is at most 65 ASCII bytes: `v1-m` + 19 + `-p` + 19 + `-s` + 19.
    pub fn token_for_track(&self, track_id: &str) -> Option<String> {
        let mut token = format!(
            "v1-m{}-p{}",
            self.snapshot.media_revision, self.snapshot.probe_revision
        );
        if let Some(sidecar) = self.sidecars.iter().find(|s| s.track_id == track_id) {
            token.push_str(&format!("-s{}", sidecar.generation));
            return Some(token);
        }
        // An embedded member is a text track in the certified inventory.
        if embedded_stream_index(track_id).is_some_and(|index| {
            self.snapshot
                .snapshot
                .subtitle_tracks
                .iter()
                .any(|t| t.stream_index == index && t.kind == "text")
        }) {
            return Some(token);
        }
        None
    }

    /// Whether `track_id` is a serveable member of this source: an embedded
    /// text stream or a serveable sidecar row.
    pub fn is_member(&self, track_id: &str) -> bool {
        self.token_for_track(track_id).is_some()
    }

    /// Internal single-flight identity of the captured source (ADR-0013 §13.6).
    ///
    /// It is a deterministic function of the revisions and the per-sidecar
    /// generations, so a newer generation is distinct work. It is never a URL
    /// or a path component: the on-wire/on-disk identity is the per-track
    /// [`Self::token_for_track`].
    pub fn source_identity(&self) -> String {
        let mut identity = format!(
            "m{}.p{}",
            self.snapshot.media_revision, self.snapshot.probe_revision
        );
        for sidecar in &self.sidecars {
            identity.push('.');
            identity.push_str(&sidecar.track_id);
            identity.push('=');
            identity.push_str(&sidecar.generation.to_string());
        }
        identity
    }

    /// Whether `other` describes the same certified source: the same
    /// certification, revisions, membership, sidecar generations, and sidecar
    /// identity tuples. This is the publication compare-and-swap comparison.
    pub fn matches(&self, other: &Self) -> bool {
        self.snapshot.media_revision == other.snapshot.media_revision
            && self.snapshot.probe_revision == other.snapshot.probe_revision
            && self.snapshot.content_id == other.snapshot.content_id
            && self.snapshot.item_id == other.snapshot.item_id
            && self.sidecars == other.sidecars
    }
}

/// The absolute stream index encoded in an embedded `e{index}` track id.
fn embedded_stream_index(track_id: &str) -> Option<i64> {
    let digits = track_id.strip_prefix('e')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Whether a sidecar format is one the pipeline converts to WebVTT.
///
/// `nightjar-transcode::is_serveable_sidecar_format` is the canonical rule; the
/// db crate stays free of that dependency, so this is the same two-extension
/// set the certified-source read already applies. It decides only whether a
/// reconciliation delta invalidates the committed artifact reference.
fn sidecar_format_is_serveable(format: &str) -> bool {
    matches!(format.to_ascii_lowercase().as_str(), "srt" | "vtt")
}

/// Whether `token` matches the ADR-0013 §13.1 grammar exactly and is within the
/// 65-byte bound. The server mints every token, so this is the gate that stops
/// a client-supplied string from being used as a path segment or compared
/// against a minted one.
pub fn is_valid_generation_token(token: &str) -> bool {
    if token.len() > 65 || !token.is_ascii() {
        return false;
    }
    let Some(rest) = token.strip_prefix("v1-m") else {
        return false;
    };
    let Some((media, rest)) = rest.split_once("-p") else {
        return false;
    };
    if !is_revision(media) {
        return false;
    }
    let Some((probe, sidecar)) = rest.split_once("-s") else {
        return is_revision(rest);
    };
    is_revision(probe) && is_revision(sidecar)
}

/// `0` or `[1-9][0-9]*`: unsigned decimal with no leading zero.
fn is_revision(value: &str) -> bool {
    if value == "0" {
        return true;
    }
    let mut bytes = value.bytes();
    match bytes.next() {
        Some(b'1'..=b'9') => {}
        _ => return false,
    }
    bytes.all(|b| b.is_ascii_digit())
}

/// What a subtitle artifact publication did (D2B.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitlePublication {
    Published,
    Stale,
}

// Test-only rendezvous inside [`Db::coherent_probe_read`].
//
// The hook runs after the item SELECT has taken the transaction's read
// snapshot and before the inventory SELECTs. A test uses it to commit a new
// generation through a second connection in that window, which is the only
// schedule that distinguishes one read transaction from a sequence of
// autocommit reads.
#[cfg(test)]
thread_local! {
    static PROBE_READ_AFTER_ITEM_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
fn set_probe_read_after_item_hook(hook: impl FnOnce() + 'static) {
    PROBE_READ_AFTER_ITEM_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
fn run_probe_read_after_item_hook() {
    PROBE_READ_AFTER_ITEM_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
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

    /// Read one item's certified probe snapshot inside one database read
    /// transaction (ADR-0058 Reads).
    ///
    /// Returns `Ok(None)` for an unknown item. An existing item is
    /// [`CoherentProbeRead::Ready`] only when `probe_status` is `probed`,
    /// `probe_revision` is positive, `probed_media_revision` equals
    /// `media_revision`, `content_id` is nonempty and equals
    /// `probed_content_id`, and every stored audio and subtitle row bears that
    /// same probe revision. Anything else is [`CoherentProbeRead::Unverified`];
    /// a partially trusted snapshot is never returned.
    ///
    /// The item row and both inventories come from one deferred transaction.
    /// A deferred read takes a single WAL snapshot at its first statement, so a
    /// concurrent publication is either fully visible or not at all: the read
    /// sees the prior coherent generation or the next one, never a mix.
    pub fn coherent_probe_read(&self, item_id: i64) -> Result<Option<CoherentProbeRead>, String> {
        let conn = self.lock()?;
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("begin probe read for item {item_id}: {e}"))?;
        let read = Self::coherent_probe_read_tx(&tx, item_id)?;
        tx.commit()
            .map_err(|e| format!("end probe read for item {item_id}: {e}"))?;
        Ok(read)
    }

    /// The body of [`Self::coherent_probe_read`] inside an already-open
    /// transaction. Shared with the D2B.2 certified subtitle source so the
    /// certification rule has one implementation (Rule 4.11).
    fn coherent_probe_read_tx(
        tx: &Transaction<'_>,
        item_id: i64,
    ) -> Result<Option<CoherentProbeRead>, String> {
        let item = tx
            .query_row(
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
                [item_id],
                map_item,
            )
            .optional()
            .map_err(|e| format!("read item {item_id}: {e}"))?;
        let Some(item) = item else {
            return Ok(None);
        };

        // The item SELECT above fixed this transaction's read snapshot; a test
        // can commit a new generation before the inventory SELECTs run.
        #[cfg(test)]
        run_probe_read_after_item_hook();

        // The child revision is read with the row: the returned inventory rows
        // do not carry it, but certification requires every stored row to match
        // the item's publication.
        let audio = {
            let mut stmt = tx
                .prepare(
                    "SELECT probe_revision, stream_index, codec, language, channels,
                            channel_layout, title, is_default
                     FROM media_item_audio_tracks
                     WHERE media_item_id = ?1
                     ORDER BY stream_index",
                )
                .map_err(|e| format!("prepare audio inventory for item {item_id}: {e}"))?;
            stmt.query_map([item_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    AudioTrackRow {
                        stream_index: r.get(1)?,
                        codec: r.get(2)?,
                        language: r.get(3)?,
                        channels: r.get(4)?,
                        channel_layout: r.get(5)?,
                        title: r.get(6)?,
                        is_default: r.get::<_, i64>(7)? != 0,
                    },
                ))
            })
            .map_err(|e| format!("read audio inventory for item {item_id}: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("read audio inventory for item {item_id}: {e}"))?
        };

        let subtitles = {
            let mut stmt = tx
                .prepare(
                    "SELECT probe_revision, media_item_id, stream_index, codec, language,
                            title, forced, sdh, kind
                     FROM media_item_subtitle_tracks
                     WHERE media_item_id = ?1
                     ORDER BY stream_index",
                )
                .map_err(|e| format!("prepare subtitle inventory for item {item_id}: {e}"))?;
            stmt.query_map([item_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    SubtitleTrackRow {
                        media_item_id: r.get(1)?,
                        stream_index: r.get(2)?,
                        codec: r.get(3)?,
                        language: r.get(4)?,
                        title: r.get(5)?,
                        forced: r.get::<_, i64>(6)? != 0,
                        sdh: r.get::<_, i64>(7)? != 0,
                        kind: r.get(8)?,
                    },
                ))
            })
            .map_err(|e| format!("read subtitle inventory for item {item_id}: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("read subtitle inventory for item {item_id}: {e}"))?
        };

        let Some(content_id) = item.content_id.as_deref().filter(|id| !id.is_empty()) else {
            return Ok(Some(CoherentProbeRead::Unverified));
        };
        let certified = item.probe_status == "probed"
            && item.probe_revision > 0
            && item.probed_media_revision == Some(item.media_revision)
            && item.probed_content_id.as_deref() == Some(content_id)
            && audio
                .iter()
                .all(|(revision, _)| *revision == item.probe_revision)
            && subtitles
                .iter()
                .all(|(revision, _)| *revision == item.probe_revision);
        if !certified {
            return Ok(Some(CoherentProbeRead::Unverified));
        }

        let content_id = content_id.to_string();
        Ok(Some(CoherentProbeRead::Ready(Box::new(
            CertifiedProbeSnapshot {
                item_id: item.id,
                library_id: item.library_id,
                path: item.path,
                media_revision: item.media_revision,
                probe_revision: item.probe_revision,
                content_id,
                mtime_ms: item.mtime_ms,
                size_bytes: item.size_bytes,
                snapshot: ProbeSnapshot {
                    duration_ms: item.duration_ms,
                    container: item.container,
                    video_codec: item.video_codec,
                    video_stream_index: item.video_stream_index,
                    audio_codec: item.audio_codec,
                    audio_channels: item.audio_channels,
                    width: item.width,
                    height: item.height,
                    video_bitrate_bps: item.video_bitrate_bps,
                    video_frame_rate_num: item.video_frame_rate_num,
                    video_frame_rate_den: item.video_frame_rate_den,
                    hdr: item.hdr,
                    audio_tracks: audio.into_iter().map(|(_, track)| track).collect(),
                    subtitle_tracks: subtitles.into_iter().map(|(_, track)| track).collect(),
                    subtitle_status: item.subtitle_status,
                },
            },
        ))))
    }

    /// Read one item's certified subtitle source in one transaction (D2B.2).
    ///
    /// Returns `Ok(None)` for an unknown item, an item whose probe snapshot is
    /// not certified, or an item that carries a serveable sidecar without a
    /// durable generation (a legacy unverified row). A partially trusted source
    /// is never returned.
    pub fn certified_subtitle_source(
        &self,
        item_id: i64,
    ) -> Result<Option<CertifiedSubtitleSource>, String> {
        Ok(self
            .subtitle_listing_source(item_id)?
            .and_then(|listing| listing.certified()))
    }

    /// Read one item's subtitle source for playback-info listing and demand in
    /// one transaction (ADR-0013 §13.4).
    ///
    /// Returns `Ok(None)` for an unknown item. Every part — certification, the
    /// durable sidecar membership, the committed per-track publications and the
    /// coarse lifecycle fields — comes from one read snapshot, so a caller
    /// never combines an observation of one generation with another's
    /// publication state.
    pub fn subtitle_listing_source(
        &self,
        item_id: i64,
    ) -> Result<Option<SubtitleListingSource>, String> {
        let conn = self.lock()?;
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("begin subtitle source read for item {item_id}: {e}"))?;
        let source = Self::read_listing_source_tx(&tx, item_id)?;
        tx.commit()
            .map_err(|e| format!("end subtitle source read for item {item_id}: {e}"))?;
        Ok(source)
    }

    /// The body of [`Self::subtitle_listing_source`] inside an already-open
    /// transaction. [`Self::read_certified_source_tx`] is the certified
    /// projection of it, so the certification rule has one implementation
    /// (Rule 4.11).
    fn read_listing_source_tx(
        tx: &Transaction<'_>,
        item_id: i64,
    ) -> Result<Option<SubtitleListingSource>, String> {
        let Some(read) = Self::coherent_probe_read_tx(tx, item_id)? else {
            return Ok(None);
        };
        let snapshot = match read {
            CoherentProbeRead::Ready(snapshot) => Some(*snapshot),
            CoherentProbeRead::Unverified => None,
        };
        let (subtitle_status, subtitle_next_retry_at) = tx
            .query_row(
                "SELECT subtitle_status, subtitle_next_retry_at
                 FROM media_items WHERE id = ?1",
                [item_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .map_err(|e| format!("read subtitle lifecycle for item {item_id}: {e}"))?;
        Ok(Some(SubtitleListingSource {
            item_id,
            snapshot,
            sidecars: load_sidecars(tx, item_id)?,
            artifacts: load_subtitle_artifacts(tx, item_id)?,
            subtitle_status,
            subtitle_next_retry_at,
        }))
    }

    /// The certified projection of one listing read (ADR-0013 §13.4).
    ///
    /// `None` when the item's probe snapshot is not certified, or when it
    /// carries a serveable sidecar without a durable generation (a legacy
    /// unverified row). A partially trusted source is never returned.
    fn read_certified_source_tx(
        tx: &Transaction<'_>,
        item_id: i64,
    ) -> Result<Option<CertifiedSubtitleSource>, String> {
        Ok(Self::read_listing_source_tx(tx, item_id)?.and_then(|listing| listing.certified()))
    }

    /// Reserve the next immutable artifact revision for one item
    /// (ADR-0013 §13.2).
    ///
    /// The allocation is monotonic per item and reserves candidate identity
    /// only: nothing becomes serveable until the source compare-and-swap
    /// commits a publication at this revision. A candidate that is never
    /// committed leaves a gap in the sequence, which is harmless because
    /// serving resolves the filename from the committed publication row alone.
    pub fn reserve_subtitle_artifact_revision(&self, item_id: i64) -> Result<u64, String> {
        let conn = self.lock()?;
        let revision: i64 = conn
            .query_row(
                "UPDATE media_items
                 SET subtitle_artifact_sequence = subtitle_artifact_sequence + 1
                 WHERE id = ?1
                 RETURNING subtitle_artifact_sequence",
                [item_id],
                |r| r.get(0),
            )
            .map_err(|e| format!("reserve subtitle artifact revision for item {item_id}: {e}"))?;
        u64::try_from(revision)
            .map_err(|_| format!("negative subtitle artifact revision {revision}"))
    }

    /// Commit one changed body's publication at its exact reserved artifact
    /// revision (D2B.2 acceptance 3/5, ADR-0013 §13.3.4, §13.4, §13.5).
    ///
    /// One `BEGIN IMMEDIATE` transaction re-reads the certified source and
    /// requires the captured certification and this track's membership to still
    /// match: the token carries the media and probe revisions and the track's
    /// own sidecar generation, and the recorded `subtitle_content_id` catches a
    /// certification that moved no revision. On a mismatch it returns
    /// [`SubtitlePublication::Stale`] and writes nothing, so bytes finalized by
    /// work the database did not accept stay unreferenced.
    ///
    /// A committed `complete` reference is never replaced or downgraded
    /// (ADR-0013 §13.2, §13.4): a later `partial` or `complete` write for the
    /// same track and token is a no-op. The item-level coarse lifecycle becomes
    /// `ready` only once every serveable member has a complete reference.
    pub fn publish_subtitle_artifact(
        &self,
        captured: &CertifiedSubtitleSource,
        publication: &SubtitleArtifactPublication,
    ) -> Result<SubtitlePublication, String> {
        let item_id = captured.item_id();
        let conn = self.lock()?;
        with_write_tx(&conn, |tx| {
            let Some(current) = Self::read_certified_source_tx(tx, item_id)? else {
                return Ok(SubtitlePublication::Stale);
            };
            if current.snapshot.content_id != captured.snapshot.content_id {
                return Ok(SubtitlePublication::Stale);
            }
            if current.token_for_track(&publication.track_id).as_deref()
                != Some(publication.token.as_str())
            {
                return Ok(SubtitlePublication::Stale);
            }
            // Complete immutable bytes are never replaced or downgraded.
            if current.is_complete(&publication.track_id) {
                return Ok(SubtitlePublication::Published);
            }
            let revision = current
                .artifacts
                .iter()
                .find(|a| a.track_id == publication.track_id && a.token == publication.token)
                .map(|a| a.revision.saturating_add(1))
                .unwrap_or(1);
            let sidecar_generation = current
                .sidecars
                .iter()
                .find(|s| s.track_id == publication.track_id)
                .map(|s| s.generation);
            insert_subtitle_artifact(
                tx,
                item_id,
                &SubtitleArtifact {
                    track_id: publication.track_id.clone(),
                    token: publication.token.clone(),
                    state: publication.state,
                    artifact_revision: publication.artifact_revision,
                    revision,
                    media_revision: current.snapshot.media_revision,
                    probe_revision: current.snapshot.probe_revision,
                    sidecar_generation,
                    subtitle_content_id: current.snapshot.content_id.clone(),
                },
            )?;
            if publication.state == SubtitleArtifactState::Complete
                && current.all_members_complete(publication)
            {
                set_subtitle_status_tx(tx, item_id, "ready")?;
            }
            Ok(SubtitlePublication::Published)
        })
    }

    /// Record a non-serveable outcome (`none`/`partial`/`error`/`unavailable`)
    /// for the captured source (D2B.2 acceptance 3, round-2 item 3).
    ///
    /// The same source compare-and-swap gates it: a worker whose captured source
    /// has been superseded writes nothing, so stale work can never mutate the
    /// subtitle state of a newer source. A `partial` outcome leaves the item
    /// `eligible` for a later pass; the landed tracks keep their own committed
    /// per-track references.
    pub fn record_subtitle_status(
        &self,
        captured: &CertifiedSubtitleSource,
        status: &str,
    ) -> Result<SubtitlePublication, String> {
        let status = parse_subtitle_status(status)?;
        let item_id = captured.item_id();
        let conn = self.lock()?;
        with_write_tx(&conn, |tx| {
            let Some(current) = Self::read_certified_source_tx(tx, item_id)? else {
                return Ok(SubtitlePublication::Stale);
            };
            if !current.matches(captured) {
                return Ok(SubtitlePublication::Stale);
            }
            set_subtitle_status_tx(tx, item_id, status)?;
            Ok(SubtitlePublication::Published)
        })
    }

    /// Whether the captured certified source is still the current one.
    ///
    /// Progressive publication runs this before every growing write (ADR-0013
    /// §13.5): stale work is rejected by the same source comparison the final
    /// publication CAS uses, so it cannot become ready for a newer generation.
    pub fn subtitle_source_is_current(
        &self,
        captured: &CertifiedSubtitleSource,
    ) -> Result<bool, String> {
        Ok(self
            .certified_subtitle_source(captured.item_id())?
            .is_some_and(|current| current.matches(captured)))
    }

    /// Write the item-level coarse subtitle lifecycle field unconditionally
    /// (ADR-0013 §6).
    ///
    /// This is the administrative/fixture writer. A subtitle worker must not
    /// use it: it has no source compare-and-swap, so a stale run could mutate a
    /// newer source's state. Workers use [`Self::record_subtitle_status`], which
    /// is the same SQL behind the captured-source CAS.
    pub fn set_subtitle_status(&self, item_id: i64, status: &str) -> Result<(), String> {
        let status = parse_subtitle_status(status)?;
        let conn = self.lock()?;
        set_subtitle_status_tx(&conn, item_id, status)
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

    /// Reconcile the stored sidecar set for one item against one complete,
    /// successful discovery (ADR-0010 §4).
    ///
    /// One transaction. Added and changed rows receive a generation greater
    /// than every generation previously allocated for the item; unchanged rows
    /// are not rewritten and keep their row and generation; rows absent from
    /// `observed` are removed. Membership after commit is exactly the observed
    /// set.
    ///
    /// `observed` must come from a successful complete discovery with a
    /// computed identity for every entry. A duplicate `track_id`, an invalid
    /// path, or any write failure returns an error and rolls the whole set
    /// back, so no partial delta or partial membership becomes visible.
    ///
    /// The bounded digest is change detection, not proof that two files are
    /// equal: a change outside both 64-KiB windows with the same size can
    /// collide (ADR-0023 §6, ADR-0010 §4). Generations prevent remove/re-add
    /// ABA; they do not widen the fingerprint.
    pub fn reconcile_item_sidecars(
        &self,
        media_item_id: i64,
        observed: &[ObservedSidecar],
    ) -> Result<SidecarDelta, String> {
        // Duplicate track ids would break the (item, track_id) primary key
        // part-way through the set. Discovery already applies its deterministic
        // format winner, so a duplicate here is a caller defect; refuse the
        // whole set before a transaction can write anything.
        let mut seen: HashSet<&str> = HashSet::with_capacity(observed.len());
        for s in observed {
            if !seen.insert(s.track_id.as_str()) {
                return Err(format!(
                    "duplicate sidecar track_id {} for item {media_item_id}",
                    s.track_id
                ));
            }
            require_relpath(&s.path)?;
        }

        // Reads before it writes, so it takes the write lock up front. As a
        // deferred transaction this SELECT took a read snapshot that the
        // metadata drain's next commit invalidated, and the DELETE then failed
        // instantly with SQLITE_BUSY_SNAPSHOT — 285 times on the 2026-08-07
        // cold scan, median 93 µs apart, each one a WARN with no retry and
        // nothing to revisit the item. The external subtitle was simply never
        // associated.
        let conn = self.lock()?;

        // Nothing found beside the file and nothing stored: there is nothing to
        // reconcile, so do not open a write transaction at all.
        //
        // The index pass calls this for *every* item it upserts, and on a
        // typical library most items have no sidecar. Without this the pass
        // pays one `BEGIN IMMEDIATE` and one no-op write per item — ~25,000 of
        // them on a cold scan of the dogfood library — each holding the write
        // lock across its own SELECT. Taking the lock up front is what makes
        // the read-then-write path correct, so this is the other half of that
        // change: keep the lock for the items that need it and stop taking it
        // for the ones that do not.
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
        if observed.is_empty() && !has_stored_sidecars(&conn, media_item_id)? {
            return Ok(SidecarDelta::default());
        }

        with_write_tx(&conn, |tx| {
            let existing = load_sidecars(tx, media_item_id)?;
            let by_track: HashMap<&str, &SidecarRow> = existing
                .iter()
                .map(|row| (row.track_id.as_str(), row))
                .collect();

            // The durable allocator: one row per item, never removed by a
            // membership change, so a removed path cannot reuse its generation
            // when it returns, including across restart. Starts at 0; the
            // first allocated generation is 1.
            let mut last_generation: i64 = tx
                .query_row(
                    "SELECT last_generation FROM media_item_sidecar_generations
                     WHERE media_item_id = ?1",
                    [media_item_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| format!("read sidecar generation for item {media_item_id}: {e}"))?
                .unwrap_or(0);

            let mut delta = SidecarDelta::default();
            {
                let mut update = tx
                    .prepare(
                        "UPDATE media_item_sidecars
                            SET path = ?3, mtime_ms = ?4, size_bytes = ?5, format = ?6,
                                language = ?7, forced = ?8, sdh = ?9,
                                content_id = ?10, sidecar_generation = ?11
                          WHERE media_item_id = ?1 AND track_id = ?2",
                    )
                    .map_err(|e| format!("prepare sidecar update: {e}"))?;
                let mut insert = tx
                    .prepare(
                        "INSERT INTO media_item_sidecars (
                            media_item_id, track_id, path, mtime_ms, size_bytes,
                            format, language, forced, sdh, content_id, sidecar_generation
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    )
                    .map_err(|e| format!("prepare sidecar insert: {e}"))?;

                for s in observed {
                    let stored = by_track.get(s.track_id.as_str()).copied();
                    if stored.is_some_and(|row| sidecar_is_unchanged(row, s)) {
                        continue;
                    }
                    last_generation += 1;
                    let generation = last_generation;
                    match stored {
                        Some(before) => {
                            update
                                .execute(params![
                                    media_item_id,
                                    s.track_id,
                                    s.path,
                                    s.mtime_ms,
                                    s.size_bytes,
                                    s.format,
                                    s.language,
                                    s.forced as i64,
                                    s.sdh as i64,
                                    s.content_id,
                                    generation,
                                ])
                                .map_err(|e| format!("update sidecar {}: {e}", s.track_id))?;
                            delta.changed.push(SidecarChange {
                                before: before.clone(),
                                after: stored_sidecar(media_item_id, s, generation),
                            });
                        }
                        None => {
                            insert
                                .execute(params![
                                    media_item_id,
                                    s.track_id,
                                    s.path,
                                    s.mtime_ms,
                                    s.size_bytes,
                                    s.format,
                                    s.language,
                                    s.forced as i64,
                                    s.sdh as i64,
                                    s.content_id,
                                    generation,
                                ])
                                .map_err(|e| format!("insert sidecar {}: {e}", s.track_id))?;
                            delta
                                .added
                                .push(stored_sidecar(media_item_id, s, generation));
                        }
                    }
                }
            }

            // Membership after commit is exactly the discovered set.
            for row in &existing {
                if seen.contains(row.track_id.as_str()) {
                    continue;
                }
                tx.execute(
                    "DELETE FROM media_item_sidecars WHERE media_item_id = ?1 AND track_id = ?2",
                    params![media_item_id, row.track_id],
                )
                .map_err(|e| format!("remove sidecar {}: {e}", row.track_id))?;
                delta.removed.push(row.clone());
            }

            if !delta.added.is_empty() || !delta.changed.is_empty() {
                tx.execute(
                    "INSERT INTO media_item_sidecar_generations (media_item_id, last_generation)
                     VALUES (?1, ?2)
                     ON CONFLICT(media_item_id)
                     DO UPDATE SET last_generation = excluded.last_generation",
                    params![media_item_id, last_generation],
                )
                .map_err(|e| format!("record sidecar generation for item {media_item_id}: {e}"))?;
            }

            // ADR-0013 §13.4: reconciliation invalidates only the affected
            // per-track publication references. A changed, added or removed
            // serveable sidecar loses its reference; every unchanged track keeps
            // its reference and its committed artifact serveable, and the coarse
            // item-level lifecycle field is not cleared item-wide.
            let mut invalidated: Vec<&str> = Vec::new();
            for s in delta
                .added
                .iter()
                .chain(delta.changed.iter().map(|c| &c.after))
                .chain(delta.removed.iter())
            {
                if sidecar_format_is_serveable(&s.format) {
                    invalidated.push(s.track_id.as_str());
                }
            }
            for change in &delta.changed {
                if sidecar_format_is_serveable(&change.before.format) {
                    invalidated.push(change.before.track_id.as_str());
                }
            }
            invalidated.sort_unstable();
            invalidated.dedup();
            for track_id in invalidated {
                tx.execute(
                    "DELETE FROM subtitle_publications
                      WHERE media_item_id = ?1 AND track_id = ?2",
                    params![media_item_id, track_id],
                )
                .map_err(|e| {
                    format!(
                        "invalidate subtitle publication {track_id} for item {media_item_id}: {e}"
                    )
                })?;
            }

            Ok(delta)
        })
    }

    pub fn list_item_sidecars(&self, media_item_id: i64) -> Result<Vec<SidecarRow>, String> {
        let conn = self.lock()?;
        load_sidecars(&conn, media_item_id)
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
                    format, language, forced, sdh, content_id, sidecar_generation
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

    /// Apply one physical probe's terminal class to the scan job that demanded
    /// it (ADR-0058).
    ///
    /// `Published` and `Unavailable` count one probe; `Error` counts one probe
    /// and one error; `ErrorOnly` counts one error with no probe; `None`
    /// writes nothing. Joined demands call this once each, so a single child
    /// can account for several logical demands without ever counting a probe
    /// that did not run.
    pub fn record_scan_job_probe(
        &self,
        job_id: i64,
        accounting: ProbeAccounting,
    ) -> Result<(), String> {
        let conn = self.lock()?;
        match accounting {
            ProbeAccounting::Published | ProbeAccounting::Unavailable => conn.execute(
                "UPDATE scan_jobs SET probed = probed + 1 WHERE id = ?1",
                [job_id],
            ),
            ProbeAccounting::Error => conn.execute(
                "UPDATE scan_jobs SET probed = probed + 1, errors = errors + 1 WHERE id = ?1",
                [job_id],
            ),
            ProbeAccounting::ErrorOnly => conn.execute(
                "UPDATE scan_jobs SET errors = errors + 1 WHERE id = ?1",
                [job_id],
            ),
            ProbeAccounting::None => return Ok(()),
        }
        .map_err(|e| format!("record scan job probe: {e}"))?;
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
        content_id: r.get(9)?,
        sidecar_generation: r.get(10)?,
    })
}

/// All stored sidecars for one item, ordered by `track_id`.
fn load_sidecars(conn: &Connection, media_item_id: i64) -> Result<Vec<SidecarRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT media_item_id, track_id, path, mtime_ms, size_bytes,
                    format, language, forced, sdh, content_id, sidecar_generation
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

/// Apply one already-validated subtitle lifecycle status inside an open
/// transaction.
///
/// `unavailable` keeps ADR-0041 Decision 8.3's backoff: every availability
/// failure increments the attempt count and pushes the re-queue deadline out on
/// the ADR-0026 §3 schedule (1d/7d/30d/90d cap), so a flapping mount cannot
/// re-drain an unfinishable title on every reachability transition.
/// `requeue_unavailable_for_library` gates on `subtitle_next_retry_at`.
fn set_subtitle_status_tx(conn: &Connection, item_id: i64, status: &str) -> Result<(), String> {
    if status == "unavailable" {
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

/// Load an item's committed per-track publication references, ordered by
/// `(track_id, token)`.
fn load_subtitle_artifacts(
    conn: &Connection,
    media_item_id: i64,
) -> Result<Vec<SubtitleArtifact>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT track_id, token, state, artifact_revision, revision,
                    media_revision, probe_revision, sidecar_generation,
                    subtitle_content_id
             FROM subtitle_publications
             WHERE media_item_id = ?1
             ORDER BY track_id, token",
        )
        .map_err(|e| format!("prepare subtitle publications for item {media_item_id}: {e}"))?;
    let rows = stmt
        .query_map([media_item_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, Option<i64>>(7)?,
                r.get::<_, String>(8)?,
            ))
        })
        .map_err(|e| format!("read subtitle publications for item {media_item_id}: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read subtitle publications for item {media_item_id}: {e}"))?;
    rows.into_iter()
        .map(
            |(
                track_id,
                token,
                state,
                artifact_revision,
                revision,
                media_revision,
                probe_revision,
                sidecar_generation,
                content_id,
            )| {
                Ok(SubtitleArtifact {
                    track_id,
                    token,
                    state: SubtitleArtifactState::parse(&state)?,
                    artifact_revision: u64::try_from(artifact_revision)
                        .map_err(|_| format!("invalid artifact revision {artifact_revision}"))?,
                    revision: u64::try_from(revision)
                        .map_err(|_| format!("negative subtitle revision {revision}"))?,
                    media_revision,
                    probe_revision,
                    sidecar_generation,
                    subtitle_content_id: content_id,
                })
            },
        )
        .collect()
}

/// Upsert one committed per-track publication reference.
fn insert_subtitle_artifact(
    conn: &Connection,
    media_item_id: i64,
    artifact: &SubtitleArtifact,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO subtitle_publications (
            media_item_id, track_id, token, state, artifact_revision, revision,
            media_revision, probe_revision, sidecar_generation,
            subtitle_content_id, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                   strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
         ON CONFLICT(media_item_id, track_id, token) DO UPDATE SET
            state = excluded.state,
            artifact_revision = excluded.artifact_revision,
            revision = excluded.revision,
            media_revision = excluded.media_revision,
            probe_revision = excluded.probe_revision,
            sidecar_generation = excluded.sidecar_generation,
            subtitle_content_id = excluded.subtitle_content_id,
            updated_at = excluded.updated_at",
        params![
            media_item_id,
            artifact.track_id,
            artifact.token,
            artifact.state.as_str(),
            i64::try_from(artifact.artifact_revision).unwrap_or(i64::MAX),
            i64::try_from(artifact.revision).unwrap_or(i64::MAX),
            artifact.media_revision,
            artifact.probe_revision,
            artifact.sidecar_generation,
            artifact.subtitle_content_id,
        ],
    )
    .map_err(|e| {
        format!(
            "write subtitle publication {} for item {media_item_id}: {e}",
            artifact.track_id
        )
    })?;
    Ok(())
}

/// A stored row as the reconciliation will leave it.
fn stored_sidecar(media_item_id: i64, s: &ObservedSidecar, generation: i64) -> SidecarRow {
    SidecarRow {
        media_item_id,
        track_id: s.track_id.clone(),
        path: s.path.clone(),
        mtime_ms: s.mtime_ms,
        size_bytes: s.size_bytes,
        format: s.format.clone(),
        language: s.language.clone(),
        forced: s.forced,
        sdh: s.sdh,
        content_id: Some(s.content_id.clone()),
        sidecar_generation: Some(generation),
    }
}

/// Whether a stored row already matches the observation exactly.
///
/// A stored row with no `content_id` is an unverified legacy row, so it can
/// never be unchanged: the first reconciliation assigns it identity and a
/// generation.
fn sidecar_is_unchanged(row: &SidecarRow, s: &ObservedSidecar) -> bool {
    row.content_id.as_deref() == Some(s.content_id.as_str())
        && row.path == s.path
        && row.mtime_ms == s.mtime_ms
        && row.size_bytes == s.size_bytes
        && row.format == s.format
        && row.language == s.language
        && row.forced == s.forced
        && row.sdh == s.sdh
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

    /// One observed sidecar with a distinct identity per `track_id`.
    fn observed(track_id: &str) -> ObservedSidecar {
        ObservedSidecar {
            track_id: track_id.into(),
            path: format!("clip.{track_id}.srt"),
            mtime_ms: 10,
            size_bytes: 20,
            format: "srt".into(),
            language: Some("en".into()),
            forced: false,
            sdh: false,
            content_id: format!("20-first-{track_id}-last-{track_id}"),
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

        let delta = db
            .reconcile_item_sidecars(item_id, &[])
            .expect("must not need the write lock when there is nothing to reconcile");
        assert!(delta.is_empty(), "nothing to reconcile is not a change");

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

        let added = db
            .reconcile_item_sidecars(item_id, &[observed("s1")])
            .unwrap();
        assert_eq!(
            added.added.len(),
            1,
            "storing the first sidecar is a change"
        );
        assert_eq!(db.list_item_sidecars(item_id).unwrap().len(), 1);

        // Sidecar file removed from disk: discovery finds nothing.
        let removed = db.reconcile_item_sidecars(item_id, &[]).unwrap();
        assert_eq!(
            removed.removed.len(),
            1,
            "removing the last sidecar is a change"
        );
        assert!(removed.added.is_empty() && removed.changed.is_empty());
        assert!(
            db.list_item_sidecars(item_id).unwrap().is_empty(),
            "stored rows must be deleted when the sidecar is gone"
        );

        // And now that both sides are empty, the fast path applies.
        assert!(db.reconcile_item_sidecars(item_id, &[]).unwrap().is_empty());
    }

    /// An unchanged set reports nothing and rewrites nothing: the row and its
    /// generation survive a second identical reconciliation.
    #[test]
    fn unchanged_reconciliation_is_an_empty_delta_and_keeps_generation() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let item_id = item_for_sidecars(&db);

        let first = db
            .reconcile_item_sidecars(item_id, &[observed("s-en")])
            .unwrap();
        assert_eq!(first.added.len(), 1);
        assert!(first.changed.is_empty() && first.removed.is_empty());
        let stored = db.get_item_sidecar(item_id, "s-en").unwrap().unwrap();
        assert_eq!(stored.sidecar_generation, Some(1));
        assert_eq!(
            stored.content_id.as_deref(),
            Some("20-first-s-en-last-s-en")
        );
        assert_eq!(first.added[0], stored);

        let second = db
            .reconcile_item_sidecars(item_id, &[observed("s-en")])
            .unwrap();
        assert!(second.is_empty(), "unchanged set must not report a delta");
        assert_eq!(
            db.get_item_sidecar(item_id, "s-en").unwrap().unwrap(),
            stored,
            "an unchanged row is not rewritten and keeps its generation"
        );
    }

    /// A row exactly as migration 033 leaves it — stored attributes, no
    /// identity, no generation — is not unchanged. The first reconciliation
    /// assigns both lazily.
    #[test]
    fn a_legacy_unverified_row_is_verified_lazily() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let item_id = item_for_sidecars(&db);

        db.with_conn(|c| {
            c.execute(
                "INSERT INTO media_item_sidecars
                    (media_item_id, track_id, path, mtime_ms, size_bytes, format, language)
                 VALUES (?1, 's-en', 'clip.s-en.srt', 10, 20, 'srt', 'en')",
                [item_id],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        })
        .unwrap();

        let delta = db
            .reconcile_item_sidecars(item_id, &[observed("s-en")])
            .unwrap();
        assert!(delta.added.is_empty() && delta.removed.is_empty());
        assert_eq!(delta.changed.len(), 1, "unverified is not unchanged");
        assert_eq!(delta.changed[0].before.content_id, None);
        assert_eq!(delta.changed[0].before.sidecar_generation, None);
        assert_eq!(delta.changed[0].after.sidecar_generation, Some(1));
        assert_eq!(
            delta.changed[0].after.content_id.as_deref(),
            Some("20-first-s-en-last-s-en")
        );
    }

    /// Same path, mtime and size; only the bounded identity differs. That is a
    /// changed row with a new generation, not an unchanged one.
    #[test]
    fn edited_bytes_receive_a_new_generation() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let item_id = item_for_sidecars(&db);

        db.reconcile_item_sidecars(item_id, &[observed("s-en")])
            .unwrap();
        let before = db.get_item_sidecar(item_id, "s-en").unwrap().unwrap();

        let mut edited = observed("s-en");
        edited.content_id = "20-first-edit-last-edit".into();
        let delta = db.reconcile_item_sidecars(item_id, &[edited]).unwrap();
        assert!(delta.added.is_empty() && delta.removed.is_empty());
        assert_eq!(delta.changed.len(), 1);
        assert_eq!(delta.changed[0].before, before);
        assert_eq!(delta.changed[0].after.sidecar_generation, Some(2));
        assert_eq!(
            delta.changed[0].after.content_id.as_deref(),
            Some("20-first-edit-last-edit")
        );
        assert_eq!(
            db.get_item_sidecar(item_id, "s-en").unwrap().unwrap(),
            delta.changed[0].after
        );
    }

    /// Add and remove report exactly the rows that moved; the unchanged row
    /// stays out of the delta, and membership after commit is the observed set.
    #[test]
    fn add_and_remove_report_the_exact_delta() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let item_id = item_for_sidecars(&db);

        let added = db
            .reconcile_item_sidecars(item_id, &[observed("s-en")])
            .unwrap();
        let ids = |rows: &[SidecarRow]| -> Vec<String> {
            rows.iter().map(|r| r.track_id.clone()).collect()
        };
        assert_eq!(ids(&added.added), vec!["s-en".to_string()]);
        assert!(added.changed.is_empty() && added.removed.is_empty());

        // Adjacent (`s-en`) and nested `Subs/` (`s-Subs.en`) membership at once.
        let both = db
            .reconcile_item_sidecars(item_id, &[observed("s-en"), observed("s-Subs.en")])
            .unwrap();
        assert_eq!(ids(&both.added), vec!["s-Subs.en".to_string()]);
        assert!(
            both.changed.is_empty() && both.removed.is_empty(),
            "the unchanged row stays out of the delta"
        );
        assert_eq!(
            ids(&db.list_item_sidecars(item_id).unwrap()),
            vec!["s-Subs.en".to_string(), "s-en".to_string()]
        );

        let removed = db
            .reconcile_item_sidecars(item_id, &[observed("s-en")])
            .unwrap();
        assert_eq!(ids(&removed.removed), vec!["s-Subs.en".to_string()]);
        assert!(removed.added.is_empty() && removed.changed.is_empty());
        assert_eq!(
            ids(&db.list_item_sidecars(item_id).unwrap()),
            vec!["s-en".to_string()]
        );
    }

    /// A removed path that returns after a restart gets a strictly newer
    /// generation: the allocator is durable and membership deletion leaves it.
    #[test]
    fn remove_then_readd_across_restart_cannot_reuse_a_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");

        let db = Db::open(&path).unwrap();
        let item_id = item_for_sidecars(&db);
        db.reconcile_item_sidecars(item_id, &[observed("s-en")])
            .unwrap();
        let first = db.get_item_sidecar(item_id, "s-en").unwrap().unwrap();
        assert_eq!(first.sidecar_generation, Some(1));

        db.reconcile_item_sidecars(item_id, &[]).unwrap();
        assert!(db.get_item_sidecar(item_id, "s-en").unwrap().is_none());
        drop(db);

        // Restart: same database file, a new connection.
        let db = Db::open(&path).unwrap();
        let readded = db
            .reconcile_item_sidecars(item_id, &[observed("s-en")])
            .unwrap();
        assert_eq!(readded.added.len(), 1);
        assert_eq!(readded.added[0].sidecar_generation, Some(2));
        assert!(
            readded.added[0].sidecar_generation > first.sidecar_generation,
            "re-adding must not reuse the deleted generation"
        );
    }

    /// A duplicate `track_id` is a caller defect and is refused before any
    /// write; the prior set and the allocator are untouched.
    #[test]
    fn duplicate_track_ids_are_rejected_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let item_id = item_for_sidecars(&db);
        db.reconcile_item_sidecars(item_id, &[observed("s-en")])
            .unwrap();
        let stored = db.get_item_sidecar(item_id, "s-en").unwrap().unwrap();

        let mut changed = observed("s-en");
        changed.content_id = "20-first-new-last-new".into();
        let err = db
            .reconcile_item_sidecars(item_id, &[changed.clone(), changed])
            .unwrap_err();
        assert!(err.contains("duplicate sidecar track_id"), "{err}");
        assert_eq!(
            db.get_item_sidecar(item_id, "s-en").unwrap().unwrap(),
            stored,
            "the prior set is preserved"
        );

        // The refused attempt allocated nothing: the next accepted change is 2.
        let mut next = observed("s-en");
        next.content_id = "20-first-two-last-two".into();
        let delta = db.reconcile_item_sidecars(item_id, &[next]).unwrap();
        assert_eq!(delta.changed[0].after.sidecar_generation, Some(2));
    }

    /// An invalid stored path is refused before any write, atomically.
    #[test]
    fn an_invalid_stored_path_is_rejected_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let item_id = item_for_sidecars(&db);

        let mut bad = observed("s-en");
        bad.path = "/absolute.srt".into();
        let err = db.reconcile_item_sidecars(item_id, &[bad]).unwrap_err();
        assert!(err.contains("absolute"), "{err}");
        assert!(db.list_item_sidecars(item_id).unwrap().is_empty());
    }

    /// A write failure inside the set rolls the whole set back: no partial
    /// delta, no partial membership, and no generation consumed.
    #[test]
    fn a_write_failure_rolls_back_the_whole_set() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let item_id = item_for_sidecars(&db);
        db.reconcile_item_sidecars(item_id, &[observed("s-en")])
            .unwrap();
        let stored = db.get_item_sidecar(item_id, "s-en").unwrap().unwrap();

        // Injected failure: the second insert aborts inside the transaction.
        db.with_conn(|c| {
            c.execute_batch(
                "CREATE TRIGGER sidecar_insert_boom BEFORE INSERT ON media_item_sidecars
                 WHEN NEW.track_id = 'boom'
                 BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
            )
            .map_err(|e| e.to_string())
        })
        .unwrap();

        let mut changed = observed("s-en");
        changed.content_id = "20-first-edit-last-edit".into();
        let err = db
            .reconcile_item_sidecars(item_id, &[changed, observed("boom")])
            .unwrap_err();
        assert!(err.contains("injected write failure"), "{err}");

        assert_eq!(
            db.list_item_sidecars(item_id).unwrap(),
            vec![stored.clone()],
            "the failed set leaves the prior membership and generation untouched"
        );

        db.with_conn(|c| {
            c.execute_batch("DROP TRIGGER sidecar_insert_boom;")
                .map_err(|e| e.to_string())
        })
        .unwrap();

        // The rolled-back attempt consumed no generation.
        let mut retry = observed("s-en");
        retry.content_id = "20-first-edit-last-edit".into();
        let delta = db.reconcile_item_sidecars(item_id, &[retry]).unwrap();
        assert_eq!(delta.changed[0].after.sidecar_generation, Some(2));
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

    /// `subtitle_attempt_count` straight from the row, so a test can prove a
    /// stale write consumed no failure backoff.
    fn subtitle_attempts(db: &Db, item_id: i64) -> i64 {
        db.with_conn(|c| {
            c.query_row(
                "SELECT subtitle_attempt_count FROM media_items WHERE id = ?1",
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

        assert_eq!(
            db.reconcile_item_sidecars(id, &[observed("s-en")])
                .unwrap()
                .added
                .len(),
            1
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

    // ------------------------------------------------------------------
    // ADR-0058 coherent probe read
    // ------------------------------------------------------------------

    /// The columns the certification gate reads, seeded directly so each case
    /// controls status, revisions, and identity.
    #[derive(Clone, Copy)]
    struct Seed {
        probe_status: &'static str,
        media_revision: i64,
        probe_revision: i64,
        probed_media_revision: Option<i64>,
        content_id: Option<&'static str>,
        probed_content_id: Option<&'static str>,
    }

    fn seed_certifiable_item(db: &Db, library_id: i64, path: &str, seed: &Seed) -> i64 {
        db.with_conn(|c| {
            c.execute(
                "INSERT INTO media_items
                    (library_id, path, mtime_ms, size_bytes, title, kind,
                     probe_status, media_revision, probe_revision,
                     probed_media_revision, content_id, probed_content_id)
                 VALUES (?1, ?2, 1, 2, 'clip', 'movie', ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    library_id,
                    path,
                    seed.probe_status,
                    seed.media_revision,
                    seed.probe_revision,
                    seed.probed_media_revision,
                    seed.content_id,
                    seed.probed_content_id,
                ],
            )
            .map_err(|e| e.to_string())?;
            Ok(c.last_insert_rowid())
        })
        .unwrap()
    }

    /// A publication's scalars and both inventories come back exactly, and each
    /// inventory is ordered by absolute stream index regardless of insert order.
    #[test]
    fn coherent_read_returns_ready_with_exact_contents_and_order() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));

        let mut published = complete_snapshot(id);
        published.audio_tracks = vec![
            AudioTrackRow {
                stream_index: 3,
                codec: "opus".into(),
                language: Some("jpn".into()),
                channels: Some(6),
                channel_layout: Some("5.1".into()),
                title: Some("Surround".into()),
                is_default: false,
            },
            AudioTrackRow {
                stream_index: 1,
                codec: "aac".into(),
                language: Some("eng".into()),
                channels: Some(2),
                channel_layout: Some("stereo".into()),
                title: Some("Main".into()),
                is_default: true,
            },
            AudioTrackRow {
                stream_index: 2,
                codec: "flac".into(),
                language: None,
                channels: None,
                channel_layout: None,
                title: None,
                is_default: false,
            },
        ];
        published.subtitle_tracks = vec![
            SubtitleTrackRow {
                media_item_id: id,
                stream_index: 5,
                codec: "ass".into(),
                language: Some("eng".into()),
                title: Some("Signs".into()),
                forced: true,
                sdh: false,
                kind: "ass".into(),
            },
            SubtitleTrackRow {
                media_item_id: id,
                stream_index: 4,
                codec: "subrip".into(),
                language: Some("eng".into()),
                title: None,
                forced: false,
                sdh: true,
                kind: "text".into(),
            },
        ];
        published.subtitle_status = "eligible".into();

        let publication = db
            .publish_probe(
                &expectation_of(&db, id),
                &ProbeOutcome::Success(Box::new(published.clone())),
            )
            .unwrap();
        assert_eq!(
            publication,
            ProbePublication::Published {
                media_revision: 1,
                probe_revision: 1,
            }
        );

        let read = db.coherent_probe_read(id).unwrap().unwrap();
        let CoherentProbeRead::Ready(ready) = read else {
            panic!("a certified publication must read as ready");
        };
        assert_eq!(ready.item_id, id);
        assert_eq!(ready.media_revision, 1);
        assert_eq!(ready.probe_revision, 1);
        assert_eq!(ready.content_id, "1-aaa-bbb");

        let mut expected = published;
        expected.audio_tracks.sort_by_key(|t| t.stream_index);
        expected.subtitle_tracks.sort_by_key(|t| t.stream_index);
        assert_eq!(ready.snapshot, expected);
        assert_eq!(
            ready
                .snapshot
                .audio_tracks
                .iter()
                .map(|t| t.stream_index)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            ready
                .snapshot
                .subtitle_tracks
                .iter()
                .map(|t| t.stream_index)
                .collect::<Vec<_>>(),
            vec![4, 5]
        );
    }

    /// The certification gate, positive and negative: status, revision
    /// positivity, media-revision agreement, and identity presence and match.
    /// The certified row with empty inventories is the control that proves the
    /// gate can return `Ready`.
    #[test]
    fn coherent_read_certifies_only_a_matching_positive_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");

        let cases: &[(&str, Seed, bool)] = &[
            (
                "certified",
                Seed {
                    probe_status: "probed",
                    media_revision: 1,
                    probe_revision: 1,
                    probed_media_revision: Some(1),
                    content_id: Some("1-aaa"),
                    probed_content_id: Some("1-aaa"),
                },
                true,
            ),
            (
                "indexed",
                Seed {
                    probe_status: "indexed",
                    media_revision: 1,
                    probe_revision: 0,
                    probed_media_revision: None,
                    content_id: Some("1-aaa"),
                    probed_content_id: None,
                },
                false,
            ),
            (
                "error",
                Seed {
                    probe_status: "error",
                    media_revision: 1,
                    probe_revision: 0,
                    probed_media_revision: None,
                    content_id: Some("1-aaa"),
                    probed_content_id: None,
                },
                false,
            ),
            (
                "unavailable",
                Seed {
                    probe_status: "unavailable",
                    media_revision: 1,
                    probe_revision: 0,
                    probed_media_revision: None,
                    content_id: Some("1-aaa"),
                    probed_content_id: None,
                },
                false,
            ),
            (
                "legacy probed at revision zero",
                Seed {
                    probe_status: "probed",
                    media_revision: 1,
                    probe_revision: 0,
                    probed_media_revision: None,
                    content_id: Some("1-aaa"),
                    probed_content_id: None,
                },
                false,
            ),
            (
                "NULL identity",
                Seed {
                    probe_status: "probed",
                    media_revision: 1,
                    probe_revision: 1,
                    probed_media_revision: Some(1),
                    content_id: None,
                    probed_content_id: None,
                },
                false,
            ),
            (
                "empty identity",
                Seed {
                    probe_status: "probed",
                    media_revision: 1,
                    probe_revision: 1,
                    probed_media_revision: Some(1),
                    content_id: Some(""),
                    probed_content_id: Some(""),
                },
                false,
            ),
            (
                "identity mismatch",
                Seed {
                    probe_status: "probed",
                    media_revision: 1,
                    probe_revision: 1,
                    probed_media_revision: Some(1),
                    content_id: Some("1-aaa"),
                    probed_content_id: Some("2-bbb"),
                },
                false,
            ),
            (
                "identity stamp absent",
                Seed {
                    probe_status: "probed",
                    media_revision: 1,
                    probe_revision: 1,
                    probed_media_revision: Some(1),
                    content_id: Some("1-aaa"),
                    probed_content_id: None,
                },
                false,
            ),
            (
                "media revision mismatch",
                Seed {
                    probe_status: "probed",
                    media_revision: 2,
                    probe_revision: 1,
                    probed_media_revision: Some(1),
                    content_id: Some("1-aaa"),
                    probed_content_id: Some("1-aaa"),
                },
                false,
            ),
        ];

        for (i, (name, seed, ready)) in cases.iter().enumerate() {
            let id = seed_certifiable_item(&db, lib, &format!("clip{i}.mkv"), seed);
            let read = db.coherent_probe_read(id).unwrap().unwrap();
            if *ready {
                assert!(
                    matches!(read, CoherentProbeRead::Ready(_)),
                    "{name}: expected ready, got {read:?}"
                );
            } else {
                assert_eq!(read, CoherentProbeRead::Unverified, "{name}");
            }
        }
    }

    /// Every stored child must bear the item's positive probe revision. A row
    /// left at the migration default zero, or one from another publication,
    /// makes the whole read `Unverified` rather than a partial snapshot.
    #[test]
    fn coherent_read_rejects_mixed_or_missing_child_revisions() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");

        let cases: &[(&str, &[i64], &[i64])] = &[
            ("audio row at revision zero", &[1, 0], &[1]),
            ("subtitle row at revision zero", &[1], &[1, 0]),
            ("subtitle row from a newer publication", &[1], &[1, 2]),
        ];
        for (i, (name, audio_revisions, subtitle_revisions)) in cases.iter().enumerate() {
            let id = seed_certifiable_item(
                &db,
                lib,
                &format!("clip{i}.mkv"),
                &Seed {
                    probe_status: "probed",
                    media_revision: 1,
                    probe_revision: 1,
                    probed_media_revision: Some(1),
                    content_id: Some("1-aaa"),
                    probed_content_id: Some("1-aaa"),
                },
            );
            db.with_conn(|c| {
                for (n, revision) in audio_revisions.iter().enumerate() {
                    c.execute(
                        "INSERT INTO media_item_audio_tracks
                            (media_item_id, probe_revision, stream_index, codec, is_default)
                         VALUES (?1, ?2, ?3, 'aac', 0)",
                        params![id, revision, n as i64],
                    )
                    .map_err(|e| e.to_string())?;
                }
                for (n, revision) in subtitle_revisions.iter().enumerate() {
                    c.execute(
                        "INSERT INTO media_item_subtitle_tracks
                            (media_item_id, probe_revision, stream_index, codec,
                             forced, sdh, kind)
                         VALUES (?1, ?2, ?3, 'subrip', 0, 0, 'text')",
                        params![id, revision, n as i64],
                    )
                    .map_err(|e| e.to_string())?;
                }
                Ok(())
            })
            .unwrap();

            assert_eq!(
                db.coherent_probe_read(id).unwrap().unwrap(),
                CoherentProbeRead::Unverified,
                "{name}"
            );
        }
    }

    /// An unknown item is `None`, never `Unverified`: a missing item is a
    /// different outcome from an existing item with uncertified facts.
    #[test]
    fn coherent_read_returns_none_for_an_unknown_item() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        revision_library(&db, "/films");

        assert_eq!(db.coherent_probe_read(4242).unwrap(), None);
    }

    /// The item row and both inventories come from one read transaction, so a
    /// reader that has fixed its snapshot cannot observe a publication that
    /// commits afterwards. The reader pauses after its item SELECT; a second
    /// connection publishes and commits a coherent next generation in that
    /// window; the reader then reads the inventories and must return the old
    /// generation whole, and a later read must return the new one whole.
    ///
    /// Three autocommit reads would see the item at revision 1 and the children
    /// at revision 2, which fails certification and returns `Unverified`, so
    /// this test fails if the read is not one transaction.
    #[test]
    fn coherent_read_holds_one_snapshot_while_a_writer_commits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let db = Db::open(&path).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));
        assert_eq!(
            publish_complete_success(&db, id),
            ProbePublication::Published {
                media_revision: 1,
                probe_revision: 1,
            }
        );

        let (snapshot_taken_tx, snapshot_taken_rx) = std::sync::mpsc::channel();
        let (committed_tx, committed_rx) = std::sync::mpsc::channel();

        // The writer waits until the reader's item SELECT has fixed its read
        // snapshot, then publishes generation 2 and commits it.
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            snapshot_taken_rx.recv().unwrap();
            let conn = Connection::open(&writer_path).unwrap();
            conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
            conn.execute(
                "UPDATE media_items
                    SET duration_ms = 2000, probe_revision = 2,
                        probed_media_revision = media_revision
                  WHERE id = ?1",
                [id],
            )
            .unwrap();
            conn.execute(
                "DELETE FROM media_item_audio_tracks WHERE media_item_id = ?1",
                [id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO media_item_audio_tracks
                    (media_item_id, probe_revision, stream_index, codec, is_default)
                 VALUES (?1, 2, 5, 'opus', 0)",
                [id],
            )
            .unwrap();
            conn.execute(
                "DELETE FROM media_item_subtitle_tracks WHERE media_item_id = ?1",
                [id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO media_item_subtitle_tracks
                    (media_item_id, probe_revision, stream_index, codec, forced, sdh, kind)
                 VALUES (?1, 2, 6, 'ass', 0, 0, 'ass')",
                [id],
            )
            .unwrap();
            conn.execute_batch("COMMIT;").unwrap();
            committed_tx.send(()).unwrap();
        });

        set_probe_read_after_item_hook(move || {
            snapshot_taken_tx.send(()).unwrap();
            committed_rx.recv().unwrap();
        });

        let old = match db.coherent_probe_read(id).unwrap().unwrap() {
            CoherentProbeRead::Ready(snapshot) => snapshot,
            CoherentProbeRead::Unverified => {
                panic!("the reader's snapshot must still certify generation 1")
            }
        };
        writer.join().unwrap();

        assert_eq!(old.probe_revision, 1, "the reader keeps its own snapshot");
        assert_eq!(old.media_revision, 1);
        assert_eq!(old.content_id, "1-aaa-bbb");
        assert_eq!(old.snapshot.duration_ms, Some(1000));
        assert_eq!(old.snapshot.audio_tracks[0].codec, "aac");
        assert_eq!(old.snapshot.subtitle_tracks[0].codec, "subrip");

        // A new snapshot sees the committed generation whole, so the writer's
        // commit is observable and the pause above was real.
        let new = match db.coherent_probe_read(id).unwrap().unwrap() {
            CoherentProbeRead::Ready(snapshot) => snapshot,
            CoherentProbeRead::Unverified => panic!("the committed generation must certify"),
        };
        assert_eq!(new.probe_revision, 2);
        assert_eq!(new.media_revision, 1);
        assert_eq!(new.content_id, "1-aaa-bbb");
        assert_eq!(new.snapshot.duration_ms, Some(2000));
        assert_eq!(new.snapshot.audio_tracks[0].codec, "opus");
        assert_eq!(new.snapshot.subtitle_tracks[0].codec, "ass");
    }

    // ------------------------------------------------------------------
    // D2B.2 certified subtitle source and readiness publication
    // ------------------------------------------------------------------

    fn observed_srt(content_id: &str, size_bytes: i64) -> ObservedSidecar {
        ObservedSidecar {
            track_id: "s-en".into(),
            path: "clip.en.srt".into(),
            mtime_ms: 5,
            size_bytes,
            format: "srt".into(),
            language: Some("eng".into()),
            forced: false,
            sdh: false,
            content_id: content_id.into(),
        }
    }

    /// A certified source needs both the ADR-0058 probe certification and a
    /// durable generation on every serveable sidecar (D2B.2 acceptance 1).
    #[test]
    fn certified_subtitle_source_requires_certification_and_generations() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));

        // Unprobed: nothing certifies.
        assert!(db.certified_subtitle_source(id).unwrap().is_none());

        // A legacy unverified sidecar has no generation, so the source stays
        // uncertified even after a successful probe.
        db.reconcile_item_sidecars(id, &[observed_srt("7-aaa-bbb", 7)])
            .unwrap();
        db.with_conn(|c| {
            c.execute(
                "UPDATE media_item_sidecars SET sidecar_generation = NULL, content_id = NULL
                  WHERE media_item_id = ?1",
                [id],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        })
        .unwrap();
        assert!(matches!(
            publish_complete_success(&db, id),
            ProbePublication::Published { .. }
        ));
        assert!(
            db.certified_subtitle_source(id).unwrap().is_none(),
            "an unverified sidecar cannot certify"
        );

        // Re-verifying the sidecar certifies, and each track's token is a
        // stable bounded function of the captured source.
        db.reconcile_item_sidecars(id, &[observed_srt("7-aaa-bbb", 7)])
            .unwrap();
        let source = db.certified_subtitle_source(id).unwrap().unwrap();
        assert_eq!(source.sidecars.len(), 1);
        assert!(source.sidecars[0].generation >= 1);
        let embedded = source.token_for_track("e2").expect("embedded token");
        let sidecar = source.token_for_track("s-en").expect("sidecar token");
        assert_eq!(
            embedded,
            db.certified_subtitle_source(id)
                .unwrap()
                .unwrap()
                .token_for_track("e2")
                .unwrap()
        );
        assert_eq!(embedded, "v1-m1-p1", "embedded token is revisions only");
        assert_eq!(
            sidecar,
            format!("v1-m1-p1-s{}", source.sidecars[0].generation),
            "a sidecar token carries only its own generation"
        );
        assert!(is_valid_generation_token(&embedded));
        assert!(is_valid_generation_token(&sidecar));
        assert!(source.token_for_track("e9").is_none());
        assert!(source.token_for_track("s-fr").is_none());
        assert_eq!(
            source.source_identity(),
            db.certified_subtitle_source(id)
                .unwrap()
                .unwrap()
                .source_identity()
        );
    }

    /// ADR-0013 §13.1: the token grammar is exact, bounded to 65 bytes, and
    /// rejects an out-of-grammar string before it can be a path segment.
    #[test]
    fn generation_token_grammar_is_exact_and_bounded() {
        for good in ["v1-m1-p0", "v1-m1-p0-s1", "v1-m10-p20-s300", "v1-m0-p0"] {
            assert!(is_valid_generation_token(good), "{good} must be valid");
        }
        for bad in [
            "",
            "v1-m1-p0-s",
            "v1-m1-p0-s1-s2",
            "v1-m1-p",
            "v1-m-p0",
            "v1-m01-p0",
            "v1-m1-p00",
            "v1-m1-p0-s01",
            "m1-p0",
            "v1-m1-p0-s1-x",
            "v1-m1-p0 ",
            "v1-m1-p0/S",
            "v2-m1-p0",
        ] {
            assert!(!is_valid_generation_token(bad), "{bad:?} must be invalid");
        }
        // The grammar's own worst case is exactly the 65-byte bound.
        let widest = format!("v1-m{}-p{}-s{}", i64::MAX, i64::MAX, i64::MAX);
        assert_eq!(widest.len(), 65);
        assert!(is_valid_generation_token(&widest));
        let too_long = format!("v1-m{}-p0", "9".repeat(64));
        assert!(too_long.len() > 65);
        assert!(!is_valid_generation_token(&too_long));
    }

    /// Commit a `complete` publication for every serveable member of the item's
    /// current certified source, the way the worker does: reserve an artifact
    /// revision for each changed body, then CAS the reference to it.
    fn publish_all_complete(db: &Db, id: i64) -> SubtitlePublication {
        let source = db.certified_subtitle_source(id).unwrap().unwrap();
        let mut outcome = SubtitlePublication::Stale;
        for track_id in source.members() {
            let Some(token) = source.token_for_track(&track_id) else {
                continue;
            };
            let revision = db.reserve_subtitle_artifact_revision(id).unwrap();
            outcome = db
                .publish_subtitle_artifact(
                    &source,
                    &SubtitleArtifactPublication {
                        track_id,
                        token,
                        artifact_revision: revision,
                        state: SubtitleArtifactState::Complete,
                    },
                )
                .unwrap();
        }
        outcome
    }

    /// Commit one `partial` publication for one track of the current certified
    /// source, with a freshly reserved artifact revision.
    fn publish_partial(db: &Db, id: i64, track_id: &str) -> SubtitlePublication {
        let source = db.certified_subtitle_source(id).unwrap().unwrap();
        let token = source.token_for_track(track_id).unwrap();
        let revision = db.reserve_subtitle_artifact_revision(id).unwrap();
        db.publish_subtitle_artifact(
            &source,
            &SubtitleArtifactPublication {
                track_id: track_id.to_string(),
                token,
                artifact_revision: revision,
                state: SubtitleArtifactState::Partial,
            },
        )
        .unwrap()
    }

    /// The publication CAS commits complete per-track references only while the
    /// captured certification, membership, revisions, and generations still
    /// match (D2B.2 acceptance 3, ADR-0013 §13.4).
    #[test]
    fn publish_subtitle_artifact_cas_requires_the_captured_source() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));
        db.reconcile_item_sidecars(id, &[observed_srt("7-aaa-bbb", 7)])
            .unwrap();
        assert!(matches!(
            publish_complete_success(&db, id),
            ProbePublication::Published { .. }
        ));
        let source = db.certified_subtitle_source(id).unwrap().unwrap();
        assert_eq!(source.members(), vec!["e2".to_string(), "s-en".to_string()]);

        assert_eq!(
            publish_all_complete(&db, id),
            SubtitlePublication::Published
        );
        assert_eq!(db.get_item(id).unwrap().unwrap().subtitle_status, "ready");
        let published = db.certified_subtitle_source(id).unwrap().unwrap();
        assert!(published.is_complete("e2"));
        assert!(published.is_complete("s-en"));
        assert_eq!(
            published.artifact_for("s-en").unwrap().state,
            SubtitleArtifactState::Complete
        );
        assert!(
            published.artifact_for("s-en").unwrap().artifact_revision > 0,
            "a committed reference names a positive artifact revision"
        );
        assert_eq!(
            published.artifact_for("s-en").unwrap().subtitle_content_id,
            published.snapshot.content_id
        );

        // Certification loss without a revision increment: a failed probe
        // clears the validity stamps but leaves `probe_revision` unchanged.
        let expectation = expectation_of(&db, id);
        let revision = expectation.probe_revision;
        assert!(matches!(
            db.publish_probe(
                &expectation,
                &ProbeOutcome::Failure {
                    probe_status: "error".into(),
                    scan_error: "boom".into(),
                },
            )
            .unwrap(),
            ProbePublication::FailureRecorded
        ));
        assert_eq!(db.get_item(id).unwrap().unwrap().probe_revision, revision);
        db.set_subtitle_status(id, "eligible").unwrap();
        let revision = db.reserve_subtitle_artifact_revision(id).unwrap();
        assert_eq!(
            db.publish_subtitle_artifact(
                &source,
                &SubtitleArtifactPublication {
                    track_id: "s-en".into(),
                    token: source.token_for_track("s-en").unwrap(),
                    artifact_revision: revision,
                    state: SubtitleArtifactState::Complete,
                },
            )
            .unwrap(),
            SubtitlePublication::Stale,
            "lost certification must reject the publication"
        );
        assert_eq!(
            db.get_item(id).unwrap().unwrap().subtitle_status,
            "eligible"
        );
        // The reference is unreachable too: the source cannot certify, so
        // serving has no source to match against.
        assert!(db.certified_subtitle_source(id).unwrap().is_none());
    }

    /// A sidecar that changes after the artifact was built receives a new
    /// generation, which invalidates the captured source (D2B.2 acceptance 5).
    #[test]
    fn publish_subtitle_artifact_is_stale_when_a_sidecar_generation_changes() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));
        db.reconcile_item_sidecars(id, &[observed_srt("7-aaa-bbb", 7)])
            .unwrap();
        assert!(matches!(
            publish_complete_success(&db, id),
            ProbePublication::Published { .. }
        ));
        let source = db.certified_subtitle_source(id).unwrap().unwrap();
        assert_eq!(
            publish_all_complete(&db, id),
            SubtitlePublication::Published
        );

        // The sidecar's bytes changed, so reconciliation allocates a new
        // generation for the same track id.
        db.reconcile_item_sidecars(id, &[observed_srt("8-bbb-ccc", 8)])
            .unwrap();
        let current = db.certified_subtitle_source(id).unwrap().unwrap();
        assert!(current.sidecars[0].generation > source.sidecars[0].generation);

        let revision = db.reserve_subtitle_artifact_revision(id).unwrap();
        assert_eq!(
            db.publish_subtitle_artifact(
                &source,
                &SubtitleArtifactPublication {
                    track_id: "s-en".into(),
                    token: source.token_for_track("s-en").unwrap(),
                    artifact_revision: revision,
                    state: SubtitleArtifactState::Complete,
                },
            )
            .unwrap(),
            SubtitlePublication::Stale
        );
        assert!(
            current.artifact_for("s-en").is_none(),
            "the changed sidecar has no committed reference for its new generation"
        );
    }

    /// D2B.2 acceptance 4 / round-2 item 5 / ADR-0013 §13.4: reconciliation
    /// invalidates only the affected per-track references. Unchanged embedded
    /// and sidecar tracks keep their committed references and their tokens, the
    /// coarse item-level lifecycle field is not cleared item-wide, and a
    /// burn-in-only sidecar change invalidates nothing.
    #[test]
    fn reconciliation_invalidates_only_affected_track_references() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));
        db.reconcile_item_sidecars(
            id,
            &[
                observed_srt("7-aaa-bbb", 7),
                ObservedSidecar {
                    track_id: "s-fr".into(),
                    path: "clip.fr.srt".into(),
                    mtime_ms: 6,
                    size_bytes: 6,
                    format: "srt".into(),
                    language: Some("fra".into()),
                    forced: false,
                    sdh: false,
                    content_id: "6-aaa-bbb".into(),
                },
            ],
        )
        .unwrap();
        assert!(matches!(
            publish_complete_success(&db, id),
            ProbePublication::Published { .. }
        ));
        let source = db.certified_subtitle_source(id).unwrap().unwrap();
        assert_eq!(
            publish_all_complete(&db, id),
            SubtitlePublication::Published
        );
        let embedded_token = source.token_for_track("e2").unwrap();
        let french_token = source.token_for_track("s-fr").unwrap();

        // The English sidecar's bytes change: a new generation for s-en only.
        db.reconcile_item_sidecars(
            id,
            &[
                observed_srt("8-bbb-ccc", 8),
                ObservedSidecar {
                    track_id: "s-fr".into(),
                    path: "clip.fr.srt".into(),
                    mtime_ms: 6,
                    size_bytes: 6,
                    format: "srt".into(),
                    language: Some("fra".into()),
                    forced: false,
                    sdh: false,
                    content_id: "6-aaa-bbb".into(),
                },
            ],
        )
        .unwrap();
        let after = db.certified_subtitle_source(id).unwrap().unwrap();
        assert!(
            after.is_complete("e2"),
            "the unchanged embedded track keeps its committed reference"
        );
        assert_eq!(after.token_for_track("e2").unwrap(), embedded_token);
        assert!(
            after.is_complete("s-fr"),
            "an unchanged sidecar keeps its committed reference"
        );
        assert_eq!(after.token_for_track("s-fr").unwrap(), french_token);
        assert!(
            after.artifact_for("s-en").is_none(),
            "only the changed track loses its reference"
        );
        assert_eq!(
            db.get_item(id).unwrap().unwrap().subtitle_status,
            "ready",
            "the item-level lifecycle field is not cleared item-wide"
        );
        let revision = db.reserve_subtitle_artifact_revision(id).unwrap();
        assert_eq!(
            db.publish_subtitle_artifact(
                &source,
                &SubtitleArtifactPublication {
                    track_id: "s-en".into(),
                    token: source.token_for_track("s-en").unwrap(),
                    artifact_revision: revision,
                    state: SubtitleArtifactState::Complete,
                },
            )
            .unwrap(),
            SubtitlePublication::Stale
        );

        // A burn-in-only sidecar change invalidates nothing at all.
        db.reconcile_item_sidecars(
            id,
            &[
                observed_srt("8-bbb-ccc", 8),
                ObservedSidecar {
                    track_id: "s-fr".into(),
                    path: "clip.fr.srt".into(),
                    mtime_ms: 6,
                    size_bytes: 6,
                    format: "srt".into(),
                    language: Some("fra".into()),
                    forced: false,
                    sdh: false,
                    content_id: "6-aaa-bbb".into(),
                },
                ObservedSidecar {
                    track_id: "s-ass".into(),
                    path: "clip.ass".into(),
                    mtime_ms: 3,
                    size_bytes: 3,
                    format: "ass".into(),
                    language: None,
                    forced: false,
                    sdh: false,
                    content_id: "3-eee-fff".into(),
                },
            ],
        )
        .unwrap();
        let after = db.certified_subtitle_source(id).unwrap().unwrap();
        assert!(after.is_complete("e2"));
        assert!(after.is_complete("s-fr"));
        assert_eq!(db.get_item(id).unwrap().unwrap().subtitle_status, "ready");
    }

    /// D2B.2 acceptance 3/5 / round-2 item 2: progressive publication is a
    /// committed per-track reference gated by the same source CAS. A stale
    /// progressive write changes nothing and cannot become serveable for a
    /// newer generation; a progressive write never downgrades a complete
    /// reference.
    #[test]
    fn progressive_publication_is_gated_by_the_source_cas() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));
        db.reconcile_item_sidecars(id, &[observed_srt("7-aaa-bbb", 7)])
            .unwrap();
        assert!(matches!(
            publish_complete_success(&db, id),
            ProbePublication::Published { .. }
        ));
        let source = db.certified_subtitle_source(id).unwrap().unwrap();
        let token = source.token_for_track("s-en").unwrap();
        let partial_revision = db.reserve_subtitle_artifact_revision(id).unwrap();
        assert_eq!(
            db.publish_subtitle_artifact(
                &source,
                &SubtitleArtifactPublication {
                    track_id: "s-en".into(),
                    token: token.clone(),
                    artifact_revision: partial_revision,
                    state: SubtitleArtifactState::Partial,
                },
            )
            .unwrap(),
            SubtitlePublication::Published
        );
        let partial = db.certified_subtitle_source(id).unwrap().unwrap();
        assert_eq!(
            partial.artifact_for("s-en").unwrap().state,
            SubtitleArtifactState::Partial
        );
        assert_eq!(
            partial.artifact_for("s-en").unwrap().artifact_revision,
            partial_revision,
            "the committed reference names the reserved revision"
        );
        assert!(!partial.is_complete("s-en"));

        // A progressive write for a complete track must not downgrade it, and
        // complete bytes are never replaced.
        assert_eq!(
            publish_all_complete(&db, id),
            SubtitlePublication::Published
        );
        let complete_revision = db
            .certified_subtitle_source(id)
            .unwrap()
            .unwrap()
            .artifact_for("s-en")
            .unwrap()
            .artifact_revision;
        assert_eq!(
            publish_partial(&db, id, "s-en"),
            SubtitlePublication::Published
        );
        let after_complete = db.certified_subtitle_source(id).unwrap().unwrap();
        assert!(after_complete.is_complete("s-en"));
        assert_eq!(
            after_complete
                .artifact_for("s-en")
                .unwrap()
                .artifact_revision,
            complete_revision,
            "a partial write must not replace a complete reference"
        );

        // The sidecar changes: the captured source is stale, so the growing
        // write is rejected and the old reference is gone.
        db.reconcile_item_sidecars(id, &[observed_srt("8-bbb-ccc", 8)])
            .unwrap();
        let stale_revision = db.reserve_subtitle_artifact_revision(id).unwrap();
        assert_eq!(
            db.publish_subtitle_artifact(
                &source,
                &SubtitleArtifactPublication {
                    track_id: "s-en".into(),
                    token: token.clone(),
                    artifact_revision: stale_revision,
                    state: SubtitleArtifactState::Partial,
                },
            )
            .unwrap(),
            SubtitlePublication::Stale,
            "a stale progressive write must be rejected by the CAS"
        );
        let after = db.certified_subtitle_source(id).unwrap().unwrap();
        assert!(after.artifact_for("s-en").is_none());
        assert!(!after.is_complete("s-en"));

        // A token the captured source does not mint for the track is rejected
        // even when the source itself still matches.
        let current = db.certified_subtitle_source(id).unwrap().unwrap();
        let current_token = current.token_for_track("s-en").unwrap();
        let wrong_revision = db.reserve_subtitle_artifact_revision(id).unwrap();
        assert_eq!(
            db.publish_subtitle_artifact(
                &current,
                &SubtitleArtifactPublication {
                    track_id: "s-en".into(),
                    token: "v1-m1-p0".into(),
                    artifact_revision: wrong_revision,
                    state: SubtitleArtifactState::Partial,
                },
            )
            .unwrap(),
            SubtitlePublication::Stale
        );
        assert!(current.artifact_for("s-en").is_none());
        assert_eq!(
            publish_partial(&db, id, "s-en"),
            SubtitlePublication::Published
        );
        let landed = db.certified_subtitle_source(id).unwrap().unwrap();
        assert_eq!(landed.token_for_track("s-en").unwrap(), current_token);
    }

    /// Round-2 item 3: a stale worker cannot mutate the subtitle state of a
    /// newer source through `none`/`eligible`/`error`/`unavailable`. Every
    /// outcome is written only when the captured source is still current.
    #[test]
    fn stale_worker_status_writes_are_rejected_by_the_source_cas() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));
        db.reconcile_item_sidecars(id, &[observed_srt("7-aaa-bbb", 7)])
            .unwrap();
        assert!(matches!(
            publish_complete_success(&db, id),
            ProbePublication::Published { .. }
        ));
        let source = db.certified_subtitle_source(id).unwrap().unwrap();
        assert_eq!(
            db.record_subtitle_status(&source, "none").unwrap(),
            SubtitlePublication::Published
        );
        assert_eq!(db.get_item(id).unwrap().unwrap().subtitle_status, "none");

        // The sidecar changes under the worker: the captured source is stale.
        db.reconcile_item_sidecars(id, &[observed_srt("8-bbb-ccc", 8)])
            .unwrap();
        for status in ["none", "eligible", "error", "unavailable"] {
            assert_eq!(
                db.record_subtitle_status(&source, status).unwrap(),
                SubtitlePublication::Stale,
                "{status} from stale work must be rejected"
            );
            let row = db.get_item(id).unwrap().unwrap();
            assert_eq!(
                row.subtitle_status, "none",
                "{status} must not mutate a newer source's state"
            );
            assert_eq!(
                subtitle_attempts(&db, id),
                0,
                "{status} must not consume failure backoff"
            );
        }

        // The current source still writes its own outcome.
        let current = db.certified_subtitle_source(id).unwrap().unwrap();
        assert_eq!(
            db.record_subtitle_status(&current, "eligible").unwrap(),
            SubtitlePublication::Published
        );
        assert_eq!(
            db.get_item(id).unwrap().unwrap().subtitle_status,
            "eligible"
        );
    }

    /// Round-1 item 7 (removal/re-add ABA): a removed and re-added sidecar gets
    /// a fresh generation, so the old publication reference is never serveable
    /// again and the old captured source is stale.
    #[test]
    fn remove_then_readd_cannot_serve_the_old_publication() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));
        db.reconcile_item_sidecars(id, &[observed_srt("7-aaa-bbb", 7)])
            .unwrap();
        assert!(matches!(
            publish_complete_success(&db, id),
            ProbePublication::Published { .. }
        ));
        let first = db.certified_subtitle_source(id).unwrap().unwrap();
        assert_eq!(
            publish_all_complete(&db, id),
            SubtitlePublication::Published
        );
        let first_token = first.token_for_track("s-en").unwrap();

        // Remove the sidecar, then re-add the very same bytes.
        db.reconcile_item_sidecars(id, &[]).unwrap();
        db.reconcile_item_sidecars(id, &[observed_srt("7-aaa-bbb", 7)])
            .unwrap();
        let readded = db.certified_subtitle_source(id).unwrap().unwrap();
        let readded_token = readded.token_for_track("s-en").unwrap();
        assert_ne!(
            first_token, readded_token,
            "a re-add must not reuse the deleted generation"
        );
        assert!(
            readded.artifact_for("s-en").is_none(),
            "the re-added track has no committed reference"
        );
        let revision = db.reserve_subtitle_artifact_revision(id).unwrap();
        assert_eq!(
            db.publish_subtitle_artifact(
                &first,
                &SubtitleArtifactPublication {
                    track_id: "s-en".into(),
                    token: first_token,
                    artifact_revision: revision,
                    state: SubtitleArtifactState::Complete,
                },
            )
            .unwrap(),
            SubtitlePublication::Stale
        );
    }

    /// D2B.2 corrective reset item 1/2: artifact revisions are allocated
    /// monotonically per item and a reserved revision is never reused, even
    /// when a candidate is never committed.
    #[test]
    fn artifact_revisions_are_monotonic_per_item() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let a = upsert_observed(&db, lib, "a.mkv", 1, Some("1-aaa-bbb"));
        let b = upsert_observed(&db, lib, "b.mkv", 2, Some("2-aaa-bbb"));

        let first = db.reserve_subtitle_artifact_revision(a).unwrap();
        let second = db.reserve_subtitle_artifact_revision(a).unwrap();
        assert_eq!(first, 1);
        assert_eq!(second, 2, "allocation is monotonic per item");
        assert_eq!(
            db.reserve_subtitle_artifact_revision(b).unwrap(),
            1,
            "each item allocates from its own sequence"
        );
    }

    /// D2B.2 corrective reset item 5: the listing read returns certification,
    /// sidecar membership, publication state and the coarse lifecycle fields
    /// from one transaction, so an uncertified item still lists its durable
    /// sidecar rows.
    #[test]
    fn listing_source_reads_sidecars_and_publications_coherently() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));
        db.reconcile_item_sidecars(id, &[observed_srt("7-aaa-bbb", 7)])
            .unwrap();

        // Not certified yet: no embedded inventory, but the durable sidecar row
        // is still listed and nothing is serveable.
        let listing = db.subtitle_listing_source(id).unwrap().unwrap();
        assert!(listing.snapshot.is_none());
        assert_eq!(listing.sidecars.len(), 1);
        assert!(listing.artifacts.is_empty());
        assert!(!listing.needs_publication());
        assert!(listing.certified().is_none());

        assert!(matches!(
            publish_complete_success(&db, id),
            ProbePublication::Published { .. }
        ));
        assert_eq!(
            publish_all_complete(&db, id),
            SubtitlePublication::Published
        );
        let listing = db.subtitle_listing_source(id).unwrap().unwrap();
        assert!(listing.snapshot.is_some());
        assert_eq!(listing.sidecars.len(), 1);
        assert_eq!(listing.artifacts.len(), 2, "one row per serveable member");
        assert_eq!(listing.subtitle_status, "ready");
        assert!(!listing.needs_publication());
    }

    /// D2B.2 corrective reset item 4: demand is a content signal. A formerly
    /// `ready` item whose sidecar was edited, and a formerly `none` item that
    /// gained its first sidecar, both report demand even though the coarse
    /// item-level status did not move.
    #[test]
    fn demand_does_not_depend_on_the_coarse_status() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));
        db.reconcile_item_sidecars(id, &[observed_srt("7-aaa-bbb", 7)])
            .unwrap();
        assert!(matches!(
            publish_complete_success(&db, id),
            ProbePublication::Published { .. }
        ));
        assert_eq!(
            publish_all_complete(&db, id),
            SubtitlePublication::Published
        );
        assert_eq!(db.get_item(id).unwrap().unwrap().subtitle_status, "ready");

        // The sidecar is edited: reconciliation withdraws only its reference,
        // and the coarse `ready` stays. Demand must still report true.
        db.reconcile_item_sidecars(id, &[observed_srt("9-ccc-ddd", 9)])
            .unwrap();
        assert_eq!(
            db.get_item(id).unwrap().unwrap().subtitle_status,
            "ready",
            "the coarse field is not cleared item-wide"
        );
        assert!(
            db.subtitle_listing_source(id)
                .unwrap()
                .unwrap()
                .needs_publication(),
            "an edited sidecar must keep demanding work under a stale `ready`"
        );

        // The other direction: an item that had no subtitle at all is `none`
        // and gains its first sidecar.
        let none_item = upsert_observed(&db, lib, "silent.mkv", 3, Some("3-aaa-bbb"));
        let mut snapshot = complete_snapshot(none_item);
        snapshot.subtitle_tracks.clear();
        snapshot.subtitle_status = "none".into();
        assert!(matches!(
            db.publish_probe(
                &expectation_of(&db, none_item),
                &ProbeOutcome::Success(Box::new(snapshot)),
            )
            .unwrap(),
            ProbePublication::Published { .. }
        ));
        assert_eq!(
            db.get_item(none_item).unwrap().unwrap().subtitle_status,
            "none"
        );
        assert!(
            !db.subtitle_listing_source(none_item)
                .unwrap()
                .unwrap()
                .needs_publication()
        );
        db.reconcile_item_sidecars(none_item, &[observed_srt("4-ddd-eee", 4)])
            .unwrap();
        assert_eq!(
            db.get_item(none_item).unwrap().unwrap().subtitle_status,
            "none",
            "reconciliation leaves the coarse field alone"
        );
        assert!(
            db.subtitle_listing_source(none_item)
                .unwrap()
                .unwrap()
                .needs_publication(),
            "a first sidecar under a stale `none` must demand work"
        );
    }

    /// D2B.2 corrective reset item 1/2: a per-track publication never deletes an
    /// unrelated track's committed reference, and a complete reference is not
    /// replaced by a later write.
    #[test]
    fn per_track_publication_preserves_unrelated_references() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));
        db.reconcile_item_sidecars(id, &[observed_srt("7-aaa-bbb", 7)])
            .unwrap();
        assert!(matches!(
            publish_complete_success(&db, id),
            ProbePublication::Published { .. }
        ));
        assert_eq!(
            publish_all_complete(&db, id),
            SubtitlePublication::Published
        );
        let embedded = db
            .certified_subtitle_source(id)
            .unwrap()
            .unwrap()
            .artifact_for("e2")
            .unwrap()
            .clone();

        let english = db
            .certified_subtitle_source(id)
            .unwrap()
            .unwrap()
            .artifact_for("s-en")
            .unwrap()
            .clone();

        // A second, unrelated publication for s-en leaves the embedded
        // reference byte-identical and never replaces the complete reference.
        assert_eq!(
            publish_partial(&db, id, "s-en"),
            SubtitlePublication::Published
        );
        let after = db.certified_subtitle_source(id).unwrap().unwrap();
        assert_eq!(
            after.artifact_for("e2").unwrap(),
            &embedded,
            "an unrelated track's reference must not move"
        );
        assert_eq!(
            after.artifact_for("s-en").unwrap(),
            &english,
            "a later write must not replace a committed complete reference"
        );
        assert_eq!(
            after.artifact_for("s-en").unwrap().state,
            SubtitleArtifactState::Complete
        );
    }

    /// D2B.2 corrective reset item 2/6: a rejected publication CAS preserves the
    /// committed reference and its artifact revision exactly. The rejected
    /// candidate is never referenced.
    #[test]
    fn a_rejected_publication_preserves_the_committed_reference() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let lib = revision_library(&db, "/films");
        let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));
        db.reconcile_item_sidecars(id, &[observed_srt("7-aaa-bbb", 7)])
            .unwrap();
        assert!(matches!(
            publish_complete_success(&db, id),
            ProbePublication::Published { .. }
        ));

        // Commit A: a partial reference at its own reserved revision.
        let a_source = db.certified_subtitle_source(id).unwrap().unwrap();
        let a_token = a_source.token_for_track("s-en").unwrap();
        let a_revision = db.reserve_subtitle_artifact_revision(id).unwrap();
        assert_eq!(
            db.publish_subtitle_artifact(
                &a_source,
                &SubtitleArtifactPublication {
                    track_id: "s-en".into(),
                    token: a_token.clone(),
                    artifact_revision: a_revision,
                    state: SubtitleArtifactState::Partial,
                },
            )
            .unwrap(),
            SubtitlePublication::Published
        );

        // B is rejected: the item loses its certification without a revision
        // increment (a failed probe clears the validity stamps). This is the
        // fault that leaves a finalized candidate unreferenced.
        assert!(matches!(
            db.publish_probe(
                &expectation_of(&db, id),
                &ProbeOutcome::Failure {
                    probe_status: "error".into(),
                    scan_error: "boom".into(),
                },
            )
            .unwrap(),
            ProbePublication::FailureRecorded
        ));
        let b_revision = db.reserve_subtitle_artifact_revision(id).unwrap();
        assert_ne!(b_revision, a_revision, "B reserves its own revision");
        assert_eq!(
            db.publish_subtitle_artifact(
                &a_source,
                &SubtitleArtifactPublication {
                    track_id: "s-en".into(),
                    token: a_token.clone(),
                    artifact_revision: b_revision,
                    state: SubtitleArtifactState::Complete,
                },
            )
            .unwrap(),
            SubtitlePublication::Stale
        );

        // A survives, exactly: same token, same revision, same partial state.
        // The listing read is not certification-gated, so it shows the row the
        // rejected write must not have touched.
        let listing = db.subtitle_listing_source(id).unwrap().unwrap();
        let stored = listing
            .artifacts
            .iter()
            .find(|a| a.track_id == "s-en")
            .expect("A is still committed");
        assert_eq!(stored.token, a_token);
        assert_eq!(stored.artifact_revision, a_revision);
        assert_eq!(stored.state, SubtitleArtifactState::Partial);
        assert!(
            listing.certified().is_none(),
            "an uncertified item has no serveable source, so A is unreachable"
        );
    }

    /// D2B.2 corrective reset item 6: the committed reference is durable. After
    /// a restart, serving resolves the committed revision, not a process-local
    /// observation and not the previous generation.
    #[test]
    fn committed_publication_is_selected_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let (id, b_revision) = {
            let db = Db::open(&path).unwrap();
            let lib = revision_library(&db, "/films");
            let id = upsert_observed(&db, lib, "clip.mkv", 1, Some("1-aaa-bbb"));
            db.reconcile_item_sidecars(id, &[observed_srt("7-aaa-bbb", 7)])
                .unwrap();
            assert!(matches!(
                publish_complete_success(&db, id),
                ProbePublication::Published { .. }
            ));

            // Generation A is committed, then the sidecar is edited and
            // generation B is committed at its own revision.
            let a = db.certified_subtitle_source(id).unwrap().unwrap();
            let a_token = a.token_for_track("s-en").unwrap();
            let a_revision = db.reserve_subtitle_artifact_revision(id).unwrap();
            db.publish_subtitle_artifact(
                &a,
                &SubtitleArtifactPublication {
                    track_id: "s-en".into(),
                    token: a_token,
                    artifact_revision: a_revision,
                    state: SubtitleArtifactState::Partial,
                },
            )
            .unwrap();

            db.reconcile_item_sidecars(id, &[observed_srt("9-ccc-ddd", 9)])
                .unwrap();
            let b = db.certified_subtitle_source(id).unwrap().unwrap();
            let b_token = b.token_for_track("s-en").unwrap();
            let b_revision = db.reserve_subtitle_artifact_revision(id).unwrap();
            assert_eq!(
                db.publish_subtitle_artifact(
                    &b,
                    &SubtitleArtifactPublication {
                        track_id: "s-en".into(),
                        token: b_token.clone(),
                        artifact_revision: b_revision,
                        state: SubtitleArtifactState::Complete,
                    },
                )
                .unwrap(),
                SubtitlePublication::Published
            );
            assert_ne!(a_revision, b_revision);
            (id, b_revision)
        };

        let reopened = Db::open(&path).unwrap();
        let source = reopened.certified_subtitle_source(id).unwrap().unwrap();
        let artifact = source.artifact_for("s-en").expect("committed reference");
        assert_eq!(artifact.state, SubtitleArtifactState::Complete);
        assert_eq!(
            artifact.artifact_revision, b_revision,
            "the restarted server resolves the committed revision, not the prior one"
        );
        assert!(
            !source.is_complete("e2"),
            "the embedded member of the newer source has no committed reference"
        );
    }
}
