//! HLS playback sessions (ADR-0007). A session either stream-copies the
//! source (remux) or re-encodes it (transcode); the two differ by
//! [`SessionMode`] and nothing else (ADR-0011).
//!
//! Fill-forward: FFmpeg starts [`ENCODE_LEAD_SEGMENTS`] before the play
//! land (`#EXT-X-START`) and encodes toward EOF. Cooked segments stay on
//! disk for the session lifetime, so scrub into finished media is a plain
//! file serve. Cold scrub restarts at the new offset (delay OK), then fills
//! forward again. Lead-in is 2 (not 0): Safari digs back ~1–2 segs near
//! land after a scrub; lead 0 left those misses 503 forever once dig-back
//! pending retreat was blocked (ADR-0011).

use super::audio::stereo_downmix_filter;
use super::subs::{
    SessionSubInput, prepare_session_subtitles, slice_webvtt, webvtt_max_cue_end_ms,
};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DEFAULT_MAX_SESSIONS: usize = 3;
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const REAPER_TICK: Duration = Duration::from_secs(5);
/// Locked HLS segment duration (ADR-0008). Future renditions must match this
/// value and the matching forced-keyframe interval.
const SEGMENT_MS: u64 = 2000;
/// How long a segment or init fetch may block before returning 503. Mid-title
/// hardware transcodes on a NAS library can exceed 15s (dogfood: ~16s to
/// seg1098 after a Chrome seek on Up 1080p).
const SEGMENT_WAIT: Duration = Duration::from_secs(30);
const SEGMENT_POLL: Duration = Duration::from_millis(100);
/// Safari's sequential prefetch reaches two segments beyond the current
/// on-disk frontier; farther requests are treated as a scrub.
const CATCH_UP_SEGMENTS: u64 = 2;
/// Safari retried refused segments at one-second intervals. A two-second floor
/// prevents adjacent prefetch misses from repeatedly moving the encode window.
const RESTART_MIN_INTERVAL: Duration = Duration::from_secs(2);
/// After the latest scrub intent while the prior encode has already landed,
/// wait this quiet period before killing FFmpeg. Rapid scrubs only update
/// the pending target (dogfood: three `seek restart` lines in ~9s; the last
/// fired 45ms after the previous `first_segment_ready`).
const RESTART_COALESCE_QUIET: Duration = Duration::from_millis(400);
/// After a seek restart, refuse retained segments behind the new play land
/// for this long. Serving them paints the prior scrub keyframe (dogfood:
/// seg455/1110/1762 200 after B applied). Forever-refuse wedges Safari on
/// A's URI; a short TTL covers cook+retarget then restores scrub-back.
const STALE_RETAIN_REFUSE: Duration = Duration::from_secs(15);
/// Maximum unprimed distance *behind the play land point* (`#EXT-X-START`)
/// that still counts as start alignment rather than attach prefetch of
/// `seg000`. Farther behind waits without yanking the encode to zero.
const ALIGN_BEHIND_SEGMENTS: u64 = 16;
/// Encode starts this many segments before the play land (`#EXT-X-START`).
/// Safari digs back ~1–2 segs near land after a currentTime scrub; with lead 0
/// those misses 503 forever once [`digback_behind_committed`] blocks pending
/// retreat. Post-land `#t=` reload (init refresh after seek-restart) digs
/// ~8 segs behind EXT-X-START when PRECISE=YES (dogfood: seg121 at land 258).
/// Lead must cover that dig-back or ranges stay empty after land-ensure 200.
/// Do not drop PRECISE to shrink dig-back — size the lead instead (ADR-0011).
const ENCODE_LEAD_SEGMENTS: u64 = 8;

/// Runtime lead-in. Default [`ENCODE_LEAD_SEGMENTS`]. Override with
/// `NIGHTJAR_ENCODE_LEAD_SEGMENTS` for local land-time / dig-back experiments
/// only — not a shipped config surface.
fn encode_lead_segments() -> u64 {
    std::env::var("NIGHTJAR_ENCODE_LEAD_SEGMENTS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(ENCODE_LEAD_SEGMENTS)
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
    fn needs_downmix(&self) -> bool {
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

/// Deliberate restart policy for cold regions of a full-title VOD playlist
/// (fill-forward: encoder runs toward EOF; cooked segs stay on disk).
///
/// - Behind the window: restart only when the miss is within
///   `ALIGN_BEHIND_SEGMENTS` of **play_start** (player settling near
///   `#EXT-X-START`). Farther behind waits without restart — attach
///   prefetch of `seg000`, and after a jump land Safari still probing the
///   *previous* region (retained segs + holes) must not yank the encode.
///   Real scrub-back is playlist `?startMs=` (ADR-0011 / ADR-0013). When
///   primed and near play_start, gate on `RESTART_MIN_INTERVAL`; unprimed
///   dig-back bypasses the interval so create's `last_restart` cannot
///   deadlock.
/// - Ahead: restart only past `CATCH_UP_SEGMENTS` of the cooking band end,
///   gated by `RESTART_MIN_INTERVAL`. Band end is
///   `max(frontier, play_start_idx)`. Frontier is the latest on-disk
///   segment at or after the window start; retained pre-window segments
///   must not count.
pub fn decide_segment_miss(
    idx: u64,
    window_start_idx: u64,
    play_start_idx: u64,
    latest_on_disk: Option<u64>,
    primed: bool,
    since_last_restart: Duration,
) -> SegmentMissAction {
    if idx < window_start_idx {
        let behind_play = play_start_idx.saturating_sub(idx);
        if behind_play > ALIGN_BEHIND_SEGMENTS {
            return SegmentMissAction::Wait;
        }
        if primed && since_last_restart < RESTART_MIN_INTERVAL {
            return SegmentMissAction::Wait;
        }
        // Near play land: follow (unprimed bypasses min-interval so create's
        // last_restart does not 503-deadlock dig-back past the lead-in).
        return SegmentMissAction::Restart;
    }
    if since_last_restart < RESTART_MIN_INTERVAL {
        return SegmentMissAction::Wait;
    }
    let frontier = latest_on_disk
        .filter(|&l| l >= window_start_idx)
        .unwrap_or(window_start_idx);
    // Lead-in encodes before play_start; requests near EXT-X-START are still
    // in the cooking band, not a scrub past the land point.
    let band_end = frontier.max(play_start_idx);
    if idx > band_end.saturating_add(CATCH_UP_SEGMENTS) {
        SegmentMissAction::Restart
    } else {
        SegmentMissAction::Wait
    }
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
/// "Far" means more than [`ALIGN_BEHIND_SEGMENTS`] from the cooking play land
/// — outside the near-land dig-back / settle band. Near retargets must still
/// wait for the cooking land (dogfood: seg415 after scrub to 1188). This is
/// stricter than [`pending_waiter_action`] Release alone: a one-segment
/// forward pending also Releases waiters but must not yank before land.
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
    latest_on_disk: Option<u64>,
    primed: bool,
) -> bool {
    let want = align_to_segment(want_ms);
    let idx = want / SEGMENT_MS;
    let window = window_start_ms / SEGMENT_MS;
    let play = play_start_ms / SEGMENT_MS;

    // Playlist scrub pending this exact land — about to cook.
    if pending_play_ms.is_some_and(|p| align_to_segment(p) == want) {
        return false;
    }

    // Behind encode window: lead-in / fill-forward will not write this index.
    // (Dig-back within ALIGN of a *new* play can still be behind that play's
    // window — that is abandoned, not in-window dig-back.)
    if idx < window {
        return true;
    }

    // In-window dig-back near committed land: lead-in may still write it.
    if digback_behind_committed(cooking_play_ms, pending_play_ms, want) {
        return false;
    }

    let cool = decide_segment_miss(
        idx,
        window,
        play,
        latest_on_disk,
        primed,
        RESTART_MIN_INTERVAL,
    );
    if cool == SegmentMissAction::Restart {
        return false;
    }

    // idx >= window and Wait: fill-forward will produce it.
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

/// Segment miss behind the committed land (cooking play and/or pending) by
/// at most [`ALIGN_BEHIND_SEGMENTS`] is Safari dig-back near `#EXT-X-START`,
/// not a scrub. Returns true when the miss must **not** call [`desire_restart`].
///
/// Real scrub-back is playlist `?startMs=` ([`decide_window_action`]). Behind
/// mirror of [`prefetch_advances_pending`].
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
    if want >= committed {
        return false;
    }
    (committed - want) / SEGMENT_MS <= ALIGN_BEHIND_SEGMENTS
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
        return PendingWaiterAction::Hold;
    }
    if pending < want && (want - pending) / SEGMENT_MS <= ALIGN_BEHIND_SEGMENTS {
        return PendingWaiterAction::Hold;
    }
    PendingWaiterAction::Release
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
    /// Actual encoder for this process. Future fallback updates this field.
    video_encoder: String,
    /// Encode window start. [`encode_start_ms`] of [`Session::play_start_ms`]
    /// (lead-in before `#EXT-X-START` when [`ENCODE_LEAD_SEGMENTS`] > 0).
    start_ms: u64,
    /// Client land point for `#EXT-X-START` / seek intent.
    play_start_ms: u64,
    duration_ms: u64,
    child: Option<Child>,
    last_access: Instant,
    /// Last encode-window restart (create counts as one) for the min-interval guard.
    last_restart: Instant,
    /// True after serving at least one segment at or past encode `start_ms`.
    primed: bool,
    /// Set once the window's first segment lands on disk (playlist-ready).
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
    /// audio does not: it starts a fresh session (ADR-0012).
    /// `subtitle_tracks` is snapshotted here and never revisited.
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
    ) -> Result<String, StartSessionError> {
        let play_start_ms = align_to_segment(start_ms);
        let start_ms = encode_start_ms(play_start_ms);
        let mut sessions = self
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
        let spawn_started = Instant::now();
        let child = spawn_ffmpeg(
            src,
            &dir,
            start_ms,
            mode,
            audio.clone(),
            &self.video_encoder,
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
            encode_lead_segments = encode_lead_segments(),
            mode = ?mode,
            audio_stream = ?audio.stream_index,
            audio_channels = audio.channels,
            encoder = %self.video_encoder,
            spawn_ms,
            "hls session started"
        );
        sessions.insert(
            id.clone(),
            Session {
                item_id,
                src: src.to_path_buf(),
                dir: dir.clone(),
                mode,
                audio,
                video_encoder: self.video_encoder.clone(),
                start_ms,
                play_start_ms,
                duration_ms,
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
        drop(sessions);
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

    /// Returns the VOD media playlist (`index.m3u8`). `start_ms` is seek
    /// intent: a divergent offset restarts this session in place.
    pub fn playlist(
        &self,
        session_id: &str,
        start_ms: Option<u64>,
    ) -> Result<Vec<u8>, PlaylistError> {
        self.with_ready_session(session_id, start_ms, |session| {
            let bytes = build_playlist(session.duration_ms, session.play_start_ms);
            log_playlist_serve(
                session_id,
                "index.m3u8",
                start_ms,
                session.play_start_ms,
                session.pending_play_ms,
                &bytes,
            );
            Ok(bytes)
        })
    }

    /// Returns the HLS master playlist (`master.m3u8`). Same seek semantics as
    /// [`playlist`]; media URI stays `index.m3u8` (ADR-0008 additive).
    pub fn master(
        &self,
        session_id: &str,
        start_ms: Option<u64>,
    ) -> Result<Vec<u8>, PlaylistError> {
        self.with_ready_session(session_id, start_ms, |session| {
            let bytes = build_master(&session.subtitle_tracks);
            log_playlist_serve(
                session_id,
                "master.m3u8",
                start_ms,
                session.play_start_ms,
                session.pending_play_ms,
                &bytes,
            );
            Ok(bytes)
        })
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
        let first = segment_name(session.start_ms / SEGMENT_MS);
        if !session.dir.join(&first).exists() {
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
        start_ms: Option<u64>,
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

        if let Some(ms) = start_ms {
            let aligned = align_to_segment(ms);
            let on_disk = session
                .dir
                .join(segment_name(aligned / SEGMENT_MS))
                .exists();
            match decide_window_action(aligned, session.play_start_ms, on_disk) {
                WindowAction::Serve => {}
                WindowAction::Restart => {
                    desire_restart(session, aligned);
                    maybe_apply_pending_restart(session)?;
                }
            }
        }

        if let Some(err) = note_child_exit(session) {
            return Err(PlaylistError::Failed(err));
        }

        let first = segment_name(session.start_ms / SEGMENT_MS);
        if !session.dir.join(&first).exists() {
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
        let (file_name, requested) = match segment_index(name) {
            Some(idx) => (segment_name(idx), Some(idx)),
            None => (name.to_string(), None),
        };
        // Register before the poll loop so a concurrent preempt sees this
        // waiter under the same mutex as stop_child (see restart_at).
        let _segment_waiter = requested.and_then(|idx| {
            SegmentWaiterGuard::attach(&self.sessions, session_id, idx * SEGMENT_MS)
        });
        let mut deadline = Instant::now() + SEGMENT_WAIT;
        // Once this request has claimed a land via desire_restart, a later
        // pending move to a different land ends the *fill* wait — enter
        // no-fill hold (not 503) so Safari never sees an app-level error.
        // (Immediate 204 on supersede was tried and rejected: wedged native
        // post-nudge on doubles — zero segment GETs after middle 204.)
        let mut holding_for_land = false;
        // No-fill hold: abandoned miss or superseded land waiter. Open until
        // IDLE_TIMEOUT / session teardown — no 503/404 while alive.
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
                // Notice cooking play-land even when this GET is for another
                // URI. Middle land-ensure may enter no-fill before a 200 on
                // that URI; final land-ensure must still flip
                // first_segment_ready so coalesced pending applies.
                note_first_segment_ready(session_id, session);
                if let Some(err) = session.failed.clone() {
                    return Err(PlaylistError::Failed(err));
                }
                if let Some(idx) = requested {
                    let want_ms = idx * SEGMENT_MS;
                    // Holding for this land and a newer scrub retargeted pending
                    // (or already applied a different land): same fate as
                    // abandoned — content will not arrive on this trajectory.
                    if holding_for_land {
                        let superseded = pending_waiter_action(session.pending_play_ms, want_ms)
                            == PendingWaiterAction::Release
                            || (session.pending_play_ms.is_none()
                                && align_to_segment(session.play_start_ms)
                                    != align_to_segment(want_ms));
                        if superseded {
                            // Far retarget: 503 immediately so WebKit drops the
                            // mid dig-back. No-fill hold here left native with
                            // land-ensure 200 and zero land GETs (desktop N=5).
                            // Near supersede still uses no-fill (lead dig-back).
                            let far = match session.pending_play_ms {
                                Some(p) => coalesce_preempt_before_land(want_ms, p),
                                None => {
                                    coalesce_preempt_before_land(want_ms, session.play_start_ms)
                                }
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
                }
                // Retained prior-window segments are served immediately —
                // unless pending apply moves the play land during this
                // request, or a short post-restart guard is still active for
                // segs behind the new play (dogfood: late A-land 200 flash /
                // wedge). After the TTL, scrub-back into cooked media works.
                if let Ok(bytes) = fs::read(session.dir.join(&file_name)) {
                    let play_before = session.play_start_ms;
                    if let Some(idx) = requested
                        && idx >= session.start_ms / SEGMENT_MS
                    {
                        session.primed = true;
                    }
                    note_first_segment_ready(session_id, session);
                    maybe_apply_pending_restart(session)?;
                    let want_ms = requested.map(|idx| idx * SEGMENT_MS);
                    if !serve_ok_after_pending_apply(play_before, session.play_start_ms, want_ms) {
                        return Err(PlaylistError::NotReady);
                    }
                    if let Some(idx) = requested {
                        let guard = match session.stale_retain_refuse_until {
                            Some(until) if Instant::now() < until => true,
                            Some(_) => {
                                session.stale_retain_refuse_until = None;
                                false
                            }
                            None => false,
                        };
                        if !serve_ok_retained_during_stale_guard(
                            idx * SEGMENT_MS,
                            session.play_start_ms,
                            guard,
                        ) {
                            return Err(PlaylistError::NotReady);
                        }
                    }
                    return Ok(bytes);
                }
                if let Some(err) = note_child_exit(session) {
                    return Err(PlaylistError::Failed(err));
                }
                // Debounced scrub intent may become due while we poll.
                maybe_apply_pending_restart(session)?;
                if file_name == "init.mp4" {
                    // Rewritten on restart; wait for the new init.
                } else if let Some(idx) = requested {
                    let window_start = session.start_ms / SEGMENT_MS;
                    let play_start = session.play_start_ms / SEGMENT_MS;
                    let latest = latest_segment_in_window(&session.dir, window_start);
                    let since = session.last_restart.elapsed();
                    let want_ms = idx * SEGMENT_MS;

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
                        // Stay in loop until file appears, session gone, or
                        // IDLE_TIMEOUT → AbandonedHoldEnded (204).
                    } else {
                        // Would this miss restart if the min-interval were cool?
                        // Used so Wait-due-to-interval still records pending.
                        let scrub_shaped = decide_segment_miss(
                            idx,
                            window_start,
                            play_start,
                            latest,
                            session.primed,
                            RESTART_MIN_INTERVAL,
                        ) == SegmentMissAction::Restart;
                        match decide_segment_miss(
                            idx,
                            window_start,
                            play_start,
                            latest,
                            session.primed,
                            since,
                        ) {
                            SegmentMissAction::Restart => {
                                if prefetch_advances_pending(session.pending_play_ms, want_ms) {
                                    // Keep the startMs (or prior) pending land;
                                    // do not yank forward to a prefetch seg.
                                    return Err(PlaylistError::NotReady);
                                }
                                if digback_behind_committed(
                                    session.play_start_ms,
                                    session.pending_play_ms,
                                    want_ms,
                                ) {
                                    // Near-ALIGN dig-back must not retreat a
                                    // committed / pending startMs land. Lead-in
                                    // may still produce this seg — long-poll
                                    // without desire_restart.
                                    if pending_waiter_action(session.pending_play_ms, want_ms)
                                        == PendingWaiterAction::Release
                                    {
                                        // Pending moved to a different scrub —
                                        // same no-fill hold as abandoned.
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
                                        // Dig-back with no pending: encoder will
                                        // not restart toward this want; lead-in
                                        // may still write it. Instant 503 so the
                                        // player retries — not a superseded
                                        // pending (leave as NotReady).
                                        return Err(PlaylistError::NotReady);
                                    }
                                } else {
                                    desire_restart(session, want_ms);
                                    holding_for_land = true;
                                    maybe_apply_pending_restart(session)?;
                                    // Pending matches this seg (or restart already
                                    // applied): long-poll like playlist wait until
                                    // the file lands.
                                    deadline = Instant::now() + SEGMENT_WAIT;
                                }
                            }
                            SegmentMissAction::Wait => {
                                // scrub_shaped: still record pending when live
                                // decision is Wait only because min-interval is
                                // hot — but never for dig-back behind committed.
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
                                } else if idx < window_start {
                                    // Far behind should be abandoned (no-fill).
                                    // Near dig-back before primed: 503 so the
                                    // player retries; lead-in will appear on disk.
                                    let behind_play = play_start.saturating_sub(idx);
                                    if behind_play > ALIGN_BEHIND_SEGMENTS || !session.primed {
                                        return Err(PlaylistError::NotReady);
                                    }
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
    if let Some(gap) = restart_spawn_gap() {
        tracing::info!(
            gap_ms = gap.as_millis(),
            play_start_ms,
            "hls restart spawn gap (NIGHTJAR_RESTART_SPAWN_GAP_MS)"
        );
        std::thread::sleep(gap);
    }
    // Gate 2 / fill-forward: do not wipe prior segments. In-flight fetches
    // and later scrub-back into cooked media must still hit disk. Only
    // remove the muxer sidecar; ffmpeg -y overwrites init and new indices.
    let index = session.dir.join("index.m3u8");
    let _ = fs::remove_file(&index);
    let child = spawn_ffmpeg(
        &session.src,
        &session.dir,
        start_ms,
        session.mode,
        session.audio.clone(),
        video_encoder,
    )
    .map_err(PlaylistError::Failed)?;
    session.child = Some(child);
    session.start_ms = start_ms;
    session.play_start_ms = play_start_ms;
    session.failed = None;
    session.last_restart = Instant::now();
    session.primed = false;
    session.first_segment_ready = false;
    session.stale_retain_refuse_until = Some(Instant::now() + STALE_RETAIN_REFUSE);
    if session.pending_play_ms == Some(play_start_ms) {
        session.pending_play_ms = None;
        session.pending_since = None;
    }
    tracing::info!(
        start_ms,
        play_start_ms,
        encoder = video_encoder,
        path = %session.src.display(),
        "hls session seek restart"
    );
    Ok(RestartAtOutcome::Applied)
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
    if session.first_segment_ready {
        return;
    }
    let land = segment_name(session.play_start_ms / SEGMENT_MS);
    if !session.dir.join(&land).exists() {
        return;
    }
    session.first_segment_ready = true;
    session.stale_retain_refuse_until = None;
    let elapsed_ms = session.last_restart.elapsed().as_millis();
    let lead_segments = session.play_start_ms.saturating_sub(session.start_ms) / SEGMENT_MS;
    tracing::info!(
        session_id,
        elapsed_ms,
        start_ms = session.start_ms,
        play_start_ms = session.play_start_ms,
        lead_segments,
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
            // Encoder reached the end of the file; segments stay servable.
            session.child = None;
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

fn align_to_segment(ms: u64) -> u64 {
    (ms / SEGMENT_MS) * SEGMENT_MS
}

/// Encode window start for a play land: [`encode_lead_segments`] before the
/// aligned play point (Safari dig-back; see module docs / ADR-0011).
fn encode_start_ms(play_ms: u64) -> u64 {
    align_to_segment(play_ms.saturating_sub(encode_lead_segments() * SEGMENT_MS))
}

fn segment_name(index: u64) -> String {
    format!("seg{index:03}.m4s")
}

/// Highest `segNNN.m4s` index at or after `window_start` in `dir`.
/// Retained segments from an earlier encode window must not count as the
/// live frontier: otherwise an in-window miss looks far-ahead of those
/// stale indices and restarts at the same offset forever.
fn latest_segment_in_window(dir: &Path, window_start: u64) -> Option<u64> {
    let mut best: Option<u64> = None;
    let Ok(entries) = fs::read_dir(dir) else {
        return None;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if let Some(idx) = segment_index(name)
            && idx >= window_start
        {
            best = Some(best.map_or(idx, |b| b.max(idx)));
        }
    }
    best
}

/// "segNNN.m4s" -> NNN; None for init.mp4.
fn segment_index(name: &str) -> Option<u64> {
    name.strip_prefix("seg")?.strip_suffix(".m4s")?.parse().ok()
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
        play_land_seg = play_start_ms / SEGMENT_MS,
        head = %head,
        "hls playlist serve"
    );
}

/// Full-title VOD media playlist (ADR-0011 amendment).
///
/// Lists every segment from `seg000` through the probed duration so the
/// scrubber is title-absolute. `start_ms` only sets `#EXT-X-START` for the
/// preferred land point; cold regions return 503 while a guarded restart
/// cooks them. The playlist does not omit pre-window indices.
fn build_playlist(duration_ms: u64, start_ms: u64) -> Vec<u8> {
    use std::fmt::Write;
    let full = duration_ms / SEGMENT_MS;
    let rem_ms = duration_ms % SEGMENT_MS;
    let segment_secs = SEGMENT_MS as f64 / 1000.0;
    // TARGETDURATION is an integer number of seconds (HLS); ceil so a
    // non-whole SEGMENT_MS still validates.
    let target = segment_secs.ceil() as u64;
    let mut out = format!(
        "#EXTM3U\n\
         #EXT-X-VERSION:7\n\
         #EXT-X-TARGETDURATION:{target}\n\
         #EXT-X-PLAYLIST-TYPE:VOD\n\
         #EXT-X-MEDIA-SEQUENCE:0\n\
         #EXT-X-MAP:URI=\"init.mp4\"\n"
    );
    if start_ms > 0 {
        // PRECISE=YES is the correct EXT-X-START contract. A temporary
        // removal (shallower dig-back under post-land `#t=`) was dropped once
        // ENCODE_LEAD_SEGMENTS covers the measured ~8-seg hole (ADR-0011).
        let _ = writeln!(
            out,
            "#EXT-X-START:TIME-OFFSET={:.3},PRECISE=YES",
            start_ms as f64 / 1000.0
        );
    }
    for i in 0..full {
        let _ = writeln!(out, "#EXTINF:{segment_secs:.6},\n{}", segment_name(i));
    }
    if rem_ms > 0 {
        let _ = writeln!(
            out,
            "#EXTINF:{:.6},\n{}",
            rem_ms as f64 / 1000.0,
            segment_name(full)
        );
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out.into_bytes()
}

/// Master playlist: one video variant + optional SUBTITLES group (ADR-0010).
/// Media URI stays relative `index.m3u8` (ADR-0008).
///
/// CODECS is omitted on purpose: a wrong value (we previously advertised
/// Main@L3.1 while VideoToolbox emits High@L4.0) makes Safari native HLS
/// refuse the variant outright. Better no hint than a lying one; the init
/// segment carries the real codec string.
fn build_master(tracks: &[HlsSubtitleTrack]) -> Vec<u8> {
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
                 FORCED={forced},URI=\"subs/{}.m3u8\"",
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
    out.push_str("index.m3u8\n");
    out.into_bytes()
}

/// Subtitle media playlist for one snapshotted track: always 2s VOD segments
/// aligned to the video timeline. Segment bodies come from the item store
/// VTT ([`HlsSubtitleTrack::item_vtt_path`]) or session-inline demux.
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
) -> Result<Child, String> {
    let start_secs = format!("{:.3}", start_ms as f64 / 1000.0);
    let start_number = (start_ms / SEGMENT_MS).to_string();
    let segment_secs = SEGMENT_MS as f64 / 1000.0;
    let force_kf = format!("expr:gte(t,n_forced*{segment_secs})");
    let hls_time = format!("{segment_secs}");
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
        // Keep output timestamps absolute so mid-title segments land at their
        // playlist position instead of restarting the clock at zero.
        cmd.args(["-output_ts_offset", &start_secs]);
    }
    let audio_map = match audio.stream_index {
        Some(index) => format!("0:{index}"),
        None => "0:a:0?".to_string(),
    };
    cmd.args(["-map", "0:v:0", "-map", &audio_map]);
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
            cmd.args([
                "-map_metadata",
                "-1",
                "-vf",
                "sidedata=delete,setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709",
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
        // ffmpeg's own playlist is never served; ours is generated from the
        // probed duration (VOD). This file just keeps the muxer happy.
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
    let Some(rest) = name.strip_prefix("seg") else {
        return false;
    };
    let Some(num) = rest.strip_suffix(".m4s") else {
        return false;
    };
    !num.is_empty() && num.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
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
        for _ in 0..100 {
            match reg.playlist(id, None) {
                Ok(bytes) => return bytes,
                Err(PlaylistError::NotReady) => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => panic!("playlist error: {e:?}"),
            }
        }
        panic!("playlist not ready in time");
    }

    fn wait_asset(reg: &HlsSessionRegistry, id: &str, name: &str) -> Vec<u8> {
        for _ in 0..150 {
            match reg.asset(id, name, None) {
                Ok(bytes) => return bytes,
                Err(PlaylistError::NotReady) => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => panic!("asset {name} error: {e:?}"),
            }
        }
        panic!("asset {name} not ready in time");
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
        // (name, idx, window_start_idx, play_start_idx, latest, primed, since, expected)
        let cases = [
            (
                "unprimed far behind waits (attach prefetch of seg000)",
                0,
                600,
                616,
                None,
                false,
                cool,
                SegmentMissAction::Wait,
            ),
            (
                "unprimed mid-title seg000 waits (play-relative, not window)",
                0,
                12,
                20,
                None,
                false,
                cool,
                SegmentMissAction::Wait,
            ),
            (
                "unprimed dig past lead-in follows play land",
                632,
                640,
                648,
                None,
                false,
                cool,
                SegmentMissAction::Restart,
            ),
            (
                "unprimed dig past lead-in bypasses create min-interval",
                632,
                640,
                648,
                None,
                false,
                hot,
                SegmentMissAction::Restart,
            ),
            (
                "unprimed behind at tolerance follows",
                632,
                640,
                648,
                None,
                false,
                cool,
                SegmentMissAction::Restart,
            ),
            (
                "unprimed behind past tolerance waits (seg000-class prefetch)",
                631,
                640,
                648,
                None,
                false,
                cool,
                SegmentMissAction::Wait,
            ),
            (
                "primed near behind play restarts (settle)",
                0,
                4,
                4,
                Some(10),
                true,
                cool,
                SegmentMissAction::Restart,
            ),
            (
                "primed near behind within min interval waits",
                0,
                4,
                4,
                Some(10),
                true,
                hot,
                SegmentMissAction::Wait,
            ),
            (
                "primed far behind play waits (stale post-jump hole)",
                490,
                729,
                729,
                Some(740),
                true,
                cool,
                SegmentMissAction::Wait,
            ),
            (
                "primed far behind at prior land waits (dogfood scrub-scrub)",
                491,
                729,
                729,
                Some(740),
                true,
                cool,
                SegmentMissAction::Wait,
            ),
            (
                "far ahead of play band restarts",
                20,
                4,
                4,
                Some(10),
                true,
                cool,
                SegmentMissAction::Restart,
            ),
            (
                "near frontier waits (cooking)",
                11,
                4,
                4,
                Some(10),
                true,
                cool,
                SegmentMissAction::Wait,
            ),
            (
                "far ahead within min interval waits",
                20,
                4,
                4,
                Some(10),
                true,
                hot,
                SegmentMissAction::Wait,
            ),
            (
                "retained pre-window latest does not fake far-ahead",
                1052,
                1052,
                1052,
                Some(7),
                false,
                cool,
                SegmentMissAction::Wait,
            ),
            (
                "in-window cooking waits when frontier is window start",
                1052,
                1052,
                1052,
                None,
                false,
                cool,
                SegmentMissAction::Wait,
            ),
            (
                "lead-in band near EXT-X-START waits (no thrash restart)",
                656,
                648,
                664,
                None,
                false,
                cool,
                SegmentMissAction::Wait,
            ),
            (
                "lead-in band at play land waits",
                664,
                648,
                664,
                Some(650),
                false,
                cool,
                SegmentMissAction::Wait,
            ),
            (
                "past play land by more than catch-up restarts",
                670,
                648,
                664,
                Some(650),
                false,
                cool,
                SegmentMissAction::Restart,
            ),
        ];
        for (name, idx, window, play, latest, primed, since, expected) in cases {
            assert_eq!(
                decide_segment_miss(idx, window, play, latest, primed, since),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn miss_restart_offset_lands_on_requested_segment() {
        // Near-land behind misses still Restart; encode aims lead before N.
        // Far-behind primed misses Wait now (stale post-jump) — scrub-back is startMs.
        let cases = [(1040u64, 1052u64), (0u64, 4u64), (1610u64, 1614u64)];
        for (idx, window) in cases {
            let action = decide_segment_miss(
                idx,
                window,
                window,
                Some(window),
                true,
                RESTART_MIN_INTERVAL,
            );
            assert_eq!(action, SegmentMissAction::Restart, "idx={idx}");
            let want_ms = idx * SEGMENT_MS;
            let new_window = encode_start_ms(want_ms) / SEGMENT_MS;
            assert_eq!(
                new_window,
                idx.saturating_sub(ENCODE_LEAD_SEGMENTS),
                "encode lead before land N (idx={idx})"
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

    /// Dogfood seg415 after scrub to 1188: yank before the cooking *play land*
    /// exists left Safari retrying the prior land URI forever. Ready must mean
    /// the play-land segment, not the encode-window first segment. Near pending
    /// must not preempt before that land exists.
    #[test]
    fn seg415_near_pending_must_not_preempt_before_cooking_land() {
        // Scrub to 1188s; encode lead puts first_window ENCODE_LEAD segs earlier.
        let cooking_play = 1_188_000u64;
        let encode = encode_start_ms(cooking_play);
        assert_eq!(
            encode,
            cooking_play - ENCODE_LEAD_SEGMENTS * SEGMENT_MS,
            "lead-in before land"
        );
        assert_ne!(
            encode / SEGMENT_MS,
            cooking_play / SEGMENT_MS,
            "first window index is not the play land (old ready bug)"
        );
        // Prior land Safari was still probing (seg415).
        let prior_land = 415 * SEGMENT_MS;
        assert_eq!(prior_land, 830_000);

        // Near retarget while cooking 1188 — inside ALIGN band, must not preempt.
        let near_fwd = cooking_play + SEGMENT_MS;
        let near_back = cooking_play - SEGMENT_MS;
        let edge = cooking_play + ALIGN_BEHIND_SEGMENTS * SEGMENT_MS;
        assert!(
            !coalesce_preempt_before_land(cooking_play, near_fwd),
            "near forward must not abandon cooking land"
        );
        assert!(
            !coalesce_preempt_before_land(cooking_play, near_back),
            "near behind must not abandon cooking land"
        );
        assert!(
            !coalesce_preempt_before_land(cooking_play, edge),
            "exactly ALIGN_BEHIND segs is still near"
        );
        assert_eq!(
            pending_restart_due(
                false,
                Some(near_fwd),
                None,
                false,
                cooking_play,
                RESTART_MIN_INTERVAL,
                true
            ),
            None,
            "near pending before land: no apply (seg415 protection)"
        );
        // Even with apply_immediate and a cool restart clock: still blocked.
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
            None
        );
        // Land ready → near pending may apply (debounce/immediate path).
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
        // Jump land: encode window at land − lead (2 segs).
        let window = land - ENCODE_LEAD_SEGMENTS * SEGMENT_MS;
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
            "ahead of attach play: must not 503 land-ensure"
        );
        // Near but still inside lead-in: may still be written — keep hold.
        let in_lead = land - SEGMENT_MS;
        assert!(
            in_lead >= window,
            "test setup: in_lead must be inside encode window"
        );
        assert!(
            !no_fill_release_for_new_land(in_lead, land, true, window),
            "near dig-back inside lead stays held"
        );
        // Single-scrub dogfood: dig-back one seg behind encode start (behind
        // window) while land is only a few segs ahead — must release.
        let single_land = 258_000u64;
        let single_window = single_land - ENCODE_LEAD_SEGMENTS * SEGMENT_MS;
        let behind_window = single_window - SEGMENT_MS;
        assert!(
            no_fill_release_for_new_land(behind_window, single_land, true, single_window),
            "behind encode window after land ready: release even if near play"
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
            PendingWaiterAction::Hold,
            "dig-back pending one seg behind land: keep holding"
        );
        assert_eq!(
            pending_waiter_action(Some(land), land + 2 * SEGMENT_MS),
            PendingWaiterAction::Hold,
            "dig-back pending two segs behind land: keep holding"
        );
        assert_eq!(
            pending_waiter_action(Some(land + 20_000), land),
            PendingWaiterAction::Release,
            "pending is a different scrub ahead"
        );
        let far_ahead_want = land + (ALIGN_BEHIND_SEGMENTS + 1) * SEGMENT_MS;
        assert_eq!(
            pending_waiter_action(Some(land), far_ahead_want),
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
        let land = segment_name(play_ms / SEGMENT_MS);
        fs::write(dir.path().join(&land), b"x").expect("land seg");
        let mut session = Session {
            item_id: 1,
            src: PathBuf::from("/dev/null"),
            dir: dir.path().to_path_buf(),
            mode: SessionMode::Transcode,
            audio: stereo(),
            video_encoder: "libx264".into(),
            start_ms: play_ms - ENCODE_LEAD_SEGMENTS * SEGMENT_MS,
            play_start_ms: play_ms,
            duration_ms: 3_600_000,
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
    fn digback_behind_committed_blocks_near_land_steal() {
        // Dogfood: cooking 1482000, Safari asks 1478000 (2 segs behind).
        let cooking = 1_482_000u64;
        let dig = 1_478_000u64;
        assert!(digback_behind_committed(cooking, None, dig));
        assert!(
            !digback_behind_committed(cooking, None, cooking),
            "same land is not dig-back"
        );
        assert!(
            !digback_behind_committed(cooking, None, cooking + SEGMENT_MS),
            "ahead is not dig-back"
        );
        // Far behind committed is not dig-back (far-behind Wait in decide_segment_miss).
        let far = cooking - (ALIGN_BEHIND_SEGMENTS + 1) * SEGMENT_MS;
        assert!(!digback_behind_committed(cooking, None, far));

        // Coalesce: cooking B, pending C — dig-back relative to C must not retreat.
        let cooking_b = 1_054_000u64;
        let pending_c = 1_482_000u64;
        assert!(digback_behind_committed(cooking_b, Some(pending_c), dig));
        assert!(
            !digback_behind_committed(cooking_b, Some(pending_c), pending_c),
            "want == committed pending"
        );
    }

    /// scrub_shaped must not retreat committed land (same gate as Restart arm).
    #[test]
    fn scrub_shaped_digback_must_not_desire() {
        let cooking = 1_482_000u64;
        let dig = 1_478_000u64;
        let idx = dig / SEGMENT_MS;
        let window = cooking / SEGMENT_MS;
        let play = window;
        // Near behind → Restart when cool (scrub_shaped true).
        assert_eq!(
            decide_segment_miss(idx, window, play, Some(window), true, RESTART_MIN_INTERVAL,),
            SegmentMissAction::Restart
        );
        assert!(
            digback_behind_committed(cooking, None, dig),
            "asset_wait must skip desire_restart for this miss"
        );
        // Hot min-interval → Wait, but scrub_shaped still true — same dig-back gate.
        assert_eq!(
            decide_segment_miss(
                idx,
                window,
                play,
                Some(window),
                true,
                Duration::from_millis(0)
            ),
            SegmentMissAction::Wait
        );
        assert!(digback_behind_committed(cooking, None, dig));
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
        let far = cooking - (ALIGN_BEHIND_SEGMENTS + 1) * SEGMENT_MS;
        let dig = cooking - 2 * SEGMENT_MS;
        let in_window = window_ms;
        let ahead = cooking + (CATCH_UP_SEGMENTS + 2) * SEGMENT_MS;

        let cases: &[(&str, u64, Option<u64>, bool, bool)] = &[
            ("far behind no pending", far, None, true, true),
            ("prior land after jump", prior, None, true, true),
            ("attach-shaped seg000", 0, None, true, true),
            ("near dig-back", dig, None, true, false),
            ("pending exact land", prior, Some(prior), true, false),
            ("in-window fill-forward", in_window, None, true, false),
            ("cool restart ahead", ahead, None, true, false),
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

    /// Dogfood: coalesce used to 503 the land seg immediately and rely on
    /// Safari retry. Hold until ready on the same connection instead.
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
            .start(1, &src, 0, 60_000, SessionMode::Transcode, stereo(), vec![])
            .unwrap();
        wait_playlist(&reg, &id);
        let _ = wait_asset(&reg, &id, "seg000.m4s");
        std::thread::sleep(RESTART_MIN_INTERVAL);

        let t0 = Instant::now();
        // Far past the few seconds of fill-forward from start — forces restart + hold.
        let bytes = reg
            .asset(&id, "seg020.m4s", None)
            .expect("land seg must 200 on the holding request");
        assert!(!bytes.is_empty());
        assert!(
            t0.elapsed() < SEGMENT_WAIT,
            "should finish within SEGMENT_WAIT"
        );
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
            .start(1, &src, 0, 60_000, SessionMode::Transcode, stereo(), vec![])
            .unwrap();
        wait_playlist(&reg, &id);
        let _ = wait_asset(&reg, &id, "seg000.m4s");
        std::thread::sleep(RESTART_MIN_INTERVAL);

        let (tx, rx) = std::sync::mpsc::channel();
        let reg_hold = Arc::clone(&reg);
        let id_hold = id.clone();
        std::thread::spawn(move || {
            let t0 = Instant::now();
            let result = reg_hold.asset(&id_hold, "seg020.m4s", None);
            let _ = tx.send((result, t0.elapsed()));
        });
        // Retarget far ahead so supersede is outside ALIGN dig-back of land.
        let probe_until = Instant::now() + Duration::from_secs(15);
        while Instant::now() < probe_until {
            let _ = reg.playlist(&id, Some(200_000));
            match rx.try_recv() {
                Ok((first, elapsed)) => {
                    assert!(
                        matches!(first, Err(PlaylistError::NotReady)) || matches!(first, Ok(_)),
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
        panic!("hold did not finish within 15s after supersede");
    }

    /// Double scrub: final land-ensure alone must notice cooking land on disk
    /// and apply pending (no middle GET).
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
        // Start already cooking land A (20s). Do not GET A's land URI.
        let id = reg
            .start(
                1,
                &src,
                20_000,
                120_000,
                SessionMode::Transcode,
                stereo(),
                vec![],
            )
            .unwrap();
        {
            let sessions = reg.sessions.lock().unwrap();
            let s = sessions.get(&id).unwrap();
            assert_eq!(s.play_start_ms, 20_000);
            assert!(!s.first_segment_ready);
        }

        // Final land-ensure for B (40s, near — land gate, no preempt).
        // Wait loop must notice A's land file and apply pending.
        let reg_b = Arc::clone(&reg);
        let id_b = id.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let t0 = Instant::now();
            let result = reg_b.asset(&id_b, "seg020.m4s", Some("land-ensure"));
            let _ = tx.send((result, t0.elapsed()));
        });

        let (result, elapsed) = rx
            .recv_timeout(Duration::from_secs(90))
            .expect("final land-ensure must complete");
        assert!(
            matches!(result, Ok(ref b) if !b.is_empty()),
            "final land must 200 after pending apply, got {:?} in {elapsed:?}",
            result
                .as_ref()
                .map(|b| b.len())
                .map_err(|e| format!("{e:?}"))
        );
        let sessions = reg.sessions.lock().unwrap();
        let s = sessions.get(&id).unwrap();
        assert_eq!(
            s.play_start_ms, 40_000,
            "pending B must have applied without a middle-land GET"
        );
    }

    #[test]
    fn encode_start_includes_lead_before_play() {
        assert_eq!(ENCODE_LEAD_SEGMENTS, 8);
        assert_eq!(encode_start_ms(1_264_000), 1_248_000);
        assert_eq!(encode_start_ms(1_000), 0);
        assert_eq!(encode_start_ms(0), 0);
        assert_eq!(encode_start_ms(4_000), 0);
        assert_eq!(encode_start_ms(16_000), 0);
        assert_eq!(encode_start_ms(18_000), 2_000);
    }

    /// Mid-title switch: encode starts lead before land. seg000 must 503
    /// without yanking to zero. Near dig-back must not retreat play land
    /// (lead covers Safari's 1–2 seg dig-back; farther is startMs).
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
        let play_ms = 40_000; // seg020; encode-at-land
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
            )
            .unwrap();
        let playlist = wait_playlist(&reg, &switched);
        let text = String::from_utf8_lossy(&playlist);
        assert!(
            text.contains("#EXT-X-START:TIME-OFFSET=40.000,PRECISE=YES"),
            "switch land stays at the requested offset: {text}"
        );

        match reg.asset(&switched, "seg000.m4s", None) {
            Err(PlaylistError::NotReady) => {}
            other => {
                eprintln!("seg000 after switch playlist: {other:?}");
            }
        }
        assert_eq!(
            decide_segment_miss(0, 20, 20, None, false, RESTART_MIN_INTERVAL),
            SegmentMissAction::Wait,
            "seg000 at a 40s switch must not count as start alignment"
        );

        // Behind encode window (lead=8 at play 40s → window from seg12): must
        // not restart / cook a retreated land. Real scrub-back is ?startMs=.
        let dig_ms = 5 * SEGMENT_MS;
        assert!(digback_behind_committed(play_ms, None, dig_ms));
        match reg.asset(&switched, "seg005.m4s", None) {
            Err(PlaylistError::NotReady) => {}
            Ok(_) => panic!("dig-back must not cook a retreated window"),
            Err(e) => panic!("unexpected dig-back error: {e:?}"),
        }
        let land = wait_asset(&reg, &switched, "seg020.m4s");
        assert!(!land.is_empty(), "switch land segment must serve");

        assert!(reg.stop(&prior));
        assert!(reg.stop(&switched));
    }

    /// Fresh mid-title session: encode-at-land cooks seg020 first.
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
        let play_ms = 40_000; // seg020; encode-at-land
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
            )
            .unwrap();
        let playlist = wait_playlist(&reg, &id);
        let text = String::from_utf8_lossy(&playlist);
        assert!(
            text.contains("#EXT-X-START:TIME-OFFSET=40.000,PRECISE=YES"),
            "play land is the encode start: {text}"
        );
        let land = wait_asset(&reg, &id, "seg020.m4s");
        assert!(!land.is_empty(), "land segment must be served");
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
        assert!(text.contains("#EXT-X-PLAYLIST-TYPE:VOD"), "{text}");
        assert!(text.contains("#EXT-X-ENDLIST"), "{text}");
        assert!(reg.asset(&id, "seg000.m4s", None).is_ok());
        assert!(reg.stop(&id));
        assert!(matches!(
            reg.playlist(&id, None),
            Err(PlaylistError::NotFound)
        ));
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
                vec![]
            ),
            Err(StartSessionError::CapFull)
        ));
        assert!(reg.stop(&a));
        assert!(matches!(
            reg.playlist(&a, None),
            Err(PlaylistError::NotFound)
        ));
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
            .start(1, &src, 0, FIXTURE_MS, SessionMode::Copy, stereo(), vec![])
            .unwrap();
        assert_eq!(
            reg.encoder(&id),
            Some(SessionEncoder {
                name: "copy".into(),
                kind: EncoderKind::Copy,
            })
        );
        wait_playlist(&reg, &id);
        assert!(reg.asset(&id, "seg000.m4s", None).is_ok());
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
            )
            .unwrap();
        wait_playlist(&reg, &id);
        let early = reg.asset(&id, "seg000.m4s", None).expect("early segment");
        // Move the window forward; prior segment must still be readable.
        for _ in 0..100 {
            match reg.playlist(&id, Some(2000)) {
                Ok(_) => break,
                Err(PlaylistError::NotReady) => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => panic!("seek: {e:?}"),
            }
        }
        let still = reg
            .asset(&id, "seg000.m4s", None)
            .expect("retained segment must not 404 after seek");
        assert_eq!(early.len(), still.len());
        assert!(reg.asset(&id, "seg001.m4s", None).is_ok());
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
        let mut child = spawn_ffmpeg(src, &enc, 0, mode, audio, encoder).unwrap();
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
    fn vod_playlist_covers_full_duration() {
        let text = String::from_utf8(build_playlist(5000, 0)).unwrap();
        assert!(text.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(text.contains("#EXT-X-MEDIA-SEQUENCE:0"));
        assert!(text.ends_with("#EXT-X-ENDLIST\n"));
        assert_eq!(text.matches(".m4s").count(), 3, "{text}");
    }

    #[test]
    fn mid_window_playlist_is_full_title_with_start_tag() {
        let text = String::from_utf8(build_playlist(10_000, 4_000)).unwrap();
        assert!(text.contains("#EXT-X-MEDIA-SEQUENCE:0"), "{text}");
        assert!(
            text.contains("#EXT-X-START:TIME-OFFSET=4.000,PRECISE=YES"),
            "{text}"
        );
        assert!(text.contains("seg000.m4s"), "{text}");
        assert!(text.contains("seg002.m4s"), "{text}");
        assert!(!text.contains("EXT-X-GAP"), "{text}");
        assert_eq!(text.matches(".m4s").count(), 5, "{text}");
    }

    #[test]
    fn zero_start_playlist_has_no_start_tag() {
        let text = String::from_utf8(build_playlist(5000, 0)).unwrap();
        assert!(!text.contains("EXT-X-START"), "{text}");
        assert!(text.contains("#EXT-X-MEDIA-SEQUENCE:0"), "{text}");
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
        let text = String::from_utf8(build_master(&tracks)).unwrap();
        assert!(text.contains("#EXT-X-MEDIA:TYPE=SUBTITLES"));
        assert!(text.contains("GROUP-ID=\"subs\""));
        assert!(text.contains("URI=\"subs/e2.m3u8\""));
        assert!(text.contains("SUBTITLES=\"subs\""));
        assert!(text.contains("\nindex.m3u8\n"));
        assert!(
            text.contains("CHARACTERISTICS=\"public.accessibility.transcribes-spoken-dialog\"")
        );
        assert!(!text.contains("CODECS="), "{text}");
        assert!(!text.contains("media.m3u8"));
    }

    #[test]
    fn master_without_tracks_has_no_subtitles_attr() {
        let text = String::from_utf8(build_master(&[])).unwrap();
        assert!(!text.contains("EXT-X-MEDIA"));
        assert!(!text.contains("SUBTITLES="));
        assert!(!text.contains("CODECS="), "{text}");
        assert!(text.contains("\nindex.m3u8\n"));
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
    fn subtitle_media_playlist_matches_video_segment_count() {
        for duration_ms in [4000u64, 5000, 90_000] {
            let video = String::from_utf8(build_playlist(duration_ms, 0)).unwrap();
            let subs =
                String::from_utf8(build_segmented_subtitle_playlist("e2", duration_ms)).unwrap();
            assert_eq!(
                video.matches(".m4s").count(),
                subs.matches(".vtt").count(),
                "duration_ms={duration_ms}"
            );
        }
    }

    #[test]
    fn asset_name_allowlist() {
        assert!(is_safe_asset("init.mp4"));
        assert!(is_safe_asset("seg000.m4s"));
        assert!(!is_safe_asset("../etc/passwd"));
        assert_eq!(segment_index("seg042.m4s"), Some(42));
        assert_eq!(segment_index("init.mp4"), None);
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
            let mut child =
                spawn_ffmpeg(src, &enc, 0, SessionMode::Transcode, stereo(), "libx264").unwrap();
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
}
