//! Text subtitle → WebVTT (ADR-0010 / ADR-0013).
//!
//! Extraction runs at scan time into derived library data under
//! `{NIGHTJAR_DATA_DIR}/subs/{itemId}/{trackId}.vtt`. Playback only reads.

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

use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

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
const ASS_BURN_EXTRACT_TIMEOUT: Duration = Duration::from_secs(1800);

/// How often to publish a growing WebVTT while FFmpeg demuxes (ADR-0013 §11).
const PROGRESS_TICK: Duration = Duration::from_millis(500);

/// Refuse extract when the data volume has less free space than this.
const MIN_FREE_BYTES: u64 = 256 * 1024 * 1024;

/// IO kinds that usually mean the mount/share is gone, not a bad subtitle file.
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
    ) || err.raw_os_error().is_some_and(|c| {
        // ESTALE / ENOTCONN on Unix when the SMB mount half-dies.
        c == 70 || c == 57 || c == 60
    })
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TrackProgress {
    readiness: TrackReadiness,
    revision: u64,
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

/// Filesystem sidecar input for a scan-time extract job.
#[derive(Debug, Clone)]
pub struct SidecarInput {
    pub track_id: String,
    pub path: PathBuf,
    pub format: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractOutcome {
    /// No serveable text tracks (embedded or sidecar).
    None,
    /// All serveable tracks written under the item directory.
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
    /// In-flight per-track readiness while demux runs (ADR-0013 §11).
    progress: Mutex<HashMap<(i64, String), TrackProgress>>,
}

impl SubsStore {
    pub fn new(root: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(&root)
            .map_err(|e| format!("create subs dir {}: {e}", root.display()))?;
        Ok(Self {
            root,
            extract_lock: Mutex::new(()),
            progress: Mutex::new(HashMap::new()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn item_dir(&self, item_id: i64) -> PathBuf {
        self.root.join(item_id.to_string())
    }

    pub fn vtt_path(&self, item_id: i64, track_id: &str) -> PathBuf {
        self.item_dir(item_id).join(format!("{track_id}.vtt"))
    }

    pub fn has_vtt(&self, item_id: i64, track_id: &str) -> bool {
        let path = self.vtt_path(item_id, track_id);
        path.exists() && fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > 0
    }

    /// Server-declared readiness for a serveable track. `None` means the track
    /// is listed but not served (ASS/SSA, image) — caller decides that from
    /// codec/format before asking.
    pub fn track_readiness(
        &self,
        item_id: i64,
        track_id: &str,
        item_subtitle_status: &str,
    ) -> (TrackReadiness, u64) {
        if let Some(p) = self
            .progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(item_id, track_id.to_string()))
            .copied()
        {
            return (p.readiness, p.revision);
        }
        if self.has_vtt(item_id, track_id) {
            // Prefer Complete once the item is marked ready; a pending item
            // with a file on disk is a partial left mid-extract (or after a
            // crash), which is still serveable.
            if item_subtitle_status == "ready" {
                (TrackReadiness::Complete, 1)
            } else {
                (TrackReadiness::Partial, 1)
            }
        } else {
            (TrackReadiness::Preparing, 0)
        }
    }

    fn set_progress(&self, item_id: i64, track_id: &str, readiness: TrackReadiness, revision: u64) {
        self.progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                (item_id, track_id.to_string()),
                TrackProgress {
                    readiness,
                    revision,
                },
            );
    }

    fn clear_item_progress(&self, item_id: i64) {
        self.progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(id, _), _| *id != item_id);
    }

    /// Write a growing WebVTT and bump revision when the body grew.
    fn publish_partial_vtt(&self, item_id: i64, track_id: &str, body: &str) -> Result<(), String> {
        let dest = self.vtt_path(item_id, track_id);
        let prev_len = fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
        if (body.len() as u64) <= prev_len && prev_len > 0 {
            return Ok(());
        }
        write_webvtt(&dest, body)?;
        let next_rev = self
            .progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(item_id, track_id.to_string()))
            .map(|p| p.revision.saturating_add(1))
            .unwrap_or(1);
        self.set_progress(item_id, track_id, TrackReadiness::Partial, next_rev);
        Ok(())
    }

    fn mark_complete(&self, item_id: i64, track_id: &str) {
        let next_rev = self
            .progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(item_id, track_id.to_string()))
            .map(|p| p.revision.saturating_add(1))
            .unwrap_or(1);
        self.set_progress(item_id, track_id, TrackReadiness::Complete, next_rev);
    }

    pub fn remove_item(&self, item_id: i64) -> Result<(), String> {
        self.clear_item_progress(item_id);
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

    /// Publish one track's completed WebVTT without touching any other file
    /// in the item directory (ADR-0041 Decision 7 / 8.5 share this invariant:
    /// a piggyback or a failed pass never deletes a previously-good track).
    /// The item directory is created on demand; existing tracks stay intact.
    pub fn publish_item_vtt(&self, item_id: i64, track_id: &str, body: &str) -> Result<(), String> {
        write_webvtt(&self.vtt_path(item_id, track_id), body)
    }

    /// Drop WebVTT files in the item directory whose track id is not in
    /// `keep`. Runs only after a fully successful pass replaces the prior
    /// generation; a failed or partial pass never deletes (ADR-0041 Decision
    /// 8.5 — the old code wiped the whole item dir up front, which a failed
    /// retry could turn into data loss).
    pub fn sweep_item_vtts(&self, item_id: i64, keep: &[String]) -> Result<usize, String> {
        let dir = self.item_dir(item_id);
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(format!("read subtitle dir {}: {e}", dir.display())),
        };
        let mut removed = 0usize;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.ends_with(".vtt") {
                continue;
            }
            let track_id = name.trim_end_matches(".vtt");
            if keep.iter().any(|k| k == track_id) {
                continue;
            }
            match fs::remove_file(entry.path()) {
                Ok(()) => removed += 1,
                Err(e) => tracing::warn!(
                    item_id,
                    path = %entry.path().display(),
                    error = %e,
                    "stale subtitle track cleanup failed"
                ),
            }
        }
        Ok(removed)
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
        fs::create_dir_all(parent)
            .map_err(|e| format!("create subtitle dir {}: {e}", parent.display()))?;
    }
    // Temp write + fsync + atomic rename per track: a reader never sees a
    // half-written WebVTT, and a crash mid-write cannot corrupt a
    // previously-good track (ADR-0041 Decision 8.4).
    let tmp = dest.with_extension("tmp.vtt");
    let mut file =
        fs::File::create(&tmp).map_err(|e| format!("write subtitle tmp {}: {e}", tmp.display()))?;
    file.write_all(body.as_bytes())
        .map_err(|e| format!("write subtitle tmp {}: {e}", tmp.display()))?;
    file.sync_all()
        .map_err(|e| format!("fsync subtitle tmp {}: {e}", tmp.display()))?;
    drop(file);
    fs::rename(&tmp, dest).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("rename subtitle {}: {e}", dest.display())
    })?;
    Ok(())
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
pub fn extract_item_subtitles(
    store: &SubsStore,
    item_id: i64,
    src: &Path,
    sidecars: &[SidecarInput],
    should_cancel: &dyn Fn() -> bool,
) -> Result<ExtractOutcome, String> {
    let _guard = store
        .extract_lock
        .lock()
        .map_err(|_| "subtitle extract lock poisoned".to_string())?;

    ensure_free_space(store.root())?;

    let serveable_sidecars: Vec<&SidecarInput> = sidecars
        .iter()
        .filter(|s| is_serveable_sidecar_format(&s.format))
        .collect();
    // Sidecar-only titles still extract when the container probe fails (corrupt
    // video, permission), so external .srt next to a bad file is not stranded.
    let embedded = match list_text_subtitles(src) {
        Ok(v) => v,
        Err(e) if !serveable_sidecars.is_empty() => {
            tracing::warn!(
                path = %src.display(),
                error = %e,
                "embedded subtitle probe failed; continuing with sidecars"
            );
            Vec::new()
        }
        Err(e) => return Err(e),
    };
    if embedded.is_empty() && serveable_sidecars.is_empty() {
        store.remove_item(item_id)?;
        return Ok(ExtractOutcome::None);
    }

    // The prior generation is NOT wiped up front: a pass that fails must not
    // delete previously-good tracks (ADR-0041 Decision 8.5). Stale tracks from
    // an older generation are swept only after a full success below.
    fs::create_dir_all(store.item_dir(item_id))
        .map_err(|e| format!("create subtitle dir for item {item_id}: {e}"))?;

    for s in &embedded {
        store.set_progress(item_id, &s.track_id(), TrackReadiness::Preparing, 0);
    }
    for s in &serveable_sidecars {
        store.set_progress(item_id, &s.track_id, TrackReadiness::Preparing, 0);
    }

    // Per-file timeout budget from source size at the measured 55 MiB/s rate
    // (ADR-0041 Decision 8.1); the old fixed 300 s constant is gone.
    let src_bytes = fs::metadata(src).map(|m| m.len()).unwrap_or(0);
    let budget = extract_timeout_budget(src_bytes);
    tracing::info!(
        path = %src.display(),
        src_bytes,
        timeout_budget_ms = budget.as_millis() as u64,
        "subtitle extract timeout budget"
    );

    let mut written = 0usize;
    let mut failed = 0usize;
    if !embedded.is_empty() {
        let refs: Vec<&TextSubtitleStream> = embedded.iter().collect();
        let (w, f) = extract_embedded_srt_batch(store, item_id, src, &refs, budget, should_cancel)?;
        written += w;
        failed += f;
    }

    let mut first_sidecar_err: Option<String> = None;
    for s in serveable_sidecars {
        if should_cancel() {
            return Err("unavailable: subtitle extract cancelled (library unreachable)".into());
        }
        match write_sidecar_webvtt(store, item_id, s) {
            Ok(()) => {
                store.mark_complete(item_id, &s.track_id);
                written += 1;
            }
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

    // Full success replaces the prior generation: drop vtt files whose track
    // is no longer in the inventory (the old code wiped the whole item dir up
    // front; only a success may delete — Decision 8.5).
    let mut keep: Vec<String> = embedded.iter().map(|s| s.track_id()).collect();
    keep.extend(sidecars.iter().map(|s| s.track_id.clone()));
    if let Err(e) = store.sweep_item_vtts(item_id, &keep) {
        tracing::warn!(item_id, error = %e, "stale subtitle sweep failed");
    }

    Ok(ExtractOutcome::Ready)
}

/// Path of an already-extracted WebVTT, or an error if missing.
pub fn stored_webvtt(store: &SubsStore, item_id: i64, track_id: &str) -> Result<PathBuf, String> {
    let path = store.vtt_path(item_id, track_id);
    if store.has_vtt(item_id, track_id) {
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
    sidecar: &SidecarInput,
) -> Result<(), String> {
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
    write_webvtt(&store.vtt_path(item_id, &sidecar.track_id), &body)
}

/// Demux every embedded text stream in one ffmpeg run, then publish each
/// track's WebVTT independently (ADR-0041 Decision 8.4: one bad stream must
/// not lose tracks that completed). Returns (written, failed) track counts;
/// an `Err` means the demux failed AND no track produced usable output.
fn extract_embedded_srt_batch(
    store: &SubsStore,
    item_id: i64,
    src: &Path,
    streams: &[&TextSubtitleStream],
    timeout: Duration,
    should_cancel: &dyn Fn() -> bool,
) -> Result<(usize, usize), String> {
    let item_dir = store.item_dir(item_id);
    let mut tmp_srts: Vec<(u32, PathBuf)> = Vec::with_capacity(streams.len());
    let mut cmd = Command::new("ffmpeg");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(src);
    for s in streams {
        let tmp = item_dir.join(format!("e{}.tmp.srt", s.stream_index));
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
        publish_growing_srts(store, item_id, &tmp_srts, &mut last_sizes);
    });

    // Salvage each track's tmp independently. A failed demux leaves whatever
    // each track produced before the abort; tracks that produced nothing
    // count as failed and the rest land (Decision 8.4).
    let mut written = 0usize;
    let mut failed = 0usize;
    for (stream_index, tmp_srt) in &tmp_srts {
        match salvage_track_vtt(store, item_id, *stream_index, tmp_srt) {
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
        store.clear_item_progress(item_id);
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
        tracing::warn!(
            path = %src.display(),
            error = %e,
            "subtitle demux failed after per-track salvage"
        );
    }
    Ok((written, failed))
}

/// Publish one embedded track's completed WebVTT from its demux tmp. A tmp
/// with no cue text (empty stream, or a stream whose packets were never
/// reached) fails the track without touching any other file.
fn salvage_track_vtt(
    store: &SubsStore,
    item_id: i64,
    stream_index: u32,
    tmp_srt: &Path,
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
    write_webvtt(&store.vtt_path(item_id, &track_id), &body)?;
    store.mark_complete(item_id, &track_id);
    Ok(())
}

fn publish_growing_srts(
    store: &SubsStore,
    item_id: i64,
    tmp_srts: &[(u32, PathBuf)],
    last_sizes: &mut HashMap<u32, u64>,
) {
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
        if let Err(e) = store.publish_partial_vtt(item_id, &track_id, &body) {
            tracing::warn!(
                item_id,
                track_id = %track_id,
                error = %e,
                "partial subtitle publish failed"
            );
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
        let p = store.vtt_path(42, "e2");
        assert_eq!(p, dir.path().join("42").join("e2.vtt"));
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
        if !corpus.exists() {
            eprintln!("skipping: missing {}", corpus.display());
            return;
        }
        let streams = list_text_subtitles(&corpus).expect("list");
        assert!(
            !streams.is_empty(),
            "expected at least one text sub on SRT fixture"
        );
        let dir = tempfile::tempdir().unwrap();
        let store = SubsStore::new(dir.path().to_path_buf()).unwrap();
        let outcome = extract_item_subtitles(&store, 1, &corpus, &[], &|| false).expect("extract");
        assert_eq!(outcome, ExtractOutcome::Ready);
        let track_id = streams[0].track_id();
        let vtt = stored_webvtt(&store, 1, &track_id).unwrap();
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
        extract_item_subtitles(&store, 9, &mkv, &[], &|| false).expect("extract");
        assert!(store.has_vtt(9, &streams[0].track_id()));
        assert!(store.has_vtt(9, &streams[1].track_id()));
        let a = fs::read_to_string(store.vtt_path(9, &streams[0].track_id())).unwrap();
        let b = fs::read_to_string(store.vtt_path(9, &streams[1].track_id())).unwrap();
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
        let outcome =
            extract_item_subtitles(&store, item_id, &mkv, &[], &|| false).expect("extract");
        let ExtractOutcome::Partial { written, failed } = outcome else {
            panic!("expected per-track partial success, got {outcome:?}");
        };
        assert_eq!((written, failed), (2, 1), "{outcome:?}");
        let landed: Vec<&TextSubtitleStream> = streams
            .iter()
            .filter(|s| store.has_vtt(item_id, &s.track_id()))
            .collect();
        let missing: Vec<&TextSubtitleStream> = streams
            .iter()
            .filter(|s| !store.has_vtt(item_id, &s.track_id()))
            .collect();
        assert_eq!(landed.len(), 2, "{streams:?}");
        assert_eq!(missing.len(), 1, "{streams:?}");
        let a = fs::read_to_string(store.vtt_path(item_id, &landed[0].track_id())).unwrap();
        let b = fs::read_to_string(store.vtt_path(item_id, &landed[1].track_id())).unwrap();
        assert!(
            (a.contains("Good A") && b.contains("Good B"))
                || (a.contains("Good B") && b.contains("Good A")),
            "{a}\n---\n{b}"
        );
        assert!(
            !store.has_vtt(item_id, &missing[0].track_id()),
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
        let outcome =
            extract_item_subtitles(&store, item_id, &truncated, &[], &|| false).expect("extract");
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
            .filter(|s| store.has_vtt(item_id, &s.track_id()))
            .collect();
        assert_eq!(landed.len(), 2, "{streams:?}");
        let a = fs::read_to_string(store.vtt_path(item_id, &landed[0].track_id())).unwrap();
        let b = fs::read_to_string(store.vtt_path(item_id, &landed[1].track_id())).unwrap();
        assert!(
            (a.contains("Good A") && b.contains("Good B"))
                || (a.contains("Good B") && b.contains("Good A")),
            "{a}\n---\n{b}"
        );
    }

    /// ADR-0041 Decision 8.5: a pass that fails after producing zero usable
    /// output must not remove a previously-good track. The old code wiped the
    /// whole item directory up front; now the prior vtt survives any failure.
    #[test]
    fn failed_pass_keeps_previously_good_tracks() {
        let dir = tempfile::tempdir().unwrap();
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        let item_id = 31i64;
        let prior = "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nPrior cue\n";
        write_webvtt(&store.vtt_path(item_id, "e2"), prior).unwrap();
        let missing = dir.path().join("gone").join("Movie.mkv");
        let err = extract_item_subtitles(&store, item_id, &missing, &[], &|| false)
            .expect_err("probe must fail on a missing source");
        assert!(!err.is_empty(), "{err:?}");
        assert!(
            store.has_vtt(item_id, "e2"),
            "a failed pass must not delete a previously-good track"
        );
        assert_eq!(
            fs::read_to_string(store.vtt_path(item_id, "e2")).unwrap(),
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
        write_webvtt(
            &store.vtt_path(item_id, "e2"),
            "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nEmb\n",
        )
        .unwrap();
        let embedded_before = fs::read_to_string(store.vtt_path(item_id, "e2")).unwrap();

        let srt_path = dir.path().join("Movie.en.srt");
        fs::write(
            &srt_path,
            "1\n00:00:00,000 --> 00:00:01,000\nSidecar hello\n",
        )
        .unwrap();
        // Simulate only writing the new sidecar track into an existing item dir
        // the way a mistaken ordinal scheme would collide; our namespaces must not.
        write_sidecar_webvtt(
            &store,
            item_id,
            &SidecarInput {
                track_id: "s-en".into(),
                path: srt_path,
                format: "srt".into(),
            },
        )
        .unwrap();

        assert!(store.has_vtt(item_id, "e2"));
        assert!(store.has_vtt(item_id, "s-en"));
        assert_eq!(
            fs::read_to_string(store.vtt_path(item_id, "e2")).unwrap(),
            embedded_before,
            "adding a sidecar must not renumber or overwrite embedded e2"
        );
        let side = fs::read_to_string(store.vtt_path(item_id, "s-en")).unwrap();
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
        let outcome = extract_item_subtitles(
            &store,
            7,
            &video,
            &[SidecarInput {
                track_id: "s-en".into(),
                path: srt_path,
                format: "srt".into(),
            }],
            &|| false,
        )
        .expect("sidecar-only extract");
        assert_eq!(outcome, ExtractOutcome::Ready);
        let body = fs::read_to_string(store.vtt_path(7, "s-en")).unwrap();
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
        let err = extract_item_subtitles(
            &store,
            8,
            &video,
            &[SidecarInput {
                track_id: "s-en".into(),
                path: missing,
                format: "srt".into(),
            }],
            &|| false,
        )
        .unwrap_err();
        assert!(err.starts_with("unavailable:"), "{err}");
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
        if !corpus.exists() {
            eprintln!("skipping: missing {}", corpus.display());
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        let err = extract_item_subtitles(&store, 51, &corpus, &[], &|| true).unwrap_err();
        assert!(err.starts_with("unavailable:"), "{err}");
        // Killed before any cue was flushed: no track may land as complete.
        let dir = store.item_dir(51);
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
        write_webvtt(&store.vtt_path(1, "e2"), "WEBVTT\n").unwrap();
        write_webvtt(&store.vtt_path(2, "e2"), "WEBVTT\n").unwrap();
        let n = store.cleanup_orphans(&[1]).unwrap();
        assert_eq!(n, 1);
        assert!(store.item_dir(1).exists());
        assert!(!store.item_dir(2).exists());
    }

    #[test]
    fn invalidation_rewrites_on_fresh_extract() {
        if skip_without_ffmpeg() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files/h264_aac_srt_mkv.mkv");
        if !corpus.exists() {
            eprintln!("skipping: missing {}", corpus.display());
            return;
        }
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        extract_item_subtitles(&store, 3, &corpus, &[], &|| false).unwrap();
        let streams = list_text_subtitles(&corpus).unwrap();
        let track = streams[0].track_id();
        let first = fs::read_to_string(store.vtt_path(3, &track)).unwrap();
        // Stale marker file that must disappear when we re-extract into a fresh dir.
        write_webvtt(&store.vtt_path(3, "e999"), "WEBVTT\n\nstale\n").unwrap();
        assert!(store.has_vtt(3, "e999"));
        extract_item_subtitles(&store, 3, &corpus, &[], &|| false).unwrap();
        assert!(
            !store.has_vtt(3, "e999"),
            "re-extract must clear prior generation"
        );
        let second = fs::read_to_string(store.vtt_path(3, &track)).unwrap();
        assert!(second.contains("WEBVTT"));
        assert_eq!(first.lines().next(), second.lines().next());
    }

    #[test]
    fn progressive_partial_bumps_revision_and_readiness() {
        let dir = tempfile::tempdir().unwrap();
        let store = SubsStore::new(dir.path().to_path_buf()).unwrap();
        fs::create_dir_all(store.item_dir(7)).unwrap();
        store.set_progress(7, "e2", TrackReadiness::Preparing, 0);
        let (r0, rev0) = store.track_readiness(7, "e2", "pending");
        assert_eq!(r0, TrackReadiness::Preparing);
        assert_eq!(rev0, 0);

        let partial = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nHello\n";
        store.publish_partial_vtt(7, "e2", partial).unwrap();
        let (r1, rev1) = store.track_readiness(7, "e2", "pending");
        assert_eq!(r1, TrackReadiness::Partial);
        assert_eq!(rev1, 1);
        assert!(store.has_vtt(7, "e2"));

        let grown = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nHello\n\n00:00:03.000 --> 00:00:04.000\nWorld\n";
        store.publish_partial_vtt(7, "e2", grown).unwrap();
        let (r2, rev2) = store.track_readiness(7, "e2", "pending");
        assert_eq!(r2, TrackReadiness::Partial);
        assert!(rev2 > rev1);

        // Same length does not bump.
        store.publish_partial_vtt(7, "e2", grown).unwrap();
        let (_, rev3) = store.track_readiness(7, "e2", "pending");
        assert_eq!(rev3, rev2);

        store.mark_complete(7, "e2");
        let (r4, rev4) = store.track_readiness(7, "e2", "pending");
        assert_eq!(r4, TrackReadiness::Complete);
        assert!(rev4 > rev3);
    }

    #[test]
    fn readiness_without_progress_map_uses_disk_and_item_status() {
        let dir = tempfile::tempdir().unwrap();
        let store = SubsStore::new(dir.path().to_path_buf()).unwrap();
        let (prep, _) = store.track_readiness(1, "e2", "pending");
        assert_eq!(prep, TrackReadiness::Preparing);

        fs::create_dir_all(store.item_dir(1)).unwrap();
        write_webvtt(
            &store.vtt_path(1, "e2"),
            "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nx\n",
        )
        .unwrap();
        let (partial, _) = store.track_readiness(1, "e2", "pending");
        assert_eq!(partial, TrackReadiness::Partial);
        let (complete, _) = store.track_readiness(1, "e2", "ready");
        assert_eq!(complete, TrackReadiness::Complete);
    }

    #[test]
    fn session_inline_prep_slices_first_segment_without_scan_extract() {
        if skip_without_ffmpeg() {
            return;
        }
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files/h264_aac_srt_mkv.mkv");
        if !corpus.exists() {
            eprintln!("skipping: missing {}", corpus.display());
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
            &store.vtt_path(item_id, "e2"),
            "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nPrior cue\n",
        )
        .unwrap();
        store
            .publish_item_vtt(
                item_id,
                "s-en",
                "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nNew cue\n",
            )
            .unwrap();
        assert!(store.has_vtt(item_id, "e2"), "prior track must survive");
        assert!(store.has_vtt(item_id, "s-en"));
        assert_eq!(
            fs::read_to_string(store.vtt_path(item_id, "e2")).unwrap(),
            "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nPrior cue\n",
            "prior track body must be byte-identical"
        );
        let new = fs::read_to_string(store.vtt_path(item_id, "s-en")).unwrap();
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
}
