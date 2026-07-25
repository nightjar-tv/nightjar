//! HLS software-transcode sessions (ADR-0007).

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
const SEGMENT_MS: u64 = 2000;
const SEGMENT_WAIT: Duration = Duration::from_secs(15);
const SEGMENT_POLL: Duration = Duration::from_millis(100);
/// How far ahead of the latest on-disk segment a request may be before we
/// treat it as a scrub that needs a window move (restart or fork).
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
    /// Other holders share this encode window; client must POST a new session
    /// at the seek offset instead of restarting this one.
    SharedSeekConflict,
}

/// Pure start decision (table-tested). Cap is checked only when creating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartAction {
    Reuse,
    Create,
    CapFull,
}

/// Pure window-move decision for a playlist/segment seek intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowAction {
    /// Target on disk, or already cooking at this window — serve/wait.
    Serve,
    /// Sole holder: restart FFmpeg at the aligned offset.
    Restart,
    /// Shared holders: client must fork via POST ?startMs=.
    Fork,
}

pub fn decide_start(reusable_exists: bool, slots_full: bool) -> StartAction {
    if reusable_exists {
        StartAction::Reuse
    } else if slots_full {
        StartAction::CapFull
    } else {
        StartAction::Create
    }
}

pub fn decide_window_action(
    refs: usize,
    requested_ms: u64,
    window_start_ms: u64,
    target_on_disk: bool,
) -> WindowAction {
    let aligned = align_to_segment(requested_ms);
    if target_on_disk || aligned == window_start_ms {
        WindowAction::Serve
    } else if refs > 1 {
        WindowAction::Fork
    } else {
        WindowAction::Restart
    }
}

pub struct HlsSessionRegistry {
    root: PathBuf,
    max_sessions: usize,
    next_id: AtomicU64,
    sessions: Mutex<HashMap<String, Session>>,
}

struct Session {
    item_id: i64,
    src: PathBuf,
    dir: PathBuf,
    start_ms: u64,
    duration_ms: u64,
    child: Option<Child>,
    last_access: Instant,
    failed: Option<String>,
    /// Open players. DELETE decrements; FFmpeg is reaped only at zero (or idle).
    refs: usize,
}

impl HlsSessionRegistry {
    /// Creates the HLS cache root, sweeps leftover session dirs from a prior
    /// process, and starts the idle reaper.
    pub fn new(root: PathBuf) -> Result<Arc<Self>, String> {
        Self::with_cap(root, DEFAULT_MAX_SESSIONS)
    }

    pub fn with_cap(root: PathBuf, max_sessions: usize) -> Result<Arc<Self>, String> {
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

        let registry = Arc::new(Self {
            root,
            max_sessions,
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

    /// Starts a session at `start_ms` (aligned), or reuses a live session for
    /// the same item at the same encode window (refcount). Divergent offsets
    /// never share — POST with a new startMs to fork (ADR-0007).
    pub fn start(
        &self,
        item_id: i64,
        src: &Path,
        start_ms: u64,
        duration_ms: u64,
    ) -> Result<String, StartSessionError> {
        let start_ms = align_to_segment(start_ms);
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| StartSessionError::Spawn("hls registry lock poisoned".into()))?;

        let reusable = sessions
            .iter()
            .find(|(_, s)| s.item_id == item_id && s.start_ms == start_ms && s.failed.is_none())
            .map(|(id, _)| id.clone());
        match decide_start(reusable.is_some(), sessions.len() >= self.max_sessions) {
            StartAction::Reuse => {
                let id = reusable.expect("decide_start Reuse");
                let session = sessions.get_mut(&id).expect("reusable id");
                session.refs += 1;
                // Do not bump last_access on join — only playlist/segment
                // traffic proves a live viewer (idle reaper must beat refs).
                tracing::info!(
                    session_id = %id,
                    item_id,
                    start_ms,
                    refs = session.refs,
                    "hls session reused"
                );
                Ok(id)
            }
            StartAction::CapFull => Err(StartSessionError::CapFull),
            StartAction::Create => {
                let id = format!("s{}", self.next_id.fetch_add(1, Ordering::Relaxed));
                let dir = self.root.join(&id);
                fs::create_dir_all(&dir).map_err(|e| {
                    StartSessionError::Spawn(format!("create session dir {}: {e}", dir.display()))
                })?;
                let child = spawn_ffmpeg(src, &dir, start_ms).map_err(StartSessionError::Spawn)?;
                sessions.insert(
                    id.clone(),
                    Session {
                        item_id,
                        src: src.to_path_buf(),
                        dir,
                        start_ms,
                        duration_ms,
                        child: Some(child),
                        last_access: Instant::now(),
                        failed: None,
                        refs: 1,
                    },
                );
                tracing::info!(session_id = %id, item_id, start_ms, "hls session started");
                Ok(id)
            }
        }
    }

    pub fn item_id(&self, session_id: &str) -> Option<i64> {
        self.sessions
            .lock()
            .ok()?
            .get(session_id)
            .map(|s| s.item_id)
    }

    pub fn refs(&self, session_id: &str) -> Option<usize> {
        self.sessions.lock().ok()?.get(session_id).map(|s| s.refs)
    }

    /// Returns the VOD playlist covering the whole title. `start_ms` is seek
    /// intent: solo holders restart in place; shared holders get 409 (fork).
    pub fn playlist(
        &self,
        session_id: &str,
        start_ms: Option<u64>,
    ) -> Result<Vec<u8>, PlaylistError> {
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
            match decide_window_action(session.refs, aligned, session.start_ms, on_disk) {
                WindowAction::Serve => {}
                WindowAction::Fork => return Err(PlaylistError::SharedSeekConflict),
                WindowAction::Restart => restart_at(session, aligned)?,
            }
        }

        if let Some(err) = note_child_exit(session) {
            return Err(PlaylistError::Failed(err));
        }

        let first = segment_name(session.start_ms / SEGMENT_MS);
        if !session.dir.join(&first).exists() {
            return Err(PlaylistError::NotReady);
        }
        Ok(build_playlist(session.duration_ms))
    }

    /// Serves init/segment files. Retained segments from a previous encode
    /// window stay readable. A request far ahead of what is on disk moves the
    /// window (restart or 409). Safari native scrub hits this path only.
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
                        match decide_window_action(session.refs, want_ms, session.start_ms, on_disk)
                        {
                            WindowAction::Fork => {
                                return Err(PlaylistError::SharedSeekConflict);
                            }
                            WindowAction::Restart => {
                                restart_at(session, want_ms)?;
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

    pub fn stop(&self, session_id: &str) -> bool {
        let mut sessions = match self.sessions.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        let Some(session) = sessions.get_mut(session_id) else {
            return false;
        };
        session.refs = session.refs.saturating_sub(1);
        // Do not bump last_access: a departing holder is not proof of playback,
        // and refreshing here would defeat the idle reaper that must beat refs.
        if session.refs > 0 {
            tracing::info!(
                session_id,
                refs = session.refs,
                "hls session release; still in use"
            );
            return true;
        }
        let mut session = sessions.remove(session_id).expect("just checked");
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

    /// Idle and failed sessions are force-reaped even when refs remain.
    /// Crashed or sleeping tabs never DELETE; without this the refcount only
    /// goes up and Gate 2's zero-orphan criterion fails 48 hours later.
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

fn restart_at(session: &mut Session, start_ms: u64) -> Result<(), PlaylistError> {
    stop_child(&mut session.child);
    // Gate 2: do not wipe the prior encode window. In-flight segment fetches
    // (seg1127…) must still hit disk while the new window is cooking. Only
    // remove the muxer sidecar; ffmpeg -y overwrites init and new seg indices.
    let index = session.dir.join("index.m3u8");
    let _ = fs::remove_file(&index);
    let child =
        spawn_ffmpeg(&session.src, &session.dir, start_ms).map_err(PlaylistError::Failed)?;
    session.child = Some(child);
    session.start_ms = start_ms;
    session.failed = None;
    tracing::info!(
        start_ms,
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
/// waits for stragglers. EXTINF is the nominal 2s cadence; fMP4 timestamps
/// carry the exact timing.
fn build_playlist(duration_ms: u64) -> Vec<u8> {
    use std::fmt::Write;
    let full = duration_ms / SEGMENT_MS;
    let rem_ms = duration_ms % SEGMENT_MS;
    let mut out = String::from(
        "#EXTM3U\n\
         #EXT-X-VERSION:7\n\
         #EXT-X-TARGETDURATION:2\n\
         #EXT-X-PLAYLIST-TYPE:VOD\n\
         #EXT-X-MEDIA-SEQUENCE:0\n\
         #EXT-X-MAP:URI=\"init.mp4\"\n",
    );
    for i in 0..full {
        let _ = writeln!(out, "#EXTINF:2.000000,\n{}", segment_name(i));
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

fn spawn_ffmpeg(src: &Path, dir: &Path, start_ms: u64) -> Result<Child, String> {
    let start_secs = format!("{:.3}", start_ms as f64 / 1000.0);
    let start_number = (start_ms / SEGMENT_MS).to_string();
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
    cmd.args([
        "-map",
        "0:v:0",
        "-map",
        "0:a:0?",
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-pix_fmt",
        "yuv420p",
        // Fixed 2s GOP so hls_time=2 cuts on keyframes. force_key_frames
        // expressions have been flaky with fMP4 on some FFmpeg builds.
        "-g",
        "48",
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
        "-f",
        "hls",
        "-hls_time",
        "2",
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
    fn start_decision_table() {
        let cases = [
            (
                "reuse when live session exists",
                true,
                false,
                StartAction::Reuse,
            ),
            (
                "reuse even if slots look full",
                true,
                true,
                StartAction::Reuse,
            ),
            ("create when free slot", false, false, StartAction::Create),
            (
                "cap full when no reusable",
                false,
                true,
                StartAction::CapFull,
            ),
        ];
        for (name, reusable, full, expected) in cases {
            assert_eq!(decide_start(reusable, full), expected, "{name}");
        }
    }

    #[test]
    fn window_decision_table() {
        // (name, refs, requested_ms, window_start_ms, on_disk, expected)
        let cases = [
            ("on disk is serve", 1, 10_000, 0, true, WindowAction::Serve),
            (
                "same window cooking is serve",
                1,
                2000,
                2000,
                false,
                WindowAction::Serve,
            ),
            (
                "solo divergent restarts",
                1,
                10_000,
                0,
                false,
                WindowAction::Restart,
            ),
            (
                "shared divergent forks",
                2,
                10_000,
                0,
                false,
                WindowAction::Fork,
            ),
            (
                "shared but on disk serves",
                3,
                4000,
                0,
                true,
                WindowAction::Serve,
            ),
            (
                "aligns request before compare",
                1,
                2500,
                2000,
                false,
                WindowAction::Serve,
            ),
        ];
        for (name, refs, req, window, on_disk, expected) in cases {
            assert_eq!(
                decide_window_action(refs, req, window, on_disk),
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
        let reg = HlsSessionRegistry::with_cap(dir.path().join("hls"), 3).unwrap();
        let id = reg.start(1, &src, 0, FIXTURE_MS).unwrap();
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

    #[test]
    fn reuse_same_item_shares_until_last_release() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture(&src);
        let reg = HlsSessionRegistry::with_cap(dir.path().join("hls"), 1).unwrap();
        let a = reg.start(1, &src, 0, FIXTURE_MS).unwrap();
        let b = reg.start(1, &src, 0, FIXTURE_MS).unwrap();
        assert_eq!(a, b);
        assert_eq!(reg.refs(&a), Some(2));
        assert!(matches!(
            reg.start(2, &src, 0, FIXTURE_MS),
            Err(StartSessionError::CapFull)
        ));
        assert!(reg.stop(&a));
        assert_eq!(reg.refs(&a), Some(1));
        wait_playlist(&reg, &a);
        assert!(reg.stop(&a));
        assert!(matches!(
            reg.playlist(&a, None),
            Err(PlaylistError::NotFound)
        ));
    }

    #[test]
    fn fork_moves_ref_off_shared_session() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture(&src);
        let reg = HlsSessionRegistry::with_cap(dir.path().join("hls"), 3).unwrap();
        let shared = reg.start(1, &src, 0, FIXTURE_MS).unwrap();
        let _other = reg.start(1, &src, 0, FIXTURE_MS).unwrap();
        assert_eq!(reg.refs(&shared), Some(2));
        wait_playlist(&reg, &shared);
        assert!(matches!(
            reg.playlist(&shared, Some(60_000)),
            Err(PlaylistError::SharedSeekConflict)
        ));
        // Scrubbing viewer forks then releases the shared session.
        let forked = reg.start(1, &src, 60_000, FIXTURE_MS).unwrap();
        assert_ne!(forked, shared);
        assert_eq!(reg.refs(&forked), Some(1));
        assert!(reg.stop(&shared));
        assert_eq!(
            reg.refs(&shared),
            Some(1),
            "other viewer still holds the original"
        );
        reg.stop(&shared);
        reg.stop(&forked);
    }

    #[test]
    fn divergent_offset_is_separate_session() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture(&src);
        let reg = HlsSessionRegistry::with_cap(dir.path().join("hls"), 3).unwrap();
        let a = reg.start(1, &src, 0, FIXTURE_MS).unwrap();
        let b = reg.start(1, &src, 2000, FIXTURE_MS).unwrap();
        assert_ne!(a, b);
        reg.stop(&a);
        reg.stop(&b);
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
        let reg = HlsSessionRegistry::with_cap(dir.path().join("hls"), 3).unwrap();
        let id = reg.start(1, &src, 0, FIXTURE_MS).unwrap();
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
    fn shared_asset_scrub_conflicts() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture(&src);
        let reg = HlsSessionRegistry::with_cap(dir.path().join("hls"), 3).unwrap();
        let a = reg.start(1, &src, 0, FIXTURE_MS).unwrap();
        let _b = reg.start(1, &src, 0, FIXTURE_MS).unwrap();
        wait_playlist(&reg, &a);
        assert!(matches!(
            reg.asset(&a, "seg100.m4s"),
            Err(PlaylistError::SharedSeekConflict)
        ));
        reg.stop(&a);
        reg.stop(&a);
    }

    #[test]
    fn cap_full_rejects() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture(&src);
        let reg = HlsSessionRegistry::with_cap(dir.path().join("hls"), 1).unwrap();
        let id = reg.start(1, &src, 0, FIXTURE_MS).unwrap();
        assert!(matches!(
            reg.start(2, &src, 0, FIXTURE_MS),
            Err(StartSessionError::CapFull)
        ));
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
    fn asset_name_allowlist() {
        assert!(is_safe_asset("init.mp4"));
        assert!(is_safe_asset("seg000.m4s"));
        assert!(!is_safe_asset("../etc/passwd"));
        assert_eq!(segment_index("seg042.m4s"), Some(42));
        assert_eq!(segment_index("init.mp4"), None);
    }
}
