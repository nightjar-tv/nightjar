//! Text subtitle → WebVTT (ADR-0010 / ADR-0013).
//!
//! Extraction writes immutable, generation-addressed derived library data under
//! `{NIGHTJAR_DATA_DIR}/subs/{itemId}/{token}/{trackId}.r{artifactRevision}.vtt`
//! (ADR-0013 §13.2). Playback only reads.

mod discover;
mod lang;
mod slice;
mod srt;

pub use discover::{
    DiscoveredSidecar, SidecarDirCache, discover_sidecars, discover_sidecars_cached,
};
pub use lang::{container_stream_language, normalize_language};
pub use slice::{slice_webvtt, webvtt_max_cue_end_ms};
pub use srt::{decode_subtitle_bytes, srt_to_webvtt};

use nightjar_db::SubtitleArtifactState;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

/// Codecs we can convert to WebVTT without burn-in.
const TEXT_SUB_CODECS: &[&str] = &["subrip", "srt", "webvtt", "mov_text", "text"];

/// Codecs that need burn-in (ADR-0018). Soft WebVTT extract is not possible.
const BURN_IN_CODECS: &[&str] = &["ass", "ssa", "hdmv_pgs_subtitle"];

/// Measured standalone-extract throughput, MiB/s (ADR-0041 Decision 4: 235 MB
/// median source at 55 MB/s, 5.0 s median wall). The extract timeout budget is
/// sized from this rate (Decision 8.1) instead of the fixed 300 s constant
/// (an unstated ~16 GB ceiling, below the 22–33 GB top of the dogfood queue).
const EXTRACT_MIB_PER_SEC: u64 = 55;

/// Startup + probe allowance added to the size-derived extract budget. The
/// 55 MB/s figure is an upper bound over a degraded array, so the size term
/// alone would give a small source a sub-second budget that kills slow
/// small-file demuxes before they finish.
const EXTRACT_TIMEOUT_STARTUP_SECS: u64 = 60;

/// Per-file extract timeout budget from source size at the measured 55 MiB/s
/// rate plus a startup allowance (ADR-0041 Decision 8.1). Deletes the fixed
/// 300 s constant (Decision 10: "the fixed 300 s extract timeout constant").
pub fn extract_timeout_budget(src_bytes: u64) -> Duration {
    let secs = src_bytes / (EXTRACT_MIB_PER_SEC * 1024 * 1024) + EXTRACT_TIMEOUT_STARTUP_SECS;
    Duration::from_secs(secs)
}

/// Kill a runaway ASS burn demux rather than leave ffmpeg reading the NAS
/// forever. Fixed: ADR-0018's session-start path, not the size-scaled budget.
///
/// **The half hour is unsourced (Rule 4.14).** ADR-0018 explains why this is a
/// fixed ceiling rather than a size-scaled one; nothing records why 1800
/// seconds. Landed in #10. A long ASS demux over a slow share is the case it
/// exists for, and no measurement of that case is recorded, so the value is a
/// guess that has not yet been wrong.
const ASS_BURN_EXTRACT_TIMEOUT: Duration = Duration::from_secs(1800);

/// How often to publish a growing WebVTT while FFmpeg demuxes (ADR-0013 §11).
const PROGRESS_TICK: Duration = Duration::from_millis(500);

/// Refuse extract when the data volume has less free space than this.
///
/// **Unsourced (Rule 4.14).** No recorded derivation, and it is not a measured
/// worst-case extract size. It reads as a round number chosen to leave the
/// volume some room rather than as a bound on what an extract needs.
const MIN_FREE_BYTES: u64 = 256 * 1024 * 1024;

/// IO kinds that usually mean the mount/share is gone, not a bad subtitle file.
///
/// Write-side kinds (`PermissionDenied`, `StorageFull`, `QuotaExceeded`,
/// `ReadOnlyFilesystem`) are here too: a full or read-only subtitle volume is
/// an operational failure that clears, so the scanner must retry it as
/// `unavailable`, never retire the item as a permanent `error` (ADR-0041
/// Decision 8.3).
pub fn io_error_is_availability(err: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        err.kind(),
        ErrorKind::NotFound
            | ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::NotConnected
            | ErrorKind::BrokenPipe
            | ErrorKind::TimedOut
            | ErrorKind::UnexpectedEof
            | ErrorKind::PermissionDenied
            | ErrorKind::StorageFull
            | ErrorKind::QuotaExceeded
            | ErrorKind::ReadOnlyFilesystem
    ) || err.raw_os_error().is_some_and(|c| {
        // ESTALE / ENOTCONN on Unix when the SMB mount half-dies; ENOSPC /
        // EDQUOT when the subtitle volume fills while the kind is still Other.
        c == 70 || c == 57 || c == 60 || c == 28 || c == 69
    })
}

/// Render an IO error for the scanner's classifier. The availability decision
/// is made here, where the error kind still exists; the scanner only ever sees
/// the string and keys off the `unavailable:` prefix (Rule 4.11).
fn io_failure_message(context: &str, err: &std::io::Error) -> String {
    if io_error_is_availability(err) {
        format!("unavailable: {context}: {err}")
    } else {
        format!("{context}: {err}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleSourceKind {
    Embedded,
    Sidecar,
}

impl SubtitleSourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Embedded => "embedded",
            Self::Sidecar => "sidecar",
        }
    }
}

/// Per-track first-play readiness declared by the server (ADR-0013 §11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackReadiness {
    Preparing,
    Partial,
    Complete,
}

impl TrackReadiness {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Partial => "partial",
            Self::Complete => "complete",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSubtitleStream {
    /// Absolute ffprobe stream index (`-map 0:N`).
    pub stream_index: u32,
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub is_default: bool,
    pub is_forced: bool,
}

impl TextSubtitleStream {
    pub fn track_id(&self) -> String {
        format!("e{}", self.stream_index)
    }
}

/// How a listed subtitle track is delivered (ADR-0018).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleRender {
    Soft,
    BurnIn,
}

impl SubtitleRender {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Soft => "soft",
            Self::BurnIn => "burnIn",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BurnInKind {
    /// ASS / SSA via libass (`ass=` on a local file).
    Ass,
    /// Bitmap PGS via overlay.
    Pgs,
}

/// One burn-in track for session encode (ADR-0018).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BurnInSelection {
    pub track_id: String,
    pub kind: BurnInKind,
    /// Absolute ffprobe stream index when embedded.
    pub stream_index: Option<u32>,
    /// 0-based index among all subtitle streams (`si=` / `0:s:N`).
    pub subtitle_ordinal: Option<u32>,
    pub sidecar_path: Option<PathBuf>,
}

/// Embedded burn-in stream discovered by ffprobe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BurnInSubtitleStream {
    pub stream_index: u32,
    pub subtitle_ordinal: u32,
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub kind: BurnInKind,
}

impl BurnInSubtitleStream {
    pub fn track_id(&self) -> String {
        format!("e{}", self.stream_index)
    }
}

/// One serveable sidecar input for an extract job, with its captured identity.
#[derive(Debug, Clone)]
pub struct SidecarInput {
    pub track_id: String,
    pub path: PathBuf,
    pub format: String,
    /// Captured sidecar mtime and size (ADR-0010 §4). Rechecked before
    /// conversion and before the rename so a changed file never publishes
    /// under the old generation (D2B.2 acceptance 5).
    pub mtime_ms: i64,
    pub size_bytes: i64,
}

/// Captured source identity and the DB source checks for one extract run
/// (ADR-0013 §13.3).
///
/// `media_path`/`mtime_ms`/`size_bytes` are the ADR-0058 capture's media tuple.
/// Progressive work revalidates them before it converts or reads and again
/// before it publishes, so a media replacement under a running extract never
/// publishes (ADR-0013 §13.3.2, §13.5). `token_for` mints the bounded per-track
/// generation token that names the artifact directory (ADR-0013 §13.1/§13.2);
/// `None` means the track is not a serveable member of the captured source.
/// `is_current` answers whether the captured certified source still matches.
/// `reserve_revision` allocates the next immutable artifact revision from the
/// DB (ADR-0013 §13.2). `publish` commits one changed body at its exact
/// revision through the DB source compare-and-swap (ADR-0013 §13.3.4/§13.5) and
/// returns `false` when the captured source has been superseded.
pub struct ExtractSource<'a> {
    pub media_path: PathBuf,
    pub mtime_ms: i64,
    pub size_bytes: i64,
    pub token_for: &'a dyn Fn(&str) -> Option<String>,
    pub is_current: &'a dyn Fn() -> bool,
    pub reserve_revision: &'a dyn Fn() -> Option<u64>,
    pub publish: &'a dyn Fn(&str, &str, u64, SubtitleArtifactState) -> bool,
}

/// True when `err` marks a source that changed under a running extract. The
/// pool defers such a run without writing a status (D2B.2 acceptance 5).
pub fn message_is_source_changed(err: &str) -> bool {
    err.starts_with("source-changed:")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractOutcome {
    /// No serveable text tracks (embedded or sidecar).
    None,
    /// Every requested serveable track is written under its generation token.
    Ready,
    /// Some tracks landed, the rest did not (ADR-0041 Decision 8.4). The item
    /// must not claim `ready`; a later pass finishes the missing tracks and
    /// previously-good files were never touched (Decision 8.5).
    Partial { written: usize, failed: usize },
}

pub fn is_text_subtitle_codec(codec: &str) -> bool {
    let c = codec.to_ascii_lowercase();
    TEXT_SUB_CODECS.iter().any(|t| *t == c)
}

pub fn is_burn_in_codec(codec: &str) -> bool {
    let c = codec.to_ascii_lowercase();
    BURN_IN_CODECS.iter().any(|t| *t == c)
}

pub fn burn_in_kind_for_codec(codec: &str) -> Option<BurnInKind> {
    match codec.to_ascii_lowercase().as_str() {
        "ass" | "ssa" => Some(BurnInKind::Ass),
        "hdmv_pgs_subtitle" => Some(BurnInKind::Pgs),
        _ => None,
    }
}

/// ADR-0041 Decision 1: derive a subtitle stream's persisted inventory `kind`
/// from the codec name ffprobe reports. Text codecs → `Text`; ASS/SSA → `Ass`;
/// bitmap subtitle codecs (PGS, VobSub) → `Image`; anything else (including an
/// empty/absent codec name) counts as `Unknown` — never silently dropped as
/// harmless (measured library: n_unknown = 0).
pub fn subtitle_codec_kind(codec: &str) -> nightjar_db::SubtitleTrackKind {
    use nightjar_db::SubtitleTrackKind as K;
    let c = codec.to_ascii_lowercase();
    if is_text_subtitle_codec(&c) {
        K::Text
    } else if matches!(c.as_str(), "ass" | "ssa") {
        K::Ass
    } else if matches!(c.as_str(), "hdmv_pgs_subtitle" | "dvd_subtitle") {
        K::Image
    } else {
        K::Unknown
    }
}

pub fn is_serveable_sidecar_format(format: &str) -> bool {
    matches!(format.to_ascii_lowercase().as_str(), "srt" | "vtt")
}

pub fn is_burn_in_sidecar_format(format: &str) -> bool {
    matches!(format.to_ascii_lowercase().as_str(), "ass" | "ssa")
}

/// Demux one embedded ASS/SSA stream to a local `.ass` for libass burn-in.
///
/// `subtitles=<src>:si=N` re-opens the container and demuxes every cue before
/// the first frame — on a multi-GB NAS title that stalls HLS for the whole
/// demux. A local file lets `ass=` start encoding immediately after this copy.
pub fn extract_embedded_ass(src: &Path, stream_index: u32, dest: &Path) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("create ASS burn dir {}: {e}", parent.display()))?;
    }
    let tmp = dest.with_extension("tmp.ass");
    let map = format!("0:{stream_index}");
    let src_bytes = fs::metadata(src).map(|m| m.len()).unwrap_or(0);
    let started = Instant::now();
    let mut cmd = Command::new("ffmpeg");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(src)
        .args(["-map", &map, "-c:s", "copy", "-flush_packets", "1"])
        .arg(&tmp);

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "ffmpeg not found on PATH".into()
        } else {
            format!("spawn ffmpeg ASS extract for {}: {e}", src.display())
        }
    })?;

    // Session-scoped burn extract: not a library bulk reader, so the pool's
    // cancel signal (ADR-0041 Decision 8.7) never fires here.
    if let Err(e) = wait_extract_child(&mut child, ASS_BURN_EXTRACT_TIMEOUT, &|| false, || {}) {
        let _ = fs::remove_file(&tmp);
        return Err(format!(
            "ASS burn extract failed for {} stream {stream_index}: {e}",
            src.display()
        ));
    }

    let meta = fs::metadata(&tmp).map_err(|e| {
        format!(
            "ASS burn extract wrote no file for {} stream {stream_index}: {e}",
            src.display()
        )
    })?;
    if meta.len() == 0 {
        let _ = fs::remove_file(&tmp);
        return Err(format!(
            "ASS burn extract empty for {} stream {stream_index}",
            src.display()
        ));
    }
    let track_bytes = meta.len();
    fs::rename(&tmp, dest).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!(
            "publish ASS burn file {} -> {}: {e}",
            tmp.display(),
            dest.display()
        )
    })?;
    // Load-bearing for cold-path wait estimates (ADR-0018 / ADR-0019): the
    // product rolls `src_mib_per_s` into the viewer's range. Field names and
    // `info` level are the contract — not temporary instrumentation.
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let elapsed_secs = (elapsed_ms as f64 / 1000.0).max(0.001);
    let src_mib_per_s = (src_bytes as f64 / (1024.0 * 1024.0)) / elapsed_secs;
    tracing::info!(
        src = %src.display(),
        stream_index,
        dest = %dest.display(),
        src_bytes,
        track_bytes,
        elapsed_ms,
        src_mib_per_s,
        "ass_burn_extract_finished"
    );
    Ok(())
}

fn probe_subtitle_streams(src: &Path) -> Result<Vec<FfSubStream>, String> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_streams",
            "-select_streams",
            "s",
        ])
        .arg(src)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "ffprobe not found on PATH".into()
            } else {
                format!("spawn ffprobe for {}: {e}", src.display())
            }
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "ffprobe failed for {}: {}",
            src.display(),
            stderr.trim()
        ));
    }
    let parsed: FfprobeSubs = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("parse ffprobe json for {}: {e}", src.display()))?;
    Ok(parsed.streams.unwrap_or_default())
}

/// Lists text subtitle streams in `src`. Image/ASS tracks are skipped.
pub fn list_text_subtitles(src: &Path) -> Result<Vec<TextSubtitleStream>, String> {
    let mut out = Vec::new();
    for stream in probe_subtitle_streams(src)? {
        let codec = stream.codec_name.unwrap_or_default();
        if !is_text_subtitle_codec(&codec) {
            continue;
        }
        let Some(index) = stream.index else {
            continue;
        };
        let tags = stream.tags.unwrap_or_default();
        let disp = stream.disposition.unwrap_or_default();
        out.push(TextSubtitleStream {
            language: container_stream_language(tags.language),
            stream_index: index,
            codec,
            title: tags.title.filter(|s| !s.is_empty()),
            is_default: disp.default == 1,
            is_forced: disp.forced == 1,
        });
    }
    Ok(out)
}

/// Lists ASS/SSA/PGS streams that need burn-in (ADR-0018).
pub fn list_burn_in_subtitles(src: &Path) -> Result<Vec<BurnInSubtitleStream>, String> {
    let mut out = Vec::new();
    let mut ordinal = 0u32;
    for stream in probe_subtitle_streams(src)? {
        let codec = stream.codec_name.unwrap_or_default();
        let Some(index) = stream.index else {
            ordinal = ordinal.saturating_add(1);
            continue;
        };
        if let Some(kind) = burn_in_kind_for_codec(&codec) {
            let tags = stream.tags.unwrap_or_default();
            out.push(BurnInSubtitleStream {
                stream_index: index,
                subtitle_ordinal: ordinal,
                codec,
                language: container_stream_language(tags.language),
                title: tags.title.filter(|s| !s.is_empty()),
                kind,
            });
        }
        ordinal = ordinal.saturating_add(1);
    }
    Ok(out)
}

/// Derived-library subtitle store under `{data}/subs` (ADR-0013). Not a cache.
pub struct SubsStore {
    root: PathBuf,
    /// Serialises extracts so two workers never share the same item tmp paths.
    extract_lock: Mutex<()>,
}

impl SubsStore {
    pub fn new(root: PathBuf) -> Result<Self, String> {
        // A newly created root directory entry, and every newly created
        // ancestor entry above it, is made durable before anything under it is
        // published (ADR-0013 §13.3.3). An existing root is left untouched.
        create_dir_durable(&root)?;
        Ok(Self {
            root,
            extract_lock: Mutex::new(()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn item_dir(&self, item_id: i64) -> PathBuf {
        self.root.join(item_id.to_string())
    }

    /// The directory holding one immutable, generation-addressed artifact set
    /// (ADR-0013 §13.2). The generation token is minted by the DB from a
    /// certified source and is an opaque path segment here.
    pub fn generation_dir(&self, item_id: i64, generation: &str) -> PathBuf {
        self.item_dir(item_id).join(generation)
    }

    /// The finalized immutable artifact of one `(track, token, revision)`
    /// (ADR-0013 §13.2). Serving resolves this from the committed publication
    /// row; no caller constructs it from a request.
    pub fn artifact_path(
        &self,
        item_id: i64,
        generation: &str,
        track_id: &str,
        artifact_revision: u64,
    ) -> PathBuf {
        self.generation_dir(item_id, generation)
            .join(format!("{track_id}.r{artifact_revision}.vtt"))
    }

    /// Whether the finalized artifact exists with bytes.
    pub fn has_artifact(
        &self,
        item_id: i64,
        generation: &str,
        track_id: &str,
        artifact_revision: u64,
    ) -> bool {
        let path = self.artifact_path(item_id, generation, track_id, artifact_revision);
        fs::metadata(&path)
            .map(|m| m.is_file() && m.len() > 0)
            .unwrap_or(false)
    }

    /// Write and fsync one complete candidate body for `artifact_revision`
    /// without publishing it (ADR-0013 §13.3.3).
    ///
    /// The candidate is a temporary file in the generation directory. It is
    /// never serveable: only [`Self::finalize_candidate`] gives it its
    /// immutable name, and only the DB source compare-and-swap makes it
    /// visible.
    ///
    /// Creation is exclusive (`create_new`). A crash between
    /// [`Self::finalize_candidate`]'s hard link and its candidate unlink leaves
    /// a candidate that shares an inode with the finalized artifact; truncating
    /// it would corrupt committed bytes, so a collision is an error instead
    /// (ADR-0013 §13.2).
    fn write_candidate(
        &self,
        item_id: i64,
        generation: &str,
        track_id: &str,
        artifact_revision: u64,
        body: &str,
    ) -> Result<PathBuf, String> {
        let dir = self.generation_dir(item_id, generation);
        create_dir_durable(&dir)?;
        let candidate = dir.join(format!("{track_id}.r{artifact_revision}.candidate"));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
            .map_err(|e| {
                io_failure_message(
                    &format!("write subtitle candidate {}", candidate.display()),
                    &e,
                )
            })?;
        file.write_all(body.as_bytes()).map_err(|e| {
            io_failure_message(
                &format!("write subtitle candidate {}", candidate.display()),
                &e,
            )
        })?;
        file.sync_all().map_err(|e| {
            io_failure_message(
                &format!("fsync subtitle candidate {}", candidate.display()),
                &e,
            )
        })?;
        drop(file);
        Ok(candidate)
    }

    /// Finalize a candidate into its immutable artifact name without
    /// overwriting an existing final artifact, then fsync the generation
    /// directory so the new entry is durable (ADR-0013 §13.3.3).
    ///
    /// A hard link refuses an existing destination; `rename(2)` would replace
    /// it silently, which the contract forbids.
    fn finalize_candidate(&self, candidate: &Path, final_path: &Path) -> Result<(), String> {
        match fs::hard_link(candidate, final_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(candidate);
                return Err(format!(
                    "subtitle artifact {} already exists and is never overwritten",
                    final_path.display()
                ));
            }
            Err(e) => {
                let _ = fs::remove_file(candidate);
                return Err(io_failure_message(
                    &format!("finalize subtitle artifact {}", final_path.display()),
                    &e,
                ));
            }
        }
        let _ = fs::remove_file(candidate);
        if let Some(parent) = final_path.parent() {
            fsync_dir(parent)?;
        }
        Ok(())
    }

    pub fn remove_item(&self, item_id: i64) -> Result<(), String> {
        let dir = self.item_dir(item_id);
        if !dir.exists() {
            return Ok(());
        }
        fs::remove_dir_all(&dir)
            .map_err(|e| format!("remove subs for item {item_id} ({}): {e}", dir.display()))
    }

    /// Delete `subs/{id}/` directories whose id is not in `keep_ids`.
    pub fn cleanup_orphans(&self, keep_ids: &[i64]) -> Result<usize, String> {
        let mut keep: std::collections::HashSet<i64> = keep_ids.iter().copied().collect();
        let mut removed = 0usize;
        let entries = match fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => {
                return Err(format!("read subs root {}: {e}", self.root.display()));
            }
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if !ft.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|s| s.parse::<i64>().ok()) else {
                continue;
            };
            if keep.remove(&id) {
                continue;
            }
            match fs::remove_dir_all(entry.path()) {
                Ok(()) => {
                    tracing::info!(item_id = id, "removed orphan subtitle directory");
                    removed += 1;
                }
                Err(e) => tracing::warn!(
                    item_id = id,
                    path = %entry.path().display(),
                    error = %e,
                    "orphan subtitle cleanup failed"
                ),
            }
        }
        Ok(removed)
    }

    /// Finalize one track's artifact body at `artifact_revision`, the way the
    /// production pipeline does: write and fsync a candidate, then finalize it
    /// without overwrite (ADR-0013 §13.3.3).
    ///
    /// This is the same two primitives the extract uses; it exists so a caller
    /// that already holds a reserved revision (a test, or D2B.3's piggyback
    /// writer) does not need a second write path.
    pub fn publish_item_vtt(
        &self,
        item_id: i64,
        generation: &str,
        track_id: &str,
        artifact_revision: u64,
        body: &str,
    ) -> Result<(), String> {
        let candidate =
            self.write_candidate(item_id, generation, track_id, artifact_revision, body)?;
        self.finalize_candidate(
            &candidate,
            &self.artifact_path(item_id, generation, track_id, artifact_revision),
        )
    }
}

/// Join per-segment WebVTT bodies (each carries its own `WEBVTT` header) into
/// one document: the header once, then every cue block in order. Blocks that
/// are not cues (NOTE/STYLE/header lines) are dropped, matching the block
/// filter `slice_webvtt` uses, so a concatenated document slices identically.
pub fn concat_webvtt_segments(bodies: &[String]) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for body in bodies {
        let normalised = body.replace("\r\n", "\n").replace('\r', "\n");
        for block in normalised.split("\n\n") {
            let block = block.trim();
            if block.is_empty()
                || block.starts_with("WEBVTT")
                || block.starts_with("NOTE")
                || block.starts_with("STYLE")
            {
                continue;
            }
            if !block.contains("-->") {
                continue;
            }
            out.push_str(block);
            out.push_str("\n\n");
        }
    }
    out
}

fn write_webvtt(dest: &Path, body: &str) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        create_dir_durable(parent)?;
    }
    // Temp write + fsync + atomic rename per track: a reader never sees a
    // half-written WebVTT, and a crash mid-write cannot corrupt a
    // previously-good track (ADR-0041 Decision 8.4).
    let tmp = dest.with_extension("tmp.vtt");
    let mut file = fs::File::create(&tmp)
        .map_err(|e| io_failure_message(&format!("write subtitle tmp {}", tmp.display()), &e))?;
    file.write_all(body.as_bytes())
        .map_err(|e| io_failure_message(&format!("write subtitle tmp {}", tmp.display()), &e))?;
    file.sync_all()
        .map_err(|e| io_failure_message(&format!("fsync subtitle tmp {}", tmp.display()), &e))?;
    drop(file);
    fs::rename(&tmp, dest).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        io_failure_message(&format!("rename subtitle {}", dest.display()), &e)
    })?;
    // Persist the directory entry before readiness is published (ADR-0013
    // §13.3.3): without the parent fsync a crash can lose the rename even
    // though the file bytes were synced.
    if let Some(parent) = dest.parent() {
        fsync_dir(parent)?;
    }
    Ok(())
}

/// Create `dir` and every missing ancestor, fsyncing each newly created
/// directory's parent so the new entry survives a crash (ADR-0013 §13.3.3:
/// "fsync every newly created ancestor directory entry"). The leaf directory's
/// own entry for the renamed file is fsynced by [`write_webvtt`] after the
/// rename.
fn create_dir_durable(dir: &Path) -> Result<(), String> {
    if dir.is_dir() {
        return Ok(());
    }
    let mut missing: Vec<PathBuf> = Vec::new();
    let mut cursor = Some(dir);
    while let Some(path) = cursor {
        if path.is_dir() {
            break;
        }
        missing.push(path.to_path_buf());
        cursor = path.parent();
    }
    fs::create_dir_all(dir)
        .map_err(|e| io_failure_message(&format!("create subtitle dir {}", dir.display()), &e))?;
    // Shallowest first: a new directory's entry lives in its parent, so syncing
    // the parent persists that entry.
    for path in missing.iter().rev() {
        if let Some(parent) = path.parent() {
            fsync_dir(parent)?;
        }
    }
    Ok(())
}

/// fsync one directory so a completed rename survives a crash.
fn fsync_dir(dir: &Path) -> Result<(), String> {
    let handle = fs::File::open(dir)
        .map_err(|e| io_failure_message(&format!("open subtitle dir {}", dir.display()), &e))?;
    handle
        .sync_all()
        .map_err(|e| io_failure_message(&format!("fsync subtitle dir {}", dir.display()), &e))
}

fn srt_bytes_to_webvtt(bytes: &[u8]) -> String {
    srt_to_webvtt(&decode_subtitle_bytes(bytes))
}

/// `-c:s copy` for native subrip; `-c:s srt` remuxes mov_text/webvtt/text into
/// SRT packets. Never `-c:s webvtt` — that muxer was the measured bottleneck.
fn srt_encoder_for_codec(codec: &str) -> &'static str {
    match codec.to_ascii_lowercase().as_str() {
        "subrip" | "srt" => "copy",
        _ => "srt",
    }
}

/// Input for session-inline subtitle prep (plan item 2). No library extract
/// required: demux/convert into the session directory.
#[derive(Debug, Clone)]
pub struct SessionSubInput {
    pub track_id: String,
    pub codec: String,
    pub stream_index: Option<u32>,
    pub sidecar_path: Option<PathBuf>,
}

/// Write growing `subs/{trackId}/full.vtt` (+ `done` marker) under `session_dir`
/// so HLS can slice 2s WebVTT segments without scan-time pre-extraction.
pub fn prepare_session_subtitles(
    src: &Path,
    session_dir: &Path,
    tracks: &[SessionSubInput],
) -> Result<(), String> {
    if tracks.is_empty() {
        return Ok(());
    }
    let subs_root = session_dir.join("subs");
    fs::create_dir_all(&subs_root)
        .map_err(|e| format!("create session subs dir {}: {e}", subs_root.display()))?;

    let mut embedded: Vec<&SessionSubInput> = Vec::new();
    for t in tracks {
        let track_dir = subs_root.join(&t.track_id);
        fs::create_dir_all(&track_dir)
            .map_err(|e| format!("create {}: {e}", track_dir.display()))?;
        if let Some(path) = &t.sidecar_path {
            let bytes = fs::read(path).map_err(|e| {
                if io_error_is_availability(&e) {
                    format!("unavailable: read session sidecar {}: {e}", path.display())
                } else {
                    format!("read session sidecar {}: {e}", path.display())
                }
            })?;
            let body = if t.codec.eq_ignore_ascii_case("vtt") {
                let text = decode_subtitle_bytes(&bytes);
                if text.contains("WEBVTT") {
                    text
                } else {
                    format!("WEBVTT\n\n{text}")
                }
            } else {
                srt_bytes_to_webvtt(&bytes)
            };
            write_webvtt(&track_dir.join("full.vtt"), &body)?;
            touch_done(&track_dir)?;
        } else if t.stream_index.is_some() {
            embedded.push(t);
        } else {
            tracing::warn!(track_id = %t.track_id, "session subtitle track has no source");
            touch_done(&track_dir)?;
        }
    }

    if !embedded.is_empty() {
        demux_embedded_into_session(src, &subs_root, &embedded)?;
    }
    Ok(())
}

fn touch_done(track_dir: &Path) -> Result<(), String> {
    fs::write(track_dir.join("done"), b"")
        .map_err(|e| format!("write done marker {}: {e}", track_dir.display()))
}

fn demux_embedded_into_session(
    src: &Path,
    subs_root: &Path,
    tracks: &[&SessionSubInput],
) -> Result<(), String> {
    let mut tmp_srts: Vec<(String, PathBuf)> = Vec::with_capacity(tracks.len());
    let mut cmd = Command::new("ffmpeg");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(src);
    for t in tracks {
        let idx = t.stream_index.expect("embedded");
        let tmp = subs_root.join(&t.track_id).join("tmp.srt");
        let map = format!("0:{idx}");
        let encoder = srt_encoder_for_codec(&t.codec);
        // Growing files need bytes on disk promptly so progressive slicing
        // can read them mid-demux (Jellyfin's append-only VTT lesson).
        cmd.args([
            "-map",
            &map,
            "-c:s",
            encoder,
            "-flush_packets",
            "1",
            "-f",
            "srt",
        ])
        .arg(&tmp);
        tmp_srts.push((t.track_id.clone(), tmp));
    }

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "ffmpeg not found on PATH".into()
        } else {
            format!("spawn ffmpeg session subs for {}: {e}", src.display())
        }
    })?;

    // Per-file budget from source size (ADR-0041 Decision 8.1), same function
    // as the library extract path — one schedule, Rule 4.11.
    let src_bytes = fs::metadata(src).map(|m| m.len()).unwrap_or(0);
    let budget = extract_timeout_budget(src_bytes);
    tracing::info!(
        path = %src.display(),
        src_bytes,
        timeout_budget_ms = budget.as_millis() as u64,
        "session subtitle demux timeout budget"
    );

    let mut last_sizes: HashMap<String, u64> = HashMap::new();
    if let Err(e) = wait_extract_child(&mut child, budget, &|| false, || {
        for (track_id, tmp) in &tmp_srts {
            let Ok(meta) = fs::metadata(tmp) else {
                continue;
            };
            let len = meta.len();
            if len == 0 {
                continue;
            }
            let prev = last_sizes.get(track_id).copied().unwrap_or(0);
            if len <= prev {
                continue;
            }
            last_sizes.insert(track_id.clone(), len);
            let Ok(bytes) = fs::read(tmp) else {
                continue;
            };
            let body = srt_bytes_to_webvtt(&bytes);
            if body.trim() == "WEBVTT" {
                continue;
            }
            let dest = subs_root.join(track_id).join("full.vtt");
            let _ = write_webvtt(&dest, &body);
        }
    }) {
        for (_, tmp) in &tmp_srts {
            let _ = fs::remove_file(tmp);
        }
        return Err(format!(
            "ffmpeg session subtitle demux failed for {}: {e}",
            src.display()
        ));
    }

    for (track_id, tmp) in tmp_srts {
        let track_dir = subs_root.join(&track_id);
        let dest = track_dir.join("full.vtt");
        let bytes = fs::read(&tmp)
            .map_err(|e| format!("read session srt for {track_id} ({}): {e}", tmp.display()))?;
        let body = srt_bytes_to_webvtt(&bytes);
        write_webvtt(&dest, &body)?;
        let _ = fs::remove_file(&tmp);
        touch_done(&track_dir)?;
    }
    Ok(())
}

/// Extract/convert every serveable track for one item (ADR-0013). One FFmpeg
/// demux fills all embedded text tracks; sidecars convert in-process.
/// Embedded demux publishes growing WebVTT so first play can show cues before
/// the full demux finishes (ADR-0013 §11).
///
/// `should_cancel` is the library reachability signal (ADR-0014): when it
/// turns true the demux child is killed and the run reports `unavailable`,
/// never `ready` (ADR-0041 Decision 8.7 — cancel in flight, not just block
/// new starts). The same signal already gates job *start* at the pool.
#[allow(clippy::too_many_arguments)]
pub fn extract_item_subtitles(
    store: &SubsStore,
    item_id: i64,
    source: &ExtractSource<'_>,
    embedded: &[TextSubtitleStream],
    sidecars: &[SidecarInput],
    should_cancel: &dyn Fn() -> bool,
) -> Result<ExtractOutcome, String> {
    let _guard = store
        .extract_lock
        .lock()
        .map_err(|_| "subtitle extract lock poisoned".to_string())?;
    extract_item_subtitles_inner(store, item_id, source, embedded, sidecars, should_cancel)
}

#[allow(clippy::too_many_arguments)]
fn extract_item_subtitles_inner(
    store: &SubsStore,
    item_id: i64,
    source: &ExtractSource<'_>,
    embedded: &[TextSubtitleStream],
    sidecars: &[SidecarInput],
    should_cancel: &dyn Fn() -> bool,
) -> Result<ExtractOutcome, String> {
    ensure_free_space(store.root())?;
    let src = source.media_path.as_path();

    let serveable_sidecars: Vec<&SidecarInput> = sidecars
        .iter()
        .filter(|s| is_serveable_sidecar_format(&s.format))
        .collect();
    // The embedded inventory comes from the caller's certified probe snapshot,
    // never from a fresh probe of the source (D2B.2 acceptance 1).
    if embedded.is_empty() && serveable_sidecars.is_empty() {
        return Ok(ExtractOutcome::None);
    }

    // ADR-0013 §13.3.2: validate the captured physical and DB source before
    // extraction. A changed or unreadable source defers the whole run.
    check_media_unchanged(src, source)?;
    check_source_current(source)?;

    // Embedded text tracks of one certified snapshot share one token
    // (ADR-0013 §13.1), so they share one immutable generation directory.
    let embedded_token = match embedded.first() {
        Some(first) => Some((source.token_for)(&first.track_id()).ok_or_else(|| {
            "source-changed: embedded track is not a member of the captured source".to_string()
        })?),
        None => None,
    };

    // This run's finalized bodies, keyed by track id. An unchanged body is
    // never rewritten and never re-finalized: partial-to-complete may reference
    // the identical finalized bytes (ADR-0013 §13.2).
    let mut finalized: HashMap<String, (u64, String)> = HashMap::new();

    // The prior generation is NOT wiped up front: a pass that fails must not
    // delete previously-good tracks (ADR-0041 Decision 8.5), and every artifact
    // revision is immutable (ADR-0013 §13.2).
    if let Some(token) = &embedded_token {
        create_dir_durable(&store.generation_dir(item_id, token))?;
    }

    // Per-file timeout budget from source size at the measured 55 MiB/s rate
    // (ADR-0041 Decision 8.1); the old fixed 300 s constant is gone.
    let src_bytes = fs::metadata(src).map(|m| m.len()).unwrap_or(0);
    let budget = extract_timeout_budget(src_bytes);
    tracing::info!(
        path = %src.display(),
        src_bytes,
        timeout_budget_ms = budget.as_millis() as u64,
        embedded_token = embedded_token.as_deref().unwrap_or("-"),
        "subtitle extract timeout budget"
    );

    let mut written = 0usize;
    let mut failed = 0usize;
    if let Some(token) = &embedded_token {
        let refs: Vec<&TextSubtitleStream> = embedded.iter().collect();
        let (w, f) = extract_embedded_srt_batch(
            store,
            item_id,
            token,
            &refs,
            budget,
            should_cancel,
            source,
            &mut finalized,
        )?;
        written += w;
        failed += f;
    }

    let mut first_sidecar_err: Option<String> = None;
    for s in serveable_sidecars {
        if should_cancel() {
            return Err("unavailable: subtitle extract cancelled (library unreachable)".into());
        }
        let Some(token) = (source.token_for)(&s.track_id) else {
            failed += 1;
            continue;
        };
        match write_sidecar_webvtt(store, item_id, &token, s, source, &mut finalized) {
            Ok(()) => written += 1,
            // A sidecar that changed under the run invalidates the whole
            // generation; defer without publishing anything (D2B.2 acceptance 5).
            Err(e) if message_is_source_changed(&e) => return Err(e),
            Err(e) => {
                failed += 1;
                if first_sidecar_err.is_none() {
                    first_sidecar_err = Some(e);
                } else {
                    tracing::warn!(
                        item_id,
                        track_id = %s.track_id,
                        error = %e,
                        "sidecar subtitle extract failed"
                    );
                }
            }
        }
    }

    if written == 0 {
        return Err(first_sidecar_err.unwrap_or_else(|| {
            format!(
                "subtitle extract produced no usable tracks for {}",
                src.display()
            )
        }));
    }

    if failed > 0 {
        // Per-track partial success: keep the item eligible for a later pass
        // and delete nothing (ADR-0041 Decision 8.4 / 8.5).
        tracing::warn!(item_id, written, failed, "subtitle extract partial");
        return Ok(ExtractOutcome::Partial { written, failed });
    }

    // No sweep: every artifact revision is immutable and unreferenced revisions
    // are never served (ADR-0013 §13.2/§13.7), so a full success has nothing to
    // delete and cannot touch an unrelated track.
    Ok(ExtractOutcome::Ready)
}

/// Publish one changed body as a per-track artifact (ADR-0013 §13.3.3,
/// §13.3.4, §13.5).
///
/// Every changed partial or complete body: validate the captured physical and
/// DB source, reserve an artifact revision, write and fsync a candidate,
/// revalidate, finalize without overwrite, fsync the directory, then commit the
/// reference through the DB source compare-and-swap at that exact revision. A
/// validation or CAS failure preserves the previous reference and bytes and
/// leaves only an unreferenced candidate.
///
/// A body identical to the one this run already finalized is not rewritten: a
/// partial-to-complete transition references the identical finalized bytes.
///
/// The last physical identity check runs *after* the last potentially blocking
/// DB check, immediately before the finalize or the CAS. A source that changes
/// while the database is consulted is therefore still rejected before any
/// finalized or committed bytes can name it. `sidecar` is the track's own
/// source when the track is a sidecar, so both sources are covered.
#[allow(clippy::too_many_arguments)]
fn publish_artifact(
    store: &SubsStore,
    item_id: i64,
    source: &ExtractSource<'_>,
    track_id: &str,
    token: &str,
    body: &str,
    state: SubtitleArtifactState,
    sidecar: Option<&SidecarInput>,
    finalized: &mut HashMap<String, (u64, String)>,
) -> Result<(), String> {
    check_source_physical(source, sidecar)?;
    check_source_current(source)?;
    let revision = match finalized.get(track_id) {
        Some((revision, previous)) if previous == body => {
            // Identical bytes are referenced, never rewritten. Revalidate the
            // DB source and then the physical source before the CAS.
            check_source_current(source)?;
            check_source_physical(source, sidecar)?;
            *revision
        }
        _ => {
            let Some(revision) = (source.reserve_revision)() else {
                return Err(format!(
                    "source-changed: no artifact revision could be reserved for {track_id}"
                ));
            };
            let candidate = store.write_candidate(item_id, token, track_id, revision, body)?;
            // The last DB check, then the last physical check: a source that
            // moved on while the database was consulted must not publish, and
            // the candidate stays unreferenced (ADR-0013 §13.3.2, §13.5).
            if let Err(e) =
                check_source_current(source).and_then(|_| check_source_physical(source, sidecar))
            {
                let _ = fs::remove_file(&candidate);
                return Err(e);
            }
            store.finalize_candidate(
                &candidate,
                &store.artifact_path(item_id, token, track_id, revision),
            )?;
            finalized.insert(track_id.to_string(), (revision, body.to_string()));
            revision
        }
    };
    // The bytes become serveable only through the committed per-track reference
    // (ADR-0013 §13.3.4). A CAS that rejects leaves them unreferenced and the
    // run defers.
    if !(source.publish)(track_id, token, revision, state) {
        return Err(format!(
            "source-changed: subtitle artifact for {track_id} was superseded before publication"
        ));
    }
    Ok(())
}

/// Path of the committed artifact, or an error if it is missing. The revision
/// comes from the committed publication row, never from the request
/// (ADR-0013 §13.2).
pub fn stored_webvtt(
    store: &SubsStore,
    item_id: i64,
    generation: &str,
    track_id: &str,
    artifact_revision: u64,
) -> Result<PathBuf, String> {
    let path = store.artifact_path(item_id, generation, track_id, artifact_revision);
    if store.has_artifact(item_id, generation, track_id, artifact_revision) {
        Ok(path)
    } else {
        Err(format!(
            "subtitle {track_id} for item {item_id} is not extracted yet"
        ))
    }
}

fn write_sidecar_webvtt(
    store: &SubsStore,
    item_id: i64,
    generation: &str,
    sidecar: &SidecarInput,
    source: &ExtractSource<'_>,
    finalized: &mut HashMap<String, (u64, String)>,
) -> Result<(), String> {
    // Recheck the captured path/mtime/size before conversion, so a changed file
    // never converts stale bytes (ADR-0013 §13.3.2). The publication path
    // revalidates the physical and DB source again before it finalizes. The
    // durable generation is checked by the DB CAS at publication; no digest is
    // computed here.
    check_sidecar_unchanged(sidecar)?;
    let bytes = fs::read(&sidecar.path).map_err(|e| {
        if io_error_is_availability(&e) {
            format!(
                "unavailable: read sidecar subtitle {}: {e}",
                sidecar.path.display()
            )
        } else {
            format!("read sidecar subtitle {}: {e}", sidecar.path.display())
        }
    })?;
    let body = if sidecar.format.eq_ignore_ascii_case("vtt") {
        let text = decode_subtitle_bytes(&bytes);
        if text.contains("WEBVTT") {
            text
        } else {
            format!("WEBVTT\n\n{text}")
        }
    } else {
        srt_bytes_to_webvtt(&bytes)
    };
    check_sidecar_unchanged(sidecar)?;
    // A sidecar conversion completes in-process, so the track's bytes are
    // complete; the publication path revalidates media *and* this sidecar.
    publish_artifact(
        store,
        item_id,
        source,
        &sidecar.track_id,
        generation,
        &body,
        SubtitleArtifactState::Complete,
        Some(sidecar),
        finalized,
    )
}

/// Validate every physical source a track's bytes came from: the media file,
/// and the track's own sidecar when it has one (ADR-0013 §13.3.2).
fn check_source_physical(
    source: &ExtractSource<'_>,
    sidecar: Option<&SidecarInput>,
) -> Result<(), String> {
    check_media_unchanged(&source.media_path, source)?;
    if let Some(sidecar) = sidecar {
        check_sidecar_unchanged(sidecar)?;
    }
    Ok(())
}

/// Compare the media file's current mtime/size with the certified capture
/// (ADR-0013 §13.3.2). A change marks the run stale; an inaccessible source is
/// the availability failure ADR-0058 already defines.
fn check_media_unchanged(src: &Path, source: &ExtractSource<'_>) -> Result<(), String> {
    let meta = match fs::metadata(src) {
        Ok(meta) => meta,
        Err(e) => {
            return Err(format!("unavailable: stat source {}: {e}", src.display()));
        }
    };
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64);
    if mtime_ms != Some(source.mtime_ms) || meta.len() as i64 != source.size_bytes {
        return Err(format!(
            "source-changed: source {} changed during extract",
            src.display()
        ));
    }
    Ok(())
}

/// Whether the captured certified source is still the current one
/// (ADR-0013 §13.3.3, §13.5). Stale work is rejected before it writes.
fn check_source_current(source: &ExtractSource<'_>) -> Result<(), String> {
    if (source.is_current)() {
        Ok(())
    } else {
        Err("source-changed: certified source is no longer current".to_string())
    }
}

/// Compare a sidecar's current mtime/size with the captured tuple. A change (or
/// a lost stat) marks the whole run stale so nothing publishes under the old
/// generation.
fn check_sidecar_unchanged(sidecar: &SidecarInput) -> Result<(), String> {
    let meta = match fs::metadata(&sidecar.path) {
        Ok(meta) => meta,
        Err(e) if io_error_is_availability(&e) => {
            return Err(format!(
                "unavailable: stat sidecar {}: {e}",
                sidecar.path.display()
            ));
        }
        Err(e) => {
            return Err(format!(
                "source-changed: stat sidecar {}: {e}",
                sidecar.path.display()
            ));
        }
    };
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64);
    if mtime_ms != Some(sidecar.mtime_ms) || meta.len() as i64 != sidecar.size_bytes {
        return Err(format!(
            "source-changed: sidecar {} changed during extract",
            sidecar.path.display()
        ));
    }
    Ok(())
}

/// Demux every embedded text stream in one ffmpeg run, then publish each
/// track's WebVTT independently (ADR-0041 Decision 8.4: one bad stream must
/// not lose tracks that completed). Returns (written, failed) track counts;
/// an `Err` means the demux failed AND no track produced usable output.
#[allow(clippy::too_many_arguments)]
fn extract_embedded_srt_batch(
    store: &SubsStore,
    item_id: i64,
    generation: &str,
    streams: &[&TextSubtitleStream],
    timeout: Duration,
    should_cancel: &dyn Fn() -> bool,
    source: &ExtractSource<'_>,
    finalized: &mut HashMap<String, (u64, String)>,
) -> Result<(usize, usize), String> {
    let src = source.media_path.as_path();
    let generation_dir = store.generation_dir(item_id, generation);
    let mut tmp_srts: Vec<(u32, PathBuf)> = Vec::with_capacity(streams.len());
    let mut cmd = Command::new("ffmpeg");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(src);
    for s in streams {
        let tmp = generation_dir.join(format!("e{}.tmp.srt", s.stream_index));
        let map = format!("0:{}", s.stream_index);
        let encoder = srt_encoder_for_codec(&s.codec);
        // Growing files need bytes on disk promptly so partial-publish reads
        // see them mid-demux (Jellyfin's append-only VTT lesson).
        cmd.args([
            "-map",
            &map,
            "-c:s",
            encoder,
            "-flush_packets",
            "1",
            "-f",
            "srt",
        ])
        .arg(&tmp);
        tmp_srts.push((s.stream_index, tmp));
    }

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "ffmpeg not found on PATH".into()
        } else {
            format!("spawn ffmpeg for {}: {e}", src.display())
        }
    })?;

    let mut last_sizes: HashMap<u32, u64> = HashMap::new();
    let demux = wait_extract_child(&mut child, timeout, should_cancel, || {
        publish_growing_srts(
            store,
            item_id,
            generation,
            &tmp_srts,
            &mut last_sizes,
            source,
            finalized,
        );
    });

    // Salvage each track's tmp independently. A failed demux leaves whatever
    // each track produced before the abort; tracks that produced nothing
    // count as failed and the rest land (Decision 8.4). Only a demux that
    // reached successful EOF may publish `Complete`; an aborted or failed run
    // salvages cues as `Partial` (ADR-0013 §13.4).
    let salvage_state = if demux.is_ok() {
        SubtitleArtifactState::Complete
    } else {
        SubtitleArtifactState::Partial
    };
    let mut written = 0usize;
    let mut failed = 0usize;
    for (stream_index, tmp_srt) in &tmp_srts {
        match salvage_track_vtt(
            store,
            item_id,
            generation,
            *stream_index,
            tmp_srt,
            salvage_state,
            source,
            finalized,
        ) {
            Ok(()) => written += 1,
            Err(e) => {
                failed += 1;
                tracing::warn!(
                    item_id,
                    stream_index = *stream_index,
                    error = %e,
                    "embedded subtitle track extract failed"
                );
            }
        }
    }
    for (_, tmp) in &tmp_srts {
        let _ = fs::remove_file(tmp);
    }

    if written == 0 {
        let msg = match &demux {
            Err(e) => e.clone(),
            Ok(()) => format!(
                "subtitle extract produced no usable tracks for {}",
                src.display()
            ),
        };
        return Err(msg);
    }
    if let Err(e) = demux {
        // A cancelled or unavailable demux never reports a partial success:
        // the salvaged cues stay `partial` and the run still reports the
        // cancellation or unavailability, so the pool stamps availability
        // instead of treating the aborted run as a landed result
        // (ADR-0013 §13.4). Any other ffmpeg failure is a per-track salvage
        // outcome (ADR-0041 Decision 8.4).
        if e.starts_with("unavailable:") {
            return Err(e);
        }
        tracing::warn!(
            path = %src.display(),
            error = %e,
            "subtitle demux failed after per-track salvage"
        );
    }
    Ok((written, failed))
}

/// Publish one embedded track's WebVTT from its demux tmp. A tmp with no cue
/// text (empty stream, or a stream whose packets were never reached) fails the
/// track without touching any other file. `state` is `Complete` only when the
/// demux reached successful EOF; a salvaged aborted run is `Partial`.
#[allow(clippy::too_many_arguments)]
fn salvage_track_vtt(
    store: &SubsStore,
    item_id: i64,
    generation: &str,
    stream_index: u32,
    tmp_srt: &Path,
    state: SubtitleArtifactState,
    source: &ExtractSource<'_>,
    finalized: &mut HashMap<String, (u64, String)>,
) -> Result<(), String> {
    let bytes = fs::read(tmp_srt).map_err(|e| {
        format!(
            "read extracted srt for stream {stream_index} ({}): {e}",
            tmp_srt.display()
        )
    })?;
    let body = srt_bytes_to_webvtt(&bytes);
    if body.trim() == "WEBVTT" {
        return Err(format!(
            "no cue text in extracted srt for stream {stream_index}"
        ));
    }
    let track_id = format!("e{stream_index}");
    // The publication path revalidates the captured physical and DB source,
    // finalizes without overwrite and CASes the reference (ADR-0013 §13.3).
    // When the body is identical to this run's last progressive body, the
    // finalized bytes are referenced again and never rewritten.
    publish_artifact(
        store, item_id, source, &track_id, generation, &body, state, None, finalized,
    )
}

#[allow(clippy::too_many_arguments)]
fn publish_growing_srts(
    store: &SubsStore,
    item_id: i64,
    generation: &str,
    tmp_srts: &[(u32, PathBuf)],
    last_sizes: &mut HashMap<u32, u64>,
    source: &ExtractSource<'_>,
    finalized: &mut HashMap<String, (u64, String)>,
) {
    // A source that moved on invalidates every remaining progressive write
    // (ADR-0013 §13.5): stale work is rejected by the same checks the final
    // publication CAS uses, so it cannot become ready for a newer generation.
    if check_source_current(source).is_err()
        || check_media_unchanged(&source.media_path, source).is_err()
    {
        return;
    }
    for (stream_index, tmp_srt) in tmp_srts {
        let Ok(meta) = fs::metadata(tmp_srt) else {
            continue;
        };
        let len = meta.len();
        if len == 0 {
            continue;
        }
        let prev = last_sizes.get(stream_index).copied().unwrap_or(0);
        if len <= prev {
            continue;
        }
        last_sizes.insert(*stream_index, len);
        let Ok(bytes) = fs::read(tmp_srt) else {
            continue;
        };
        // Trailing incomplete cue is skipped by srt_to_webvtt when timing/text
        // is truncated mid-block.
        let body = srt_bytes_to_webvtt(&bytes);
        if body.trim() == "WEBVTT" {
            continue;
        }
        let track_id = format!("e{stream_index}");
        // The committed per-track reference is what makes the growing bytes
        // serveable; the DB CAS rejects a superseded source (ADR-0013 §13.5).
        // When it rejects, the run stops publishing: nothing this worker writes
        // may become visible for the newer source.
        if let Err(e) = publish_artifact(
            store,
            item_id,
            source,
            &track_id,
            generation,
            &body,
            SubtitleArtifactState::Partial,
            None,
            finalized,
        ) {
            tracing::info!(
                item_id,
                track_id = %track_id,
                error = %e,
                "progressive subtitle publication stopped"
            );
            return;
        }
    }
}

fn wait_extract_child(
    child: &mut std::process::Child,
    timeout: Duration,
    should_cancel: &dyn Fn() -> bool,
    mut on_tick: impl FnMut(),
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let mut next_progress = Instant::now();
    loop {
        // Cancel wins over a just-completed demux: once the library is
        // unreachable the run is aborted and never reported done (ADR-0041
        // Decision 8.7). Stamped "unavailable:" so the pool's single
        // classifier (ADR-0014) routes it to `unavailable`, never `error`.
        if should_cancel() {
            let _ = child.kill();
            let _ = child.wait();
            return Err("unavailable: subtitle extract cancelled (library unreachable)".into());
        }
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                let err = child
                    .stderr
                    .as_mut()
                    .and_then(|s| {
                        let mut buf = String::new();
                        s.read_to_string(&mut buf).ok()?;
                        Some(buf)
                    })
                    .unwrap_or_default();
                let err = err.trim();
                if err.is_empty() {
                    return Err(format!("ffmpeg exited {status}"));
                }
                return Err(format!("ffmpeg exited {status}: {err}"));
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                // "unavailable:" stamps the timeout as mount/IO absence, not a
                // corrupt file, so the pool's single classifier (ADR-0014,
                // ADR-0041 Decision 8.2) routes it to `unavailable`, never
                // `error`.
                return Err(format!("unavailable: ffmpeg timed out after {timeout:?}"));
            }
            Ok(None) => {
                if Instant::now() >= next_progress {
                    on_tick();
                    next_progress = Instant::now() + PROGRESS_TICK;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("wait: {e}")),
        }
    }
}

fn ensure_free_space(path: &Path) -> Result<(), String> {
    let available = available_bytes(path)?;
    if available < MIN_FREE_BYTES {
        return Err(format!(
            "subtitle extract refused: data volume has {available} free bytes; need at least {MIN_FREE_BYTES}"
        ));
    }
    Ok(())
}

/// Free bytes on the volume containing `path`, via `df -k` (no new crate).
fn available_bytes(path: &Path) -> Result<u64, String> {
    let probe = if path.exists() {
        path.to_path_buf()
    } else {
        path.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    };
    let output = Command::new("df")
        .args(["-k"])
        .arg(&probe)
        .output()
        .map_err(|e| format!("df for {}: {e}", probe.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "df failed for {}: {}",
            probe.display(),
            stderr.trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Header + one data line; Available is the 4th field on both macOS and Linux.
    let line = stdout
        .lines()
        .nth(1)
        .ok_or_else(|| format!("df produced no data for {}", probe.display()))?;
    let avail_k = line
        .split_whitespace()
        .nth(3)
        .ok_or_else(|| format!("df line missing available column: {line}"))?
        .parse::<u64>()
        .map_err(|e| format!("parse df available: {e}"))?;
    Ok(avail_k.saturating_mul(1024))
}

#[derive(Debug, Deserialize)]
struct FfprobeSubs {
    streams: Option<Vec<FfSubStream>>,
}

#[derive(Debug, Deserialize)]
struct FfSubStream {
    index: Option<u32>,
    codec_name: Option<String>,
    disposition: Option<FfSubDisposition>,
    tags: Option<FfTags>,
}

#[derive(Debug, Default, Deserialize)]
struct FfSubDisposition {
    #[serde(default)]
    default: u8,
    #[serde(default)]
    forced: u8,
}

#[derive(Debug, Default, Deserialize)]
struct FfTags {
    language: Option<String>,
    title: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ffmpeg_available() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn skip_without_ffmpeg() -> bool {
        if ffmpeg_available() {
            return false;
        }
        if std::env::var_os("NIGHTJAR_TEST_REQUIRE_FFMPEG").is_some() {
            panic!("ffmpeg required (NIGHTJAR_TEST_REQUIRE_FFMPEG set) but not on PATH");
        }
        eprintln!("skipping: ffmpeg not on PATH");
        true
    }

    fn skip_without_fixture(path: &Path) -> bool {
        if path.is_file() {
            return false;
        }
        if std::env::var_os("NIGHTJAR_TEST_REQUIRE_FIXTURES").is_some() {
            panic!(
                "fixture required (NIGHTJAR_TEST_REQUIRE_FIXTURES set) but missing: {}",
                path.display()
            );
        }
        eprintln!("skipping: missing {}", path.display());
        true
    }

    /// Opaque generation token used by these store-level tests. Production
    /// mints it from a certified source.
    const GEN: &str = "g1";

    /// Test double for the DB half of the publication contract (ADR-0013
    /// §13.2/§13.3.4): a monotonic per-run artifact-revision allocator and a
    /// publication compare-and-swap that records every call.
    #[derive(Default)]
    struct Publications {
        next_revision: std::cell::Cell<u64>,
        accepted: std::cell::Cell<bool>,
        calls: Mutex<Vec<(String, String, u64, SubtitleArtifactState)>>,
        /// The last revision each track published, so `vtt`/`has` can resolve
        /// the immutable filename without threading it through every call.
        latest: Mutex<HashMap<String, u64>>,
    }

    impl Publications {
        fn new() -> Self {
            Self {
                accepted: std::cell::Cell::new(true),
                ..Self::default()
            }
        }

        /// A double whose CAS rejects every call, as a superseded source does.
        fn rejecting() -> Self {
            let publications = Self::new();
            publications.accepted.set(false);
            publications
        }

        fn reserve(&self) -> Option<u64> {
            let next = self.next_revision.get() + 1;
            self.next_revision.set(next);
            Some(next)
        }

        fn publish(
            &self,
            track_id: &str,
            token: &str,
            revision: u64,
            state: SubtitleArtifactState,
        ) -> bool {
            self.calls.lock().unwrap().push((
                track_id.to_string(),
                token.to_string(),
                revision,
                state,
            ));
            if !self.accepted.get() {
                return false;
            }
            self.latest
                .lock()
                .unwrap()
                .insert(track_id.to_string(), revision);
            true
        }

        fn calls(&self) -> Vec<(String, String, u64, SubtitleArtifactState)> {
            self.calls.lock().unwrap().clone()
        }

        fn latest_revision(&self, track_id: &str) -> u64 {
            self.latest
                .lock()
                .unwrap()
                .get(track_id)
                .copied()
                .unwrap_or(1)
        }
    }

    thread_local! {
        /// The publication double the last `extract` call in this thread used,
        /// so `vtt`/`has` can resolve the immutable filename the run committed
        /// without threading a revision through every call site.
        static LAST_PUBLICATIONS: std::cell::RefCell<std::rc::Rc<Publications>> =
            std::cell::RefCell::new(std::rc::Rc::new(Publications::new()));
    }

    /// Test-only extract that discovers embedded tracks from the file. The
    /// production path always supplies the certified snapshot's inventory
    /// (D2B.2 acceptance 1); this keeps the store-level tests focused. The
    /// captured media identity comes from the file and the DB source check is
    /// always current.
    fn extract(
        store: &SubsStore,
        item_id: i64,
        src: &Path,
        sidecars: &[SidecarInput],
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<ExtractOutcome, String> {
        let embedded = list_text_subtitles(src).unwrap_or_default();
        let publications = std::rc::Rc::new(Publications::new());
        LAST_PUBLICATIONS.with(|slot| *slot.borrow_mut() = publications.clone());
        extract_with_publications(
            store,
            item_id,
            src,
            &embedded,
            sidecars,
            should_cancel,
            || true,
            &publications,
        )
    }

    /// The full extract against an explicit publication double.
    #[allow(clippy::too_many_arguments)]
    fn extract_with_publications(
        store: &SubsStore,
        item_id: i64,
        src: &Path,
        embedded: &[TextSubtitleStream],
        sidecars: &[SidecarInput],
        should_cancel: &dyn Fn() -> bool,
        is_current: impl Fn() -> bool,
        publications: &Publications,
    ) -> Result<ExtractOutcome, String> {
        let meta = fs::metadata(src).unwrap();
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let token_for = |_track: &str| Some(GEN.to_string());
        let reserve = || publications.reserve();
        let publish = |track: &str, token: &str, revision: u64, state: SubtitleArtifactState| {
            publications.publish(track, token, revision, state)
        };
        let source = ExtractSource {
            media_path: src.to_path_buf(),
            mtime_ms,
            size_bytes: meta.len() as i64,
            token_for: &token_for,
            is_current: &is_current,
            reserve_revision: &reserve,
            publish: &publish,
        };
        extract_item_subtitles(store, item_id, &source, embedded, sidecars, should_cancel)
    }

    /// The final artifact path for the revision the last `extract` in this
    /// thread committed for `track_id`.
    fn vtt(store: &SubsStore, item_id: i64, track_id: &str) -> PathBuf {
        let revision = LAST_PUBLICATIONS.with(|p| p.borrow().latest_revision(track_id));
        store.artifact_path(item_id, GEN, track_id, revision)
    }

    fn has(store: &SubsStore, item_id: i64, track_id: &str) -> bool {
        let revision = LAST_PUBLICATIONS.with(|p| p.borrow().latest_revision(track_id));
        store.has_artifact(item_id, GEN, track_id, revision)
    }

    /// Whether a candidate file exists for this item/generation.
    ///
    /// The publication path writes its candidate before the last DB check and
    /// the last physical check, so a test uses this as the rendezvous that
    /// proves a source mutation landed *after* candidate creation.
    fn candidate_exists(store: &SubsStore, item_id: i64, generation: &str) -> bool {
        fs::read_dir(store.generation_dir(item_id, generation))
            .map(|entries| {
                entries.flatten().any(|entry| {
                    entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "candidate")
                })
            })
            .unwrap_or(false)
    }

    /// The captured media identity of a real file on disk, as the ADR-0058
    /// snapshot records it.
    fn capture_media(path: &Path) -> (PathBuf, i64, i64) {
        let meta = fs::metadata(path).unwrap();
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        (path.to_path_buf(), mtime_ms, meta.len() as i64)
    }

    /// Captured sidecar input from a real file on disk.
    fn sidecar(track_id: &str, path: PathBuf, format: &str) -> SidecarInput {
        let meta = fs::metadata(&path).unwrap();
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        SidecarInput {
            track_id: track_id.into(),
            path,
            format: format.into(),
            mtime_ms,
            size_bytes: meta.len() as i64,
        }
    }

    #[test]
    fn text_codec_allowlist() {
        assert!(is_text_subtitle_codec("subrip"));
        assert!(is_text_subtitle_codec("SRT"));
        assert!(is_text_subtitle_codec("mov_text"));
        assert!(!is_text_subtitle_codec("ass"));
        assert!(!is_text_subtitle_codec("hdmv_pgs_subtitle"));
        assert!(is_burn_in_codec("ass"));
        assert!(is_burn_in_codec("ssa"));
        assert!(is_burn_in_codec("hdmv_pgs_subtitle"));
        assert_eq!(burn_in_kind_for_codec("ass"), Some(BurnInKind::Ass));
        assert_eq!(
            burn_in_kind_for_codec("hdmv_pgs_subtitle"),
            Some(BurnInKind::Pgs)
        );
        assert!(!is_burn_in_sidecar_format("srt"));
        assert!(is_burn_in_sidecar_format("ass"));
    }

    #[test]
    fn subtitle_codec_kind_covers_all_ffprobe_codecs() {
        use nightjar_db::SubtitleTrackKind as K;
        assert_eq!(subtitle_codec_kind("subrip"), K::Text);
        assert_eq!(subtitle_codec_kind("srt"), K::Text);
        assert_eq!(subtitle_codec_kind("mov_text"), K::Text);
        assert_eq!(subtitle_codec_kind("webvtt"), K::Text);
        assert_eq!(subtitle_codec_kind("text"), K::Text);
        assert_eq!(subtitle_codec_kind("ass"), K::Ass);
        assert_eq!(subtitle_codec_kind("SSA"), K::Ass);
        assert_eq!(subtitle_codec_kind("hdmv_pgs_subtitle"), K::Image);
        assert_eq!(subtitle_codec_kind("dvd_subtitle"), K::Image);
        // Unrecognised codecs are counted, never silently dropped (ADR-0041
        // Decision 1: an absent/unmapped codec name is exactly the unknown case).
        assert_eq!(subtitle_codec_kind("dvb_subtitle"), K::Unknown);
        assert_eq!(subtitle_codec_kind(""), K::Unknown);
    }

    #[test]
    fn lists_ass_and_pgs_corpus_as_burn_in() {
        if skip_without_ffmpeg() {
            return;
        }
        let ass = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files/h264_aac_ass_mkv.mkv");
        let streams = list_burn_in_subtitles(&ass).expect("list ass");
        assert!(
            streams.iter().any(|s| s.kind == BurnInKind::Ass),
            "{streams:?}"
        );
        assert!(list_text_subtitles(&ass).unwrap().is_empty());

        let pgs = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files/h264_aac_pgs_mkv.mkv");
        let streams = list_burn_in_subtitles(&pgs).expect("list pgs");
        assert!(
            streams.iter().any(|s| s.kind == BurnInKind::Pgs),
            "{streams:?}"
        );
    }

    #[test]
    fn embedded_track_id() {
        let s = TextSubtitleStream {
            stream_index: 2,
            codec: "subrip".into(),
            language: Some("en".into()),
            title: None,
            is_default: false,
            is_forced: false,
        };
        assert_eq!(s.track_id(), "e2");
    }

    #[test]
    fn vtt_path_keys_on_item_and_track_not_media_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = SubsStore::new(dir.path().to_path_buf()).unwrap();
        let p = vtt(&store, 42, "e2");
        assert_eq!(p, dir.path().join("42").join(GEN).join("e2.r1.vtt"));
        // Reorganising media must not change the stored path.
        assert!(!p.to_string_lossy().contains("Movies"));
    }

    #[test]
    fn extracts_srt_from_corpus_fixture() {
        if skip_without_ffmpeg() {
            return;
        }
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files/h264_aac_srt_mkv.mkv");
        if skip_without_fixture(&corpus) {
            return;
        }
        let streams = list_text_subtitles(&corpus).expect("list");
        assert!(
            !streams.is_empty(),
            "expected at least one text sub on SRT fixture"
        );
        let dir = tempfile::tempdir().unwrap();
        let store = SubsStore::new(dir.path().to_path_buf()).unwrap();
        let outcome = extract(&store, 1, &corpus, &[], &|| false).expect("extract");
        assert_eq!(outcome, ExtractOutcome::Ready);
        let track_id = streams[0].track_id();
        let revision = LAST_PUBLICATIONS.with(|p| p.borrow().latest_revision(&track_id));
        let vtt = stored_webvtt(&store, 1, GEN, &track_id, revision).unwrap();
        let body = fs::read_to_string(&vtt).unwrap();
        assert!(
            body.contains("WEBVTT") || body.starts_with("\u{feff}WEBVTT"),
            "not webvtt: {body}"
        );
        assert!(
            body.contains("Nightjar SRT sample"),
            "converted cue missing: {body}"
        );
    }

    #[test]
    fn srt_encoder_copies_subrip_encodes_others() {
        assert_eq!(srt_encoder_for_codec("subrip"), "copy");
        assert_eq!(srt_encoder_for_codec("SRT"), "copy");
        assert_eq!(srt_encoder_for_codec("mov_text"), "srt");
        assert_eq!(srt_encoder_for_codec("webvtt"), "srt");
        assert_eq!(srt_encoder_for_codec("text"), "srt");
    }

    #[test]
    fn one_pass_fills_all_missing_tracks() {
        if skip_without_ffmpeg() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let srt_a = dir.path().join("a.srt");
        let srt_b = dir.path().join("b.srt");
        fs::write(&srt_a, "1\n00:00:00,000 --> 00:00:01,000\nTrack A\n").unwrap();
        fs::write(&srt_b, "1\n00:00:00,000 --> 00:00:01,000\nTrack B\n").unwrap();
        let mkv = dir.path().join("two_subs.mkv");
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=64x64:d=1",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=48000:cl=stereo:d=1",
                "-i",
            ])
            .arg(&srt_a)
            .arg("-i")
            .arg(&srt_b)
            .args([
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-c:s",
                "srt",
                "-map",
                "0:v:0",
                "-map",
                "1:a:0",
                "-map",
                "2:0",
                "-map",
                "3:0",
                "-shortest",
            ])
            .arg(&mkv)
            .status();
        let Ok(status) = status else {
            eprintln!("skipping: could not spawn ffmpeg");
            return;
        };
        if !status.success() {
            eprintln!("skipping: ffmpeg multi-sub mux failed");
            return;
        }
        let streams = list_text_subtitles(&mkv).expect("list");
        assert!(
            streams.len() >= 2,
            "expected two text subs, got {streams:?}"
        );
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        extract(&store, 9, &mkv, &[], &|| false).expect("extract");
        assert!(has(&store, 9, &streams[0].track_id()));
        assert!(has(&store, 9, &streams[1].track_id()));
        let a = fs::read_to_string(vtt(&store, 9, &streams[0].track_id())).unwrap();
        let b = fs::read_to_string(vtt(&store, 9, &streams[1].track_id())).unwrap();
        assert!(a.contains("Track A") || b.contains("Track A"));
        assert!(a.contains("Track B") || b.contains("Track B"));
    }

    /// ADR-0041 Decision 8.1: the per-file timeout budget is computed from
    /// source size at the measured 55 MiB/s rate plus a startup allowance —
    /// asserted as computed values, not a hardcoded constant, and it must
    /// scale with the declared size.
    #[test]
    fn extract_timeout_budget_scales_with_source_size() {
        let one_gib = 1024 * 1024 * 1024;
        let small = extract_timeout_budget(one_gib);
        let large = extract_timeout_budget(16 * one_gib);
        assert_eq!(
            small,
            Duration::from_secs(18 + EXTRACT_TIMEOUT_STARTUP_SECS)
        );
        assert_eq!(
            large,
            Duration::from_secs(297 + EXTRACT_TIMEOUT_STARTUP_SECS)
        );
        assert!(large > small, "budget must scale with declared source size");
        assert!(
            extract_timeout_budget(1024) >= Duration::from_secs(EXTRACT_TIMEOUT_STARTUP_SECS),
            "a tiny source still gets a startup allowance, never a zero budget"
        );
    }

    /// ADR-0041 Decision 8.4 acceptance: one deliberately unmappable subtitle
    /// stream among good ones → per-track partial success. The third text
    /// stream's cue lies beyond the title's end, so `-shortest` drops the
    /// packet: the track lists as text but demuxes to nothing. The good tracks
    /// land as vtt files, the bad one does not, no panic, and the outcome is
    /// Partial (the pool keeps the item eligible — never full `ready`).
    #[test]
    fn one_unmappable_stream_keeps_good_tracks() {
        if skip_without_ffmpeg() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let srt_a = dir.path().join("a.srt");
        let srt_b = dir.path().join("b.srt");
        let srt_late = dir.path().join("late.srt");
        fs::write(&srt_a, "1\n00:00:00,000 --> 00:00:01,000\nGood A\n").unwrap();
        fs::write(&srt_b, "1\n00:00:00,000 --> 00:00:01,000\nGood B\n").unwrap();
        // Cue beyond the 4 s title: -shortest never muxes the packet.
        fs::write(
            &srt_late,
            "1\n00:00:05,000 --> 00:00:06,000\nUnmappable C\n",
        )
        .unwrap();
        let mkv = dir.path().join("three_track.mkv");
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=64x64:d=4",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=48000:cl=stereo:d=4",
                "-i",
            ])
            .arg(&srt_a)
            .arg("-i")
            .arg(&srt_b)
            .arg("-i")
            .arg(&srt_late)
            .args([
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-c:s",
                "srt",
                "-map",
                "0:v:0",
                "-map",
                "1:a:0",
                "-map",
                "2:0",
                "-map",
                "3:0",
                "-map",
                "4:0",
                "-shortest",
            ])
            .arg(&mkv)
            .status();
        let Ok(status) = status else {
            eprintln!("skipping: could not spawn ffmpeg");
            return;
        };
        if !status.success() {
            eprintln!("skipping: ffmpeg multi-sub mux failed");
            return;
        }
        let streams = list_text_subtitles(&mkv).expect("list");
        assert_eq!(streams.len(), 3, "expected three text tracks: {streams:?}");

        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        let item_id = 21i64;
        let outcome = extract(&store, item_id, &mkv, &[], &|| false).expect("extract");
        let ExtractOutcome::Partial { written, failed } = outcome else {
            panic!("expected per-track partial success, got {outcome:?}");
        };
        assert_eq!((written, failed), (2, 1), "{outcome:?}");
        let landed: Vec<&TextSubtitleStream> = streams
            .iter()
            .filter(|s| has(&store, item_id, &s.track_id()))
            .collect();
        let missing: Vec<&TextSubtitleStream> = streams
            .iter()
            .filter(|s| !has(&store, item_id, &s.track_id()))
            .collect();
        assert_eq!(landed.len(), 2, "{streams:?}");
        assert_eq!(missing.len(), 1, "{streams:?}");
        let a = fs::read_to_string(vtt(&store, item_id, &landed[0].track_id())).unwrap();
        let b = fs::read_to_string(vtt(&store, item_id, &landed[1].track_id())).unwrap();
        assert!(
            (a.contains("Good A") && b.contains("Good B"))
                || (a.contains("Good B") && b.contains("Good A")),
            "{a}\n---\n{b}"
        );
        assert!(
            !has(&store, item_id, &missing[0].track_id()),
            "the unmappable track must not land"
        );
    }

    /// Decision 8.4's demux-abort case: a container truncated mid-file makes
    /// the single ffmpeg invocation fail, but tracks whose cues were already
    /// demuxed survive the abort and land independently. The late cue (8–9 s)
    /// sits in clusters cut off at 35 % of a 10 s file; the early cues
    /// (0–1 s) were written before the abort.
    #[test]
    fn demux_abort_salvages_completed_tracks() {
        if skip_without_ffmpeg() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let srt_a = dir.path().join("a.srt");
        let srt_b = dir.path().join("b.srt");
        let srt_late = dir.path().join("late.srt");
        fs::write(&srt_a, "1\n00:00:00,000 --> 00:00:01,000\nGood A\n").unwrap();
        fs::write(&srt_b, "1\n00:00:00,000 --> 00:00:01,000\nGood B\n").unwrap();
        fs::write(&srt_late, "1\n00:00:08,000 --> 00:00:09,000\nLate C\n").unwrap();
        let mkv = dir.path().join("noisy.mkv");
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=320x240:d=10:r=30",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=48000:cl=stereo:d=10",
                "-i",
            ])
            .arg(&srt_a)
            .arg("-i")
            .arg(&srt_b)
            .arg("-i")
            .arg(&srt_late)
            .args([
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-c:s",
                "srt",
                "-map",
                "0:v:0",
                "-map",
                "1:a:0",
                "-map",
                "2:0",
                "-map",
                "3:0",
                "-map",
                "4:0",
                "-shortest",
            ])
            .arg(&mkv)
            .status();
        let Ok(status) = status else {
            eprintln!("skipping: could not spawn ffmpeg");
            return;
        };
        if !status.success() {
            eprintln!("skipping: ffmpeg multi-sub mux failed");
            return;
        }
        // Cut mid-file: clusters are time-ordered, so the early cues survive
        // and the late one is beyond the cut.
        let data = fs::read(&mkv).unwrap();
        let cut = (data.len() as f64 * 0.35) as usize;
        let truncated = dir.path().join("trunc.mkv");
        fs::write(&truncated, &data[..cut]).unwrap();

        let streams = list_text_subtitles(&truncated).expect("list");
        assert_eq!(streams.len(), 3, "{streams:?}");
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        let item_id = 22i64;
        let outcome = extract(&store, item_id, &truncated, &[], &|| false).expect("extract");
        let ExtractOutcome::Partial { written, failed } = outcome else {
            panic!("expected per-track partial success, got {outcome:?}");
        };
        assert!(
            written >= 2,
            "completed tracks must survive the abort: {outcome:?}"
        );
        assert!(failed >= 1, "the cut-off track must not land: {outcome:?}");
        let landed: Vec<&TextSubtitleStream> = streams
            .iter()
            .filter(|s| has(&store, item_id, &s.track_id()))
            .collect();
        assert_eq!(landed.len(), 2, "{streams:?}");
        let a = fs::read_to_string(vtt(&store, item_id, &landed[0].track_id())).unwrap();
        let b = fs::read_to_string(vtt(&store, item_id, &landed[1].track_id())).unwrap();
        assert!(
            (a.contains("Good A") && b.contains("Good B"))
                || (a.contains("Good B") && b.contains("Good A")),
            "{a}\n---\n{b}"
        );
    }

    /// Final correction 1: a *cancelled* demux may salvage the cues it already
    /// flushed, but only as `Partial`, and the cancellation must still reach the
    /// pool as `unavailable` instead of a partial success. The cancel signal is
    /// a rendezvous on the demux's own output, so the abort provably lands
    /// after cues exist (no fixed sleep).
    #[test]
    fn cancelled_demux_salvages_cues_as_partial_and_reports_unavailable() {
        if skip_without_ffmpeg() {
            return;
        }
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files/h264_aac_srt_mkv.mkv");
        if skip_without_fixture(&corpus) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        let streams = list_text_subtitles(&corpus).expect("list");
        assert!(!streams.is_empty(), "fixture must carry a text track");
        let item_id = 60i64;
        // The demux writes `e{stream}.tmp.srt`; cancel only once one of those
        // tmp files has cue bytes.
        let generation_dir = store.generation_dir(item_id, GEN);
        let should_cancel = || {
            fs::read_dir(&generation_dir)
                .map(|entries| {
                    entries.flatten().any(|entry| {
                        entry
                            .path()
                            .extension()
                            .is_some_and(|extension| extension == "srt")
                            && entry.metadata().map(|m| m.len() > 0).unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        };
        let publications = Publications::new();
        let (media_path, mtime_ms, size_bytes) = capture_media(&corpus);
        let token_for = |_: &str| Some(GEN.to_string());
        let reserve = || publications.reserve();
        let publish = |track: &str, token: &str, revision: u64, state: SubtitleArtifactState| {
            publications.publish(track, token, revision, state)
        };
        let source = ExtractSource {
            media_path,
            mtime_ms,
            size_bytes,
            token_for: &token_for,
            is_current: &|| true,
            reserve_revision: &reserve,
            publish: &publish,
        };
        let err = extract_item_subtitles(&store, item_id, &source, &streams, &[], &should_cancel)
            .unwrap_err();
        assert!(err.starts_with("unavailable:"), "{err}");
        let calls = publications.calls();
        assert!(
            !calls.is_empty(),
            "the cues flushed before the abort must be salvaged"
        );
        assert!(
            calls
                .iter()
                .all(|(_, _, _, state)| *state == SubtitleArtifactState::Partial),
            "a cancelled demux must never publish Complete: {calls:?}"
        );
    }

    /// ADR-0041 Decision 8.5: a pass that fails must not remove a
    /// previously-good track. D2B.2 additionally makes the artifact
    /// generation-addressed, so a failed pass never touches the prior
    /// generation's directory at all.
    #[test]
    fn failed_pass_keeps_previously_good_tracks() {
        let dir = tempfile::tempdir().unwrap();
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        let item_id = 31i64;
        let prior = "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nPrior cue\n";
        write_webvtt(&vtt(&store, item_id, "e2"), prior).unwrap();
        let video = dir.path().join("Movie.mp4");
        fs::write(&video, b"not a real mp4").unwrap();
        let missing = dir.path().join("gone").join("Movie.en.srt");
        let err = extract(
            &store,
            item_id,
            &video,
            &[SidecarInput {
                track_id: "s-en".into(),
                path: missing,
                format: "srt".into(),
                mtime_ms: 0,
                size_bytes: 0,
            }],
            &|| false,
        )
        .expect_err("a vanished sidecar must fail the pass");
        assert!(!err.is_empty(), "{err:?}");
        assert!(
            has(&store, item_id, "e2"),
            "a failed pass must not delete a previously-good track"
        );
        assert_eq!(
            fs::read_to_string(vtt(&store, item_id, "e2")).unwrap(),
            prior,
            "prior track body must be byte-identical"
        );
    }

    #[test]
    fn sidecar_addition_does_not_shadow_embedded_store_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        let item_id = 7i64;
        fs::create_dir_all(store.item_dir(item_id)).unwrap();
        store
            .publish_item_vtt(
                item_id,
                GEN,
                "e2",
                1,
                "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nEmb\n",
            )
            .unwrap();
        let embedded_before = fs::read_to_string(vtt(&store, item_id, "e2")).unwrap();

        let srt_path = dir.path().join("Movie.en.srt");
        fs::write(
            &srt_path,
            "1\n00:00:00,000 --> 00:00:01,000\nSidecar hello\n",
        )
        .unwrap();
        // Simulate only writing the new sidecar track into an existing item dir
        // the way a mistaken ordinal scheme would collide; our namespaces must not.
        let captured = sidecar("s-en", srt_path, "srt");
        let publications = Publications::new();
        let mut finalized = HashMap::new();
        let token_for = |_: &str| Some(GEN.to_string());
        let reserve = || publications.reserve();
        let publish = |track: &str, token: &str, revision: u64, state: SubtitleArtifactState| {
            publications.publish(track, token, revision, state)
        };
        let media = dir.path().join("Movie.mp4");
        fs::write(&media, b"not a real mp4").unwrap();
        let (media_path, mtime_ms, size_bytes) = capture_media(&media);
        let source = ExtractSource {
            media_path,
            mtime_ms,
            size_bytes,
            token_for: &token_for,
            is_current: &|| true,
            reserve_revision: &reserve,
            publish: &publish,
        };
        write_sidecar_webvtt(&store, item_id, GEN, &captured, &source, &mut finalized).unwrap();

        assert!(store.has_artifact(item_id, GEN, "e2", 1));
        let side_revision = publications.latest_revision("s-en");
        assert!(store.has_artifact(item_id, GEN, "s-en", side_revision));
        assert_eq!(
            fs::read_to_string(store.artifact_path(item_id, GEN, "e2", 1)).unwrap(),
            embedded_before,
            "adding a sidecar must not renumber or overwrite embedded e2"
        );
        let side =
            fs::read_to_string(store.artifact_path(item_id, GEN, "s-en", side_revision)).unwrap();
        assert!(side.contains("Sidecar hello"));
    }

    #[test]
    fn converts_sidecar_srt() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("Movie.mp4");
        fs::write(&video, b"not a real mp4").unwrap();
        let srt_path = dir.path().join("Movie.en.srt");
        fs::write(
            &srt_path,
            "1\n00:00:00,000 --> 00:00:01,000\nSidecar hello\n",
        )
        .unwrap();
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        let outcome = extract(
            &store,
            7,
            &video,
            &[sidecar("s-en", srt_path, "srt")],
            &|| false,
        )
        .expect("sidecar-only extract");
        assert_eq!(outcome, ExtractOutcome::Ready);
        let body = fs::read_to_string(vtt(&store, 7, "s-en")).unwrap();
        assert!(body.contains("WEBVTT"));
        assert!(body.contains("Sidecar hello"));
    }

    #[test]
    fn sidecar_extract_reports_unavailable_when_share_drops() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("Movie.mp4");
        fs::write(&video, b"not a real mp4").unwrap();
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        let missing = dir.path().join("gone").join("Movie.en.srt");
        let err = extract(
            &store,
            8,
            &video,
            &[SidecarInput {
                track_id: "s-en".into(),
                path: missing,
                format: "srt".into(),
                mtime_ms: 0,
                size_bytes: 0,
            }],
            &|| false,
        )
        .unwrap_err();
        assert!(err.starts_with("unavailable:"), "{err}");
    }

    /// R4 storage bounds: a write-side failure (full, read-only, or
    /// permission-denied volume) is availability, so the scanner retries it
    /// instead of retiring the item as a permanent error. `InvalidData` is the
    /// negative control: a corrupt sidecar keeps the old classification.
    #[test]
    fn write_side_io_errors_are_availability() {
        use std::io::{Error, ErrorKind};
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::StorageFull,
            ErrorKind::QuotaExceeded,
            ErrorKind::ReadOnlyFilesystem,
        ] {
            assert!(
                io_error_is_availability(&Error::new(kind, "simulated")),
                "{kind:?} must be availability"
            );
        }
        assert!(!io_error_is_availability(&Error::new(
            ErrorKind::InvalidData,
            "corrupt sidecar"
        )));
    }

    /// ADR-0041 Decision 8.7: the library-reachability cancel signal kills an
    /// in-flight demux and stamps the run `unavailable`, never `ready`. The
    /// signal is checked before completion, so a library that flips
    /// unreachable exactly as the demux finishes still aborts the run.
    #[test]
    fn extract_cancel_kills_demux_and_stamps_unavailable() {
        if skip_without_ffmpeg() {
            return;
        }
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files/h264_aac_srt_mkv.mkv");
        if skip_without_fixture(&corpus) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        let err = extract(&store, 51, &corpus, &[], &|| true).unwrap_err();
        assert!(err.starts_with("unavailable:"), "{err}");
        // Killed before any cue was flushed: no track may land as complete.
        let dir = store.generation_dir(51, GEN);
        let landed = fs::read_dir(&dir)
            .map(|it| {
                it.flatten()
                    .filter(|e| e.path().extension().is_some_and(|x| x == "vtt"))
                    .count()
            })
            .unwrap_or(0);
        assert_eq!(
            landed, 0,
            "a cancelled extract must not leave tracks behind"
        );
    }

    #[test]
    fn cleanup_removes_orphan_item_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let store = SubsStore::new(dir.path().to_path_buf()).unwrap();
        fs::create_dir_all(store.item_dir(1)).unwrap();
        fs::create_dir_all(store.item_dir(2)).unwrap();
        write_webvtt(&vtt(&store, 1, "e2"), "WEBVTT\n").unwrap();
        write_webvtt(&vtt(&store, 2, "e2"), "WEBVTT\n").unwrap();
        let n = store.cleanup_orphans(&[1]).unwrap();
        assert_eq!(n, 1);
        assert!(store.item_dir(1).exists());
        assert!(!store.item_dir(2).exists());
    }

    /// D2B.2 corrective reset item 1/2: a fresh extract reserves a new artifact
    /// revision and finalizes new bytes there. The prior revision's bytes are
    /// never overwritten (there is no mutable path and no sweep), and the new
    /// revision is the one the run committed.
    #[test]
    fn fresh_extract_finalizes_a_new_revision_without_touching_the_old_one() {
        if skip_without_ffmpeg() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files/h264_aac_srt_mkv.mkv");
        if skip_without_fixture(&corpus) {
            return;
        }
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        let streams = list_text_subtitles(&corpus).unwrap();
        let track = streams[0].track_id();

        // One monotonic allocator across both runs, as the DB sequence is.
        let publications = Publications::new();
        extract_with_publications(
            &store,
            3,
            &corpus,
            &streams,
            &[],
            &|| false,
            || true,
            &publications,
        )
        .unwrap();
        let first_revision = publications.latest_revision(&track);
        let first_body =
            fs::read_to_string(store.artifact_path(3, GEN, &track, first_revision)).unwrap();

        extract_with_publications(
            &store,
            3,
            &corpus,
            &streams,
            &[],
            &|| false,
            || true,
            &publications,
        )
        .unwrap();
        let second_revision = publications.latest_revision(&track);
        assert_ne!(
            first_revision, second_revision,
            "a fresh run must reserve its own artifact revision"
        );
        assert_eq!(
            fs::read_to_string(store.artifact_path(3, GEN, &track, first_revision)).unwrap(),
            first_body,
            "the prior revision's bytes are immutable"
        );
        let new_body =
            fs::read_to_string(store.artifact_path(3, GEN, &track, second_revision)).unwrap();
        assert!(new_body.contains("WEBVTT"));
        assert_eq!(first_body.lines().next(), new_body.lines().next());
    }

    /// D2B.2 corrective reset item 2: every changed body reserves its own
    /// artifact revision and finalizes new immutable bytes. An identical body
    /// references the bytes already finalized and is never rewritten, and a
    /// changed body never overwrites the revision that is already committed
    /// (ADR-0013 §13.2, §13.5).
    #[test]
    fn every_changed_body_finalizes_its_own_revision() {
        let dir = tempfile::tempdir().unwrap();
        let store = SubsStore::new(dir.path().to_path_buf()).unwrap();
        let publications = Publications::new();
        let mut finalized = HashMap::new();
        let token_for = |_: &str| Some(GEN.to_string());
        let reserve = || publications.reserve();
        let publish = |track: &str, token: &str, revision: u64, state: SubtitleArtifactState| {
            publications.publish(track, token, revision, state)
        };
        let media = dir.path().join("Movie.mp4");
        fs::write(&media, b"not a real mp4").unwrap();
        let (media_path, mtime_ms, size_bytes) = capture_media(&media);
        let source = ExtractSource {
            media_path,
            mtime_ms,
            size_bytes,
            token_for: &token_for,
            is_current: &|| true,
            reserve_revision: &reserve,
            publish: &publish,
        };
        let partial = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nHello\n";
        publish_artifact(
            &store,
            7,
            &source,
            "e2",
            GEN,
            partial,
            SubtitleArtifactState::Partial,
            None,
            &mut finalized,
        )
        .unwrap();
        let first = publications.latest_revision("e2");
        assert_eq!(first, 1);
        assert!(store.has_artifact(7, GEN, "e2", first));
        assert_eq!(
            fs::read_to_string(store.artifact_path(7, GEN, "e2", first)).unwrap(),
            partial
        );

        // The identical body is referenced again: no new revision is reserved
        // and the bytes are not rewritten.
        let before = fs::metadata(store.artifact_path(7, GEN, "e2", first))
            .unwrap()
            .modified()
            .unwrap();
        publish_artifact(
            &store,
            7,
            &source,
            "e2",
            GEN,
            partial,
            SubtitleArtifactState::Complete,
            None,
            &mut finalized,
        )
        .unwrap();
        assert_eq!(
            publications.latest_revision("e2"),
            first,
            "identical bytes are referenced, not rewritten"
        );
        assert_eq!(
            fs::metadata(store.artifact_path(7, GEN, "e2", first))
                .unwrap()
                .modified()
                .unwrap(),
            before,
            "the finalized artifact is not rewritten"
        );
        assert_eq!(
            publications.calls().last().unwrap().3,
            SubtitleArtifactState::Complete,
            "the partial-to-complete transition commits the same revision"
        );

        // A changed body reserves a new revision. The committed revision's
        // bytes stay exactly as they were: nothing truncates or overwrites.
        let grown = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nHello\n\n00:00:03.000 --> 00:00:04.000\nWorld\n";
        publish_artifact(
            &store,
            7,
            &source,
            "e2",
            GEN,
            grown,
            SubtitleArtifactState::Partial,
            None,
            &mut finalized,
        )
        .unwrap();
        let second = publications.latest_revision("e2");
        assert_ne!(second, first, "a changed body gets its own revision");
        assert_eq!(
            fs::read_to_string(store.artifact_path(7, GEN, "e2", first)).unwrap(),
            partial,
            "the prior revision is never overwritten"
        );
        assert_eq!(
            fs::read_to_string(store.artifact_path(7, GEN, "e2", second)).unwrap(),
            grown
        );
    }

    /// D2B.2 corrective reset item 2: an existing final artifact is never
    /// overwritten. A finalize at an occupied revision fails and leaves the
    /// committed bytes untouched.
    #[test]
    fn an_existing_artifact_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let store = SubsStore::new(dir.path().to_path_buf()).unwrap();
        let committed = "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nCommitted\n";
        store.publish_item_vtt(7, GEN, "e2", 4, committed).unwrap();
        let err = store
            .publish_item_vtt(
                7,
                GEN,
                "e2",
                4,
                "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nNew\n",
            )
            .unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(
            fs::read_to_string(store.artifact_path(7, GEN, "e2", 4)).unwrap(),
            committed,
            "the existing artifact must keep its exact bytes"
        );
    }

    /// Final correction 2: candidate creation is exclusive. A crash between the
    /// finalize's hard link and its candidate unlink leaves a candidate that
    /// shares an inode with the committed artifact; a retry at the same
    /// revision must fail instead of truncating the committed bytes.
    #[test]
    fn a_crash_left_candidate_never_truncates_a_final_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let store = SubsStore::new(dir.path().to_path_buf()).unwrap();
        let committed = "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nCommitted\n";
        store.publish_item_vtt(7, GEN, "e2", 4, committed).unwrap();
        let final_path = store.artifact_path(7, GEN, "e2", 4);
        // The crash-left candidate has the name `write_candidate` would reuse
        // for the same revision and shares the finalized artifact's inode.
        let candidate = store.generation_dir(7, GEN).join("e2.r4.candidate");
        fs::hard_link(&final_path, &candidate).unwrap();

        let err = store
            .publish_item_vtt(
                7,
                GEN,
                "e2",
                4,
                "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nNew\n",
            )
            .unwrap_err();
        assert!(err.contains("candidate"), "{err}");
        assert_eq!(
            fs::read_to_string(&final_path).unwrap(),
            committed,
            "the finalized bytes must survive a colliding retry"
        );
        assert!(
            candidate.exists(),
            "the crash-left candidate is never truncated or renamed"
        );
    }

    #[test]
    fn session_inline_prep_slices_first_segment_without_scan_extract() {
        if skip_without_ffmpeg() {
            return;
        }
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files/h264_aac_srt_mkv.mkv");
        if skip_without_fixture(&corpus) {
            return;
        }
        let streams = list_text_subtitles(&corpus).expect("list");
        assert!(!streams.is_empty());
        let track = &streams[0];
        let session = tempfile::tempdir().unwrap();
        prepare_session_subtitles(
            &corpus,
            session.path(),
            &[SessionSubInput {
                track_id: track.track_id(),
                codec: track.codec.clone(),
                stream_index: Some(track.stream_index),
                sidecar_path: None,
            }],
        )
        .expect("session prep");
        let full = session
            .path()
            .join("subs")
            .join(track.track_id())
            .join("full.vtt");
        assert!(full.exists(), "expected {}", full.display());
        assert!(
            session
                .path()
                .join("subs")
                .join(track.track_id())
                .join("done")
                .exists()
        );
        let body = fs::read_to_string(&full).unwrap();
        assert!(body.contains("Nightjar SRT sample"), "{body}");
        let seg0 = slice_webvtt(&body, 0, 2000);
        assert!(seg0.contains("\nNightjar SRT sample\n"), "{seg0}");
    }

    /// A piggyback publish must add one track and leave every other file in
    /// the item directory untouched (ADR-0041 Decision 7 / 8.5: never delete
    /// a previously-good track).
    #[test]
    fn publish_item_vtt_adds_without_wiping_prior_tracks() {
        let dir = tempfile::tempdir().unwrap();
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        let item_id = 17i64;
        write_webvtt(
            &vtt(&store, item_id, "e2"),
            "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nPrior cue\n",
        )
        .unwrap();
        store
            .publish_item_vtt(
                item_id,
                GEN,
                "s-en",
                2,
                "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nNew cue\n",
            )
            .unwrap();
        assert!(has(&store, item_id, "e2"), "prior track must survive");
        assert!(store.has_artifact(item_id, GEN, "s-en", 2));
        assert_eq!(
            fs::read_to_string(vtt(&store, item_id, "e2")).unwrap(),
            "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nPrior cue\n",
            "prior track body must be byte-identical"
        );
        let new = fs::read_to_string(store.artifact_path(item_id, GEN, "s-en", 2)).unwrap();
        assert!(new.contains("New cue"), "{new}");
    }

    /// Concatenated segment bodies keep the header once and every cue in
    /// order, dropping per-segment headers and non-cue blocks so the result
    /// slices identically to a single-document extract.
    #[test]
    fn concat_webvtt_segments_merges_cue_blocks_in_order() {
        let segs = [
            "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nFirst cue\n".to_string(),
            "WEBVTT\n\n00:00:02.000 --> 00:00:03.000\nSecond cue\n".to_string(),
        ];
        let joined = concat_webvtt_segments(&segs);
        assert!(joined.starts_with("WEBVTT\n\n"), "{joined}");
        assert_eq!(joined.matches("WEBVTT").count(), 1, "{joined}");
        assert_eq!(joined.matches("-->").count(), 2, "{joined}");
        let first = joined.find("First cue").unwrap();
        let second = joined.find("Second cue").unwrap();
        assert!(first < second, "cues must keep segment order: {joined}");
        assert!(!joined.contains("STYLE"), "{joined}");
        // Slicing the concatenation must yield the same windows a single
        // document would.
        let seg0 = slice_webvtt(&joined, 0, 2000);
        assert!(
            seg0.contains("First cue") && !seg0.contains("Second cue"),
            "{seg0}"
        );
    }

    /// D2B.2 acceptance 5, final correction 3: a sidecar that changes *after*
    /// the candidate is written is rejected by the last physical check, which
    /// runs after the last potentially blocking DB check. Nothing is finalized
    /// or committed under the stale generation.
    #[test]
    fn sidecar_changed_during_extract_defers_without_publishing() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("Movie.mp4");
        fs::write(&video, b"not a real mp4").unwrap();
        let srt_path = dir.path().join("Movie.en.srt");
        fs::write(&srt_path, "1\n00:00:00,000 --> 00:00:01,000\nHi\n").unwrap();
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        let captured = sidecar("s-en", srt_path.clone(), "srt");

        // Rendezvous: mutate the sidecar only once the publication path has
        // written its candidate, while reporting the DB source still current.
        // The mutation is therefore provably later than the candidate write,
        // and only the last physical check can catch it.
        let mutated = std::cell::Cell::new(false);
        let srt_for_closure = srt_path.clone();
        let store_for_closure = &store;
        let is_current = || {
            if !mutated.get() && candidate_exists(store_for_closure, 40, GEN) {
                fs::write(
                    &srt_for_closure,
                    "1\n00:00:00,000 --> 00:00:03,000\nChanged\n",
                )
                .unwrap();
                mutated.set(true);
            }
            true
        };
        let publications = Publications::new();
        let err = extract_with_publications(
            &store,
            40,
            &video,
            &[],
            &[captured],
            &|| false,
            is_current,
            &publications,
        )
        .unwrap_err();
        assert!(mutated.get(), "the mutation rendezvous must have run");
        assert!(message_is_source_changed(&err), "{err}");
        assert!(
            publications.calls().is_empty(),
            "a changed sidecar must not reach the publication CAS"
        );
        assert!(
            !store.has_artifact(40, GEN, "s-en", 1),
            "a changed sidecar must not finalize an artifact"
        );
        assert!(
            !candidate_exists(&store, 40, GEN),
            "the stale candidate must be removed"
        );
    }

    /// D2B.2 acceptance 3: artifacts are written into a generation directory and
    /// named by their artifact revision, so neither a later generation nor a
    /// later revision overwrites the prior one.
    #[test]
    fn generations_address_immutable_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        let first = store.artifact_path(1, "g1", "e2", 1);
        let second = store.artifact_path(1, "g2", "e2", 1);
        let third = store.artifact_path(1, "g1", "e2", 2);
        assert_ne!(first, second, "different generations are different paths");
        assert_ne!(first, third, "different revisions are different paths");
        store
            .publish_item_vtt(
                1,
                "g1",
                "e2",
                1,
                "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\ng1\n",
            )
            .unwrap();
        store
            .publish_item_vtt(
                1,
                "g2",
                "e2",
                1,
                "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\ng2\n",
            )
            .unwrap();
        assert!(fs::read_to_string(&first).unwrap().contains("g1"));
        assert!(fs::read_to_string(&second).unwrap().contains("g2"));
    }

    /// D2B.2 acceptance 3/5, final correction 3: the last physical source check
    /// runs after the candidate is written and after the last potentially
    /// blocking DB check, immediately before the finalize and the commit. A
    /// media replacement during that window is detected, so no artifact is
    /// finalized and no reference is committed.
    #[test]
    fn media_replaced_during_extract_leaves_no_renamed_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("Movie.mp4");
        fs::write(&video, b"not a real mp4").unwrap();
        let srt_path = dir.path().join("Movie.en.srt");
        fs::write(&srt_path, "1\n00:00:00,000 --> 00:00:01,000\nHi\n").unwrap();
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        let sidecar = sidecar("s-en", srt_path, "srt");

        // Rendezvous: replace the media only once the candidate exists on
        // disk, while reporting the DB source still current. Only the physical
        // check that follows the last DB check can catch it.
        let mutated = std::cell::Cell::new(false);
        let video_for_closure = video.clone();
        let store_for_closure = &store;
        let is_current = || {
            if !mutated.get() && candidate_exists(store_for_closure, 41, GEN) {
                fs::write(&video_for_closure, b"a different, longer body").unwrap();
                mutated.set(true);
            }
            true
        };
        let publications = Publications::new();
        let err = extract_with_publications(
            &store,
            41,
            &video,
            &[],
            &[sidecar],
            &|| false,
            is_current,
            &publications,
        )
        .unwrap_err();
        assert!(mutated.get(), "the mutation rendezvous must have run");
        assert!(message_is_source_changed(&err), "{err}");
        assert!(
            publications.calls().is_empty(),
            "a failed revalidation must not reach the publication CAS"
        );
        assert!(
            !store.has_artifact(41, GEN, "s-en", 1),
            "a failed final source check must not finalize stale bytes"
        );
        assert!(
            !candidate_exists(&store, 41, GEN),
            "the stale candidate must be removed"
        );
    }

    /// D2B.2 corrective reset item 3: progressive work revalidates the captured
    /// media path plus mtime/size before it converts or reads and again before
    /// it publishes. A media replacement under a running conversion therefore
    /// publishes nothing, and the run stops rather than committing bytes for a
    /// source that no longer exists.
    #[test]
    fn progressive_work_stops_when_the_media_changed() {
        let dir = tempfile::tempdir().unwrap();
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        let media = dir.path().join("Movie.mkv");
        fs::write(&media, b"the captured body").unwrap();
        let (media_path, mtime_ms, size_bytes) = capture_media(&media);
        let tmp = dir.path().join("e2.tmp.srt");
        fs::write(
            &tmp,
            "1\n00:00:00,000 --> 00:00:01,000\nHi\n\n2\n00:00:02,000 --> 00:00:03,000\nThere\n",
        )
        .unwrap();
        let tmp_srts = vec![(2u32, tmp)];

        // Rendezvous: replace the media only once the candidate is written,
        // while reporting the DB source still current. The last physical check
        // must reject the work, so the progressive publication stops.
        let mutated = std::cell::Cell::new(false);
        let media_for_closure = media.clone();
        let store_for_closure = &store;
        let is_current = || {
            if !mutated.get() && candidate_exists(store_for_closure, 7, "v1-m1-p0") {
                fs::write(&media_for_closure, b"a different, longer replacement body").unwrap();
                mutated.set(true);
            }
            true
        };
        let publications = Publications::new();
        let token_for = |_: &str| Some("v1-m1-p0".to_string());
        let reserve = || publications.reserve();
        let publish = |track: &str, token: &str, revision: u64, state: SubtitleArtifactState| {
            publications.publish(track, token, revision, state)
        };
        let source = ExtractSource {
            media_path,
            mtime_ms,
            size_bytes,
            token_for: &token_for,
            is_current: &is_current,
            reserve_revision: &reserve,
            publish: &publish,
        };
        let mut sizes = HashMap::new();
        let mut finalized = HashMap::new();
        publish_growing_srts(
            &store,
            7,
            "v1-m1-p0",
            &tmp_srts,
            &mut sizes,
            &source,
            &mut finalized,
        );

        assert!(mutated.get(), "the mutation rendezvous must have run");
        assert!(
            publications.calls().is_empty(),
            "a replaced media must not reach the publication CAS"
        );
        assert!(
            !store.has_artifact(7, "v1-m1-p0", "e2", 1),
            "a replaced media must not finalize an artifact"
        );
        assert!(
            !candidate_exists(&store, 7, "v1-m1-p0"),
            "the stale candidate must be removed, never finalized"
        );
    }

    /// D2B.2 acceptance 3/4/5: progressive publication is gated by the same
    /// source CAS as the final publication. A stale run writes no bytes and
    /// commits nothing; a current run writes and commits under its own token.
    #[test]
    fn progressive_publication_is_gated_by_the_source_cas() {
        let dir = tempfile::tempdir().unwrap();
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        let tmp = dir.path().join("e2.tmp.srt");
        fs::write(
            &tmp,
            "1\n00:00:00,000 --> 00:00:01,000\nHi\n\n2\n00:00:02,000 --> 00:00:03,000\nThere\n",
        )
        .unwrap();
        let (media_path, mtime_ms, size_bytes) = capture_media(&tmp);
        let tmp_srts = vec![(2u32, tmp)];
        let token_for = |_: &str| Some("v1-m1-p0".to_string());

        // A stale DB source: no bytes are written and the CAS is never reached.
        let stale_publications = Publications::new();
        let stale_reserve = || stale_publications.reserve();
        let stale_publish =
            |track: &str, token: &str, revision: u64, state: SubtitleArtifactState| {
                stale_publications.publish(track, token, revision, state)
            };
        let stale = ExtractSource {
            media_path: media_path.clone(),
            mtime_ms,
            size_bytes,
            token_for: &token_for,
            is_current: &|| false,
            reserve_revision: &stale_reserve,
            publish: &stale_publish,
        };
        let mut sizes = HashMap::new();
        let mut finalized = HashMap::new();
        publish_growing_srts(
            &store,
            7,
            "v1-m1-p0",
            &tmp_srts,
            &mut sizes,
            &stale,
            &mut finalized,
        );
        assert!(
            !store.has_artifact(7, "v1-m1-p0", "e2", 1),
            "stale progressive work must not write bytes"
        );
        assert!(
            stale_publications.calls().is_empty(),
            "stale progressive work must not reach the CAS"
        );

        // Current work: bytes land and the committed publication is keyed by
        // the token. A CAS that rejects stops further publication.
        let publications = Publications::new();
        let reserve = || publications.reserve();
        let publish = |track: &str, token: &str, revision: u64, state: SubtitleArtifactState| {
            publications.publish(track, token, revision, state)
        };
        let current = ExtractSource {
            media_path: media_path.clone(),
            mtime_ms,
            size_bytes,
            token_for: &token_for,
            is_current: &|| true,
            reserve_revision: &reserve,
            publish: &publish,
        };
        let mut sizes = HashMap::new();
        let mut finalized = HashMap::new();
        publish_growing_srts(
            &store,
            7,
            "v1-m1-p0",
            &tmp_srts,
            &mut sizes,
            &current,
            &mut finalized,
        );
        assert!(store.has_artifact(7, "v1-m1-p0", "e2", publications.latest_revision("e2")));
        assert_eq!(
            publications.calls().as_slice(),
            &[(
                "e2".to_string(),
                "v1-m1-p0".to_string(),
                1,
                SubtitleArtifactState::Partial
            )]
        );
        assert!(
            !store.has_artifact(7, "v1-m1-p0-s2", "e2", 1),
            "another generation's directory must never be written"
        );

        // A CAS that rejects mid-run stops the growing writes, so no later
        // body can be committed for a superseded source.
        let dir2 = tempfile::tempdir().unwrap();
        let store2 = SubsStore::new(dir2.path().join("subs")).unwrap();
        let tmp2 = dir2.path().join("e2.tmp.srt");
        fs::write(
            &tmp2,
            "1\n00:00:00,000 --> 00:00:01,000\nHi\n\n2\n00:00:02,000 --> 00:00:03,000\nThere\n",
        )
        .unwrap();
        let (media_path2, mtime_ms2, size_bytes2) = capture_media(&tmp2);
        let tmp_srts2 = vec![(2u32, tmp2)];
        let rejected = Publications::rejecting();
        let rejected_reserve = || rejected.reserve();
        let rejected_publish =
            |track: &str, token: &str, revision: u64, state: SubtitleArtifactState| {
                rejected.publish(track, token, revision, state)
            };
        let rejected_source = ExtractSource {
            media_path: media_path2.clone(),
            mtime_ms: mtime_ms2,
            size_bytes: size_bytes2,
            token_for: &token_for,
            is_current: &|| true,
            reserve_revision: &rejected_reserve,
            publish: &rejected_publish,
        };
        let mut sizes = HashMap::new();
        let mut finalized = HashMap::new();
        publish_growing_srts(
            &store2,
            7,
            "v1-m1-p0",
            &tmp_srts2,
            &mut sizes,
            &rejected_source,
            &mut finalized,
        );
        assert!(
            rejected.calls().len() == 1,
            "the first body is finalized before the CAS decides"
        );
        assert!(
            store2.has_artifact(7, "v1-m1-p0", "e2", 1),
            "the finalized candidate is unreferenced, never served"
        );
    }
}
