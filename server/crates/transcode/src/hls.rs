//! HLS playback sessions (ADR-0007). A session either stream-copies the
//! source (remux) or re-encodes it (transcode); the two differ by
//! [`SessionMode`] and nothing else (ADR-0011).

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
const SEGMENT_WAIT: Duration = Duration::from_secs(15);
const SEGMENT_POLL: Duration = Duration::from_millis(100);
/// How far ahead of the latest on-disk segment a request may be before we
/// treat it as a scrub that needs a window move.
const CATCH_UP_SEGMENTS: u64 = 2;

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

/// Pure window-move decision for a playlist/segment seek intent.
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
    /// Actual encoder for this process. Future fallback updates this field.
    video_encoder: String,
    start_ms: u64,
    duration_ms: u64,
    child: Option<Child>,
    last_access: Instant,
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
    /// session; seeking restarts that session in place (ADR-0011).
    /// `subtitle_tracks` is snapshotted here and never revisited.
    pub fn start(
        &self,
        item_id: i64,
        src: &Path,
        start_ms: u64,
        duration_ms: u64,
        mode: SessionMode,
        subtitle_tracks: Vec<HlsSubtitleTrack>,
    ) -> Result<String, StartSessionError> {
        let start_ms = align_to_segment(start_ms);
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
        let child = spawn_ffmpeg(src, &dir, start_ms, mode, &self.video_encoder)
            .map_err(StartSessionError::Spawn)?;
        sessions.insert(
            id.clone(),
            Session {
                item_id,
                src: src.to_path_buf(),
                dir,
                mode,
                video_encoder: self.video_encoder.clone(),
                start_ms,
                duration_ms,
                child: Some(child),
                last_access: Instant::now(),
                failed: None,
                subtitle_tracks,
            },
        );
        tracing::info!(
            session_id = %id,
            item_id,
            start_ms,
            mode = ?mode,
            encoder = %self.video_encoder,
            "hls session started"
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
            Ok(build_playlist(session.duration_ms))
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
            match decide_window_action(aligned, session.start_ms, on_disk) {
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
        build(session)
    }

    /// Serves init/segment files. Retained segments from a previous encode
    /// window stay readable. A request far ahead of what is on disk restarts
    /// the window. Safari native scrub hits this path only.
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
                    return Ok(bytes);
                }
                if let Some(err) = note_child_exit(session) {
                    return Err(PlaylistError::Failed(err));
                }
                if file_name == "init.mp4" {
                    // Rewritten on restart; wait for the new init.
                } else if let Some(idx) = requested {
                    let latest = latest_segment_index(&session.dir);
                    let far_ahead = match latest {
                        Some(l) => idx > l.saturating_add(CATCH_UP_SEGMENTS),
                        None => idx > session.start_ms / SEGMENT_MS + CATCH_UP_SEGMENTS,
                    };
                    if far_ahead {
                        let want_ms = idx * SEGMENT_MS;
                        let on_disk = false;
                        match decide_window_action(want_ms, session.start_ms, on_disk) {
                            WindowAction::Restart => {
                                let encoder = session.video_encoder.clone();
                                restart_at(session, want_ms, &encoder)?;
                                deadline = Instant::now() + SEGMENT_WAIT;
                            }
                            WindowAction::Serve => {}
                        }
                    } else if session.child.is_none() && idx < session.start_ms / SEGMENT_MS {
                        // Before the current window and not retained on disk.
                        return Err(PlaylistError::NotFound);
                    } else if session.child.is_none() {
                        return Err(PlaylistError::NotFound);
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
    start_ms: u64,
    video_encoder: &str,
) -> Result<(), PlaylistError> {
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
        video_encoder,
    )
    .map_err(PlaylistError::Failed)?;
    session.child = Some(child);
    session.start_ms = start_ms;
    session.failed = None;
    tracing::info!(
        start_ms,
        encoder = video_encoder,
        path = %session.src.display(),
        "hls session seek restart"
    );
    Ok(())
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

fn segment_name(index: u64) -> String {
    format!("seg{index:03}.m4s")
}

/// Highest `segNNN.m4s` index present in `dir`, if any.
fn latest_segment_index(dir: &Path) -> Option<u64> {
    let mut best: Option<u64> = None;
    let Ok(entries) = fs::read_dir(dir) else {
        return None;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if let Some(idx) = segment_index(name) {
            best = Some(best.map_or(idx, |b| b.max(idx)));
        }
    }
    best
}

/// "segNNN.m4s" -> NNN; None for init.mp4.
fn segment_index(name: &str) -> Option<u64> {
    name.strip_prefix("seg")?.strip_suffix(".m4s")?.parse().ok()
}

/// Static VOD playlist for the full title. Players get the real duration and
/// a working scrubber; the encoder fills segments in behind it and `asset`
/// waits for stragglers. EXTINF follows SEGMENT_MS; fMP4 timestamps carry the
/// exact timing.
fn build_playlist(duration_ms: u64) -> Vec<u8> {
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
        out.push_str(
            "#EXT-X-STREAM-INF:BANDWIDTH=5000000,CODECS=\"avc1.4d401f,mp4a.40.2\",SUBTITLES=\"subs\"\n",
        );
    } else {
        out.push_str("#EXT-X-STREAM-INF:BANDWIDTH=5000000,CODECS=\"avc1.4d401f,mp4a.40.2\"\n");
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
    cmd.args(["-map", "0:v:0", "-map", "0:a:0?"]);
    match mode {
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
                "-c:a",
                "aac",
                "-ac",
                "2",
                "-b:a",
                "192k",
            ]);
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
        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=64x64:d=4",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=4",
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
            .start(1, &src, 0, FIXTURE_MS, SessionMode::Transcode, vec![])
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
            .start(1, &src, 0, FIXTURE_MS, SessionMode::Transcode, vec![])
            .unwrap();
        let b = reg
            .start(1, &src, 0, FIXTURE_MS, SessionMode::Transcode, vec![])
            .unwrap();
        assert_ne!(a, b);
        assert!(matches!(
            reg.start(2, &src, 0, FIXTURE_MS, SessionMode::Transcode, vec![]),
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
            .start(1, &src, 0, FIXTURE_MS, SessionMode::Copy, vec![])
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
            .start(1, &src, 0, FIXTURE_MS, SessionMode::Transcode, vec![])
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

    #[test]
    fn vod_playlist_covers_full_duration() {
        let text = String::from_utf8(build_playlist(5000)).unwrap();
        assert!(text.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(text.ends_with("#EXT-X-ENDLIST\n"));
        assert_eq!(text.matches(".m4s").count(), 3, "{text}");
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
        assert!(!text.contains("media.m3u8"));
    }

    #[test]
    fn master_without_tracks_has_no_subtitles_attr() {
        let text = String::from_utf8(build_master(&[])).unwrap();
        assert!(!text.contains("EXT-X-MEDIA"));
        assert!(!text.contains("SUBTITLES="));
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
            let mut child = spawn_ffmpeg(src, &enc, 0, SessionMode::Transcode, "libx264").unwrap();
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
