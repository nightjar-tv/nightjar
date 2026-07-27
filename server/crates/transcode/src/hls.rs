//! HLS playback sessions (ADR-0007). A session either stream-copies the
//! source (remux) or re-encodes it (transcode); the two differ by
//! [`SessionMode`] and nothing else (ADR-0011).

use super::audio::stereo_downmix_filter;
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
/// Maximum unprimed distance *behind the play land point* (`#EXT-X-START`)
/// that still counts as start alignment rather than attach prefetch of
/// `seg000`. Measured from `play_start`, not the encode window: lead-in puts
/// the window eight segments earlier, and window-relative math treated
/// `seg000` at a ~40 s switch (12 behind the window, 20 behind play) as
/// alignment and yanked the encode back to zero.
const ALIGN_BEHIND_SEGMENTS: u64 = 16;
/// Safari requested eight segments before EXT-X-START when switching tracks.
/// Encoding exactly that span early serves its first request without making
/// each seek transcode the former 16-segment margin before first frame.
const ENCODE_LEAD_SEGMENTS: u64 = 8;

#[derive(Debug)]
pub enum StartSessionError {
    CapFull,
    Spawn(String),
}

#[derive(Debug)]
pub enum PlaylistError {
    NotFound,
    NotReady,
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

/// Deliberate restart policy for cold regions of a full-title VOD playlist.
///
/// - Behind the window: once `primed`, always restart (real scrub back),
///   gated by `RESTART_MIN_INTERVAL`. Before that, follow only when the
///   miss is within `ALIGN_BEHIND_SEGMENTS` of **play_start** (the player
///   settling near `#EXT-X-START`) — and do it even inside the min
///   interval, otherwise a fresh mid-title session deadlocks for two
///   seconds on dig-back past the encode lead-in. Farther behind (attach
///   prefetch of `seg000`) waits without restart so the encode is not
///   yanked to zero.
/// - Ahead: restart only past `CATCH_UP_SEGMENTS` of the cooking band end,
///   gated by `RESTART_MIN_INTERVAL`. The band end is
///   `max(frontier, play_start_idx)` so encode lead-in (window starts
///   before `#EXT-X-START`) does not treat land-point prefetch as a
///   forward scrub. Frontier is the latest on-disk segment **at or after**
///   the window start; retained pre-window segments must not count.
pub fn decide_segment_miss(
    idx: u64,
    window_start_idx: u64,
    play_start_idx: u64,
    latest_on_disk: Option<u64>,
    primed: bool,
    since_last_restart: Duration,
) -> SegmentMissAction {
    if idx < window_start_idx {
        if primed {
            return if since_last_restart < RESTART_MIN_INTERVAL {
                SegmentMissAction::Wait
            } else {
                SegmentMissAction::Restart
            };
        }
        let behind_play = play_start_idx.saturating_sub(idx);
        return if behind_play <= ALIGN_BEHIND_SEGMENTS {
            // Start alignment: bypass min-interval so create's last_restart
            // does not 503-deadlock dig-back past the lead-in.
            SegmentMissAction::Restart
        } else {
            SegmentMissAction::Wait
        };
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

pub struct HlsSessionRegistry {
    root: PathBuf,
    max_sessions: usize,
    /// Verified H.264 encoder name from ADR-0009 probe (`libx264` fallback).
    video_encoder: String,
    next_id: AtomicU64,
    sessions: Mutex<HashMap<String, Session>>,
}

/// Serveable text track snapshot taken at session create (ADR-0010).
/// Mid-session sidecar additions do not appear until the next session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsSubtitleTrack {
    pub track_id: String,
    pub language: Option<String>,
    pub name: String,
    pub is_default: bool,
    pub forced: bool,
    pub sdh: bool,
}

struct Session {
    item_id: i64,
    src: PathBuf,
    dir: PathBuf,
    mode: SessionMode,
    audio: AudioSelection,
    /// Actual encoder for this process. Future fallback updates this field.
    video_encoder: String,
    /// Encode window start (may lead `play_start_ms` so a client that
    /// digs a few segments behind EXT-X-START still lands in-window).
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
    failed: Option<String>,
    /// Tracks declared in the master, snapshotted at create.
    subtitle_tracks: Vec<HlsSubtitleTrack>,
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
        let spawn_ms = spawn_started.elapsed().as_millis();
        tracing::info!(
            session_id = %id,
            item_id,
            start_ms,
            play_start_ms,
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
                dir,
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
                failed: None,
                subtitle_tracks,
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

    /// Returns the VOD media playlist (`index.m3u8`). `start_ms` is seek
    /// intent: a divergent offset restarts this session in place.
    pub fn playlist(
        &self,
        session_id: &str,
        start_ms: Option<u64>,
    ) -> Result<Vec<u8>, PlaylistError> {
        self.with_ready_session(session_id, start_ms, |session| {
            Ok(build_playlist(session.duration_ms, session.play_start_ms))
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
            Ok(build_master(&session.subtitle_tracks))
        })
    }

    /// One-segment subtitle media playlist for a snapshotted track.
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
        Ok(build_subtitle_playlist(
            session.item_id,
            track_id,
            session.duration_ms,
        ))
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
                    let encoder = session.video_encoder.clone();
                    restart_at(session, aligned, &encoder)?;
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
        build(session)
    }

    /// Serves init/segment files. Retained segments from a previous encode
    /// window stay readable. Missing segments in a cold region of the
    /// full-title VOD return 503 while a guarded restart cooks them
    /// (ADR-0011 amendment). Safari native scrub often hits this path only.
    pub fn asset(&self, session_id: &str, name: &str) -> Result<Vec<u8>, PlaylistError> {
        if !is_safe_asset(name) {
            return Err(PlaylistError::NotFound);
        }
        let (file_name, requested) = match segment_index(name) {
            Some(idx) => (segment_name(idx), Some(idx)),
            None => (name.to_string(), None),
        };
        let mut deadline = Instant::now() + SEGMENT_WAIT;
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
                // Retained prior-window segments are served immediately.
                if let Ok(bytes) = fs::read(session.dir.join(&file_name)) {
                    if let Some(idx) = requested
                        && idx >= session.start_ms / SEGMENT_MS
                    {
                        session.primed = true;
                    }
                    note_first_segment_ready(session_id, session);
                    return Ok(bytes);
                }
                if let Some(err) = note_child_exit(session) {
                    return Err(PlaylistError::Failed(err));
                }
                if file_name == "init.mp4" {
                    // Rewritten on restart; wait for the new init.
                } else if let Some(idx) = requested {
                    let window_start = session.start_ms / SEGMENT_MS;
                    let play_start = session.play_start_ms / SEGMENT_MS;
                    let latest = latest_segment_in_window(&session.dir, window_start);
                    let since = session.last_restart.elapsed();
                    match decide_segment_miss(
                        idx,
                        window_start,
                        play_start,
                        latest,
                        session.primed,
                        since,
                    ) {
                        SegmentMissAction::Restart => {
                            let want_ms = idx * SEGMENT_MS;
                            // Same encode window is a no-op that only kills
                            // the cooking encoder (the retained-frontier bug).
                            if encode_start_ms(want_ms) != session.start_ms {
                                let encoder = session.video_encoder.clone();
                                restart_at(session, want_ms, &encoder)?;
                                deadline = Instant::now() + SEGMENT_WAIT;
                            }
                        }
                        SegmentMissAction::Wait => {
                            if session.child.is_none() {
                                return Err(PlaylistError::NotFound);
                            }
                            // Attach prefetch of pre-window indices must not
                            // burn SEGMENT_WAIT; 503 immediately so the client
                            // can fetch the EXT-X-START region instead.
                            if idx < window_start && !session.primed {
                                return Err(PlaylistError::NotReady);
                            }
                        }
                    }
                }
            }
            if Instant::now() >= deadline {
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

fn restart_at(
    session: &mut Session,
    play_ms: u64,
    video_encoder: &str,
) -> Result<(), PlaylistError> {
    let play_start_ms = align_to_segment(play_ms);
    let start_ms = encode_start_ms(play_start_ms);
    stop_child(&mut session.child);
    // Gate 2: do not wipe the prior encode window. In-flight segment fetches
    // (seg1127…) must still hit disk while the new window is cooking. Only
    // remove the muxer sidecar; ffmpeg -y overwrites init and new seg indices.
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
    tracing::info!(
        start_ms,
        play_start_ms,
        encoder = video_encoder,
        path = %session.src.display(),
        "hls session seek restart"
    );
    Ok(())
}

/// Logs once when the encode window's first segment appears. This is the
/// playlist-ready moment — distinct from client-observed HTTP latency, and
/// the number that decides whether a timeout bump is papering over encode
/// startup rather than fixing it.
fn note_first_segment_ready(session_id: &str, session: &mut Session) {
    if session.first_segment_ready {
        return;
    }
    let first = segment_name(session.start_ms / SEGMENT_MS);
    if !session.dir.join(&first).exists() {
        return;
    }
    session.first_segment_ready = true;
    let elapsed_ms = session.last_restart.elapsed().as_millis();
    let lead_segments = session
        .play_start_ms
        .saturating_sub(session.start_ms)
        / SEGMENT_MS;
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

/// Encode a little earlier than the play land point so a client that
/// requests a few segments behind `#EXT-X-START` (Safari near-start
/// alignment) hits the cooking window instead of a behind-window miss.
fn encode_start_ms(play_ms: u64) -> u64 {
    align_to_segment(play_ms.saturating_sub(ENCODE_LEAD_SEGMENTS * SEGMENT_MS))
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

/// One-segment VOD subtitle playlist pointing at the item VTT URL.
fn build_subtitle_playlist(item_id: i64, track_id: &str, duration_ms: u64) -> Vec<u8> {
    use std::fmt::Write;
    let secs = (duration_ms as f64 / 1000.0).max(0.001);
    let target = secs.ceil() as u64;
    let mut out = format!(
        "#EXTM3U\n\
         #EXT-X-VERSION:6\n\
         #EXT-X-TARGETDURATION:{target}\n\
         #EXT-X-PLAYLIST-TYPE:VOD\n\
         #EXT-X-MEDIA-SEQUENCE:0\n"
    );
    let _ = writeln!(
        out,
        "#EXTINF:{secs:.6},\n/api/v0/items/{item_id}/subtitles/{track_id}.vtt"
    );
    out.push_str("#EXT-X-ENDLIST\n");
    out.into_bytes()
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
            match reg.asset(id, name) {
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
                "primed behind restarts (scrub back)",
                0,
                4,
                4,
                Some(10),
                true,
                cool,
                SegmentMissAction::Restart,
            ),
            (
                "primed behind within min interval waits",
                0,
                4,
                4,
                Some(10),
                true,
                hot,
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
        // Invariant: a Restart for segment N aims play at N (encode may
        // lead by ENCODE_LEAD_SEGMENTS), so the new window contains N.
        let cases = [(13u64, 1052u64), (0, 100), (1614, 1052)];
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
            assert!(
                new_window <= idx,
                "window after restart must contain segment N (idx={idx}, window={new_window})"
            );
            assert!(
                idx - new_window <= ENCODE_LEAD_SEGMENTS,
                "lead-in must stay within ENCODE_LEAD_SEGMENTS"
            );
        }
    }

    #[test]
    fn encode_lead_in_covers_measured_safari_dig_back() {
        assert_eq!(
            encode_start_ms(1_264_000),
            1_264_000 - ENCODE_LEAD_SEGMENTS * SEGMENT_MS
        );
        assert_eq!(encode_start_ms(1_000), 0);
        assert_eq!(encode_start_ms(0), 0);
    }

    /// Switch session (fresh POST at mid-title with a new audio config): the
    /// first real request is Safari's EXT-X-START dig-back, not seg000.
    /// Encode lead-in must serve it; seg000 must 503 without yanking the
    /// window back to zero (play-relative ALIGN, ADR-0011).
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
        let play_ms = 40_000; // seg020; encode lead-in → seg012
        let reg = HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264").unwrap();

        // Prior session still holding a cap slot (s9→s10 dogfood pattern).
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

        // Capture: attach prefetch of seg000 must not move the window.
        // Immediate NotReady while FFmpeg is alive; never a silent yank to 0
        // (which would make the next dig-back miss and look like a 503 storm).
        match reg.asset(&switched, "seg000.m4s") {
            Err(PlaylistError::NotReady) => {}
            other => {
                // Tiny fixtures may finish the encode before this assert; the
                // window must still be the mid-title lead-in either way.
                eprintln!("seg000 after switch playlist: {other:?}");
            }
        }
        assert_eq!(
            decide_segment_miss(0, 12, 20, None, false, RESTART_MIN_INTERVAL),
            SegmentMissAction::Wait,
            "seg000 at a 40s switch must not count as start alignment"
        );

        // First real request: dig-back inside lead-in (same as new_session).
        let bytes = wait_asset(&reg, &switched, "seg013.m4s");
        assert!(
            !bytes.is_empty(),
            "switch session first dig-back segment must be served without 503-forever"
        );
        let land = wait_asset(&reg, &switched, "seg020.m4s");
        assert!(!land.is_empty(), "switch land segment must serve");

        assert!(reg.stop(&prior));
        assert!(reg.stop(&switched));
    }

    /// Fresh mid-title session: first segment request behind the play land
    /// point (Safari EXT-X-START dig-back) must still be served. Encode
    /// lead-in keeps it in-window; decide_segment_miss tolerance is the
    /// backup when the dig exceeds the lead-in.
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
        let play_ms = 40_000; // seg020; encode lead-in → seg012
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
            "play land stays at the requested offset, not the encode lead-in: {text}"
        );
        // Seven segments behind play (~Safari dig); still inside lead-in.
        let bytes = wait_asset(&reg, &id, "seg013.m4s");
        assert!(!bytes.is_empty(), "first requested segment must be served");
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
        assert!(reg.asset(&id, "seg000.m4s").is_ok());
        assert!(reg.stop(&id));
        assert!(matches!(
            reg.playlist(&id, None),
            Err(PlaylistError::NotFound)
        ));
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
        assert!(reg.asset(&id, "seg000.m4s").is_ok());
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
        let early = reg.asset(&id, "seg000.m4s").expect("early segment");
        // Move the window forward; prior segment must still be readable.
        for _ in 0..100 {
            match reg.playlist(&id, Some(2000)) {
                Ok(_) => break,
                Err(PlaylistError::NotReady) => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => panic!("seek: {e:?}"),
            }
        }
        let still = reg
            .asset(&id, "seg000.m4s")
            .expect("retained segment must not 404 after seek");
        assert_eq!(early.len(), still.len());
        assert!(reg.asset(&id, "seg001.m4s").is_ok());
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
            },
            HlsSubtitleTrack {
                track_id: "e3".into(),
                language: Some("en".into()),
                name: "en".into(),
                is_default: false,
                forced: false,
                sdh: false,
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
    fn subtitle_media_playlist_is_one_segment_vod() {
        let text = String::from_utf8(build_subtitle_playlist(176, "e2", 90_000)).unwrap();
        assert!(text.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(text.contains("/api/v0/items/176/subtitles/e2.vtt"));
        assert!(text.ends_with("#EXT-X-ENDLIST\n"));
        assert_eq!(text.matches("#EXTINF:").count(), 1);
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
