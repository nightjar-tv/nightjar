//! HLS playback sessions (ADR-0007). A session either stream-copies the
//! source (remux) or re-encodes it (transcode); the two differ by
//! [`SessionMode`] and nothing else (ADR-0011).
//!
//! ADR-0020: producer-owned boundaries. Each encode/copy run writes into
//! `run_<n>/`; a session-global time-keyed map (`seg_<ms:011>.m4s`) is the
//! serve truth. Playlists are assembled from that map (EVENT while cooking,
//! ENDLIST at run EOF) under a fresh URI per run. Clients seek via
//! `POST /sessions/{id}/seek?startMs=` → fresh `playlistUrl`, not by
//! mutating one VOD or poking segment URIs.
//!
//! Fill-forward: FFmpeg starts at the play land ([`ENCODE_LEAD_SEGMENTS`] is
//! 0 under producer-truth playlists; the old Safari dig-back lead fitted the
//! synthetic full-title VOD and does not carry). Mapped segments from prior
//! runs stay on disk so scrub-back is a plain file serve. Per-run dirs count
//! against [`SESSION_RUN_CACHE_BUDGET_BYTES`]; oldest finished runs evict.

use super::audio::stereo_downmix_filter;
use super::subs::{
    BurnInKind, BurnInSelection, SessionSubInput, extract_embedded_ass, prepare_session_subtitles,
    slice_webvtt, webvtt_max_cue_end_ms,
};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const DEFAULT_MAX_SESSIONS: usize = 3;
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const REAPER_TICK: Duration = Duration::from_secs(5);
/// Per-session on-disk budget for run dirs (ADR-0020 §12). Oldest finished
/// (non-current) runs are evicted first when exceeded. Override with
/// `NIGHTJAR_HLS_SESSION_CACHE_BYTES` for local experiments only.
const SESSION_RUN_CACHE_BUDGET_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// EOF this far short of probed duration → record usable extent (damaged).
const USABLE_SHORTFALL_MS: u64 = 30_000;
/// Locked HLS segment duration for **transcode** force-IDR / subtitle VTT
/// grid (ADR-0008 / ADR-0010). Copy segment durations come from the producer.
const SEGMENT_MS: u64 = 2000;
/// How long a segment or init fetch may block before returning 503. Mid-title
/// hardware transcodes on a NAS library can exceed 15s (dogfood: ~16s to
/// seg1098 after a Chrome seek on Up 1080p).
const SEGMENT_WAIT: Duration = Duration::from_secs(30);
const SEGMENT_POLL: Duration = Duration::from_millis(100);
/// Still justified under producer-truth: EVENT playlists list segments the
/// producer is still writing; Safari prefetches ~two past the on-disk
/// frontier. Those GETs Wait (cook), they do not scrub. Far scrub is
/// `POST /seek`, not a segment miss past this band.
const CATCH_UP_SEGMENTS: u64 = 2;
/// Safari retried refused segments at one-second intervals. A two-second floor
/// prevents adjacent prefetch misses from repeatedly moving the encode window.
const RESTART_MIN_INTERVAL: Duration = Duration::from_secs(2);
/// After the latest scrub intent while the prior encode has already landed,
/// wait this quiet period before killing FFmpeg. Rapid scrubs only update
/// the pending target (dogfood: three `seek restart` lines in ~9s; the last
/// fired 45ms after the previous `first_segment_ready`).
const RESTART_COALESCE_QUIET: Duration = Duration::from_millis(400);
/// Re-derived under ADR-0020: after `POST /seek` + source swap, in-flight
/// GETs for mapped segments behind the new play land can still arrive. Serving
/// them paints the prior scrub keyframe. Short TTL covers cook+retarget; then
/// scrub-back via the global map is a plain file serve again. Not the old
/// "Safari 503-retrying a superseded full-title land" band.
const STALE_RETAIN_REFUSE: Duration = Duration::from_secs(15);
/// Deleted under ADR-0020. Was the dig-back band for unlisted-but-requested
/// segments on the synthetic full-title VOD. Producer-truth playlists do not
/// list those URIs; far scrub is `POST /seek`. Kept as 0 so coalesce "far"
/// means any different pending land (see [`coalesce_preempt_before_land`]).
const ALIGN_BEHIND_SEGMENTS: u64 = 0;
/// Encode lead before play land. **0** under ADR-0020: the value 8 existed
/// so Safari dig-back behind `#EXT-X-START` on the synthetic full-title VOD
/// still hit lead-in files. That playlist is gone; window-relative START is
/// 0 and clients seek via the session API. Do not carry 8 as "still
/// meaningful as time." Override `NIGHTJAR_ENCODE_LEAD_SEGMENTS` only for
/// local experiments — not a shipped config surface.
const ENCODE_LEAD_SEGMENTS: u64 = 0;

/// Runtime lead-in. Default [`ENCODE_LEAD_SEGMENTS`].
fn encode_lead_segments() -> u64 {
    std::env::var("NIGHTJAR_ENCODE_LEAD_SEGMENTS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(ENCODE_LEAD_SEGMENTS)
}

fn session_run_cache_budget_bytes() -> u64 {
    std::env::var("NIGHTJAR_HLS_SESSION_CACHE_BYTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(SESSION_RUN_CACHE_BUDGET_BYTES)
}

/// Sum of bytes under every `run_*` dir in a session cache directory.
fn session_disk_bytes(session_dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(session_dir) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with("run_") {
            total = total.saturating_add(dir_tree_bytes(&entry.path()));
        }
    }
    total
}

#[derive(Debug)]
pub enum StartSessionError {
    CapFull,
    Spawn(String),
}

#[derive(Debug)]
pub enum PlaylistError {
    NotFound,
    NotReady,
    /// Abandoned / superseded miss hold reached [`IDLE_TIMEOUT`] while the
    /// session still exists. Mapped to empty HTTP 204 (ADR-0011 §7): not
    /// 4xx/5xx so Safari does not see an application-level media failure
    /// after a long hold. Session teardown uses [`NotFound`].
    AbandonedHoldEnded,
    Failed(String),
}

/// What FFmpeg does with the source for this session (ADR-0011).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMode {
    /// Stream copy: codecs already play, only the container changes.
    Copy,
    /// Re-encode to H.264 + AAC.
    Transcode,
}

/// Which audio track a session maps, and the ceiling it must fit (ADR-0012).
/// Switching tracks is a new session, so this never changes in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSelection {
    /// Absolute ffprobe stream index; `None` maps the first audio stream.
    pub stream_index: Option<u32>,
    /// Channel count of the selected track.
    pub channels: u32,
    /// ffprobe `channel_layout` when present (`5.1`, `6.0`, …).
    pub channel_layout: Option<String>,
    /// Client ceiling from the capability profile.
    pub max_channels: u32,
}

impl AudioSelection {
    pub fn needs_downmix(&self) -> bool {
        self.channels > self.max_channels
    }
}

/// Pure window-move decision for an explicit playlist `?startMs=` seek.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowAction {
    /// Target on disk, or already cooking at this window — serve/wait.
    Serve,
    /// Restart FFmpeg at the aligned offset.
    Restart,
}

pub fn decide_window_action(
    requested_ms: u64,
    window_start_ms: u64,
    target_on_disk: bool,
) -> WindowAction {
    let aligned = align_to_segment(requested_ms);
    if target_on_disk || aligned == window_start_ms {
        WindowAction::Serve
    } else {
        WindowAction::Restart
    }
}

/// What to do when a segment is missing from disk (ADR-0011 amendment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentMissAction {
    /// In-window cooking, restart suppressed, or too soon since last restart.
    Wait,
    /// Move the encode window to the requested index.
    Restart,
}

/// Deliberate miss policy under ADR-0020 producer-truth playlists.
///
/// Segment GETs never move the encode window. Far scrub is
/// `POST /sessions/{id}/seek`. A miss is always Wait: listed-but-not-ready
/// cooks under fill-forward; unlisted URIs are 404'd by the asset path once
/// unreachable. The old behind-play Restart band ([`ALIGN_BEHIND_SEGMENTS`])
/// and ahead-of-frontier Restart past [`CATCH_UP_SEGMENTS`] fitted WebKit
/// requesting URIs the synthetic full-title VOD listed but the producer
/// never wrote — that playlist is gone.
pub fn decide_segment_miss(
    want_ms: u64,
    window_start_ms: u64,
    play_start_ms: u64,
    latest_on_disk_ms: Option<u64>,
    primed: bool,
    since_last_restart: Duration,
) -> SegmentMissAction {
    let _ = (
        want_ms,
        window_start_ms,
        play_start_ms,
        latest_on_disk_ms,
        primed,
        since_last_restart,
    );
    SegmentMissAction::Wait
}

/// How a scrub intent interacts with an in-flight or just-landed encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoalesceDesire {
    /// Already cooking or serving this play land.
    Nop,
    /// Record pending; apply when cooking land is ready, or earlier when
    /// [`coalesce_preempt_before_land`] allows (far pending + min interval).
    HoldInFlight,
    /// Record pending; apply after [`RESTART_COALESCE_QUIET`] of quiet.
    HoldDebounce,
}

/// Classify a scrub toward `want_play_ms` without mutating session state.
/// Used by [`desire_restart`] and unit tests (three rapid desires → last
/// pending, one apply).
pub fn classify_restart_desire(
    want_play_ms: u64,
    play_start_ms: u64,
    encode_start_ms_now: u64,
    first_segment_ready: bool,
) -> CoalesceDesire {
    let aligned = align_to_segment(want_play_ms);
    if encode_start_ms(aligned) == encode_start_ms_now && aligned == play_start_ms {
        return CoalesceDesire::Nop;
    }
    if !first_segment_ready {
        CoalesceDesire::HoldInFlight
    } else {
        CoalesceDesire::HoldDebounce
    }
}

/// Whether a recorded pending play land is due to apply.
///
/// When the cooking land is not ready yet, apply only if `allow_preempt`,
/// [`coalesce_preempt_before_land`] says the pending target is far outside
/// the near-land band, and [`RESTART_MIN_INTERVAL`] has elapsed since the
/// last restart (anti-thrash on the preempt path only — not a substitute
/// for the land gate). Near pending must still wait for the cooking land
/// (dogfood: seg415 after scrub to 1188 — yank before land left Safari
/// retrying the prior URI).
///
/// `allow_preempt` mirrors [`disable_preempt`]: unset leaves preempt **on**.
/// Pass `!disable_preempt()` from production callers.
pub fn pending_restart_due(
    first_segment_ready: bool,
    pending_play_ms: Option<u64>,
    pending_quiet_elapsed: Option<Duration>,
    apply_immediate: bool,
    cooking_play_ms: u64,
    since_last_restart: Duration,
    allow_preempt: bool,
) -> Option<u64> {
    let pending = pending_play_ms?;
    if !first_segment_ready {
        if allow_preempt
            && coalesce_preempt_before_land(cooking_play_ms, pending)
            && since_last_restart >= RESTART_MIN_INTERVAL
        {
            return Some(pending);
        }
        return None;
    }
    if apply_immediate {
        return Some(pending);
    }
    let elapsed = pending_quiet_elapsed?;
    if elapsed < RESTART_COALESCE_QUIET {
        return None;
    }
    Some(pending)
}

/// `NIGHTJAR_DISABLE_PREEMPT=1` (or `true`/`yes`): never preempt before land.
/// Unset (and any value other than an explicit disable) leaves preempt **on** —
/// the polarity measured as scrub-before-play pass under Config D.
fn disable_preempt() -> bool {
    matches!(
        std::env::var("NIGHTJAR_DISABLE_PREEMPT").as_deref(),
        Ok("1" | "true" | "TRUE" | "yes" | "YES")
    )
}

/// Whether [`restart_at`] may `stop_child` while the cooking encode is still
/// unfinished. Land-ready always kills (land-then-yank). Before land, a
/// cooking-land waiter blocks kill so dig-back can still see mid bytes;
/// zero waiters keep far-scrub preempt speed.
///
/// Pure helper for unit tests — production uses the same predicate under the
/// session mutex immediately before `stop_child`.
pub fn may_kill_cooking_encode(first_segment_ready: bool, cooking_land_waiter_count: u32) -> bool {
    first_segment_ready || cooking_land_waiter_count == 0
}

/// Optional pause after kill before the next FFmpeg spawn (`restart_at`).
/// `NIGHTJAR_RESTART_SPAWN_GAP_MS` — distinct from [`RESTART_MIN_INTERVAL`]
/// (decision gate). Used to probe whether rapid dual-init boundaries wedge
/// Safari while keeping preempt's fast target selection.
fn restart_spawn_gap() -> Option<Duration> {
    let ms: u64 = std::env::var("NIGHTJAR_RESTART_SPAWN_GAP_MS")
        .ok()?
        .parse()
        .ok()?;
    if ms == 0 {
        None
    } else {
        Some(Duration::from_millis(ms))
    }
}

/// Far pending may abandon an in-flight cook before its land exists.
///
/// Under ADR-0020 [`ALIGN_BEHIND_SEGMENTS`] is 0 (dig-back band deleted), so
/// any different pending land is "far" and may preempt. Near-identical
/// retargets (same aligned ms) still wait for the cooking land.
pub fn coalesce_preempt_before_land(cooking_play_ms: u64, pending_play_ms: u64) -> bool {
    let cooking = align_to_segment(cooking_play_ms);
    let pending = align_to_segment(pending_play_ms);
    if pending == cooking {
        return false;
    }
    let segs = if pending > cooking {
        (pending - cooking) / SEGMENT_MS
    } else {
        (cooking - pending) / SEGMENT_MS
    };
    segs > ALIGN_BEHIND_SEGMENTS
}

/// Whether a no-fill hold on `want_ms` should end with 503 once the committed
/// play land is ready. Land-ensure 200 does not fill WebKit's buffer; a held
/// dig-back GET can leave the player seeking with zero native land fetches
/// (desktop-native single scrub: seg126 held while land-ensure got seg129).
///
/// Release when the want will never fill under the new encode window
/// (`want` behind `encode_window_start_ms`), or when it is **far** behind
/// play. Near dig-back still inside the lead-in window stays held — lead
/// may still write it. Ahead-of-play / attach-window misses must not use
/// this path (that 503'd in-flight land-ensure while play was still 0).
pub fn no_fill_release_for_new_land(
    want_ms: u64,
    play_start_ms: u64,
    first_segment_ready: bool,
    encode_window_start_ms: u64,
) -> bool {
    if !first_segment_ready {
        return false;
    }
    let want = align_to_segment(want_ms);
    let play = align_to_segment(play_start_ms);
    if want >= play {
        return false;
    }
    let window = align_to_segment(encode_window_start_ms);
    if want < window {
        return true;
    }
    coalesce_preempt_before_land(want, play)
}

/// Missing segment that current policy will not [`desire_restart`] toward:
/// behind the encode window (abandoned / superseded prior land), or otherwise
/// never filled without a fresh `?startMs=`. Callers **hold** the connection
/// instead of 503/404 while the session lives.
pub fn segment_miss_unreachable(
    want_ms: u64,
    cooking_play_ms: u64,
    pending_play_ms: Option<u64>,
    window_start_ms: u64,
    play_start_ms: u64,
    latest_on_disk_ms: Option<u64>,
    primed: bool,
) -> bool {
    let want = align_to_segment(want_ms);
    let window = align_to_segment(window_start_ms);

    // Playlist scrub pending this exact land — about to cook.
    if pending_play_ms.is_some_and(|p| align_to_segment(p) == want) {
        return false;
    }

    // Behind encode window: lead-in / fill-forward will not write this index.
    // (Dig-back within ALIGN of a *new* play can still be behind that play's
    // window — that is abandoned, not in-window dig-back.)
    if want < window {
        return true;
    }

    // In-window dig-back near committed land: lead-in may still write it.
    if digback_behind_committed(cooking_play_ms, pending_play_ms, want) {
        return false;
    }

    let cool = decide_segment_miss(
        want,
        window,
        play_start_ms,
        latest_on_disk_ms,
        primed,
        RESTART_MIN_INTERVAL,
    );
    if cool == SegmentMissAction::Restart {
        return false;
    }

    // In window and Wait: fill-forward will produce it.
    false
}

/// Segment-miss desire that only nudges an existing pending land a few
/// segments forward is Safari prefetch, not a new scrub. Returns true when
/// the miss should **not** call [`desire_restart`].
///
/// Consults **pending only**, never cooking `play_start_ms`: a deliberate
/// short forward scrub (one segment) and buffer-ahead look identical on the
/// segment path alone; short scrubs land via playlist `?startMs=` instead.
pub fn prefetch_advances_pending(pending_play_ms: Option<u64>, want_play_ms: u64) -> bool {
    let Some(pending) = pending_play_ms else {
        return false;
    };
    let pending = align_to_segment(pending);
    let want = align_to_segment(want_play_ms);
    if want <= pending {
        return false;
    }
    (want - pending) / SEGMENT_MS <= CATCH_UP_SEGMENTS
}

/// Segment miss behind the committed land (cooking play and/or pending).
/// Under ADR-0020 any behind-committed GET is dig-back / stale — do not call
/// [`desire_restart`] (far scrub is `POST /seek`). The old
/// [`ALIGN_BEHIND_SEGMENTS`] near-band is deleted with the synthetic VOD.
pub fn digback_behind_committed(
    cooking_play_ms: u64,
    pending_play_ms: Option<u64>,
    want_play_ms: u64,
) -> bool {
    let want = align_to_segment(want_play_ms);
    let cooking = align_to_segment(cooking_play_ms);
    let committed = match pending_play_ms {
        Some(p) => align_to_segment(p).max(cooking),
        None => cooking,
    };
    want < committed
}

/// Whether a long-poll for `want_play_ms` should keep holding for fill or
/// treat the want as superseded (pending moved to a different scrub).
///
/// Superseded waiters use the no-fill hold (same as abandoned misses): no
/// 503/404 while the session lives. Dig-back pending a few segments *behind*
/// this land still counts as Hold — do not starve the deliberate land
/// waiter for a near-ALIGN steal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingWaiterAction {
    Hold,
    Release,
}

/// Whether a long-poll for `want_play_ms` should keep holding for fill or
/// treat the want as superseded (pending moved to a different scrub).
///
/// Exact pending match Holds. Any other want Releases — the old ALIGN near
/// band for dig-back pending is gone with producer-truth playlists.
pub fn pending_waiter_action(
    pending_play_ms: Option<u64>,
    want_play_ms: u64,
) -> PendingWaiterAction {
    let Some(pending) = pending_play_ms else {
        return PendingWaiterAction::Hold;
    };
    let pending = align_to_segment(pending);
    let want = align_to_segment(want_play_ms);
    if pending == want {
        PendingWaiterAction::Hold
    } else {
        PendingWaiterAction::Release
    }
}

pub struct HlsSessionRegistry {
    root: PathBuf,
    max_sessions: usize,
    /// Verified H.264 encoder name from ADR-0009 probe (`libx264` fallback).
    video_encoder: String,
    next_id: AtomicU64,
    sessions: Mutex<HashMap<String, Session>>,
}

/// Serveable text track snapshot taken at session create (ADR-0010 / ADR-0013).
/// Mid-session sidecar additions do not appear until the next session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsSubtitleTrack {
    pub track_id: String,
    pub language: Option<String>,
    pub name: String,
    pub is_default: bool,
    pub forced: bool,
    pub sdh: bool,
    /// Item id (for logging / future use).
    pub item_id: i64,
    /// Embedded stream index, or None for a sidecar.
    pub stream_index: Option<u32>,
    /// Sidecar file path when `stream_index` is None.
    pub sidecar_path: Option<PathBuf>,
    /// Source codec / sidecar format (subrip, srt, vtt, …).
    pub codec: String,
    /// When set, 2s HLS segments are sliced from this on-disk item VTT
    /// (ready extract). No session demux. When None, session-inline demux
    /// writes `subs/{trackId}/full.vtt` instead.
    pub item_vtt_path: Option<PathBuf>,
}

struct Session {
    item_id: i64,
    src: PathBuf,
    dir: PathBuf,
    mode: SessionMode,
    audio: AudioSelection,
    /// Burn-in baked into this session's encode (ADR-0018). Seek restarts
    /// keep the same selection; switching burn-in is a fresh POST.
    burn_in: Option<BurnInSelection>,
    /// Actual encoder for this process. Future fallback updates this field.
    video_encoder: String,
    /// Encode window start for the current run (`-ss` / lead-in).
    start_ms: u64,
    /// Client land point / seek intent (title-absolute).
    play_start_ms: u64,
    /// Producer-observed land (first mapped segment start after the latest
    /// run began). Exposed on the session API (ADR-0020).
    landed_ms: u64,
    /// Lazy usable extent when EOF is materially short of [`Self::duration_ms`].
    usable_extent_ms: Option<u64>,
    duration_ms: u64,
    /// Current producer run id; playlist URI is per-run (ADR-0020).
    current_run_id: u64,
    /// Next run id to allocate on restart.
    next_run_id: u64,
    /// Session-global time-keyed segment map (ADR-0020).
    segment_map: crate::hls_segment_map::SegmentMap,
    /// True after the current run's ffmpeg exited successfully (ENDLIST).
    current_run_eof: bool,
    child: Option<Child>,
    last_access: Instant,
    /// Last encode-window restart (create counts as one) for the min-interval guard.
    last_restart: Instant,
    /// True after serving at least one segment at or past encode `start_ms`.
    primed: bool,
    /// Set once the play land is present in the segment map.
    first_segment_ready: bool,
    /// Latest aligned play land requested while coalescing rapid scrubs.
    pending_play_ms: Option<u64>,
    /// When [`Session::pending_play_ms`] was last updated (debounce clock).
    pending_since: Option<Instant>,
    /// Refuse retained behind-play serves until this instant (see
    /// [`STALE_RETAIN_REFUSE`]). Cleared when elapsed, or when the new play
    /// land appears ([`note_first_segment_ready`]) so Safari is not stuck
    /// 503-retrying a superseded middle land for the full TTL after cook.
    stale_retain_refuse_until: Option<Instant>,
    failed: Option<String>,
    /// Tracks declared in the master, snapshotted at create.
    subtitle_tracks: Vec<HlsSubtitleTrack>,
    /// Refcount of in-flight [`HlsSessionRegistry::asset_wait`] calls keyed by
    /// aligned want_ms. Used to defer preempt kill while a client still holds
    /// the cooking land (native dig-back / land-ensure).
    segment_waiters: HashMap<u64, u32>,
    /// Avoid log spam while polls re-hit deferred preempt before land.
    preempt_defer_logged: bool,
}

/// Snapshot returned by start / seek / get (ADR-0020 wire fields).
#[derive(Debug, Clone)]
pub struct SessionView {
    pub session_id: String,
    pub item_id: i64,
    pub playlist_url: String,
    pub video_encoder: String,
    pub encoder_kind: EncoderKind,
    pub landed_ms: u64,
    pub usable_extent_ms: Option<u64>,
    pub run_id: u64,
}

fn playlist_url_for(session_id: &str, run_id: u64) -> String {
    format!("/api/v0/sessions/{session_id}/runs/{run_id}/master.m3u8")
}

fn run_dir(session: &Session) -> PathBuf {
    session.dir.join(format!("run_{}", session.current_run_id))
}

fn write_run_encode_start(run_dir: &Path, start_ms: u64) -> Result<(), String> {
    fs::write(run_dir.join("encode_start_ms"), start_ms.to_string())
        .map_err(|e| format!("write encode_start_ms {}: {e}", run_dir.display()))
}

fn read_run_encode_start(run_dir: &Path) -> u64 {
    fs::read_to_string(run_dir.join("encode_start_ms"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn sync_segment_map(session: &mut Session) {
    let run = run_dir(session);
    let index_path = run.join("index.m3u8");
    let Ok(text) = fs::read_to_string(&index_path) else {
        return;
    };
    let encode_start_ms = read_run_encode_start(&run);
    if let Err(e) = crate::hls_segment_map::ingest_run_index(
        &mut session.segment_map,
        &session.dir,
        session.current_run_id,
        &text,
        encode_start_ms,
    ) {
        tracing::warn!(
            run_id = session.current_run_id,
            error = %e,
            "hls map ingest failed"
        );
    }
}

/// Re-read every `run_*/index.m3u8` so scrub-back map hits see prior runs
/// even if the current run's index is empty after stop_child.
fn sync_all_run_indexes(session: &mut Session) {
    let Ok(entries) = fs::read_dir(&session.dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(id_str) = name.strip_prefix("run_") else {
            continue;
        };
        let Ok(run_id) = id_str.parse::<u64>() else {
            continue;
        };
        let run_path = entry.path();
        let index_path = run_path.join("index.m3u8");
        let Ok(text) = fs::read_to_string(&index_path) else {
            continue;
        };
        let encode_start_ms = read_run_encode_start(&run_path);
        if let Err(e) = crate::hls_segment_map::ingest_run_index(
            &mut session.segment_map,
            &session.dir,
            run_id,
            &text,
            encode_start_ms,
        ) {
            tracing::warn!(run_id, error = %e, "hls map ingest failed (all-runs sync)");
        }
    }
}

fn latest_mapped_start_in_window(
    map: &crate::hls_segment_map::SegmentMap,
    window_start_ms: u64,
) -> Option<u64> {
    map.iter_ordered()
        .rev()
        .find(|s| s.start_ms >= window_start_ms)
        .map(|s| s.start_ms)
}

fn current_run_has_mapped_segment(session: &Session) -> bool {
    // Match [`build_run_media_playlist`]: map rows without bytes must not
    // flip ready (header-only playlist / listed-404 class under ADR-0020).
    let in_playlist_window = |s: &crate::hls_segment_map::MappedSegment| {
        s.start_ms.saturating_add(s.duration_ms) > session.start_ms
            && session.dir.join(&s.rel_path).is_file()
    };
    if session
        .segment_map
        .iter_ordered()
        .any(|s| s.run_id == session.current_run_id && in_playlist_window(s))
    {
        return true;
    }
    // Duplicate-write stop: fresh run id with no new producer bytes; playlist
    // is assembled from the global map (prior runs).
    session.child.is_none() && session.segment_map.iter_ordered().any(in_playlist_window)
}

fn first_current_run_start(session: &Session) -> Option<u64> {
    let in_playlist_window = |s: &crate::hls_segment_map::MappedSegment| {
        s.start_ms.saturating_add(s.duration_ms) > session.start_ms
            && session.dir.join(&s.rel_path).is_file()
    };
    if let Some(ms) = session
        .segment_map
        .iter_ordered()
        .find(|s| s.run_id == session.current_run_id && in_playlist_window(s))
        .map(|s| s.start_ms)
    {
        return Some(ms);
    }
    session
        .segment_map
        .iter_ordered()
        .find(|s| in_playlist_window(s))
        .map(|s| s.start_ms)
}

fn build_run_media_playlist(session_id: &str, session: &Session) -> Vec<u8> {
    let window = session.start_ms;
    // ADR-0020: never list a URI whose bytes are gone. Eviction updates the
    // map, but defend in depth so a race cannot reintroduce listed-404.
    let segs: Vec<&crate::hls_segment_map::MappedSegment> = session
        .segment_map
        .iter_ordered()
        .filter(|s| s.start_ms.saturating_add(s.duration_ms) > window)
        .filter(|s| session.dir.join(&s.rel_path).is_file())
        .collect();
    // Path-absolute URIs (ADR-0008): run-dir depth cannot break resolution.
    let init_uri = format!(
        "/api/v0/sessions/{session_id}/runs/{}/init.mp4",
        session.current_run_id
    );
    let bytes =
        crate::hls_segment_map::build_map_playlist(&segs, &init_uri, session.current_run_eof);
    with_session_absolute_segment_uris(session_id, &bytes)
}

/// Rewrite bare `seg_<ms>.m4s` lines to path-absolute session asset URLs.
/// Relative `../` climbs were the cutover failure class under `/runs/{n}/`.
fn with_session_absolute_segment_uris(session_id: &str, playlist: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(playlist);
    let mut out = String::with_capacity(text.len() + 64);
    for line in text.lines() {
        if let Some(ms) = crate::hls_segment_map::parse_time_keyed_segment_name(line) {
            out.push_str(&format!(
                "/api/v0/sessions/{session_id}/{}",
                crate::hls_segment_map::time_keyed_segment_name(ms)
            ));
            out.push('\n');
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.into_bytes()
}

fn dir_tree_bytes(path: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            total = total.saturating_add(dir_tree_bytes(&p));
        } else if let Ok(meta) = entry.metadata() {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

/// Per-run cache eviction (ADR-0020 §12). Map is authoritative:
/// - Prefer orphan run dirs (no map refs) so scrub-back stays a file serve.
/// - When a referenced run must go, `remove_run` before unlinking.
/// - Zero-byte dirs are reaped quietly — not budget evictions.
fn maybe_evict_finished_runs(session: &mut Session) {
    reap_empty_finished_run_dirs(session);
    let budget = session_run_cache_budget_bytes();
    loop {
        let total = session_disk_bytes(&session.dir);
        if total <= budget {
            return;
        }
        let referenced = session.segment_map.referenced_run_ids();
        let mut orphans: Vec<(u64, u64)> = Vec::new();
        let mut referenced_finished: Vec<(u64, u64)> = Vec::new();
        let Ok(entries) = fs::read_dir(&session.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(id_str) = name.strip_prefix("run_") else {
                continue;
            };
            let Ok(id) = id_str.parse::<u64>() else {
                continue;
            };
            if id == session.current_run_id {
                continue;
            }
            let bytes = dir_tree_bytes(&entry.path());
            if bytes == 0 {
                continue;
            }
            if referenced.contains(&id) {
                referenced_finished.push((id, bytes));
            } else {
                orphans.push((id, bytes));
            }
        }
        orphans.sort_by_key(|(id, _)| *id);
        referenced_finished.sort_by_key(|(id, _)| *id);
        let victim = orphans
            .first()
            .copied()
            .or_else(|| referenced_finished.first().copied());
        let Some((victim_id, victim_bytes)) = victim else {
            return;
        };
        let path = session.dir.join(format!("run_{victim_id}"));
        let had_map_refs = session.segment_map.run_is_referenced(victim_id);
        // Drop map entries before unlinking so serve never sees a mapped
        // path whose file is already gone.
        session.segment_map.remove_run(victim_id);
        if let Err(e) = fs::remove_dir_all(&path) {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "hls run eviction failed"
            );
            return;
        }
        let after = session_disk_bytes(&session.dir);
        tracing::info!(
            run_id = victim_id,
            evicted_bytes = victim_bytes,
            had_map_refs,
            orphan = !had_map_refs,
            session_disk_bytes_before = total,
            session_disk_bytes = after,
            budget_bytes = budget,
            session_dir = %session.dir.display(),
            "hls evicted finished run (cache budget)"
        );
    }
}

/// Remove finished run directories that hold no bytes. Not a budget eviction.
fn reap_empty_finished_run_dirs(session: &mut Session) {
    let Ok(entries) = fs::read_dir(&session.dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(id_str) = name.strip_prefix("run_") else {
            continue;
        };
        let Ok(id) = id_str.parse::<u64>() else {
            continue;
        };
        if id == session.current_run_id {
            continue;
        }
        if dir_tree_bytes(&entry.path()) > 0 {
            continue;
        }
        if session.segment_map.run_is_referenced(id) {
            session.segment_map.remove_run(id);
        }
        let _ = fs::remove_dir_all(entry.path());
    }
}

fn session_view(session_id: &str, session: &Session) -> SessionView {
    let encoder_kind = if session.mode == SessionMode::Copy && session.burn_in.is_none() {
        EncoderKind::Copy
    } else if session.video_encoder == "libx264" {
        EncoderKind::Software
    } else {
        EncoderKind::Hardware
    };
    SessionView {
        session_id: session_id.to_string(),
        item_id: session.item_id,
        playlist_url: playlist_url_for(session_id, session.current_run_id),
        video_encoder: if session.mode == SessionMode::Copy && session.burn_in.is_none() {
            "copy".into()
        } else {
            session.video_encoder.clone()
        },
        encoder_kind,
        landed_ms: session.landed_ms,
        usable_extent_ms: session.usable_extent_ms,
        run_id: session.current_run_id,
    }
}

impl HlsSessionRegistry {
    /// Creates the HLS cache root, sweeps leftover session dirs from a prior
    /// process, and starts the idle reaper. `video_encoder` is the preferred
    /// verified H.264 encoder from ADR-0009 (`libx264` if nothing else works).
    pub fn new(root: PathBuf, video_encoder: impl Into<String>) -> Result<Arc<Self>, String> {
        Self::with_cap(root, DEFAULT_MAX_SESSIONS, video_encoder)
    }

    pub fn with_cap(
        root: PathBuf,
        max_sessions: usize,
        video_encoder: impl Into<String>,
    ) -> Result<Arc<Self>, String> {
        fs::create_dir_all(&root)
            .map_err(|e| format!("create hls cache dir {}: {e}", root.display()))?;
        for entry in fs::read_dir(&root)
            .map_err(|e| format!("read hls cache dir {}: {e}", root.display()))?
            .flatten()
        {
            let path = entry.path();
            if path.is_dir() {
                if let Err(e) = fs::remove_dir_all(&path) {
                    tracing::warn!(path = %path.display(), error = %e, "hls startup sweep failed");
                } else {
                    tracing::info!(path = %path.display(), "swept orphaned hls session dir");
                }
            }
        }

        let video_encoder = video_encoder.into();
        let registry = Arc::new(Self {
            root,
            max_sessions,
            video_encoder,
            next_id: AtomicU64::new(1),
            sessions: Mutex::new(HashMap::new()),
        });
        let reaper = Arc::clone(&registry);
        std::thread::Builder::new()
            .name("hls-reaper".into())
            .spawn(move || reaper.reaper_loop())
            .map_err(|e| format!("spawn hls reaper: {e}"))?;
        Ok(registry)
    }

    /// Starts a session at `start_ms` (aligned). Every call creates its own
    /// session; seeking restarts that session in place (ADR-0011). Switching
    /// audio or burn-in does not: it starts a fresh session (ADR-0012 /
    /// ADR-0018). `subtitle_tracks` is snapshotted here and never revisited.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        &self,
        item_id: i64,
        src: &Path,
        start_ms: u64,
        duration_ms: u64,
        mode: SessionMode,
        audio: AudioSelection,
        subtitle_tracks: Vec<HlsSubtitleTrack>,
        burn_in: Option<BurnInSelection>,
    ) -> Result<String, StartSessionError> {
        let play_start_ms = align_to_segment(start_ms);
        let start_ms = encode_start_ms(play_start_ms);
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| StartSessionError::Spawn("hls registry lock poisoned".into()))?;
        if sessions.len() >= self.max_sessions {
            return Err(StartSessionError::CapFull);
        }

        let id = format!("s{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let dir = self.root.join(&id);
        fs::create_dir_all(&dir).map_err(|e| {
            StartSessionError::Spawn(format!("create session dir {}: {e}", dir.display()))
        })?;
        let run_id = 0u64;
        let run_dir = dir.join(format!("run_{run_id}"));
        fs::create_dir_all(&run_dir).map_err(|e| {
            StartSessionError::Spawn(format!("create run dir {}: {e}", run_dir.display()))
        })?;
        write_run_encode_start(&run_dir, start_ms).map_err(StartSessionError::Spawn)?;
        // Release before ASS demux / ffmpeg spawn so a multi-minute NAS extract
        // does not freeze every other HLS request on this lock.
        drop(sessions);

        let spawn_started = Instant::now();
        let burn_in =
            prepare_ass_burn_file(src, &dir, burn_in).map_err(StartSessionError::Spawn)?;
        let child = spawn_ffmpeg(
            src,
            &run_dir,
            start_ms,
            mode,
            audio.clone(),
            &self.video_encoder,
            burn_in.as_ref(),
        )
        .map_err(StartSessionError::Spawn)?;
        // Only cold (non-store) tracks need a session demux. Ready tracks
        // point MEDIA at the item VTT and must not re-read the source.
        spawn_session_subtitle_worker(src, &dir, &subtitle_tracks);
        let spawn_ms = spawn_started.elapsed().as_millis();
        tracing::info!(
            session_id = %id,
            item_id,
            start_ms,
            play_start_ms,
            run_id,
            encode_lead_segments = encode_lead_segments(),
            mode = ?mode,
            audio_stream = ?audio.stream_index,
            audio_channels = audio.channels,
            burn_in = burn_in.as_ref().map(|b| b.track_id.as_str()),
            encoder = %self.video_encoder,
            spawn_ms,
            "hls session started"
        );
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| StartSessionError::Spawn("hls registry lock poisoned".into()))?;
        sessions.insert(
            id.clone(),
            Session {
                item_id,
                src: src.to_path_buf(),
                dir: dir.clone(),
                mode,
                audio,
                burn_in,
                video_encoder: self.video_encoder.clone(),
                start_ms,
                play_start_ms,
                landed_ms: play_start_ms,
                usable_extent_ms: None,
                duration_ms,
                current_run_id: run_id,
                next_run_id: 1,
                segment_map: crate::hls_segment_map::SegmentMap::default(),
                current_run_eof: false,
                child: Some(child),
                last_access: Instant::now(),
                last_restart: Instant::now(),
                primed: false,
                first_segment_ready: false,
                pending_play_ms: None,
                pending_since: None,
                stale_retain_refuse_until: None,
                failed: None,
                subtitle_tracks,
                segment_waiters: HashMap::new(),
                preempt_defer_logged: false,
            },
        );
        Ok(id)
    }

    pub fn item_id(&self, session_id: &str) -> Option<i64> {
        self.sessions
            .lock()
            .ok()?
            .get(session_id)
            .map(|s| s.item_id)
    }

    pub fn encoder(&self, session_id: &str) -> Option<SessionEncoder> {
        let sessions = self.sessions.lock().ok()?;
        let session = sessions.get(session_id)?;
        Some(match session.mode {
            SessionMode::Copy => SessionEncoder {
                name: "copy".into(),
                kind: EncoderKind::Copy,
            },
            SessionMode::Transcode => SessionEncoder {
                name: session.video_encoder.clone(),
                kind: if session.video_encoder == "libx264" {
                    EncoderKind::Software
                } else {
                    EncoderKind::Hardware
                },
            },
        })
    }

    /// Returns the media playlist for `run_id` (ADR-0020). `start_ms` on
    /// this path is ignored for seek — use [`Self::seek`].
    pub fn playlist(&self, session_id: &str, run_id: u64) -> Result<Vec<u8>, PlaylistError> {
        self.with_ready_session(session_id, run_id, |session| {
            let bytes = build_run_media_playlist(session_id, session);
            log_playlist_serve(
                session_id,
                "index.m3u8",
                None,
                session.play_start_ms,
                session.pending_play_ms,
                &bytes,
            );
            Ok(bytes)
        })
    }

    /// Returns the HLS master playlist for `run_id`. Media and subtitle URIs
    /// are path-absolute under `/api/v0/sessions/…` (ADR-0008).
    pub fn master(&self, session_id: &str, run_id: u64) -> Result<Vec<u8>, PlaylistError> {
        self.with_ready_session(session_id, run_id, |session| {
            let bytes = build_master(session_id, run_id, &session.subtitle_tracks);
            log_playlist_serve(
                session_id,
                "master.m3u8",
                None,
                session.play_start_ms,
                session.pending_play_ms,
                &bytes,
            );
            Ok(bytes)
        })
    }

    /// Apply a far scrub: new producer run + fresh playlist URI (ADR-0020).
    pub fn seek(&self, session_id: &str, start_ms: u64) -> Result<SessionView, PlaylistError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| PlaylistError::Failed("hls registry lock poisoned".into()))?;
        let session = sessions
            .get_mut(session_id)
            .ok_or(PlaylistError::NotFound)?;
        session.last_access = Instant::now();
        if let Some(err) = session.failed.clone() {
            return Err(PlaylistError::Failed(err));
        }
        let aligned = align_to_segment(start_ms);
        if aligned == session.play_start_ms {
            sync_segment_map(session);
            return Ok(session_view(session_id, session));
        }
        let enc = session.video_encoder.clone();
        match restart_at(session, aligned, &enc)? {
            RestartAtOutcome::Applied => {
                maybe_evict_finished_runs(session);
            }
            RestartAtOutcome::DeferredLandWaiter => {
                // Keep intent; client can poll view until the run advances.
                session.pending_play_ms = Some(aligned);
                session.pending_since = Some(Instant::now());
            }
        }
        Ok(session_view(session_id, session))
    }

    /// Current session wire snapshot (playlist URL, landed, usable extent).
    pub fn view(&self, session_id: &str) -> Result<SessionView, PlaylistError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| PlaylistError::Failed("hls registry lock poisoned".into()))?;
        let session = sessions
            .get_mut(session_id)
            .ok_or(PlaylistError::NotFound)?;
        session.last_access = Instant::now();
        let _ = note_child_exit(session);
        sync_segment_map(session);
        Ok(session_view(session_id, session))
    }

    /// Init (or other run-local file) under `run_<n>/`.
    pub fn run_asset(
        &self,
        session_id: &str,
        run_id: u64,
        name: &str,
    ) -> Result<Vec<u8>, PlaylistError> {
        if name != "init.mp4" {
            return Err(PlaylistError::NotFound);
        }
        let deadline = Instant::now() + SEGMENT_WAIT;
        loop {
            {
                let mut sessions = self
                    .sessions
                    .lock()
                    .map_err(|_| PlaylistError::Failed("hls registry lock poisoned".into()))?;
                let session = sessions
                    .get_mut(session_id)
                    .ok_or(PlaylistError::NotFound)?;
                session.last_access = Instant::now();
                if let Some(err) = session.failed.clone() {
                    return Err(PlaylistError::Failed(err));
                }
                let path = session.dir.join(format!("run_{run_id}")).join("init.mp4");
                if let Ok(bytes) = fs::read(&path) {
                    return Ok(bytes);
                }
                if let Some(err) = note_child_exit(session) {
                    return Err(PlaylistError::Failed(err));
                }
            }
            if Instant::now() >= deadline {
                return Err(PlaylistError::NotReady);
            }
            std::thread::sleep(SEGMENT_POLL);
        }
    }

    /// Multi-segment subtitle media playlist for a snapshotted track (plan item 2).
    pub fn subtitle_playlist(
        &self,
        session_id: &str,
        track_id: &str,
    ) -> Result<Vec<u8>, PlaylistError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| PlaylistError::Failed("hls registry lock poisoned".into()))?;
        let session = sessions
            .get_mut(session_id)
            .ok_or(PlaylistError::NotFound)?;
        session.last_access = Instant::now();
        if !session
            .subtitle_tracks
            .iter()
            .any(|t| t.track_id == track_id)
        {
            return Err(PlaylistError::NotFound);
        }
        // Hold until video is ready so clients attach media + subs together.
        sync_segment_map(session);
        if !current_run_has_mapped_segment(session) {
            if let Some(err) = note_child_exit(session) {
                return Err(PlaylistError::Failed(err));
            }
            return Err(PlaylistError::NotReady);
        }
        let track = session
            .subtitle_tracks
            .iter()
            .find(|t| t.track_id == track_id)
            .expect("track checked above");
        Ok(build_subtitle_playlist_for(track, session.duration_ms))
    }

    /// Sliced WebVTT for one 2s window (`subs/{trackId}/segNNN.vtt`).
    pub fn subtitle_segment(
        &self,
        session_id: &str,
        track_id: &str,
        segment_idx: u64,
    ) -> Result<Vec<u8>, PlaylistError> {
        let deadline = Instant::now() + SEGMENT_WAIT;
        loop {
            let result = {
                let mut sessions = self
                    .sessions
                    .lock()
                    .map_err(|_| PlaylistError::Failed("hls registry lock poisoned".into()))?;
                let session = sessions
                    .get_mut(session_id)
                    .ok_or(PlaylistError::NotFound)?;
                session.last_access = Instant::now();
                if !session
                    .subtitle_tracks
                    .iter()
                    .any(|t| t.track_id == track_id)
                {
                    return Err(PlaylistError::NotFound);
                }
                read_subtitle_segment(session, track_id, segment_idx)
            };
            match result {
                Ok(bytes) => return Ok(bytes),
                Err(PlaylistError::NotReady) if Instant::now() < deadline => {
                    std::thread::sleep(SEGMENT_POLL);
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn with_ready_session<F>(
        &self,
        session_id: &str,
        run_id: u64,
        build: F,
    ) -> Result<Vec<u8>, PlaylistError>
    where
        F: FnOnce(&Session) -> Result<Vec<u8>, PlaylistError>,
    {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| PlaylistError::Failed("hls registry lock poisoned".into()))?;
        let session = sessions
            .get_mut(session_id)
            .ok_or(PlaylistError::NotFound)?;
        session.last_access = Instant::now();

        if let Some(err) = session.failed.clone() {
            return Err(PlaylistError::Failed(err));
        }
        if run_id != session.current_run_id {
            return Err(PlaylistError::NotFound);
        }

        if let Some(err) = note_child_exit(session) {
            return Err(PlaylistError::Failed(err));
        }

        sync_segment_map(session);
        if !current_run_has_mapped_segment(session) {
            // Producer EOF with nothing in-window (damaged mid-title land):
            // serve empty ENDLIST playlists so the client can read
            // usableExtentMs instead of hanging on master 503.
            if session.current_run_eof {
                return build(session);
            }
            return Err(PlaylistError::NotReady);
        }
        note_first_segment_ready(session_id, session);
        maybe_apply_pending_restart(session)?;
        build(session)
    }

    /// Serves init/segment files. Retained segments from a previous encode
    /// window stay readable. Missing segments in a cold region of the
    /// full-title VOD return 503 while a guarded restart cooks them
    /// (ADR-0011 amendment). Safari native scrub often hits this path only.
    ///
    /// Logs terminal outcomes here (not only in the HTTP route after
    /// `.await`) so a client-aborted long-poll that still finishes cooking
    /// is visible as `hls asset ready` without a matching route 200.
    ///
    /// `fetcher` is log-only (optional `njFetcher` query): JS land-ensure /
    /// attach-wait probes set it; Safari's native HLS engine does not. Used
    /// to tell probe traffic from WebKit's own segment GETs in dogfood logs.
    /// (Native instant-503 while cooking was tried and rejected: broke
    /// fill-forward prefetch and double-scrub dig-back.)
    pub fn asset(
        &self,
        session_id: &str,
        name: &str,
        fetcher: Option<&str>,
    ) -> Result<Vec<u8>, PlaylistError> {
        let t0 = Instant::now();
        let result = self.asset_wait(session_id, name);
        // Always log not-ready/fail. Log ready only when we waited (long-poll /
        // cook) so aborted holds show up even if the HTTP route never runs;
        // skip hot-path disk hits (route 200 is enough).
        let waited = t0.elapsed() > Duration::from_millis(100);
        let fetcher = fetcher.unwrap_or("-");
        match &result {
            Ok(bytes) if waited => {
                tracing::info!(
                    session_id,
                    asset = %name,
                    fetcher,
                    bytes = bytes.len(),
                    waited_ms = t0.elapsed().as_millis(),
                    "hls asset ready"
                );
            }
            Ok(_) => {}
            Err(PlaylistError::NotReady) => {
                tracing::info!(
                    session_id,
                    asset = %name,
                    fetcher,
                    waited_ms = t0.elapsed().as_millis(),
                    "hls asset not ready"
                );
            }
            Err(PlaylistError::NotFound) => {
                tracing::info!(
                    session_id,
                    asset = %name,
                    fetcher,
                    "hls asset not found"
                );
            }
            Err(PlaylistError::AbandonedHoldEnded) => {
                tracing::info!(
                    session_id,
                    asset = %name,
                    fetcher,
                    waited_ms = t0.elapsed().as_millis(),
                    "hls asset abandoned hold ended"
                );
            }
            Err(PlaylistError::Failed(err)) => {
                tracing::warn!(
                    session_id,
                    asset = %name,
                    fetcher,
                    error = %err,
                    "hls asset failed"
                );
            }
        }
        result
    }

    fn asset_wait(&self, session_id: &str, name: &str) -> Result<Vec<u8>, PlaylistError> {
        if !is_safe_asset(name) {
            return Err(PlaylistError::NotFound);
        }
        let file_name = name.to_string();
        let requested_ms = crate::hls_segment_map::parse_time_keyed_segment_name(name);
        // Register before the poll loop so a concurrent preempt sees this
        // waiter under the same mutex as stop_child (see restart_at).
        let _segment_waiter =
            requested_ms.and_then(|ms| SegmentWaiterGuard::attach(&self.sessions, session_id, ms));
        let mut deadline = Instant::now() + SEGMENT_WAIT;
        let mut holding_for_land = false;
        let mut holding_no_fill = false;
        let enter_no_fill = |reason: &str,
                             session_id: &str,
                             file_name: &str,
                             session: &Session,
                             holding_no_fill: &mut bool,
                             holding_for_land: &mut bool,
                             deadline: &mut Instant| {
            if !*holding_no_fill {
                *holding_no_fill = true;
                *holding_for_land = false;
                *deadline = Instant::now() + IDLE_TIMEOUT;
                tracing::info!(
                    session_id,
                    asset = %file_name,
                    play_start_ms = session.play_start_ms,
                    pending_play_ms = session.pending_play_ms,
                    hold_ms = IDLE_TIMEOUT.as_millis(),
                    reason,
                    "hls asset no-fill hold"
                );
            }
        };
        loop {
            {
                let mut sessions = self
                    .sessions
                    .lock()
                    .map_err(|_| PlaylistError::Failed("hls registry lock poisoned".into()))?;
                let session = sessions
                    .get_mut(session_id)
                    .ok_or(PlaylistError::NotFound)?;
                session.last_access = Instant::now();
                if let Some(err) = session.failed.clone() {
                    return Err(PlaylistError::Failed(err));
                }
                note_first_segment_ready(session_id, session);
                if let Some(err) = session.failed.clone() {
                    return Err(PlaylistError::Failed(err));
                }
                if let Some(want_ms) = requested_ms
                    && holding_for_land
                {
                    let superseded = pending_waiter_action(session.pending_play_ms, want_ms)
                        == PendingWaiterAction::Release
                        || (session.pending_play_ms.is_none()
                            && align_to_segment(session.play_start_ms)
                                != align_to_segment(want_ms));
                    if superseded {
                        let far = match session.pending_play_ms {
                            Some(p) => coalesce_preempt_before_land(want_ms, p),
                            None => coalesce_preempt_before_land(want_ms, session.play_start_ms),
                        };
                        if far {
                            tracing::info!(
                                session_id,
                                asset = %file_name,
                                play_start_ms = session.play_start_ms,
                                pending_play_ms = session.pending_play_ms,
                                want_ms,
                                "hls asset superseded far — 503 (no hold)"
                            );
                            return Err(PlaylistError::NotReady);
                        }
                        enter_no_fill(
                            "superseded",
                            session_id,
                            &file_name,
                            session,
                            &mut holding_no_fill,
                            &mut holding_for_land,
                            &mut deadline,
                        );
                    }
                }

                let resolved = if file_name == "init.mp4" {
                    fs::read(run_dir(session).join("init.mp4")).ok()
                } else if let Some(ms) = requested_ms {
                    sync_segment_map(session);
                    match session.segment_map.get(ms) {
                        Some(seg) => {
                            let abs = session.dir.join(&seg.rel_path);
                            match fs::read(&abs) {
                                Ok(bytes) => Some(bytes),
                                Err(_) => {
                                    // Map entry without bytes — drop this key
                                    // so we never keep advertising a dead URI.
                                    session.segment_map.remove_start(ms);
                                    None
                                }
                            }
                        }
                        None => None,
                    }
                } else {
                    None
                };

                if let Some(bytes) = resolved {
                    let play_before = session.play_start_ms;
                    if let Some(ms) = requested_ms
                        && ms >= session.start_ms
                    {
                        session.primed = true;
                    }
                    note_first_segment_ready(session_id, session);
                    maybe_apply_pending_restart(session)?;
                    if !serve_ok_after_pending_apply(
                        play_before,
                        session.play_start_ms,
                        requested_ms,
                    ) {
                        return Err(PlaylistError::NotReady);
                    }
                    if let Some(ms) = requested_ms {
                        let guard = match session.stale_retain_refuse_until {
                            Some(until) if Instant::now() < until => true,
                            Some(_) => {
                                session.stale_retain_refuse_until = None;
                                false
                            }
                            None => false,
                        };
                        if !serve_ok_retained_during_stale_guard(ms, session.play_start_ms, guard) {
                            return Err(PlaylistError::NotReady);
                        }
                    }
                    return Ok(bytes);
                }
                if let Some(err) = note_child_exit(session) {
                    return Err(PlaylistError::Failed(err));
                }
                maybe_apply_pending_restart(session)?;
                if file_name == "init.mp4" {
                    // Rewritten on restart; wait for the new init.
                } else if let Some(want_ms) = requested_ms {
                    let window_start = session.start_ms;
                    let play_start = session.play_start_ms;
                    let latest = latest_segment_in_window(&session.segment_map, window_start);
                    let since = session.last_restart.elapsed();

                    if holding_no_fill
                        || segment_miss_unreachable(
                            want_ms,
                            session.play_start_ms,
                            session.pending_play_ms,
                            session.start_ms,
                            session.play_start_ms,
                            latest,
                            session.primed,
                        )
                    {
                        if !holding_no_fill {
                            enter_no_fill(
                                "abandoned",
                                session_id,
                                &file_name,
                                session,
                                &mut holding_no_fill,
                                &mut holding_for_land,
                                &mut deadline,
                            );
                        } else if no_fill_release_for_new_land(
                            want_ms,
                            session.play_start_ms,
                            session.first_segment_ready,
                            session.start_ms,
                        ) {
                            tracing::info!(
                                session_id,
                                asset = %file_name,
                                play_start_ms = session.play_start_ms,
                                want_ms,
                                "hls asset no-fill release (new land ready)"
                            );
                            return Err(PlaylistError::NotReady);
                        }
                    } else {
                        let scrub_shaped = decide_segment_miss(
                            want_ms,
                            window_start,
                            play_start,
                            latest,
                            session.primed,
                            RESTART_MIN_INTERVAL,
                        ) == SegmentMissAction::Restart;
                        match decide_segment_miss(
                            want_ms,
                            window_start,
                            play_start,
                            latest,
                            session.primed,
                            since,
                        ) {
                            SegmentMissAction::Restart => {
                                if prefetch_advances_pending(session.pending_play_ms, want_ms) {
                                    return Err(PlaylistError::NotReady);
                                }
                                if digback_behind_committed(
                                    session.play_start_ms,
                                    session.pending_play_ms,
                                    want_ms,
                                ) {
                                    if pending_waiter_action(session.pending_play_ms, want_ms)
                                        == PendingWaiterAction::Release
                                    {
                                        enter_no_fill(
                                            "superseded",
                                            session_id,
                                            &file_name,
                                            session,
                                            &mut holding_no_fill,
                                            &mut holding_for_land,
                                            &mut deadline,
                                        );
                                    } else if session.pending_play_ms.is_none()
                                        && align_to_segment(session.play_start_ms)
                                            != align_to_segment(want_ms)
                                    {
                                        return Err(PlaylistError::NotReady);
                                    }
                                } else {
                                    desire_restart(session, want_ms);
                                    holding_for_land = true;
                                    maybe_apply_pending_restart(session)?;
                                    deadline = Instant::now() + SEGMENT_WAIT;
                                }
                            }
                            SegmentMissAction::Wait => {
                                if scrub_shaped
                                    && !prefetch_advances_pending(session.pending_play_ms, want_ms)
                                    && !digback_behind_committed(
                                        session.play_start_ms,
                                        session.pending_play_ms,
                                        want_ms,
                                    )
                                {
                                    desire_restart(session, want_ms);
                                    holding_for_land = true;
                                }
                                maybe_apply_pending_restart(session)?;
                                if pending_waiter_action(session.pending_play_ms, want_ms)
                                    == PendingWaiterAction::Release
                                {
                                    enter_no_fill(
                                        "superseded",
                                        session_id,
                                        &file_name,
                                        session,
                                        &mut holding_no_fill,
                                        &mut holding_for_land,
                                        &mut deadline,
                                    );
                                } else if session.child.is_none() {
                                    return Err(PlaylistError::NotFound);
                                } else if want_ms < window_start {
                                    // Producer-truth: URI behind the cooking
                                    // window was never listed for this run.
                                    return Err(PlaylistError::NotFound);
                                }
                            }
                        }
                    }
                }
            }
            if Instant::now() >= deadline {
                if holding_no_fill {
                    return Err(PlaylistError::AbandonedHoldEnded);
                }
                return Err(PlaylistError::NotReady);
            }
            std::thread::sleep(SEGMENT_POLL);
        }
    }

    /// Stops the session this player owns. One session per POST, so there is
    /// no other holder to consider.
    pub fn stop(&self, session_id: &str) -> bool {
        let mut sessions = match self.sessions.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        let Some(mut session) = sessions.remove(session_id) else {
            return false;
        };
        stop_child(&mut session.child);
        if let Err(e) = fs::remove_dir_all(&session.dir) {
            tracing::warn!(
                path = %session.dir.display(),
                error = %e,
                "hls session dir cleanup failed"
            );
        }
        tracing::info!(session_id, "hls session stopped");
        true
    }

    /// Idle and failed sessions are reaped without a DELETE. Crashed or
    /// sleeping tabs never send one; without this Gate 2's zero-orphan
    /// criterion fails 48 hours later.
    fn reaper_loop(&self) {
        loop {
            std::thread::sleep(REAPER_TICK);
            let stale: Vec<String> = {
                let Ok(sessions) = self.sessions.lock() else {
                    continue;
                };
                sessions
                    .iter()
                    .filter(|(_, s)| s.last_access.elapsed() > IDLE_TIMEOUT || s.failed.is_some())
                    .map(|(id, _)| id.clone())
                    .collect()
            };
            for id in stale {
                tracing::info!(session_id = %id, "hls session idle or failed force-reap");
                self.force_stop(&id);
            }
        }
    }

    fn force_stop(&self, session_id: &str) {
        let mut sessions = match self.sessions.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let Some(mut session) = sessions.remove(session_id) else {
            return;
        };
        stop_child(&mut session.child);
        let _ = fs::remove_dir_all(&session.dir);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEncoder {
    pub name: String,
    pub kind: EncoderKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderKind {
    Hardware,
    Software,
    Copy,
}

impl EncoderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hardware => "hardware",
            Self::Software => "software",
            Self::Copy => "copy",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartAtOutcome {
    Applied,
    /// Cooking land still unfinished and an asset_wait holds it — leave the
    /// encoder alone so mid bytes can land; caller must keep pending.
    DeferredLandWaiter,
}

fn restart_at(
    session: &mut Session,
    play_ms: u64,
    video_encoder: &str,
) -> Result<RestartAtOutcome, PlaylistError> {
    let play_start_ms = align_to_segment(play_ms);
    let start_ms = encode_start_ms(play_start_ms);
    let prior_play = session.play_start_ms;
    let prior_ready = session.first_segment_ready;
    let cooking_land = align_to_segment(prior_play);
    let cooking_waiters = session
        .segment_waiters
        .get(&cooking_land)
        .copied()
        .unwrap_or(0);
    // Gate immediately before kill, under the same sessions mutex that
    // SegmentWaiterGuard attach/drop uses — check-then-kill is atomic vs
    // concurrent waiter attach for this process.
    if !may_kill_cooking_encode(prior_ready, cooking_waiters) {
        if !session.preempt_defer_logged {
            session.preempt_defer_logged = true;
            tracing::info!(
                prior_play_start_ms = prior_play,
                prior_first_segment_ready = prior_ready,
                cooking_land_waiters = cooking_waiters,
                new_play_start_ms = play_start_ms,
                "hls seek restart_at: defer kill (cooking land waiter)"
            );
        }
        return Ok(RestartAtOutcome::DeferredLandWaiter);
    }
    session.preempt_defer_logged = false;
    let had_child = session.child.is_some();
    // Snapshot all waiters at kill time — correlates attach/drop races on
    // double-scrub sticks (cooking land may be 0 while another want_ms holds).
    let waiters_snapshot: String = {
        let mut parts: Vec<String> = session
            .segment_waiters
            .iter()
            .map(|(ms, n)| format!("{ms}:{n}"))
            .collect();
        parts.sort();
        if parts.is_empty() {
            "-".into()
        } else {
            parts.join(",")
        }
    };
    if !prior_ready && cooking_waiters == 0 {
        tracing::info!(
            prior_play_start_ms = prior_play,
            cooking_land_ms = cooking_land,
            cooking_land_waiters = cooking_waiters,
            segment_waiters = %waiters_snapshot,
            new_play_start_ms = play_start_ms,
            "hls seek restart_at: preempt kill before land (no cooking waiter)"
        );
    }
    tracing::info!(
        prior_play_start_ms = prior_play,
        prior_first_segment_ready = prior_ready,
        killing_encoder = had_child,
        cooking_land_waiters = cooking_waiters,
        segment_waiters = %waiters_snapshot,
        new_play_start_ms = play_start_ms,
        "hls seek restart_at: stop prior encode"
    );
    stop_child(&mut session.child);
    sync_segment_map(session);
    sync_all_run_indexes(session);
    // Duplicate-write stop: scrub-back (or re-land) into media the global map
    // already holds — mint a fresh playlist URI, copy init, do not re-encode.
    if let Some(mapped) = map_segment_covering(session, play_start_ms) {
        let src_run = mapped.run_id;
        let run_id = session.next_run_id;
        session.next_run_id += 1;
        let new_dir = session.dir.join(format!("run_{run_id}"));
        fs::create_dir_all(&new_dir).map_err(|e| {
            PlaylistError::Failed(format!("create run dir {}: {e}", new_dir.display()))
        })?;
        let init_src = session.dir.join(format!("run_{src_run}/init.mp4"));
        let init_dst = new_dir.join("init.mp4");
        if init_src.exists() {
            fs::copy(&init_src, &init_dst).map_err(|e| {
                PlaylistError::Failed(format!(
                    "copy init {} -> {}: {e}",
                    init_src.display(),
                    init_dst.display()
                ))
            })?;
        }
        session.current_run_id = run_id;
        session.current_run_eof = true;
        session.start_ms = play_start_ms;
        session.play_start_ms = play_start_ms;
        session.landed_ms = mapped.start_ms;
        session.failed = None;
        session.last_restart = Instant::now();
        session.primed = true;
        session.first_segment_ready = true;
        session.stale_retain_refuse_until = None;
        if session.pending_play_ms == Some(play_start_ms) {
            session.pending_play_ms = None;
            session.pending_since = None;
        }
        maybe_evict_finished_runs(session);
        tracing::info!(
            play_start_ms,
            run_id,
            src_run_id = src_run,
            mapped_start_ms = mapped.start_ms,
            session_disk_bytes = session_disk_bytes(&session.dir),
            path = %session.src.display(),
            "hls session seek map hit (duplicate-write stop)"
        );
        return Ok(RestartAtOutcome::Applied);
    }
    if let Some(gap) = restart_spawn_gap() {
        tracing::info!(
            gap_ms = gap.as_millis(),
            play_start_ms,
            "hls restart spawn gap (NIGHTJAR_RESTART_SPAWN_GAP_MS)"
        );
        std::thread::sleep(gap);
    }
    // Gate 2 / fill-forward: do not wipe prior run dirs. Scrub-back into
    // mapped media is a plain file serve (ADR-0020 global map). New producer
    // output goes in a fresh run_* directory.
    let run_id = session.next_run_id;
    session.next_run_id += 1;
    let run_dir = session.dir.join(format!("run_{run_id}"));
    fs::create_dir_all(&run_dir)
        .map_err(|e| PlaylistError::Failed(format!("create run dir {}: {e}", run_dir.display())))?;
    write_run_encode_start(&run_dir, start_ms).map_err(PlaylistError::Failed)?;
    let burn_in = prepare_ass_burn_file(&session.src, &session.dir, session.burn_in.clone())
        .map_err(PlaylistError::Failed)?;
    session.burn_in = burn_in;
    let child = spawn_ffmpeg(
        &session.src,
        &run_dir,
        start_ms,
        session.mode,
        session.audio.clone(),
        video_encoder,
        session.burn_in.as_ref(),
    )
    .map_err(PlaylistError::Failed)?;
    session.child = Some(child);
    session.current_run_id = run_id;
    session.current_run_eof = false;
    session.start_ms = start_ms;
    session.play_start_ms = play_start_ms;
    session.landed_ms = play_start_ms;
    session.failed = None;
    session.last_restart = Instant::now();
    session.primed = false;
    session.first_segment_ready = false;
    session.stale_retain_refuse_until = Some(Instant::now() + STALE_RETAIN_REFUSE);
    if session.pending_play_ms == Some(play_start_ms) {
        session.pending_play_ms = None;
        session.pending_since = None;
    }
    maybe_evict_finished_runs(session);
    tracing::info!(
        start_ms,
        play_start_ms,
        run_id,
        session_disk_bytes = session_disk_bytes(&session.dir),
        encoder = video_encoder,
        path = %session.src.display(),
        "hls session seek restart"
    );
    Ok(RestartAtOutcome::Applied)
}

/// Mapped segment that already covers title-absolute `play_ms`.
///
/// Producer sidx can land a few tens of ms after `-ss` (dogfood: start 80 for
/// play 0). Treat the first mapped segment at or after `play` that starts
/// before `play + 2*SEGMENT_MS` as a hit so scrub-back does not re-encode.
fn map_segment_covering(
    session: &Session,
    play_ms: u64,
) -> Option<crate::hls_segment_map::MappedSegment> {
    let play = align_to_segment(play_ms);
    if let Some(exact) = session.segment_map.get(play) {
        return Some(exact.clone());
    }
    if let Some(s) = session.segment_map.iter_ordered().find(|s| {
        let end = s.start_ms.saturating_add(s.duration_ms.max(1));
        s.start_ms <= play && play < end
    }) {
        return Some(s.clone());
    }
    let slack = SEGMENT_MS.saturating_mul(2);
    session
        .segment_map
        .iter_ordered()
        .find(|s| s.start_ms >= play && s.start_ms < play.saturating_add(slack))
        .cloned()
}

/// Record scrub intent. In-flight encodes keep cooking until land (or a far
/// pending preempts after [`RESTART_MIN_INTERVAL`]); after land, rapid intents
/// debounce into one restart (see [`RESTART_COALESCE_QUIET`]).
fn desire_restart(session: &mut Session, want_play_ms: u64) {
    let aligned = align_to_segment(want_play_ms);
    match classify_restart_desire(
        aligned,
        session.play_start_ms,
        session.start_ms,
        session.first_segment_ready,
    ) {
        CoalesceDesire::Nop => {
            if session.pending_play_ms == Some(aligned) {
                session.pending_play_ms = None;
                session.pending_since = None;
            }
        }
        CoalesceDesire::HoldInFlight => {
            // Same target: keep the pending clock so land still applies once.
            if session.pending_play_ms != Some(aligned) {
                session.pending_play_ms = Some(aligned);
                session.pending_since = Some(Instant::now());
                tracing::info!(
                    pending_play_ms = aligned,
                    cooking_play_ms = session.play_start_ms,
                    "hls seek restart coalesced (in flight)"
                );
            }
        }
        CoalesceDesire::HoldDebounce => {
            // Same target: do not reset the quiet clock on every 503 retry
            // (that would never elapse RESTART_COALESCE_QUIET).
            if session.pending_play_ms != Some(aligned) {
                session.pending_play_ms = Some(aligned);
                session.pending_since = Some(Instant::now());
                tracing::info!(
                    pending_play_ms = aligned,
                    play_start_ms = session.play_start_ms,
                    "hls seek restart coalesced (debounce)"
                );
            }
        }
    }
}

fn maybe_apply_pending_restart(session: &mut Session) -> Result<(), PlaylistError> {
    let elapsed = session.pending_since.map(|t| t.elapsed());
    // `pending_since == None` with a pending target means apply immediately
    // (used right after first_segment_ready for in-flight coalesce).
    let apply_immediate = session.pending_play_ms.is_some() && session.pending_since.is_none();
    let ready = session.first_segment_ready;
    let cooking = session.play_start_ms;
    let since = session.last_restart.elapsed();
    let allow_preempt = !disable_preempt();
    let Some(pending) = pending_restart_due(
        ready,
        session.pending_play_ms,
        elapsed,
        apply_immediate,
        cooking,
        since,
        allow_preempt,
    ) else {
        return Ok(());
    };
    // Do not clear pending before restart_at: a deferred land-waiter kill
    // must leave the far target recorded for land-then-yank.
    if encode_start_ms(pending) == session.start_ms && pending == session.play_start_ms {
        session.pending_play_ms = None;
        session.pending_since = None;
        return Ok(());
    }
    let preempt_before_land = !ready
        && allow_preempt
        && coalesce_preempt_before_land(cooking, pending)
        && since >= RESTART_MIN_INTERVAL;
    let encoder = session.video_encoder.clone();
    match restart_at(session, pending, &encoder)? {
        RestartAtOutcome::DeferredLandWaiter => {
            // restart_at already logged once per defer streak.
            Ok(())
        }
        RestartAtOutcome::Applied => {
            if preempt_before_land {
                tracing::info!(
                    pending_play_ms = pending,
                    cooking_play_ms = cooking,
                    since_last_restart_ms = since.as_millis(),
                    "hls seek restart preempted (before land)"
                );
            }
            // restart_at clears pending when it matches the new play; clear
            // any leftover (e.g. already applied path).
            if session.pending_play_ms == Some(pending) {
                session.pending_play_ms = None;
                session.pending_since = None;
            }
            Ok(())
        }
    }
}

/// Whether bytes read for a segment request may still be returned after
/// [`note_first_segment_ready`] / [`maybe_apply_pending_restart`].
///
/// If `play_start_ms` moved away from this request's land, the bytes belong
/// to the pre-apply window — 503 so the client retries. If play moved *to*
/// this request's land (`want_ms`), the bytes are the new land and must
/// still 200 (land-ensure for the final scrub). Pure helper for serve + tests.
pub fn serve_ok_after_pending_apply(
    play_before_ms: u64,
    play_after_ms: u64,
    want_ms: Option<u64>,
) -> bool {
    if play_before_ms == play_after_ms {
        return true;
    }
    match want_ms {
        Some(want) => align_to_segment(want) == align_to_segment(play_after_ms),
        None => false,
    }
}

/// Whether a retained on-disk segment may be served while the post-restart
/// stale guard is active.
///
/// Near-land dig-back (within [`ENCODE_LEAD_SEGMENTS`]) and anything at/ahead
/// of play stay servable. Farther behind is the superseded scrub Safari still
/// GETs after coalesce — refuse only while `guard_active` (TTL until land
/// ready, or until TTL elapses if land never clears it), not forever.
pub fn serve_ok_retained_during_stale_guard(
    want_ms: u64,
    play_start_ms: u64,
    guard_active: bool,
) -> bool {
    if !guard_active {
        return true;
    }
    let want = align_to_segment(want_ms);
    let play = align_to_segment(play_start_ms);
    want + encode_lead_segments() * SEGMENT_MS >= play
}

/// Logs once when the **play land** segment appears (not merely the lead-in
/// first window). Pending scrub apply waits for this when the new target is
/// near the cooking land so a coalesced restart does not yank before that
/// land exists — that left Safari retrying the prior land seg forever
/// (dogfood: seg415 after scrub to 1188). Far pending may preempt earlier
/// via [`coalesce_preempt_before_land`] once [`RESTART_MIN_INTERVAL`] elapses.
///
/// Clears [`Session::stale_retain_refuse_until`]: the guard protects while the
/// new land cooks; keeping it for the full TTL after land is ready left Safari
/// 503-retrying the superseded middle land (~15s) before dig-back (dogfood).
///
/// Called from playlist serve and from every `asset_wait` poll — not only when
/// the requested URI is the cooking land. Middle waiters may enter no-fill
/// before a 200 on that URI; final land-ensure must still notice.
fn note_first_segment_ready(session_id: &str, session: &mut Session) {
    sync_segment_map(session);
    if session.first_segment_ready {
        return;
    }
    if !current_run_has_mapped_segment(session) {
        return;
    }
    if let Some(landed) = first_current_run_start(session) {
        session.landed_ms = landed;
    }
    session.first_segment_ready = true;
    session.stale_retain_refuse_until = None;
    let elapsed_ms = session.last_restart.elapsed().as_millis();
    let lead_ms = session.play_start_ms.saturating_sub(session.start_ms);
    let disk_bytes = session_disk_bytes(&session.dir);
    tracing::info!(
        session_id,
        elapsed_ms,
        start_ms = session.start_ms,
        play_start_ms = session.play_start_ms,
        landed_ms = session.landed_ms,
        lead_ms,
        session_disk_bytes = disk_bytes,
        encoder = %session.video_encoder,
        path = %session.src.display(),
        "hls_session_first_segment_ready"
    );
    // Scrubs that arrived while this encode was landing: apply latest now
    // (do not wait for debounce quiet — the client already waited on land).
    if session.pending_play_ms.is_some() {
        session.pending_since = None;
        if let Err(e) = maybe_apply_pending_restart(session) {
            session.failed = Some(match e {
                PlaylistError::Failed(msg) => msg,
                PlaylistError::NotFound => "pending seek restart: not found".into(),
                PlaylistError::NotReady => "pending seek restart: not ready".into(),
                PlaylistError::AbandonedHoldEnded => {
                    "pending seek restart: abandoned hold ended".into()
                }
            });
        }
    }
}

fn note_child_exit(session: &mut Session) -> Option<String> {
    let child = session.child.as_mut()?;
    match child.try_wait() {
        Ok(Some(status)) if !status.success() => {
            let msg = format!("ffmpeg exited with {status}");
            session.failed = Some(msg.clone());
            Some(msg)
        }
        Ok(Some(_)) => {
            apply_run_eof(session);
            None
        }
        Ok(None) => None,
        Err(e) => {
            let msg = format!("ffmpeg wait: {e}");
            session.failed = Some(msg.clone());
            Some(msg)
        }
    }
}

/// Producer reached EOF: mark ENDLIST and record usable extent when the
/// farthest mapped end (or 0 if nothing was written) is materially short of
/// claimed duration. Empty map at a mid-title land is still damage — clients
/// must see usableExtentMs instead of hanging on master 503.
fn apply_run_eof(session: &mut Session) {
    session.child = None;
    session.current_run_eof = true;
    sync_segment_map(session);
    let end = session
        .segment_map
        .iter_ordered()
        .next_back()
        .map(|last| last.start_ms.saturating_add(last.duration_ms))
        .unwrap_or(0);
    if session.duration_ms.saturating_sub(end) > USABLE_SHORTFALL_MS {
        session.usable_extent_ms = Some(end);
        tracing::info!(
            usable_extent_ms = end,
            duration_ms = session.duration_ms,
            run_id = session.current_run_id,
            "hls usable extent recorded (EOF short of claimed duration)"
        );
    }
}

fn align_to_segment(ms: u64) -> u64 {
    (ms / SEGMENT_MS) * SEGMENT_MS
}

/// Encode window start for a play land: [`encode_lead_segments`] before the
/// aligned play point (Safari dig-back; see module docs / ADR-0011).
fn encode_start_ms(play_ms: u64) -> u64 {
    align_to_segment(play_ms.saturating_sub(encode_lead_segments() * SEGMENT_MS))
}

/// Highest mapped segment start at or after `window_start_ms`.
fn latest_segment_in_window(
    map: &crate::hls_segment_map::SegmentMap,
    window_start_ms: u64,
) -> Option<u64> {
    latest_mapped_start_in_window(map, window_start_ms)
}

/// Dogfood: what EXT-X-START / session land was when a playlist was served.
/// Full index bodies are huge (every seg URI); log header lines only.
fn log_playlist_serve(
    session_id: &str,
    resource: &str,
    req_start_ms: Option<u64>,
    play_start_ms: u64,
    pending_play_ms: Option<u64>,
    bytes: &[u8],
) {
    let text = String::from_utf8_lossy(bytes);
    let mut ext_x_start: Option<f64> = None;
    let mut head_lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        if head_lines.len() < 14 && (line.starts_with('#') || line.is_empty()) {
            head_lines.push(line);
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-START:TIME-OFFSET=") {
            let offset = rest.split(',').next().unwrap_or(rest);
            ext_x_start = offset.parse().ok();
        }
        if !line.starts_with('#') && !line.is_empty() {
            // First media URI — stop header capture.
            if head_lines.len() < 14 {
                head_lines.push(line);
            }
            break;
        }
    }
    let head = head_lines.join("|");
    tracing::info!(
        session_id,
        resource,
        req_start_ms,
        play_start_ms,
        pending_play_ms,
        ext_x_start_s = ext_x_start,
        play_land_ms = play_start_ms,
        head = %head,
        "hls playlist serve"
    );
}

/// Master playlist: one video variant + optional SUBTITLES group (ADR-0010).
/// Media and subtitle URIs are path-absolute under `/api/v0/sessions/…`
/// so run-directory depth cannot break client resolution (ADR-0008).
///
/// CODECS is omitted on purpose: a wrong value (we previously advertised
/// Main@L3.1 while VideoToolbox emits High@L4.0) makes Safari native HLS
/// refuse the variant outright. Better no hint than a lying one; the init
/// segment carries the real codec string.
fn build_master(session_id: &str, run_id: u64, tracks: &[HlsSubtitleTrack]) -> Vec<u8> {
    use std::fmt::Write;
    let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:7\n");
    if !tracks.is_empty() {
        for t in tracks {
            let lang = t.language.as_deref().unwrap_or("und");
            let default = if t.is_default { "YES" } else { "NO" };
            let forced = if t.forced { "YES" } else { "NO" };
            let autoselect = if t.forced { "NO" } else { "YES" };
            let name = escape_hls_quoted(&t.name);
            let mut line = format!(
                "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"{name}\",\
                 LANGUAGE=\"{lang}\",DEFAULT={default},AUTOSELECT={autoselect},\
                 FORCED={forced},URI=\"/api/v0/sessions/{session_id}/subs/{}.m3u8\"",
                t.track_id
            );
            if t.sdh {
                line.push_str(
                    ",CHARACTERISTICS=\"public.accessibility.transcribes-spoken-dialog\"",
                );
            }
            let _ = writeln!(out, "{line}");
        }
        out.push_str("#EXT-X-STREAM-INF:BANDWIDTH=5000000,SUBTITLES=\"subs\"\n");
    } else {
        out.push_str("#EXT-X-STREAM-INF:BANDWIDTH=5000000\n");
    }
    let _ = writeln!(
        out,
        "/api/v0/sessions/{session_id}/runs/{run_id}/index.m3u8"
    );
    out.into_bytes()
}

/// Subtitle media playlist for one snapshotted track.
///
/// ADR-0020 §10 / ADR-0010: VTT stays on the fixed 2s index grid (`segNNN.vtt`)
/// even though video URIs are time-keyed (`seg_<ms:011>.m4s`). Do not "align"
/// subtitle segment names to producer video boundaries — cue slicing is
/// title-time on SEGMENT_MS, independent of copy GOP cuts.
fn build_subtitle_playlist_for(track: &HlsSubtitleTrack, duration_ms: u64) -> Vec<u8> {
    build_segmented_subtitle_playlist(&track.track_id, duration_ms)
}

/// Multi-segment VOD subtitle playlist aligned to SEGMENT_MS.
/// Segment URIs are relative to `subs/{trackId}.m3u8`.
fn build_segmented_subtitle_playlist(track_id: &str, duration_ms: u64) -> Vec<u8> {
    use std::fmt::Write;
    let full = duration_ms / SEGMENT_MS;
    let rem_ms = duration_ms % SEGMENT_MS;
    let segment_secs = SEGMENT_MS as f64 / 1000.0;
    let target = segment_secs.ceil() as u64;
    let mut out = format!(
        "#EXTM3U\n\
         #EXT-X-VERSION:6\n\
         #EXT-X-TARGETDURATION:{target}\n\
         #EXT-X-PLAYLIST-TYPE:VOD\n\
         #EXT-X-MEDIA-SEQUENCE:0\n"
    );
    for i in 0..full {
        let _ = writeln!(
            out,
            "#EXTINF:{segment_secs:.6},\n{track_id}/{}",
            segment_vtt_name(i)
        );
    }
    if rem_ms > 0 {
        let _ = writeln!(
            out,
            "#EXTINF:{:.6},\n{track_id}/{}",
            rem_ms as f64 / 1000.0,
            segment_vtt_name(full)
        );
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out.into_bytes()
}

fn segment_vtt_name(index: u64) -> String {
    format!("seg{index:03}.vtt")
}

fn spawn_session_subtitle_worker(src: &Path, dir: &Path, tracks: &[HlsSubtitleTrack]) {
    let inputs: Vec<SessionSubInput> = tracks
        .iter()
        .filter(|t| t.item_vtt_path.is_none())
        .map(|t| SessionSubInput {
            track_id: t.track_id.clone(),
            codec: t.codec.clone(),
            stream_index: t.stream_index,
            sidecar_path: t.sidecar_path.clone(),
        })
        .collect();
    if inputs.is_empty() {
        return;
    }
    let src = src.to_path_buf();
    let dir = dir.to_path_buf();
    let _ = std::thread::Builder::new()
        .name("hls-subs".into())
        .spawn(move || {
            if let Err(e) = prepare_session_subtitles(&src, &dir, &inputs) {
                tracing::warn!(
                    path = %src.display(),
                    error = %e,
                    "session subtitle prep failed"
                );
            }
        });
}

fn read_subtitle_segment(
    session: &Session,
    track_id: &str,
    segment_idx: u64,
) -> Result<Vec<u8>, PlaylistError> {
    // Belt: track_id must be a single Normal component (API allowlist is the
    // primary gate; this stops a regression from joining `..` into session.dir).
    use std::path::Component;
    let mut comps = Path::new(track_id).components();
    match (comps.next(), comps.next()) {
        (Some(Component::Normal(_)), None) => {}
        _ => return Err(PlaylistError::NotFound),
    }
    let track = session
        .subtitle_tracks
        .iter()
        .find(|t| t.track_id == track_id)
        .ok_or(PlaylistError::NotFound)?;

    let (full_path, done) = if let Some(path) = &track.item_vtt_path {
        // Ready extract: complete file, slice in-process (no session demux).
        (path.clone(), true)
    } else {
        let track_dir = session.dir.join("subs").join(track_id);
        (track_dir.join("full.vtt"), track_dir.join("done").exists())
    };
    if !full_path.exists() {
        return Err(PlaylistError::NotReady);
    }
    let body = fs::read_to_string(&full_path).map_err(|e| {
        PlaylistError::Failed(format!("read subtitle {}: {e}", full_path.display()))
    })?;
    let start_ms = segment_idx * SEGMENT_MS;
    let end_ms = start_ms + SEGMENT_MS;
    if !done {
        match webvtt_max_cue_end_ms(&body) {
            None => return Err(PlaylistError::NotReady),
            Some(max_end) if max_end < start_ms => return Err(PlaylistError::NotReady),
            Some(_) => {}
        }
    }
    Ok(slice_webvtt(&body, start_ms, end_ms).into_bytes())
}

fn escape_hls_quoted(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn spawn_ffmpeg(
    src: &Path,
    dir: &Path,
    start_ms: u64,
    mode: SessionMode,
    audio: AudioSelection,
    video_encoder: &str,
    burn_in: Option<&BurnInSelection>,
) -> Result<Child, String> {
    let start_secs = format!("{:.3}", start_ms as f64 / 1000.0);
    let start_number = (start_ms / SEGMENT_MS).to_string();
    let segment_secs = SEGMENT_MS as f64 / 1000.0;
    let force_kf = format!("expr:gte(t,n_forced*{segment_secs})");
    let hls_time = format!("{segment_secs}");
    // Burn-in always re-encodes video (ADR-0018).
    let mode = if burn_in.is_some() {
        SessionMode::Transcode
    } else {
        mode
    };
    let mut cmd = Command::new("ffmpeg");
    cmd.current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Discard stderr: a piped and unread stderr fills (~64KiB) and
        // deadlocks ffmpeg so the playlist never appears.
        .stderr(Stdio::null())
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"]);
    if start_ms > 0 {
        cmd.args(["-ss", &start_secs]);
    }
    cmd.arg("-i").arg(src);
    if start_ms > 0 {
        // ADR-0020: load-bearing under copy. Does not rewrite tfdt/trun (those
        // stay segment-local at 0); it stamps title-absolute time into the
        // init `elst` empty-edit and each fragment's `sidx.earliest_presentation_time`,
        // which the session map uses as the wire key. Removing or moving this
        // flag silently reintroduces zero-based / mislabelled segment times.
        cmd.args(["-output_ts_offset", &start_secs]);
    }
    let audio_map = match audio.stream_index {
        Some(index) => format!("0:{index}"),
        None => "0:a:0?".to_string(),
    };
    let downmix = if audio.needs_downmix() {
        let filter = stereo_downmix_filter(audio.channels, audio.channel_layout.as_deref());
        if filter.is_none() {
            tracing::warn!(
                channels = audio.channels,
                layout = audio.channel_layout.as_deref().unwrap_or("unknown"),
                path = %src.display(),
                "no downmix matrix for this layout; falling back to -ac 2"
            );
        }
        filter
    } else {
        None
    };

    let sdr_chain =
        "sidedata=delete,setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709";
    // ASS uses libass `-vf` filters; PGS uses overlay `filter_complex`
    // (ADR-0018). Never overlay text ASS — sub2video draws blank.
    if let Some(burn) = burn_in {
        ensure_libass_for_ass(burn.kind, ffmpeg_has_libass_filters())?;
    }
    let pgs_overlay = burn_in.and_then(pgs_overlay_graph);
    let ass_vf = match burn_in {
        Some(burn) if burn.kind == BurnInKind::Ass => Some(ass_burn_vf(burn, start_ms)?),
        _ => None,
    };
    if let Some(ref complex) = pgs_overlay {
        let full = format!("{complex},{sdr_chain}[vout]");
        cmd.args(["-filter_complex", &full]);
        cmd.args(["-map", "[vout]", "-map", &audio_map]);
    } else {
        cmd.args(["-map", "0:v:0", "-map", &audio_map]);
    }

    match mode {
        // Hybrid: the codecs already copy and only the channel layout forces
        // work, so video still copies while audio is encoded (ADR-0012).
        SessionMode::Copy if audio.needs_downmix() => {
            cmd.args(["-c:v", "copy"]);
            push_audio_encode(&mut cmd, downmix.as_deref());
        }
        // Copy cannot place IDRs, so -hls_time is only a target: segments
        // break at source keyframes (ADR-0011).
        SessionMode::Copy => {
            cmd.args(["-c", "copy"]);
        }
        SessionMode::Transcode => {
            cmd.args(["-c:v", video_encoder]);
            if video_encoder == "libx264" {
                cmd.args(["-preset", "veryfast", "-pix_fmt", "yuv420p"]);
            } else {
                // Hardware paths: keep pixel format explicit where the encoder
                // accepts it; backends that need device-specific graphs failed
                // verification.
                cmd.args(["-pix_fmt", "yuv420p"]);
            }
            // Browser sessions are SDR. VideoToolbox otherwise copies PQ/BT.2020
            // VUI and HDR10 side data from an HDR source onto an 8-bit encode;
            // Safari native HLS rejects that (Chrome/hls.js is more forgiving).
            // Color flags alone are not enough on videotoolbox — strip side
            // data and force BT.709 through setparams.
            if pgs_overlay.is_some() {
                cmd.args(["-map_metadata", "-1"]);
            } else {
                let vf = match ass_vf.as_deref() {
                    Some(ass) => format!("{ass},{sdr_chain}"),
                    None => sdr_chain.to_string(),
                };
                cmd.args(["-map_metadata", "-1", "-vf", &vf]);
            }
            cmd.args([
                "-colorspace",
                "bt709",
                "-color_primaries",
                "bt709",
                "-color_trc",
                "bt709",
            ]);
            cmd.args([
                // Time-based IDRs derived from SEGMENT_MS (same source as
                // -hls_time and the generated playlist EXTINF). A frame-count
                // -g alone is only 2s at 24 fps; at 60 fps it splits every
                // 0.8s (ADR-0008).
                "-force_key_frames",
                force_kf.as_str(),
                // Ceiling only; force_key_frames owns the cadence. Keep this
                // large enough that high-fps sources still hit the SEGMENT_MS
                // wall first. Scenecut off so FFmpeg cannot insert unaligned
                // IDRs.
                "-g",
                "600",
                "-keyint_min",
                "48",
                "-sc_threshold",
                "0",
            ]);
            push_audio_encode(&mut cmd, downmix.as_deref());
        }
    }
    cmd.args([
        "-f",
        "hls",
        "-hls_time",
        hls_time.as_str(),
        "-hls_list_size",
        "0",
        "-hls_flags",
        "independent_segments+temp_file",
        "-hls_segment_type",
        "fmp4",
        "-hls_fmp4_init_filename",
        "init.mp4",
        "-hls_segment_filename",
        "seg%03d.m4s",
        "-start_number",
        &start_number,
        // Muxer index is ingested into the session-global time-keyed map
        // (ADR-0020); clients never fetch this file.
        "index.m3u8",
    ]);
    cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "ffmpeg not found on PATH".into()
        } else {
            format!("spawn ffmpeg for {}: {e}", src.display())
        }
    })
}

/// Error when ASS burn is requested but FFmpeg lacks libass filters.
const LIBASS_REQUIRED: &str =
    "ASS/SSA burn-in requires FFmpeg built with libass (ass and subtitles filters)";

/// Fail closed for ASS burn when libass filters are absent (ADR-0018).
fn ensure_libass_for_ass(kind: BurnInKind, has_libass: bool) -> Result<(), String> {
    if kind == BurnInKind::Ass && !has_libass {
        Err(LIBASS_REQUIRED.into())
    } else {
        Ok(())
    }
}

/// True when `ffmpeg -filters` lists both `ass` and `subtitles`.
fn libass_filters_listed(filters_text: &str) -> bool {
    let mut has_ass = false;
    let mut has_subtitles = false;
    for line in filters_text.lines() {
        match line.split_whitespace().nth(1) {
            Some("ass") => has_ass = true,
            Some("subtitles") => has_subtitles = true,
            _ => {}
        }
    }
    has_ass && has_subtitles
}

/// Cached probe of the host FFmpeg filter list for libass.
fn ffmpeg_has_libass_filters() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let output = match Command::new("ffmpeg")
            .args(["-hide_banner", "-filters"])
            .output()
        {
            Ok(o) => o,
            Err(_) => return false,
        };
        // ffmpeg writes the filter table to stdout; some builds mix help on stderr.
        let text = if output.stdout.is_empty() {
            String::from_utf8_lossy(&output.stderr)
        } else {
            String::from_utf8_lossy(&output.stdout)
        };
        libass_filters_listed(&text)
    })
}

/// Escape a filesystem path for an FFmpeg filter option value.
fn escape_ffmpeg_filter_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '\\' | ':' | '\'' | '[' | ']' | ',' | ';' | ' ' | '(' | ')' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Ensure embedded ASS burn-in has a local `.ass` path (ADR-0018).
/// Sidecar and PGS selections pass through unchanged. Reuses an existing
/// session extract on seek restart.
fn prepare_ass_burn_file(
    src: &Path,
    session_dir: &Path,
    burn_in: Option<BurnInSelection>,
) -> Result<Option<BurnInSelection>, String> {
    let Some(mut burn) = burn_in else {
        return Ok(None);
    };
    if burn.kind != BurnInKind::Ass || burn.sidecar_path.is_some() {
        return Ok(Some(burn));
    }
    let stream_index = burn
        .stream_index
        .ok_or_else(|| "embedded ASS burn-in missing stream_index".to_string())?;
    let dest = session_dir.join(format!("burn_{}.ass", burn.track_id));
    let reuse = fs::metadata(&dest).ok().is_some_and(|m| m.len() > 0);
    if !reuse {
        tracing::info!(
            path = %src.display(),
            track_id = %burn.track_id,
            stream_index,
            dest = %dest.display(),
            "extracting embedded ASS for burn-in"
        );
        extract_embedded_ass(src, stream_index, &dest)?;
    }
    burn.sidecar_path = Some(dest);
    Ok(Some(burn))
}

/// libass `-vf` fragment for ASS/SSA burn-in (ADR-0018).
/// Always `ass=<local path>` — embedded tracks are demuxed first by
/// [`prepare_ass_burn_file`]. Mid-window `-ss` before `-i` resets frame PTS
/// to ~0; wrap with setpts so libass still matches absolute cue times, then
/// restore PTS for the muxer.
fn ass_burn_vf(burn: &BurnInSelection, start_ms: u64) -> Result<String, String> {
    if burn.kind != BurnInKind::Ass {
        return Err("ass_burn_vf called for non-ASS burn-in".into());
    }
    let path = burn
        .sidecar_path
        .as_ref()
        .ok_or_else(|| "ASS burn-in missing local .ass path (extract first)".to_string())?;
    let core = format!("ass={}", escape_ffmpeg_filter_path(path));
    if start_ms == 0 {
        return Ok(core);
    }
    let start_secs = start_ms as f64 / 1000.0;
    Ok(format!(
        "setpts=PTS+{start_secs}/TB,{core},setpts=PTS-{start_secs}/TB"
    ))
}

/// PGS overlay graph prefix (ADR-0018). SDR chain and `[vout]` are appended
/// by the caller. Embedded uses `0:s:N`.
fn pgs_overlay_graph(burn: &BurnInSelection) -> Option<String> {
    if burn.kind != BurnInKind::Pgs {
        return None;
    }
    let ordinal = burn.subtitle_ordinal?;
    Some(format!("[0:v:0][0:s:{ordinal}]overlay"))
}

/// Stereo AAC for the mapped track. With a matrix, `pan` does the mixdown;
/// without one, bare `-ac 2` is the fallback (ADR-0012) — swresample's
/// default matrix under-weights centre, which is why the matrix exists.
fn push_audio_encode(cmd: &mut Command, downmix: Option<&str>) {
    cmd.args(["-c:a", "aac", "-b:a", "192k"]);
    match downmix {
        Some(filter) => cmd.args(["-filter:a", filter]),
        None => cmd.args(["-ac", "2"]),
    };
}

fn stop_child(child: &mut Option<Child>) {
    if let Some(mut c) = child.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// Holds a refcount on `Session::segment_waiters` for one asset_wait call.
/// Attach and Drop take the registry mutex — the same lock `restart_at` holds
/// when deciding whether to `stop_child`, so waiter presence and kill are
/// mutually exclusive (no check-then-kill race against concurrent attach).
struct SegmentWaiterGuard<'a> {
    sessions: &'a Mutex<HashMap<String, Session>>,
    session_id: String,
    want_ms: u64,
}

impl<'a> SegmentWaiterGuard<'a> {
    fn attach(
        sessions: &'a Mutex<HashMap<String, Session>>,
        session_id: &str,
        want_ms: u64,
    ) -> Option<Self> {
        let mut guard = sessions.lock().ok()?;
        let session = guard.get_mut(session_id)?;
        let count = session.segment_waiters.entry(want_ms).or_insert(0);
        *count += 1;
        tracing::info!(
            session_id,
            want_ms,
            waiter_count = *count,
            cooking_play_ms = session.play_start_ms,
            pending_play_ms = session.pending_play_ms,
            "hls segment waiter attach"
        );
        Some(Self {
            sessions,
            session_id: session_id.to_string(),
            want_ms,
        })
    }
}

impl Drop for SegmentWaiterGuard<'_> {
    fn drop(&mut self) {
        let Ok(mut guard) = self.sessions.lock() else {
            return;
        };
        let Some(session) = guard.get_mut(&self.session_id) else {
            return;
        };
        let Some(count) = session.segment_waiters.get_mut(&self.want_ms) else {
            return;
        };
        *count = count.saturating_sub(1);
        let after = *count;
        if after == 0 {
            session.segment_waiters.remove(&self.want_ms);
        }
        tracing::info!(
            session_id = %self.session_id,
            want_ms = self.want_ms,
            waiter_count = after,
            cooking_play_ms = session.play_start_ms,
            pending_play_ms = session.pending_play_ms,
            "hls segment waiter drop"
        );
    }
}

fn is_safe_asset(name: &str) -> bool {
    if name == "init.mp4" {
        return true;
    }
    crate::hls_segment_map::parse_time_keyed_segment_name(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BurnInKind;
    use std::process::Command;

    fn ffmpeg_available() -> bool {
        let ok = Command::new("ffmpeg")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok && std::env::var_os("NIGHTJAR_TEST_REQUIRE_FFMPEG").is_some() {
            panic!("NIGHTJAR_TEST_REQUIRE_FFMPEG is set but ffmpeg is not on PATH");
        }
        ok
    }

    fn make_fixture(path: &Path) {
        make_fixture_secs(path, 4);
    }

    fn make_fixture_secs(path: &Path, secs: u32) {
        let d = secs.to_string();
        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                &format!("color=c=black:s=64x64:d={d}"),
                "-f",
                "lavfi",
                "-i",
                &format!("sine=frequency=440:duration={d}"),
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                path.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(status.success());
    }

    const FIXTURE_MS: u64 = 4000;

    /// First audio track, already inside the browser ceiling.
    fn stereo() -> AudioSelection {
        AudioSelection {
            stream_index: None,
            channels: 2,
            channel_layout: Some("stereo".into()),
            max_channels: 2,
        }
    }

    fn wait_playlist(reg: &HlsSessionRegistry, id: &str) -> Vec<u8> {
        let deadline = Instant::now() + SEGMENT_WAIT;
        loop {
            let run_id = {
                let sessions = reg.sessions.lock().unwrap();
                sessions.get(id).map(|s| s.current_run_id).unwrap_or(0)
            };
            match reg.playlist(id, run_id) {
                Ok(bytes) => {
                    if first_listed_seg_opt(&bytes).is_some() {
                        return bytes;
                    }
                    if Instant::now() >= deadline {
                        panic!(
                            "playlist ready without time-keyed segments: {}",
                            String::from_utf8_lossy(&bytes)
                        );
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(PlaylistError::NotReady) | Err(PlaylistError::NotFound)
                    if Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => panic!("playlist: {e:?}"),
            }
        }
    }

    fn wait_playlist_run(reg: &HlsSessionRegistry, id: &str, run_id: u64) -> Vec<u8> {
        let deadline = Instant::now() + SEGMENT_WAIT;
        loop {
            match reg.playlist(id, run_id) {
                Ok(bytes) => {
                    if first_listed_seg_opt(&bytes).is_some() {
                        return bytes;
                    }
                    if Instant::now() >= deadline {
                        panic!(
                            "playlist ready without time-keyed segments: {}",
                            String::from_utf8_lossy(&bytes)
                        );
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(PlaylistError::NotReady) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => panic!("playlist: {e:?}"),
            }
        }
    }

    /// First time-keyed segment URI listed in a media playlist body.
    fn first_listed_seg_opt(playlist: &[u8]) -> Option<String> {
        for line in String::from_utf8_lossy(playlist).lines() {
            let base = line.rsplit('/').next().unwrap_or(line);
            if crate::hls_segment_map::parse_time_keyed_segment_name(base).is_some() {
                return Some(base.to_string());
            }
        }
        None
    }

    fn first_listed_seg(playlist: &[u8]) -> String {
        first_listed_seg_opt(playlist)
            .unwrap_or_else(|| panic!("no time-keyed segment in playlist"))
    }

    /// Producer sidx land for a mid-start / seek window (may be tens of ms
    /// off the aligned play ms — do not hardcode `seg_00000040000`).
    fn wait_land_near(reg: &HlsSessionRegistry, id: &str, play_ms: u64) -> (String, u64) {
        let playlist = wait_playlist(reg, id);
        let name = first_listed_seg(&playlist);
        let ms = crate::hls_segment_map::parse_time_keyed_segment_name(&name)
            .expect("listed segment parses");
        let slack = SEGMENT_MS.saturating_mul(2);
        assert!(
            ms + slack >= play_ms && ms < play_ms.saturating_add(slack),
            "land {ms} not near play {play_ms} (slack {slack}): {name}"
        );
        let _ = wait_asset(reg, id, &name);
        (name, ms)
    }

    fn wait_first_listed_asset(reg: &HlsSessionRegistry, id: &str) -> Vec<u8> {
        let pl = wait_playlist(reg, id);
        wait_asset(reg, id, &first_listed_seg(&pl))
    }

    fn wait_asset(reg: &HlsSessionRegistry, id: &str, name: &str) -> Vec<u8> {
        let deadline = Instant::now() + SEGMENT_WAIT + Duration::from_secs(5);
        loop {
            match reg.asset(id, name, None) {
                Ok(bytes) => return bytes,
                Err(PlaylistError::NotReady) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => panic!("asset {name}: {e:?}"),
            }
        }
    }

    #[test]
    fn window_decision_table() {
        // (name, requested_ms, window_start_ms, on_disk, expected)
        let cases = [
            ("on disk is serve", 10_000, 0, true, WindowAction::Serve),
            (
                "same window cooking is serve",
                2000,
                2000,
                false,
                WindowAction::Serve,
            ),
            (
                "divergent offset restarts",
                10_000,
                0,
                false,
                WindowAction::Restart,
            ),
            (
                "aligns request before compare",
                2500,
                2000,
                false,
                WindowAction::Serve,
            ),
        ];
        for (name, req, window, on_disk, expected) in cases {
            assert_eq!(
                decide_window_action(req, window, on_disk),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn segment_miss_decision_table() {
        let cool = RESTART_MIN_INTERVAL;
        let hot = Duration::from_millis(0);
        // ADR-0020: segment GETs never Restart. Far scrub is POST /seek.
        let cases = [
            (
                "behind window waits (no dig-back restart)",
                0,
                600,
                616,
                None,
                false,
                cool,
            ),
            ("near behind play waits", 0, 4, 4, Some(10), true, cool),
            (
                "far ahead of frontier waits (seek API owns scrub)",
                20,
                4,
                4,
                Some(10),
                true,
                cool,
            ),
            (
                "near frontier waits (cooking)",
                11,
                4,
                4,
                Some(10),
                true,
                cool,
            ),
            (
                "hot restart interval still waits",
                20,
                4,
                4,
                Some(10),
                true,
                hot,
            ),
        ];
        for (name, idx, window, play, latest, primed, since) in cases {
            assert_eq!(
                decide_segment_miss(
                    idx * SEGMENT_MS,
                    window * SEGMENT_MS,
                    play * SEGMENT_MS,
                    latest.map(|l| l * SEGMENT_MS),
                    primed,
                    since,
                ),
                SegmentMissAction::Wait,
                "{name}"
            );
        }
    }

    #[test]
    fn miss_never_restarts_encode_from_segment_get() {
        let cases = [(1040u64, 1052u64), (0u64, 4u64), (1610u64, 1614u64)];
        for (idx, window) in cases {
            let action = decide_segment_miss(
                idx * SEGMENT_MS,
                window * SEGMENT_MS,
                window * SEGMENT_MS,
                Some(window * SEGMENT_MS),
                true,
                RESTART_MIN_INTERVAL,
            );
            assert_eq!(action, SegmentMissAction::Wait, "idx={idx}");
            let want_ms = idx * SEGMENT_MS;
            let new_window = encode_start_ms(want_ms) / SEGMENT_MS;
            assert_eq!(
                new_window,
                idx.saturating_sub(ENCODE_LEAD_SEGMENTS),
                "encode_start still defined for seek path"
            );
        }
    }

    /// Dogfood incident timing: three scrub intents (1084s → 1840s → 2454s)
    /// while the first encode is still landing. Only the last pending applies
    /// when ready — one follow-up restart, not three racing kills.
    #[test]
    fn rapid_restart_intents_coalesce_to_last_target() {
        let targets = [1_084_000u64, 1_840_000, 2_454_000];
        let mut play = 0u64;
        let mut encode = 0u64;
        let mut ready = false;
        let mut pending: Option<u64> = None;
        let mut apply_count = 0u32;

        for &want in &targets {
            let phase = classify_restart_desire(want, play, encode, ready);
            assert_eq!(phase, CoalesceDesire::HoldInFlight, "want={want}");
            pending = Some(align_to_segment(want));
        }
        assert_eq!(pending, Some(2_454_000));

        // First encode lands at the initial scrub target.
        ready = true;
        play = 1_084_000;
        encode = encode_start_ms(play);
        let due = pending_restart_due(ready, pending, None, true, play, RESTART_MIN_INTERVAL, true);
        assert_eq!(due, Some(2_454_000));
        // Apply once to the last intent.
        if let Some(p) = due {
            apply_count += 1;
            play = p;
            encode = encode_start_ms(p);
            pending = None;
        }
        assert_eq!(apply_count, 1);
        assert_eq!(play, 2_454_000);
        assert_eq!(encode, encode_start_ms(2_454_000));
        assert_eq!(encode, 2_454_000 - ENCODE_LEAD_SEGMENTS * SEGMENT_MS);

        // Debounce after land: three quick intents → one apply after quiet.
        ready = true;
        let burst = [2_500_000u64, 2_600_000, 2_700_000];
        for &want in &burst {
            assert_eq!(
                classify_restart_desire(want, play, encode, ready),
                CoalesceDesire::HoldDebounce
            );
            pending = Some(align_to_segment(want));
        }
        assert_eq!(
            pending_restart_due(
                ready,
                pending,
                Some(Duration::from_millis(100)),
                false,
                play,
                RESTART_MIN_INTERVAL,
                true
            ),
            None,
            "quiet not elapsed"
        );
        let due2 = pending_restart_due(
            ready,
            pending,
            Some(RESTART_COALESCE_QUIET),
            false,
            play,
            RESTART_MIN_INTERVAL,
            true,
        );
        assert_eq!(due2, Some(2_700_000));
    }

    /// ADR-0020: lead is 0, so encode window start equals play land. Any
    /// different pending land is "far" (ALIGN dig-back band deleted) and may
    /// preempt after RESTART_MIN_INTERVAL; same-land pending never preempts.
    #[test]
    fn pending_preempt_policy_under_producer_truth() {
        let cooking_play = 1_188_000u64;
        assert_eq!(encode_start_ms(cooking_play), cooking_play);
        assert!(
            !coalesce_preempt_before_land(cooking_play, cooking_play),
            "same land is not preempt"
        );
        let near_fwd = cooking_play + SEGMENT_MS;
        assert!(
            coalesce_preempt_before_land(cooking_play, near_fwd),
            "any different land is preempt-eligible"
        );
        // Before cooking land is ready: far pending may apply only after
        // RESTART_MIN_INTERVAL (see far_pending_preempts_before_land…).
        assert_eq!(
            pending_restart_due(
                false,
                Some(near_fwd),
                None,
                false,
                cooking_play,
                Duration::from_millis(0),
                true
            ),
            None,
            "hot clock: no preempt"
        );
        assert_eq!(
            pending_restart_due(
                false,
                Some(near_fwd),
                None,
                true,
                cooking_play,
                RESTART_MIN_INTERVAL * 2,
                true
            ),
            Some(near_fwd),
            "cool clock + far pending: preempt"
        );
        // Land ready → pending may apply.
        assert_eq!(
            pending_restart_due(
                true,
                Some(near_fwd),
                None,
                true,
                cooking_play,
                RESTART_MIN_INTERVAL,
                true
            ),
            Some(near_fwd)
        );
    }

    /// Rapid B then far C while B still cooking: after RESTART_MIN_INTERVAL,
    /// C preempts without waiting for B's land (Bug 1 third gate). Middle
    /// cook must not block the third target for a full land.
    #[test]
    fn far_pending_preempts_before_land_after_min_interval() {
        let land_b = 1_494_000u64;
        let land_c = 2_070_000u64;
        assert!(
            coalesce_preempt_before_land(land_b, land_c),
            "far C: beyond ALIGN_BEHIND from B"
        );
        // Too soon after B's restart: anti-thrash holds preempt.
        assert_eq!(
            pending_restart_due(
                false,
                Some(land_c),
                Some(Duration::from_millis(470)),
                false,
                land_b,
                Duration::from_millis(470),
                true
            ),
            None,
            "preempt still gated by RESTART_MIN_INTERVAL"
        );
        // Interval elapsed, B's land still missing: apply C.
        assert_eq!(
            pending_restart_due(
                false,
                Some(land_c),
                Some(Duration::from_millis(470)),
                false,
                land_b,
                RESTART_MIN_INTERVAL,
                true
            ),
            Some(land_c),
            "far C applies before B land once interval cools"
        );
        // Product default (allow_preempt=false): far pending stays held until land.
        assert_eq!(
            pending_restart_due(
                false,
                Some(land_c),
                Some(Duration::from_millis(470)),
                false,
                land_b,
                RESTART_MIN_INTERVAL,
                false,
            ),
            None,
            "allow_preempt=false never preempts before cooking land"
        );
    }

    #[test]
    fn no_fill_releases_far_mid_once_new_land_ready() {
        let mid = 258_000u64;
        let land = 748_000u64;
        // lead=0 ⇒ encode window == play land.
        let window = land - ENCODE_LEAD_SEGMENTS * SEGMENT_MS;
        assert_eq!(window, land);
        assert!(
            !no_fill_release_for_new_land(mid, land, false, window),
            "still cooking: keep no-fill hold"
        );
        assert!(
            no_fill_release_for_new_land(mid, land, true, window),
            "far mid after land ready: 503 so WebKit leaves dig-back"
        );
        assert!(
            !no_fill_release_for_new_land(land, land, true, window),
            "want is the play land: do not release"
        );
        assert!(
            !no_fill_release_for_new_land(land, 0, true, 0),
            "ahead of attach play: must not 503"
        );
        // Behind window (lead=0: one seg behind land) → release.
        let behind_window = land - SEGMENT_MS;
        assert!(
            no_fill_release_for_new_land(behind_window, land, true, window),
            "behind encode window after land ready: release"
        );
    }

    /// Waiter on cooking land blocks preempt kill; no waiter matches today's
    /// preempt-on immediate kill. Land-ready always may kill (land-then-yank).
    #[test]
    fn waiter_gates_kill_before_land() {
        assert!(
            may_kill_cooking_encode(false, 0),
            "no waiter: preempt-on may kill before land"
        );
        assert!(
            !may_kill_cooking_encode(false, 1),
            "waiter present: must not kill before land"
        );
        assert!(
            !may_kill_cooking_encode(false, 3),
            "any positive waiter count blocks kill"
        );
        assert!(
            may_kill_cooking_encode(true, 1),
            "land ready: kill allowed even with waiter (land-then-yank)"
        );
        assert!(
            may_kill_cooking_encode(true, 0),
            "land ready + no waiter: kill allowed"
        );
        // pending_restart_due still selects far C when allow_preempt; the
        // kill gate is separate (restart_at / may_kill_cooking_encode).
        let land_b = 1_494_000u64;
        let land_c = 2_070_000u64;
        assert_eq!(
            pending_restart_due(
                false,
                Some(land_c),
                Some(Duration::from_millis(470)),
                false,
                land_b,
                RESTART_MIN_INTERVAL,
                true,
            ),
            Some(land_c),
            "due decision unchanged when a waiter may later defer the kill"
        );
        assert!(
            !may_kill_cooking_encode(false, 1),
            "same due pending must still defer kill while waiter holds B's land"
        );
    }

    /// Prefetch seg ahead of an existing pending land must not advance it.
    /// Intentional short forward (startMs / desire at L+1 with no pending)
    /// must still be accepted — never clamp against cooking play_start alone.
    #[test]
    fn prefetch_does_not_advance_pending_short_scrub_via_start_ms_does() {
        let land = 100_000u64; // seg050
        let pending = Some(land);
        assert!(
            prefetch_advances_pending(pending, land + SEGMENT_MS),
            "L+1 miss is prefetch yank"
        );
        assert!(
            prefetch_advances_pending(pending, land + CATCH_UP_SEGMENTS * SEGMENT_MS),
            "L+CATCH_UP miss is prefetch yank"
        );
        assert!(
            !prefetch_advances_pending(pending, land + (CATCH_UP_SEGMENTS + 1) * SEGMENT_MS),
            "far ahead replaces pending"
        );
        assert!(
            !prefetch_advances_pending(pending, land.saturating_sub(SEGMENT_MS)),
            "behind pending is a real dig-back"
        );
        assert!(
            !prefetch_advances_pending(None, land + SEGMENT_MS),
            "no pending: segment path unchanged (short scrub lands via startMs)"
        );

        // startMs-shaped short forward while cooking at L: desire accepts L+1.
        let play = land;
        let encode = encode_start_ms(play);
        let short = land + SEGMENT_MS;
        assert_eq!(
            classify_restart_desire(short, play, encode, true),
            CoalesceDesire::HoldDebounce,
            "intentional short forward is a real land, not prefetch noise"
        );
        let mut pending_after = Some(align_to_segment(short));
        assert_eq!(pending_after, Some(short));
        // Once pending is L+1, a further +1 prefetch must not advance again.
        assert!(prefetch_advances_pending(pending_after, short + SEGMENT_MS));
        // Simulate applying the startMs land (pending becomes cooking).
        let _ = pending_after.take();
        assert!(!prefetch_advances_pending(None, short + SEGMENT_MS));
    }

    #[test]
    fn pending_waiter_holds_match_releases_mismatch() {
        let land = 100_000u64;
        assert_eq!(
            pending_waiter_action(None, land),
            PendingWaiterAction::Hold,
            "no pending: keep polling the cooking window"
        );
        assert_eq!(
            pending_waiter_action(Some(land), land),
            PendingWaiterAction::Hold,
            "pending matches this request"
        );
        assert_eq!(
            pending_waiter_action(Some(land), land + SEGMENT_MS),
            PendingWaiterAction::Release,
            "any non-exact want is superseded"
        );
        assert_eq!(
            pending_waiter_action(Some(land + 20_000), land),
            PendingWaiterAction::Release,
            "pending is a different scrub ahead"
        );
        assert_eq!(
            pending_waiter_action(Some(land), land + 60_000),
            PendingWaiterAction::Release,
            "pending far behind want: real supersede, release"
        );
    }

    #[test]
    fn refuse_serve_when_pending_apply_moved_play_land() {
        let land_a = 622_000u64;
        let land_b = 1_078_000u64;
        assert!(
            serve_ok_after_pending_apply(land_a, land_a, Some(land_a)),
            "same land: serve retained/cooked bytes"
        );
        assert!(
            !serve_ok_after_pending_apply(land_a, land_b, Some(land_a)),
            "play moved during request: do not return prior-land bytes"
        );
        assert!(
            serve_ok_after_pending_apply(land_a, land_b, Some(land_b)),
            "play moved to this request's land: land-ensure must still 200"
        );
        assert!(
            !serve_ok_after_pending_apply(land_a, land_b, None),
            "non-segment asset: play move refuses"
        );
    }

    #[test]
    fn stale_retain_cleared_when_play_land_ready() {
        let dir = tempfile::tempdir().expect("tempdir");
        let play_ms = 2_538_000u64;
        let mut session = Session {
            item_id: 1,
            src: PathBuf::from("/dev/null"),
            dir: dir.path().to_path_buf(),
            mode: SessionMode::Transcode,
            audio: stereo(),
            burn_in: None,
            video_encoder: "libx264".into(),
            start_ms: play_ms - ENCODE_LEAD_SEGMENTS * SEGMENT_MS,
            play_start_ms: play_ms,
            landed_ms: play_ms,
            usable_extent_ms: None,
            duration_ms: 3_600_000,
            current_run_id: 0,
            next_run_id: 1,
            segment_map: crate::hls_segment_map::SegmentMap::default(),
            current_run_eof: false,
            child: None,
            last_access: Instant::now(),
            last_restart: Instant::now(),
            primed: false,
            first_segment_ready: false,
            pending_play_ms: None,
            pending_since: None,
            stale_retain_refuse_until: Some(Instant::now() + STALE_RETAIN_REFUSE),
            failed: None,
            subtitle_tracks: vec![],
            segment_waiters: HashMap::new(),
            preempt_defer_logged: false,
        };
        session
            .segment_map
            .insert(crate::hls_segment_map::MappedSegment {
                start_ms: play_ms,
                duration_ms: SEGMENT_MS,
                run_id: 0,
                rel_path: PathBuf::from("run_0/seg000.m4s"),
            });
        fs::create_dir_all(dir.path().join("run_0")).unwrap();
        fs::write(dir.path().join("run_0/seg000.m4s"), b"seg").unwrap();
        note_first_segment_ready("test", &mut session);
        assert!(session.first_segment_ready);
        assert!(
            session.stale_retain_refuse_until.is_none(),
            "land ready must clear stale guard so Safari is not stuck 503-retrying"
        );
    }

    #[test]
    fn stale_retain_guard_refuses_far_behind_only_while_active() {
        // Lifecycle: armed during cook (refuse far-behind retained); cleared
        // when play land is ready (note_first_segment_ready) or TTL elapses —
        // same as guard_active=false so Safari can dig-back after coalesce.
        let play_b = 1_332_000u64;
        let land_a = 910_000u64;
        assert!(
            !serve_ok_retained_during_stale_guard(land_a, play_b, true),
            "prior land during guard: refuse"
        );
        assert!(
            serve_ok_retained_during_stale_guard(land_a, play_b, false),
            "after land-ready clear / TTL: retained prior land may serve"
        );
        assert!(
            serve_ok_retained_during_stale_guard(play_b, play_b, true),
            "exact play land: serve"
        );
        assert!(
            serve_ok_retained_during_stale_guard(
                play_b - ENCODE_LEAD_SEGMENTS * SEGMENT_MS,
                play_b,
                true
            ),
            "lead dig-back: serve"
        );
        assert!(
            serve_ok_retained_during_stale_guard(play_b + SEGMENT_MS, play_b, true),
            "ahead of play: serve"
        );
    }

    #[test]
    fn digback_behind_committed_blocks_behind_land_steal() {
        // Any want behind committed is dig-back (no segment-path restart).
        let cooking = 1_482_000u64;
        let dig = 1_478_000u64;
        let far = cooking - 60_000;
        assert!(digback_behind_committed(cooking, None, dig));
        assert!(digback_behind_committed(cooking, None, far));
        assert!(
            !digback_behind_committed(cooking, None, cooking),
            "same land is not dig-back"
        );
        assert!(
            !digback_behind_committed(cooking, None, cooking + SEGMENT_MS),
            "ahead is not dig-back"
        );

        let cooking_b = 1_054_000u64;
        let pending_c = 1_482_000u64;
        assert!(digback_behind_committed(cooking_b, Some(pending_c), dig));
        assert!(
            !digback_behind_committed(cooking_b, Some(pending_c), pending_c),
            "want == committed pending"
        );
    }

    /// Segment miss never Restarts; dig-back still blocks desire_restart.
    #[test]
    fn scrub_shaped_digback_must_not_desire() {
        let cooking = 1_482_000u64;
        let dig = 1_478_000u64;
        let idx = dig / SEGMENT_MS;
        let window = cooking / SEGMENT_MS;
        let play = window;
        assert_eq!(
            decide_segment_miss(
                idx * SEGMENT_MS,
                window * SEGMENT_MS,
                play * SEGMENT_MS,
                Some(window * SEGMENT_MS),
                true,
                RESTART_MIN_INTERVAL,
            ),
            SegmentMissAction::Wait
        );
        assert!(
            digback_behind_committed(cooking, None, dig),
            "asset_wait must skip desire_restart for this miss"
        );
    }

    /// Abandoned miss predicate: far behind / prior land → hold path; dig-back
    /// and pending land stay reachable (503 cook / desire).
    #[test]
    fn segment_miss_unreachable_table() {
        let cooking = 1_482_000u64;
        let window_ms = encode_start_ms(cooking);
        let play = cooking;
        let latest = Some(window_ms / SEGMENT_MS);
        let prior = 473 * SEGMENT_MS;
        let far = cooking - 60_000;
        let dig = cooking - 2 * SEGMENT_MS;
        let in_window = window_ms;
        let ahead = cooking + (CATCH_UP_SEGMENTS + 2) * SEGMENT_MS;

        // lead=0 ⇒ window == cooking; any behind-window miss is unreachable.
        let cases: &[(&str, u64, Option<u64>, bool, bool)] = &[
            ("far behind no pending", far, None, true, true),
            ("prior land after jump", prior, None, true, true),
            ("attach-shaped seg000", 0, None, true, true),
            ("behind land dig-back", dig, None, true, true),
            ("pending exact land", prior, Some(prior), true, false),
            ("in-window fill-forward", in_window, None, true, false),
            (
                "ahead of frontier (seek owns scrub)",
                ahead,
                None,
                true,
                false,
            ),
            ("unprimed far still unreachable", far, None, false, true),
        ];
        for &(name, want, pending, primed, expect_unreachable) in cases {
            assert_eq!(
                segment_miss_unreachable(want, cooking, pending, window_ms, play, latest, primed),
                expect_unreachable,
                "{name}"
            );
        }
    }

    /// ADR-0020: far scrub is POST /seek, not a segment GET. Seek then hold
    /// the land segment until the new run cooks it.
    #[test]
    fn scrub_segment_hold_returns_200_on_same_request() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture_secs(&src, 60);
        let reg = HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264").unwrap();
        let id = reg
            .start(
                1,
                &src,
                0,
                60_000,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
            )
            .unwrap();
        wait_playlist(&reg, &id);
        let _ = wait_first_listed_asset(&reg, &id);
        std::thread::sleep(RESTART_MIN_INTERVAL);

        let view = reg.seek(&id, 40_000).expect("seek");
        assert_ne!(view.run_id, 0, "fresh run after far seek");
        let t0 = Instant::now();
        let (land, _) = wait_land_near(&reg, &id, 40_000);
        assert!(!wait_asset(&reg, &id, &land).is_empty());
        assert!(
            t0.elapsed() < SEGMENT_WAIT + Duration::from_secs(5),
            "should finish within SEGMENT_WAIT"
        );
    }

    /// ADR-0020: producer-truth runs have no dig-back lead before their land.
    #[test]
    fn lead_zero_digback_before_land_is_not_covered() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture_secs(&src, 60);
        let play_ms = 40_000;
        assert_eq!(ENCODE_LEAD_SEGMENTS, 0);
        assert_eq!(encode_start_ms(play_ms), play_ms);
        let reg = HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264").unwrap();
        let id = reg
            .start(
                1,
                &src,
                play_ms,
                60_000,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
            )
            .unwrap();
        let (land, land_ms) = wait_land_near(&reg, &id, play_ms);
        assert!(!wait_asset(&reg, &id, &land).is_empty());

        let digback = crate::hls_segment_map::time_keyed_segment_name(land_ms - SEGMENT_MS);
        let t0 = Instant::now();
        match reg.asset(&id, &digback, None) {
            Err(PlaylistError::NotFound) | Err(PlaylistError::NotReady) => {
                assert!(
                    t0.elapsed() < Duration::from_secs(5),
                    "dig-back before land must fail quickly"
                );
            }
            Ok(_) => panic!("dig-back must not cook a retreated window"),
            Err(e) => panic!("unexpected dig-back error: {e:?}"),
        }
    }

    /// ADR-0020: a segment miss cannot make a new far-ahead producer run.
    /// Same-run fill-forward may eventually produce the bytes; that is not a
    /// scrub. What must not happen is a new `run_id` without POST /seek.
    #[test]
    fn segment_miss_without_seek_cannot_cook_far_ahead() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture_secs(&src, 60);
        let reg = HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264").unwrap();
        let id = reg
            .start(
                1,
                &src,
                0,
                60_000,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
            )
            .unwrap();
        let playlist = wait_playlist(&reg, &id);
        let _ = wait_asset(&reg, &id, &first_listed_seg(&playlist));
        std::thread::sleep(RESTART_MIN_INTERVAL);

        let run_before = {
            let sessions = reg.sessions.lock().unwrap();
            sessions.get(&id).unwrap().current_run_id
        };
        let land_ms = 40_000;
        let land = crate::hls_segment_map::time_keyed_segment_name(land_ms);
        assert!(
            !String::from_utf8_lossy(&playlist).contains(&land),
            "far-ahead segment must not already be listed"
        );
        // Miss may Wait/404/abandon, or Ok via same-run fill-forward — never
        // a new producer run.
        let _ = reg.asset(&id, &land, None);
        let run_after_miss = {
            let sessions = reg.sessions.lock().unwrap();
            sessions.get(&id).unwrap().current_run_id
        };
        assert_eq!(
            run_after_miss, run_before,
            "segment miss must not cook a far-ahead land"
        );

        let view = reg.seek(&id, land_ms).expect("seek");
        assert_ne!(view.run_id, run_before, "seek starts a new producer run");
        let seek_playlist = wait_playlist_run(&reg, &id, view.run_id);
        let seek_land = first_listed_seg(&seek_playlist);
        assert!(!wait_asset(&reg, &id, &seek_land).is_empty());
    }

    /// While a waiter holds for land A, a newer scrub moves pending to B.
    /// Once B's encode window is ready, a behind-window hold on A must 503
    /// (`no_fill_release_for_new_land`) so WebKit leaves dig-back — not sit
    /// until teardown (desktop-native single scrub: held mid, zero land GETs).
    /// Immediate 204 on supersede stays rejected (wedged Safari on doubles).
    #[test]
    fn held_segment_waiter_no_fill_when_pending_moves() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture_secs(&src, 60);
        let reg = HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264").unwrap();
        let id = reg
            .start(
                1,
                &src,
                0,
                60_000,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
            )
            .unwrap();
        wait_playlist(&reg, &id);
        let _ = wait_first_listed_asset(&reg, &id);
        std::thread::sleep(RESTART_MIN_INTERVAL);

        let (tx, rx) = std::sync::mpsc::channel();
        let reg_hold = Arc::clone(&reg);
        let id_hold = id.clone();
        // Far of the initial window so the GET Waits; seek then supersedes.
        let hold_name = crate::hls_segment_map::time_keyed_segment_name(40_000);
        std::thread::spawn(move || {
            let t0 = Instant::now();
            let result = reg_hold.asset(&id_hold, &hold_name, None);
            let _ = tx.send((result, t0.elapsed()));
        });
        // Retarget within the fixture so the new run can land (EOF on a
        // past-duration seek never flips first_segment_ready). Budget covers
        // SEGMENT_WAIT on the hold plus land cook on the seek.
        let probe_until = Instant::now() + SEGMENT_WAIT + Duration::from_secs(20);
        while Instant::now() < probe_until {
            let _ = reg.seek(&id, 50_000);
            match rx.try_recv() {
                Ok((first, elapsed)) => {
                    assert!(
                        matches!(first, Err(PlaylistError::NotReady)) || first.is_ok(),
                        "superseded behind-window hold: 503 after new land, or 200 if A cooked first; got {:?} elapsed={elapsed:?}",
                        first
                            .as_ref()
                            .map(|b| b.len())
                            .map_err(|e| format!("{e:?}"))
                    );
                    let _ = reg.stop(&id);
                    return;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    panic!("hold thread disconnected without sending");
                }
            }
        }
        let _ = reg.stop(&id);
        panic!("hold did not finish within SEGMENT_WAIT+20s after supersede");
    }

    /// ADR-0020: far scrub is seek API. Pending apply from a second seek
    /// while land A cooks; land B segment then 200s from the new run.
    #[test]
    fn final_land_waiter_applies_pending_when_cooking_land_appears() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture_secs(&src, 120);
        let reg = HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264").unwrap();
        let id = reg
            .start(
                1,
                &src,
                20_000,
                120_000,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
            )
            .unwrap();
        {
            let sessions = reg.sessions.lock().unwrap();
            let s = sessions.get(&id).unwrap();
            assert_eq!(s.play_start_ms, 20_000);
            assert!(!s.first_segment_ready);
        }

        // Seek to B while A may still be cooking (may defer if land waiter).
        let _ = reg.seek(&id, 40_000).expect("seek B");
        // Drive readiness: playlist/asset poll notices land and applies pending.
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            let _ = reg.playlist(&id, reg.view(&id).map(|v| v.run_id).unwrap_or(0));
            let sessions = reg.sessions.lock().unwrap();
            let s = sessions.get(&id).unwrap();
            if s.play_start_ms == 40_000 && s.first_segment_ready {
                break;
            }
            drop(sessions);
            if Instant::now() >= deadline {
                panic!("pending B did not apply within 90s");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let bytes = wait_asset(&reg, &id, "seg_00000040000.m4s");
        assert!(!bytes.is_empty());
        let sessions = reg.sessions.lock().unwrap();
        assert_eq!(sessions.get(&id).unwrap().play_start_ms, 40_000);
    }

    #[test]
    fn encode_start_includes_lead_before_play() {
        assert_eq!(ENCODE_LEAD_SEGMENTS, 0);
        assert_eq!(encode_start_ms(1_264_000), 1_264_000);
        assert_eq!(encode_start_ms(1_000), 0);
        assert_eq!(encode_start_ms(0), 0);
        assert_eq!(encode_start_ms(4_000), 4_000);
        assert_eq!(encode_start_ms(16_000), 16_000);
        assert_eq!(encode_start_ms(18_000), 18_000);
    }

    /// Mid-title switch: encode starts at land. Behind-window dig-back must
    /// not retreat play land; real scrub-back is POST /seek.
    #[test]
    fn switch_session_serves_first_requested_segment() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture_secs(&src, 60);
        let duration_ms = 60_000;
        let play_ms = 40_000;
        let reg = HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264").unwrap();

        let prior = reg
            .start(
                1,
                &src,
                0,
                duration_ms,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
            )
            .unwrap();
        wait_playlist(&reg, &prior);

        let switched = reg
            .start(
                1,
                &src,
                play_ms,
                duration_ms,
                SessionMode::Transcode,
                AudioSelection {
                    stream_index: Some(0),
                    channels: 2,
                    channel_layout: Some("stereo".into()),
                    max_channels: 2,
                },
                vec![],
                None,
            )
            .unwrap();
        let playlist = wait_playlist(&reg, &switched);
        let text = String::from_utf8_lossy(&playlist);
        assert!(
            text.contains("#EXT-X-START:TIME-OFFSET=0.000,PRECISE=YES"),
            "EXT-X-START is window-relative (ADR-0020): {text}"
        );
        let (land, land_ms) = wait_land_near(&reg, &switched, play_ms);
        assert!(
            !wait_asset(&reg, &switched, &land).is_empty(),
            "play-land segment servable"
        );
        assert_eq!(
            decide_segment_miss(0, 40_000, 40_000, None, false, RESTART_MIN_INTERVAL),
            SegmentMissAction::Wait,
            "seg at t=0 behind a 40s window must wait"
        );

        // Behind encode window: unlisted under producer-truth → 404, not a
        // retreated cook. Real scrub-back is POST /seek.
        let dig_ms = 5 * SEGMENT_MS;
        assert!(digback_behind_committed(play_ms, None, dig_ms));
        match reg.asset(&switched, "seg_00000010000.m4s", None) {
            Err(PlaylistError::NotFound) | Err(PlaylistError::NotReady) => {}
            Ok(_) => panic!("dig-back must not cook a retreated window"),
            Err(e) => panic!("unexpected dig-back error: {e:?}"),
        }
        assert!(
            !wait_asset(
                &reg,
                &switched,
                &crate::hls_segment_map::time_keyed_segment_name(land_ms)
            )
            .is_empty(),
            "switch land segment must serve"
        );

        assert!(reg.stop(&prior));
        assert!(reg.stop(&switched));
    }

    /// Fresh mid-title session: encode-at-land cooks near play first.
    #[test]
    fn new_session_serves_first_requested_segment() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture_secs(&src, 60);
        let duration_ms = 60_000;
        let play_ms = 40_000;
        let reg = HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264").unwrap();
        let id = reg
            .start(
                1,
                &src,
                play_ms,
                duration_ms,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
            )
            .unwrap();
        let playlist = wait_playlist(&reg, &id);
        let text = String::from_utf8_lossy(&playlist);
        assert!(
            text.contains("#EXT-X-START:TIME-OFFSET=0.000,PRECISE=YES"),
            "EXT-X-START is window-relative (ADR-0020): {text}"
        );
        let (land, _) = wait_land_near(&reg, &id, play_ms);
        assert!(
            !wait_asset(&reg, &id, &land).is_empty(),
            "land segment must be served"
        );
        assert!(reg.stop(&id));
    }

    #[test]
    fn session_produces_vod_playlist_and_stop_reaps() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture(&src);
        let reg = HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264").unwrap();
        let id = reg
            .start(
                1,
                &src,
                0,
                FIXTURE_MS,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
            )
            .unwrap();
        assert_eq!(
            reg.encoder(&id),
            Some(SessionEncoder {
                name: "libx264".into(),
                kind: EncoderKind::Software,
            })
        );
        let playlist = wait_playlist(&reg, &id);
        let text = String::from_utf8_lossy(&playlist);
        assert!(text.contains("#EXT-X-PLAYLIST-TYPE:EVENT"), "{text}");
        assert!(text.contains("#EXT-X-START:TIME-OFFSET=0.000"), "{text}");
        let land = first_listed_seg(&playlist);
        assert!(reg.asset(&id, &land, None).is_ok(), "land={land}");
        assert!(reg.stop(&id));
        assert!(matches!(reg.playlist(&id, 0), Err(PlaylistError::NotFound)));
    }

    /// Session-inline demux with no scan-time extract: the video segment and
    /// the first subtitle window both become servable from one session start.
    #[test]
    fn session_subtitle_segment_without_scan_extract() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files/h264_aac_srt_mkv.mkv");
        if !corpus.exists() {
            eprintln!("skipping: missing {}", corpus.display());
            return;
        }
        let streams = crate::list_text_subtitles(&corpus).expect("list");
        assert!(!streams.is_empty());
        let track = &streams[0];
        let track_id = track.track_id();
        let dir = tempfile::tempdir().unwrap();
        let reg = HlsSessionRegistry::with_cap(dir.path().join("hls"), 2, "libx264").unwrap();
        // Short window: we only need first video seg + first subtitle slice.
        let id = reg
            .start(
                1,
                &corpus,
                0,
                4000,
                SessionMode::Copy,
                stereo(),
                vec![HlsSubtitleTrack {
                    track_id: track_id.clone(),
                    language: track.language.clone(),
                    name: track_id.clone(),
                    is_default: true,
                    forced: false,
                    sdh: false,
                    item_id: 1,
                    stream_index: Some(track.stream_index),
                    sidecar_path: None,
                    codec: track.codec.clone(),
                    item_vtt_path: None,
                }],
                None,
            )
            .unwrap();
        wait_playlist(&reg, &id);
        let sub_pl = reg.subtitle_playlist(&id, &track_id).expect("sub playlist");
        let sub_text = String::from_utf8_lossy(&sub_pl);
        assert!(
            sub_text.contains(&format!("{track_id}/seg000.vtt")),
            "{sub_text}"
        );
        let mut seg = None;
        for _ in 0..100 {
            match reg.subtitle_segment(&id, &track_id, 0) {
                Ok(bytes) => {
                    seg = Some(bytes);
                    break;
                }
                Err(PlaylistError::NotReady) => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => panic!("subtitle segment: {e:?}"),
            }
        }
        let seg = String::from_utf8(seg.expect("subtitle seg000 not ready in time")).unwrap();
        assert!(seg.contains("\nNightjar SRT sample\n"), "{seg}");
        reg.stop(&id);
    }

    /// One session per start, even for the same item at the same offset, and
    /// stopping one leaves the other playing (ADR-0011).
    #[test]
    fn every_start_is_its_own_session() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture(&src);
        let reg = HlsSessionRegistry::with_cap(dir.path().join("hls"), 2, "libx264").unwrap();
        let a = reg
            .start(
                1,
                &src,
                0,
                FIXTURE_MS,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
            )
            .unwrap();
        let b = reg
            .start(
                1,
                &src,
                0,
                FIXTURE_MS,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
            )
            .unwrap();
        assert_ne!(a, b);
        assert!(matches!(
            reg.start(
                2,
                &src,
                0,
                FIXTURE_MS,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None
            ),
            Err(StartSessionError::CapFull)
        ));
        assert!(reg.stop(&a));
        assert!(matches!(reg.playlist(&a, 0), Err(PlaylistError::NotFound)));
        wait_playlist(&reg, &b);
        reg.stop(&b);
    }

    /// A copy session must never reach the configured encoder: an unusable
    /// encoder name still yields segments because video is stream-copied.
    #[test]
    fn copy_session_bypasses_the_encoder() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture(&src);
        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "no_such_encoder").unwrap();
        let id = reg
            .start(
                1,
                &src,
                0,
                FIXTURE_MS,
                SessionMode::Copy,
                stereo(),
                vec![],
                None,
            )
            .unwrap();
        assert_eq!(
            reg.encoder(&id),
            Some(SessionEncoder {
                name: "copy".into(),
                kind: EncoderKind::Copy,
            })
        );
        wait_playlist(&reg, &id);
        assert!(
            reg.asset(&id, &first_listed_seg(&wait_playlist(&reg, &id)), None)
                .is_ok()
        );
        reg.stop(&id);
    }

    #[test]
    fn seek_retains_prior_window_segments() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture(&src);
        let reg = HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264").unwrap();
        let id = reg
            .start(
                1,
                &src,
                0,
                FIXTURE_MS,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
            )
            .unwrap();
        wait_playlist(&reg, &id);
        let early_name = first_listed_seg(&wait_playlist(&reg, &id));
        let early = wait_asset(&reg, &id, &early_name);
        // Move the window forward; stale-retain may 503 behind-play until the
        // new land is ready (ENCODE_LEAD=0). Then prior bytes stay readable.
        for _ in 0..100 {
            match reg.seek(&id, 2000) {
                Ok(_) => break,
                Err(PlaylistError::NotReady) => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => panic!("seek: {e:?}"),
            }
        }
        let _ = wait_playlist_run(&reg, &id, 1);
        let still = wait_asset(&reg, &id, &early_name);
        assert_eq!(early.len(), still.len());
        assert!(reg.asset(&id, &early_name, None).is_ok());
        // Scrub-back to already-mapped media: duplicate-write stop (no ffmpeg).
        let view = reg.seek(&id, 0).expect("seek back");
        {
            let sessions = reg.sessions.lock().unwrap();
            let s = sessions.get(&id).unwrap();
            assert!(s.first_segment_ready, "expected map-hit ready");
            assert!(s.child.is_none(), "map hit must not spawn ffmpeg");
            assert_eq!(s.play_start_ms, 0);
        }
        assert!(reg.asset(&id, &early_name, None).is_ok());
        assert!(view.run_id >= 2, "fresh playlist URI even on map hit");
        reg.stop(&id);
    }

    /// Encodes a fixture whose audio streams carry `layouts`, one stream each.
    fn make_fixture_layouts(path: &Path, layouts: &[&str]) {
        let mut cmd = Command::new("ffmpeg");
        cmd.args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=64x64:d=4",
        ]);
        for layout in layouts {
            cmd.args([
                "-f",
                "lavfi",
                "-i",
                &format!("anullsrc=r=48000:cl={layout}:d=4"),
            ]);
        }
        cmd.args(["-map", "0:v:0"]);
        for i in 1..=layouts.len() {
            cmd.args(["-map", &format!("{i}:a:0")]);
        }
        cmd.args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"]);
        let status = cmd.arg(path).status().unwrap();
        assert!(status.success(), "fixture encode failed for {layouts:?}");
    }

    /// Runs one session encode to completion and joins its segments back into
    /// a single file so the delivered streams can be probed.
    fn encode_and_join(
        src: &Path,
        dir: &Path,
        mode: SessionMode,
        audio: AudioSelection,
        encoder: &str,
    ) -> PathBuf {
        let enc = dir.join("enc");
        fs::create_dir_all(&enc).unwrap();
        let mut child = spawn_ffmpeg(src, &enc, 0, mode, audio, encoder, None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        stop_child(&mut Some(child));
        assert!(
            enc.join("seg000.m4s").exists(),
            "session produced no segments"
        );
        let joined = dir.join("joined.mp4");
        let status = Command::new("ffmpeg")
            .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
            .arg(enc.join("index.m3u8"))
            .args(["-c", "copy"])
            .arg(&joined)
            .status()
            .unwrap();
        assert!(status.success(), "remux from HLS failed");
        joined
    }

    fn probe_entry(path: &Path, select: &str, entry: &str) -> String {
        let out = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                select,
                "-show_entries",
                &format!("stream={entry}"),
                "-of",
                "csv=p=0",
            ])
            .arg(path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "ffprobe failed for {}",
            path.display()
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// ADR-0012 decision 2: a 5.1 track above the ceiling forces an audio
    /// encode, never a video one. The registry encoder is unusable, so any
    /// attempt to re-encode video fails the whole session.
    #[test]
    fn copy_session_downmixes_audio_without_touching_video() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("surround.mp4");
        make_fixture_layouts(&src, &["5.1"]);
        let audio = AudioSelection {
            stream_index: None,
            channels: 6,
            channel_layout: Some("5.1".into()),
            max_channels: 2,
        };
        let joined = encode_and_join(
            &src,
            dir.path(),
            SessionMode::Copy,
            audio,
            "no_such_encoder",
        );
        assert_eq!(probe_entry(&joined, "v:0", "codec_name"), "h264");
        assert_eq!(probe_entry(&joined, "a:0", "channels"), "2");
    }

    /// 6.0 shares a channel count with 5.1 but not the index map; falling
    /// back to -ac 2 is the correct (if imperfect) path (ADR-0012).
    #[test]
    fn unknown_named_layout_falls_back_to_ac2() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("six_oh.mkv");
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
                "testsrc=size=320x240:rate=24:duration=1",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=channel_layout=6.0:sample_rate=48000:duration=1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "flac",
                "-shortest",
                src.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(status.success());
        assert!(
            stereo_downmix_filter(6, Some("6.0")).is_none(),
            "6.0 must not use the 5.1 pan table"
        );
        let audio = AudioSelection {
            stream_index: None,
            channels: 6,
            channel_layout: Some("6.0".into()),
            max_channels: 2,
        };
        let joined = encode_and_join(&src, dir.path(), SessionMode::Transcode, audio, "libx264");
        assert_eq!(probe_entry(&joined, "a:0", "channels"), "2");
    }

    /// A non-default track is reachable by absolute stream index (ADR-0012).
    #[test]
    fn session_maps_the_selected_audio_stream() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("two_audio.mp4");
        make_fixture_layouts(&src, &["stereo", "mono"]);
        let second = AudioSelection {
            stream_index: Some(2),
            channels: 1,
            channel_layout: Some("mono".into()),
            max_channels: 2,
        };
        let joined = encode_and_join(&src, dir.path(), SessionMode::Copy, second, "libx264");
        assert_eq!(
            probe_entry(&joined, "a:0", "channels"),
            "1",
            "expected the mono second track, not the stereo default"
        );
    }

    #[test]
    fn master_playlist_declares_subtitle_group() {
        let tracks = vec![
            HlsSubtitleTrack {
                track_id: "e2".into(),
                language: Some("en".into()),
                name: "SDH".into(),
                is_default: true,
                forced: false,
                sdh: true,
                item_id: 176,
                stream_index: Some(2),
                sidecar_path: None,
                codec: "subrip".into(),
                item_vtt_path: None,
            },
            HlsSubtitleTrack {
                track_id: "e3".into(),
                language: Some("en".into()),
                name: "en".into(),
                is_default: false,
                forced: false,
                sdh: false,
                item_id: 176,
                stream_index: Some(3),
                sidecar_path: None,
                codec: "subrip".into(),
                item_vtt_path: None,
            },
        ];
        let text = String::from_utf8(build_master("s1", 0, &tracks)).unwrap();
        assert!(text.contains("#EXT-X-MEDIA:TYPE=SUBTITLES"));
        assert!(text.contains("GROUP-ID=\"subs\""));
        assert!(text.contains("URI=\"/api/v0/sessions/s1/subs/e2.m3u8\""));
        assert!(text.contains("SUBTITLES=\"subs\""));
        assert!(text.contains("\n/api/v0/sessions/s1/runs/0/index.m3u8\n"));
        assert!(
            text.contains("CHARACTERISTICS=\"public.accessibility.transcribes-spoken-dialog\"")
        );
        assert!(!text.contains("CODECS="), "{text}");
        assert!(!text.contains("media.m3u8"));
    }

    #[test]
    fn master_without_tracks_has_no_subtitles_attr() {
        let text = String::from_utf8(build_master("s1", 0, &[])).unwrap();
        assert!(!text.contains("EXT-X-MEDIA"));
        assert!(!text.contains("SUBTITLES="));
        assert!(!text.contains("CODECS="), "{text}");
        assert!(text.contains("\n/api/v0/sessions/s1/runs/0/index.m3u8\n"));
    }

    #[test]
    fn subtitle_media_playlist_is_segmented_vod() {
        let text = String::from_utf8(build_segmented_subtitle_playlist("e2", 5000)).unwrap();
        assert!(text.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(text.contains("e2/seg000.vtt"));
        assert!(text.contains("e2/seg001.vtt"));
        assert!(text.contains("e2/seg002.vtt"));
        assert!(!text.contains("/api/v0/items/"));
        assert!(text.ends_with("#EXT-X-ENDLIST\n"));
        assert_eq!(text.matches("#EXTINF:").count(), 3);
    }

    /// Ready-store path: slice cues from an on-disk item VTT without session demux.
    #[test]
    fn item_store_vtt_slices_into_hls_segments() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture_secs(&src, 4);
        let vtt = dir.path().join("e2.vtt");
        fs::write(
            &vtt,
            "WEBVTT\n\n1\n00:00:00.500 --> 00:00:01.500\nHello\n\n2\n00:00:02.500 --> 00:00:03.500\nWorld\n\n",
        )
        .unwrap();
        let reg = HlsSessionRegistry::with_cap(dir.path().join("hls"), 2, "libx264").unwrap();
        let id = reg
            .start(
                1,
                &src,
                0,
                4000,
                SessionMode::Copy,
                stereo(),
                vec![HlsSubtitleTrack {
                    track_id: "e2".into(),
                    language: Some("en".into()),
                    name: "en".into(),
                    is_default: true,
                    forced: false,
                    sdh: false,
                    item_id: 1,
                    stream_index: Some(2),
                    sidecar_path: None,
                    codec: "subrip".into(),
                    item_vtt_path: Some(vtt),
                }],
                None,
            )
            .unwrap();
        wait_playlist(&reg, &id);
        let sub_pl = String::from_utf8(reg.subtitle_playlist(&id, "e2").unwrap()).unwrap();
        assert!(sub_pl.contains("e2/seg000.vtt"), "{sub_pl}");
        assert!(!sub_pl.contains("/api/v0/items/"), "{sub_pl}");
        let seg0 = String::from_utf8(reg.subtitle_segment(&id, "e2", 0).unwrap()).unwrap();
        assert!(seg0.contains("Hello"), "{seg0}");
        assert!(!seg0.contains("World"), "{seg0}");
        let seg1 = String::from_utf8(reg.subtitle_segment(&id, "e2", 1).unwrap()).unwrap();
        assert!(seg1.contains("World"), "{seg1}");
        assert!(!seg1.contains("Hello"), "{seg1}");
        reg.stop(&id);
    }

    /// Segment count and boundaries mirror the video VOD playlist so a
    /// player's segment index maps to the same subtitle window.
    #[test]
    fn asset_name_allowlist() {
        assert!(is_safe_asset("init.mp4"));
        assert!(is_safe_asset("seg_00000008000.m4s"));
        assert!(!is_safe_asset("seg000.m4s"));
        assert!(!is_safe_asset("../etc/passwd"));
        assert_eq!(
            crate::hls_segment_map::parse_time_keyed_segment_name("seg_00000008000.m4s"),
            Some(8000)
        );
    }

    /// Keyframe PTS must land on SEGMENT_MS boundaries regardless of source
    /// frame rate. A frame-count -g fails this at 60 fps and on VFR.
    #[test]
    fn keyframes_align_to_segment_duration() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let fps60 = dir.path().join("60fps.mp4");
        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=60:duration=6",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=6",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-shortest",
                fps60.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(status.success(), "60fps fixture encode failed");

        let vfr = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files/h264_aac_vfr_mp4.mp4");
        let cases: &[(&str, &Path)] = &[("60fps", fps60.as_path()), ("vfr", vfr.as_path())];

        for (name, src) in cases {
            if !src.exists() {
                eprintln!("skipping {name}: missing {}", src.display());
                continue;
            }
            let enc = dir.path().join(name);
            fs::create_dir_all(&enc).unwrap();
            let mut child = spawn_ffmpeg(
                src,
                &enc,
                0,
                SessionMode::Transcode,
                stereo(),
                "libx264",
                None,
            )
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            stop_child(&mut Some(child));
            assert!(
                enc.join("index.m3u8").exists(),
                "{name}: ffmpeg playlist missing"
            );
            assert!(
                enc.join("seg000.m4s").exists(),
                "{name}: expected at least seg000.m4s"
            );

            let joined = dir.path().join(format!("{name}-joined.mp4"));
            let status = Command::new("ffmpeg")
                .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
                .arg(enc.join("index.m3u8"))
                .args(["-c", "copy"])
                .arg(&joined)
                .status()
                .unwrap();
            assert!(status.success(), "{name}: remux from HLS failed");

            let out = Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-select_streams",
                    "v:0",
                    "-show_frames",
                    "-show_entries",
                    "frame=key_frame,pts_time",
                    "-of",
                    "csv=p=0",
                ])
                .arg(&joined)
                .output()
                .unwrap();
            assert!(out.status.success(), "{name}: ffprobe failed");
            let text = String::from_utf8_lossy(&out.stdout);
            let key_pts: Vec<f64> = text
                .lines()
                .filter_map(|line| {
                    let mut parts = line.split(',');
                    let key = parts.next()?;
                    let pts = parts.next()?;
                    if key == "1" { pts.parse().ok() } else { None }
                })
                .collect();
            assert!(!key_pts.is_empty(), "{name}: no keyframes\n{text}");

            let segment_s = SEGMENT_MS as f64 / 1000.0;
            for pts in &key_pts {
                let nearest = (pts / segment_s).round() * segment_s;
                assert!(
                    (pts - nearest).abs() < 0.05,
                    "{name}: keyframe at {pts} not on a {segment_s}s boundary ({key_pts:?})"
                );
            }
            assert!(
                key_pts.iter().any(|p| (*p - 0.0).abs() < 0.05),
                "{name}: missing IDR at 0 ({key_pts:?})"
            );
            assert!(
                key_pts.iter().any(|p| (*p - segment_s).abs() < 0.05),
                "{name}: missing IDR at {segment_s}s ({key_pts:?})"
            );
        }
    }

    #[test]
    fn escape_ffmpeg_filter_path_escapes_colon() {
        let escaped = escape_ffmpeg_filter_path(Path::new("/Volumes/NAS:share/a.ass"));
        assert!(escaped.contains(r"\:"), "{escaped}");
    }

    #[test]
    fn libass_filters_listed_requires_ass_and_subtitles() {
        let with = "\
 .. overlay           VV->V      Overlay a video source on top of the input.
 .. ass               V->V       Render ASS subtitles onto input video using the libass library.
 .. subtitles         V->V       Render text subtitles onto input video using the libass library.
";
        assert!(libass_filters_listed(with));
        let without_ass = "\
 .. overlay           VV->V      Overlay a video source on top of the input.
 .. subtitles         V->V       Render text subtitles onto input video using the libass library.
";
        assert!(!libass_filters_listed(without_ass));
        let without_both = "\
 .. overlay           VV->V      Overlay a video source on top of the input.
";
        assert!(!libass_filters_listed(without_both));
    }

    #[test]
    fn libass_filters_listed_live_ffmpeg_and_stripped() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let output = Command::new("ffmpeg")
            .args(["-hide_banner", "-filters"])
            .output()
            .expect("ffmpeg -filters");
        let text = if output.stdout.is_empty() {
            String::from_utf8_lossy(&output.stderr).into_owned()
        } else {
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        // Equipped host (this dogfood machine): both filters present.
        // Lacking host: strip those lines from the same real table shape.
        let live = libass_filters_listed(&text);
        let stripped: String = text
            .lines()
            .filter(|line| {
                !matches!(
                    line.split_whitespace().nth(1),
                    Some("ass") | Some("subtitles")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !libass_filters_listed(&stripped),
            "stripping ass/subtitles from live -filters must fail closed"
        );
        if live {
            assert!(
                ensure_libass_for_ass(BurnInKind::Ass, live).is_ok()
                    && ensure_libass_for_ass(BurnInKind::Ass, false).is_err()
            );
        } else {
            assert!(ensure_libass_for_ass(BurnInKind::Ass, live).is_err());
        }
    }

    #[test]
    fn ensure_libass_for_ass_fails_closed_without_filters() {
        assert!(ensure_libass_for_ass(BurnInKind::Ass, false).is_err());
        assert!(ensure_libass_for_ass(BurnInKind::Ass, true).is_ok());
        assert!(ensure_libass_for_ass(BurnInKind::Pgs, false).is_ok());
    }

    #[test]
    fn ass_burn_vf_embedded_and_sidecar() {
        let side = BurnInSelection {
            track_id: "s-en".into(),
            kind: BurnInKind::Ass,
            stream_index: None,
            subtitle_ordinal: None,
            sidecar_path: Some(PathBuf::from("/tmp/a.ass")),
        };
        assert_eq!(ass_burn_vf(&side, 0).unwrap(), "ass=/tmp/a.ass");
        let mid = ass_burn_vf(&side, 10_000).unwrap();
        assert!(
            mid.starts_with("setpts=PTS+10/TB,ass=") && mid.ends_with(",setpts=PTS-10/TB"),
            "{mid}"
        );
        let spaced = BurnInSelection {
            track_id: "s-en".into(),
            kind: BurnInKind::Ass,
            stream_index: None,
            subtitle_ordinal: None,
            sidecar_path: Some(PathBuf::from("/tmp/The Movie (2007)/a.ass")),
        };
        let vf = ass_burn_vf(&spaced, 0).unwrap();
        assert!(
            vf.contains(r"\(") && vf.contains(r"\)") && vf.contains(r"\ "),
            "{vf}"
        );
        assert!(
            ass_burn_vf(
                &BurnInSelection {
                    track_id: "e2".into(),
                    kind: BurnInKind::Ass,
                    stream_index: Some(2),
                    subtitle_ordinal: Some(0),
                    sidecar_path: None,
                },
                0
            )
            .is_err()
        );
        assert!(pgs_overlay_graph(&side).is_none());
    }

    #[test]
    fn pgs_overlay_graph_embedded_only() {
        let pgs = BurnInSelection {
            track_id: "e3".into(),
            kind: BurnInKind::Pgs,
            stream_index: Some(3),
            subtitle_ordinal: Some(0),
            sidecar_path: None,
        };
        assert_eq!(
            pgs_overlay_graph(&pgs).as_deref(),
            Some("[0:v:0][0:s:0]overlay")
        );
    }

    fn rgb24_abs_diff(a: &[u8], b: &[u8]) -> u64 {
        assert_eq!(a.len(), b.len());
        a.iter()
            .zip(b.iter())
            .map(|(x, y)| u64::from(x.abs_diff(*y)))
            .sum()
    }

    fn ffmpeg_rgb24_frame(src: &Path, vf: Option<&str>, ss_ms: u64) -> Vec<u8> {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-hide_banner", "-loglevel", "error", "-y"]);
        if ss_ms > 0 {
            cmd.args(["-ss", &format!("{:.3}", ss_ms as f64 / 1000.0)]);
        }
        cmd.arg("-i").arg(src).args(["-an", "-frames:v", "1"]);
        if let Some(vf) = vf {
            cmd.args(["-vf", vf]);
        }
        cmd.args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-"]);
        let out = cmd.output().expect("spawn ffmpeg");
        assert!(
            out.status.success(),
            "ffmpeg frame failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    #[test]
    fn burn_in_ass_corpus_changes_pixels() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        if !ffmpeg_has_libass_filters() {
            eprintln!("skipping: host ffmpeg lacks libass filters");
            return;
        }
        let corpus = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files/h264_aac_ass_mkv.mkv");
        if !corpus.exists() {
            eprintln!("skipping: missing {}", corpus.display());
            return;
        }
        let streams = crate::list_burn_in_subtitles(&corpus).expect("list");
        let burn = streams
            .iter()
            .find(|s| s.kind == BurnInKind::Ass)
            .expect("ass track");
        let dir = tempfile::tempdir().unwrap();
        let extracted = dir.path().join("burn_e.ass");
        crate::extract_embedded_ass(&corpus, burn.stream_index, &extracted).expect("extract");
        let selection = BurnInSelection {
            track_id: burn.track_id(),
            kind: BurnInKind::Ass,
            stream_index: Some(burn.stream_index),
            subtitle_ordinal: Some(burn.subtitle_ordinal),
            sidecar_path: Some(extracted.clone()),
        };
        let vf = ass_burn_vf(&selection, 0).unwrap();
        assert!(vf.starts_with("ass="), "{vf}");
        let plain = ffmpeg_rgb24_frame(&corpus, None, 0);
        let burned = ffmpeg_rgb24_frame(&corpus, Some(&vf), 0);
        let diff = rgb24_abs_diff(&plain, &burned);
        assert!(
            diff > 100_000,
            "embedded ASS burn should change pixels; diff_sum={diff}"
        );

        // Sidecar path: extract ASS, burn via ass=
        let side = dir.path().join("track.ass");
        let extract = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(&corpus)
            .args(["-map", "0:s:0", "-c", "copy"])
            .arg(&side)
            .output()
            .unwrap();
        assert!(extract.status.success());
        let side_sel = BurnInSelection {
            track_id: "s-en".into(),
            kind: BurnInKind::Ass,
            stream_index: None,
            subtitle_ordinal: None,
            sidecar_path: Some(side),
        };
        let side_vf = ass_burn_vf(&side_sel, 0).unwrap();
        assert!(side_vf.starts_with("ass="), "{side_vf}");
        let side_burned = ffmpeg_rgb24_frame(&corpus, Some(&side_vf), 0);
        let side_diff = rgb24_abs_diff(&plain, &side_burned);
        assert!(
            side_diff > 100_000,
            "sidecar ASS burn should change pixels; diff_sum={side_diff}"
        );
    }

    #[test]
    fn burn_in_ass_session_produces_segments() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        if !ffmpeg_has_libass_filters() {
            eprintln!("skipping: host ffmpeg lacks libass filters");
            return;
        }
        let corpus = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files/h264_aac_ass_mkv.mkv");
        if !corpus.exists() {
            eprintln!("skipping: missing {}", corpus.display());
            return;
        }
        let streams = crate::list_burn_in_subtitles(&corpus).expect("list");
        let burn = streams
            .iter()
            .find(|s| s.kind == BurnInKind::Ass)
            .expect("ass track");
        let selection = BurnInSelection {
            track_id: burn.track_id(),
            kind: BurnInKind::Ass,
            stream_index: Some(burn.stream_index),
            subtitle_ordinal: Some(burn.subtitle_ordinal),
            sidecar_path: None,
        };
        let dir = tempfile::tempdir().unwrap();
        let reg = HlsSessionRegistry::with_cap(dir.path().join("hls"), 2, "libx264").unwrap();
        let id = reg
            .start(
                1,
                &corpus,
                0,
                2000,
                SessionMode::Copy,
                stereo(),
                vec![],
                Some(selection),
            )
            .unwrap();
        {
            let sessions = reg.sessions.lock().unwrap();
            let s = sessions.get(&id).unwrap();
            assert_eq!(s.mode, SessionMode::Copy);
            assert!(s.burn_in.is_some());
        }
        wait_playlist(&reg, &id);
        let _ = wait_asset(&reg, &id, "init.mp4");
        let _ = wait_first_listed_asset(&reg, &id);
        reg.stop(&id);
    }

    #[test]
    fn burn_in_sidecar_ass_session_produces_segments() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        if !ffmpeg_has_libass_filters() {
            eprintln!("skipping: host ffmpeg lacks libass filters");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("movie.mp4");
        make_fixture_secs(&src, 2);
        let ass = dir.path().join("movie.ass");
        fs::write(
            &ass,
            "[Script Info]\nScriptType: v4.00+\n\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: Default,Arial,48,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,2,10,10,20,1\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:00.00,0:00:02.00,Default,,0,0,0,,Sidecar ASS\n",
        )
        .unwrap();
        let selection = BurnInSelection {
            track_id: "s-en".into(),
            kind: BurnInKind::Ass,
            stream_index: None,
            subtitle_ordinal: None,
            sidecar_path: Some(ass),
        };
        let reg = HlsSessionRegistry::with_cap(dir.path().join("hls"), 2, "libx264").unwrap();
        let id = reg
            .start(
                1,
                &src,
                0,
                2000,
                SessionMode::Transcode,
                stereo(),
                vec![],
                Some(selection),
            )
            .unwrap();
        wait_playlist(&reg, &id);
        let _ = wait_first_listed_asset(&reg, &id);
        reg.stop(&id);
    }

    #[test]
    fn burn_in_ass_mid_start_matches_late_cue() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        if !ffmpeg_has_libass_filters() {
            eprintln!("skipping: host ffmpeg lacks libass filters");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("movie.mp4");
        make_fixture_secs(&src, 15);
        let ass = dir.path().join("late.ass");
        fs::write(
            &ass,
            "[Script Info]\nScriptType: v4.00+\nPlayResX: 320\nPlayResY: 240\n\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: Default,Arial,24,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,2,10,10,20,1\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:10.00,0:00:12.00,Default,,0,0,0,,LATE CUE\n",
        )
        .unwrap();
        let selection = BurnInSelection {
            track_id: "s-en".into(),
            kind: BurnInKind::Ass,
            stream_index: None,
            subtitle_ordinal: None,
            sidecar_path: Some(ass.clone()),
        };
        let vf = ass_burn_vf(&selection, 10_000).unwrap();
        let plain = ffmpeg_rgb24_frame(&src, None, 10_000);
        let burned = ffmpeg_rgb24_frame(&src, Some(&vf), 10_000);
        let diff = rgb24_abs_diff(&plain, &burned);
        assert!(
            diff > 10_000,
            "mid-start ASS burn must show late cue; diff_sum={diff} vf={vf}"
        );
    }

    #[test]
    fn burn_in_pgs_session_produces_segments() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let corpus = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files/h264_aac_pgs_mkv.mkv");
        if !corpus.exists() {
            eprintln!("skipping: missing {}", corpus.display());
            return;
        }
        let streams = crate::list_burn_in_subtitles(&corpus).expect("list");
        let burn = streams
            .iter()
            .find(|s| s.kind == BurnInKind::Pgs)
            .expect("pgs track");
        let selection = BurnInSelection {
            track_id: burn.track_id(),
            kind: BurnInKind::Pgs,
            stream_index: Some(burn.stream_index),
            subtitle_ordinal: Some(burn.subtitle_ordinal),
            sidecar_path: None,
        };
        let dir = tempfile::tempdir().unwrap();
        let reg = HlsSessionRegistry::with_cap(dir.path().join("hls"), 2, "libx264").unwrap();
        let id = reg
            .start(
                1,
                &corpus,
                0,
                2000,
                SessionMode::Copy,
                stereo(),
                vec![],
                Some(selection),
            )
            .unwrap();
        wait_playlist(&reg, &id);
        let _ = wait_first_listed_asset(&reg, &id);
        reg.stop(&id);
    }

    /// Segment URIs in the media playlist are path-absolute under the session
    /// root so run-directory depth cannot break resolution (ADR-0008).
    #[test]
    fn media_playlist_segment_uris_are_session_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let run0 = dir.path().join("run_0");
        fs::create_dir_all(&run0).unwrap();
        let seg_rel = PathBuf::from("run_0/seg_disk.m4s");
        fs::write(dir.path().join(&seg_rel), [0u8; 64]).unwrap();
        let mut map = crate::hls_segment_map::SegmentMap::default();
        map.insert(crate::hls_segment_map::MappedSegment {
            start_ms: 21,
            duration_ms: 2002,
            run_id: 0,
            rel_path: seg_rel,
        });
        let session = Session {
            item_id: 33,
            src: PathBuf::from("/dev/null"),
            dir: dir.path().to_path_buf(),
            mode: SessionMode::Copy,
            audio: stereo(),
            burn_in: None,
            video_encoder: "copy".into(),
            start_ms: 0,
            play_start_ms: 0,
            landed_ms: 21,
            usable_extent_ms: None,
            duration_ms: 60_000,
            current_run_id: 0,
            next_run_id: 1,
            segment_map: map,
            current_run_eof: false,
            child: None,
            last_access: Instant::now(),
            last_restart: Instant::now(),
            primed: false,
            first_segment_ready: false,
            pending_play_ms: None,
            pending_since: None,
            stale_retain_refuse_until: None,
            failed: None,
            subtitle_tracks: vec![],
            segment_waiters: HashMap::new(),
            preempt_defer_logged: false,
        };
        let pl = build_run_media_playlist("s1", &session);
        let text = String::from_utf8_lossy(&pl);
        let uri = text
            .lines()
            .find(|l| l.contains("seg_"))
            .expect("listed segment URI");
        assert_eq!(
            uri, "/api/v0/sessions/s1/seg_00000000021.m4s",
            "must be path-absolute under the session"
        );
        assert!(
            text.contains("#EXT-X-MAP:URI=\"/api/v0/sessions/s1/runs/0/init.mp4\""),
            "MAP URI must be path-absolute, got {text}"
        );
    }

    /// Client-shaped link walk: master → media → MAP → first segment, each
    /// hop resolved the way a browser resolves relative HLS URIs, then served
    /// as real bytes (not string-only checks). Locks the relative-URI class
    /// that produced three cutover defects (segment depth, sub climb, and the
    /// mistaken "master points at session-root index" hypothesis — master is
    /// per-run, so bare `index.m3u8` is correct; this walk fails if that ever
    /// moves without updating the media URI).
    #[test]
    fn session_hls_link_walk_resolves_to_real_bytes() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture_secs(&src, 12);
        let reg = HlsSessionRegistry::with_cap(dir.path().join("hls"), 2, "libx264").unwrap();
        let id = reg
            .start(
                1,
                &src,
                0,
                12_000,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
            )
            .unwrap();
        wait_playlist(&reg, &id);
        let _ = wait_first_listed_asset(&reg, &id);
        let view = reg.view(&id).expect("view");
        walk_run_playlist_chain(&reg, &id, view.run_id, &view.playlist_url);

        std::thread::sleep(RESTART_MIN_INTERVAL);
        let after = reg.seek(&id, 4_000).expect("seek");
        assert_ne!(after.run_id, view.run_id, "seek must mint a fresh run");
        // Hold until the new run's media playlist lists a segment, then walk.
        let pl = wait_playlist_run(&reg, &id, after.run_id);
        let _ = wait_asset(&reg, &id, &first_listed_seg(&pl));
        walk_run_playlist_chain(&reg, &id, after.run_id, &after.playlist_url);
        reg.stop(&id);
    }

    /// Resolve `relative` against an absolute path URL (no scheme), the same
    /// way `urljoin` / browsers resolve HLS playlist references.
    fn resolve_hls_uri(base_url: &str, relative: &str) -> String {
        if relative.starts_with('/') {
            return relative.to_string();
        }
        let base_dir = base_url
            .rsplit_once('/')
            .map(|(d, _)| d)
            .unwrap_or(base_url);
        let mut parts: Vec<&str> = base_dir.split('/').filter(|p| !p.is_empty()).collect();
        for seg in relative.split('/') {
            match seg {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                other => parts.push(other),
            }
        }
        format!("/{}", parts.join("/"))
    }

    fn walk_run_playlist_chain(reg: &HlsSessionRegistry, id: &str, run_id: u64, master_url: &str) {
        assert!(
            master_url.ends_with(&format!("/runs/{run_id}/master.m3u8")),
            "playlistUrl must be per-run master, got {master_url}"
        );
        let master = reg.master(id, run_id).expect("master bytes");
        let master_text = String::from_utf8_lossy(&master);
        assert!(master_text.starts_with("#EXTM3U"), "{master_text}");
        let media_rel = master_text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .expect("master must list a media playlist URI");
        // Content types match the HTTP layer (sessions.rs): playlists →
        // application/vnd.apple.mpegurl; init → video/mp4; segments →
        // video/iso.segment. Asserted here as path→kind, bytes below.
        assert!(
            media_rel.ends_with(".m3u8") || media_rel == "index.m3u8",
            "media playlist URI must be m3u8, got {media_rel}"
        );
        let media_url = resolve_hls_uri(master_url, media_rel);
        assert_eq!(
            media_url,
            format!("/api/v0/sessions/{id}/runs/{run_id}/index.m3u8"),
            "master must emit path-absolute media playlist URI"
        );
        // Dead class: session-root index must not be what the master points at.
        assert_ne!(
            media_url,
            format!("/api/v0/sessions/{id}/index.m3u8"),
            "session-root index is not on the wire"
        );

        let media = reg.playlist(id, run_id).expect("media playlist");
        let media_text = String::from_utf8_lossy(&media);
        assert!(media_text.contains("#EXTINF:"), "{media_text}");
        let map_uri = media_text
            .lines()
            .find_map(|l| {
                l.trim()
                    .strip_prefix("#EXT-X-MAP:URI=\"")
                    .map(|rest| rest.trim_end_matches('"').to_string())
            })
            .expect("EXT-X-MAP");
        let init_url = resolve_hls_uri(&media_url, &map_uri);
        assert_eq!(
            init_url,
            format!("/api/v0/sessions/{id}/runs/{run_id}/init.mp4")
        );
        let init = reg
            .run_asset(id, run_id, "init.mp4")
            .expect("init.mp4 bytes");
        assert!(
            init.len() > 8 && &init[4..8] == b"ftyp",
            "init must be fMP4"
        );

        let seg_rel = media_text
            .lines()
            .map(str::trim)
            .find(|l| l.contains("seg_") && l.ends_with(".m4s"))
            .expect("first segment URI")
            .to_string();
        let seg_url = resolve_hls_uri(&media_url, &seg_rel);
        assert!(
            seg_url.starts_with(&format!("/api/v0/sessions/{id}/seg_")),
            "segment must resolve to session-root asset route, got {seg_url} from {seg_rel}"
        );
        let seg_name = seg_url.rsplit('/').next().expect("seg name");
        let seg = reg.asset(id, seg_name, None).expect("segment bytes");
        assert!(!seg.is_empty(), "first listed segment must have bytes");
    }

    /// Empty mid-title EOF must record usable extent (even with an empty map)
    /// so clients see damage instead of hanging on master 503 (DEF-8519 mask).
    #[test]
    fn empty_eof_records_usable_extent_zero() {
        let dir = tempfile::tempdir().unwrap();
        let run0 = dir.path().join("run_0");
        fs::create_dir_all(&run0).unwrap();
        let mut session = Session {
            item_id: 8519,
            src: PathBuf::from("/dev/null"),
            dir: dir.path().to_path_buf(),
            mode: SessionMode::Copy,
            audio: stereo(),
            burn_in: None,
            video_encoder: "copy".into(),
            start_ms: 1_014_000,
            play_start_ms: 1_014_000,
            landed_ms: 1_014_000,
            usable_extent_ms: None,
            duration_ms: 1_354_496,
            current_run_id: 0,
            next_run_id: 1,
            segment_map: crate::hls_segment_map::SegmentMap::default(),
            current_run_eof: false,
            child: None,
            last_access: Instant::now(),
            last_restart: Instant::now(),
            primed: false,
            first_segment_ready: false,
            pending_play_ms: None,
            pending_since: None,
            stale_retain_refuse_until: None,
            failed: None,
            subtitle_tracks: vec![],
            segment_waiters: HashMap::new(),
            preempt_defer_logged: false,
        };
        apply_run_eof(&mut session);
        assert!(session.current_run_eof);
        assert_eq!(
            session.usable_extent_ms,
            Some(0),
            "empty map + mid-title EOF → usableExtentMs=0"
        );
        let pl = build_run_media_playlist("s1", &session);
        let text = String::from_utf8_lossy(&pl);
        assert!(text.contains("#EXT-X-ENDLIST"), "empty ENDLIST for clients");
        assert!(
            !text.contains("seg_"),
            "no listed URIs when nothing is on disk"
        );
    }

    /// Eviction: orphans first; map-referenced run survives while orphans
    /// remain; after a referenced eviction the map no longer points at gone
    /// files; playlist never lists missing bytes.
    #[test]
    fn eviction_map_authoritative_orphans_first() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path();
        // run_0: referenced map bytes (large)
        let run0 = session_dir.join("run_0");
        fs::create_dir_all(&run0).unwrap();
        let seg_path = run0.join("seg.m4s");
        fs::write(&seg_path, vec![0u8; 50_000]).unwrap();
        // run_1: orphan (no map refs) with bytes
        let run1 = session_dir.join("run_1");
        fs::create_dir_all(&run1).unwrap();
        fs::write(run1.join("init.mp4"), vec![0u8; 10_000]).unwrap();
        // run_2: empty finished (noise)
        let run2 = session_dir.join("run_2");
        fs::create_dir_all(&run2).unwrap();
        // run_3: current (small)
        let run3 = session_dir.join("run_3");
        fs::create_dir_all(&run3).unwrap();
        fs::write(run3.join("init.mp4"), vec![0u8; 100]).unwrap();

        let mut map = crate::hls_segment_map::SegmentMap::default();
        map.insert(crate::hls_segment_map::MappedSegment {
            start_ms: 0,
            duration_ms: 2000,
            run_id: 0,
            rel_path: PathBuf::from("run_0/seg.m4s"),
        });

        let mut session = Session {
            item_id: 1,
            src: PathBuf::from("/dev/null"),
            dir: session_dir.to_path_buf(),
            mode: SessionMode::Transcode,
            audio: stereo(),
            burn_in: None,
            video_encoder: "libx264".into(),
            start_ms: 0,
            play_start_ms: 0,
            landed_ms: 0,
            usable_extent_ms: None,
            duration_ms: 60_000,
            current_run_id: 3,
            next_run_id: 4,
            segment_map: map,
            current_run_eof: true,
            child: None,
            last_access: Instant::now(),
            last_restart: Instant::now(),
            primed: true,
            first_segment_ready: true,
            pending_play_ms: None,
            pending_since: None,
            stale_retain_refuse_until: None,
            failed: None,
            subtitle_tracks: vec![],
            segment_waiters: HashMap::new(),
            preempt_defer_logged: false,
        };

        // Budget between orphan (10k) and total (~60k): one eviction of orphan.
        // SAFETY: single-threaded test; restore below.
        unsafe {
            std::env::set_var("NIGHTJAR_HLS_SESSION_CACHE_BYTES", "55000");
        }
        maybe_evict_finished_runs(&mut session);
        assert!(!run1.exists(), "orphan run_1 must be evicted first");
        assert!(
            run0.exists() && session.segment_map.run_is_referenced(0),
            "map-referenced run_0 must survive while orphans remain"
        );
        assert!(!run2.exists(), "empty run_2 reaped quietly");
        assert!(
            session
                .dir
                .join(&session.segment_map.get(0).unwrap().rel_path)
                .is_file(),
            "mapped file still on disk"
        );

        // Force referenced eviction: budget below run_0 size.
        unsafe {
            std::env::set_var("NIGHTJAR_HLS_SESSION_CACHE_BYTES", "1000");
        }
        maybe_evict_finished_runs(&mut session);
        assert!(!run0.exists(), "referenced run evicted under hard pressure");
        assert!(
            !session.segment_map.run_is_referenced(0),
            "map must drop entries before/with delete"
        );
        let pl = build_run_media_playlist("s1", &session);
        assert!(
            !String::from_utf8_lossy(&pl).contains("seg_"),
            "playlist must not list URIs whose files are gone"
        );
        unsafe {
            std::env::remove_var("NIGHTJAR_HLS_SESSION_CACHE_BYTES");
        }
    }
}
