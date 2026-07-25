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
const HLS_LIST_SIZE: &str = "5";
const REAPER_TICK: Duration = Duration::from_secs(5);

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
    child: Option<Child>,
    last_access: Instant,
    failed: Option<String>,
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

    pub fn start(&self, item_id: i64, src: &Path) -> Result<String, StartSessionError> {
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

        let child = spawn_ffmpeg(src, &dir, 0).map_err(StartSessionError::Spawn)?;
        sessions.insert(
            id.clone(),
            Session {
                item_id,
                src: src.to_path_buf(),
                dir,
                start_ms: 0,
                child: Some(child),
                last_access: Instant::now(),
                failed: None,
            },
        );
        tracing::info!(session_id = %id, item_id, "hls session started");
        Ok(id)
    }

    pub fn item_id(&self, session_id: &str) -> Option<i64> {
        self.sessions
            .lock()
            .ok()?
            .get(session_id)
            .map(|s| s.item_id)
    }

    /// Returns playlist bytes, applying a seek restart when `start_ms` differs
    /// from the session's current encode window.
    pub fn playlist(&self, session_id: &str, start_ms: u64) -> Result<Vec<u8>, PlaylistError> {
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

        if start_ms != session.start_ms {
            restart_at(session, start_ms)?;
        }

        // Reap a dead child so failures surface.
        if let Some(child) = session.child.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) if !status.success() => {
                    let msg = format!("ffmpeg exited with {status}");
                    session.failed = Some(msg.clone());
                    return Err(PlaylistError::Failed(msg));
                }
                Ok(Some(_)) => {
                    // Encoder finished the file; playlist may still be valid.
                    session.child = None;
                }
                Ok(None) => {}
                Err(e) => {
                    let msg = format!("ffmpeg wait: {e}");
                    session.failed = Some(msg.clone());
                    return Err(PlaylistError::Failed(msg));
                }
            }
        }

        let path = session.dir.join("index.m3u8");
        match fs::read(&path) {
            Ok(bytes) if !bytes.is_empty() => Ok(bytes),
            Ok(_) | Err(_) => Err(PlaylistError::NotReady),
        }
    }

    pub fn asset(&self, session_id: &str, name: &str) -> Result<Vec<u8>, PlaylistError> {
        if !is_safe_asset(name) {
            return Err(PlaylistError::NotFound);
        }
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| PlaylistError::Failed("hls registry lock poisoned".into()))?;
        let session = sessions
            .get_mut(session_id)
            .ok_or(PlaylistError::NotFound)?;
        session.last_access = Instant::now();
        let path = session.dir.join(name);
        fs::read(&path).map_err(|_| PlaylistError::NotFound)
    }

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

    fn reaper_loop(&self) {
        loop {
            std::thread::sleep(REAPER_TICK);
            let stale: Vec<String> = {
                let Ok(sessions) = self.sessions.lock() else {
                    continue;
                };
                sessions
                    .iter()
                    .filter(|(_, s)| s.last_access.elapsed() > IDLE_TIMEOUT)
                    .map(|(id, _)| id.clone())
                    .collect()
            };
            for id in stale {
                tracing::info!(session_id = %id, "hls session idle timeout");
                self.stop(&id);
            }
        }
    }
}

fn restart_at(session: &mut Session, start_ms: u64) -> Result<(), PlaylistError> {
    stop_child(&mut session.child);
    clear_dir(&session.dir).map_err(PlaylistError::Failed)?;
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

fn spawn_ffmpeg(src: &Path, dir: &Path, start_ms: u64) -> Result<Child, String> {
    let start_secs = format!("{:.3}", start_ms as f64 / 1000.0);
    let mut cmd = Command::new("ffmpeg");
    cmd.current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"]);
    if start_ms > 0 {
        cmd.args(["-ss", &start_secs]);
    }
    cmd.arg("-i").arg(src).args([
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
        HLS_LIST_SIZE,
        "-hls_flags",
        "delete_segments+append_list",
        "-hls_segment_type",
        "fmp4",
        "-hls_fmp4_init_filename",
        "init.mp4",
        "-hls_segment_filename",
        "seg%03d.m4s",
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

fn clear_dir(dir: &Path) -> Result<(), String> {
    for entry in fs::read_dir(dir)
        .map_err(|e| format!("read session dir {}: {e}", dir.display()))?
        .flatten()
    {
        let path = entry.path();
        if path.is_file() {
            fs::remove_file(&path).map_err(|e| format!("remove {}: {e}", path.display()))?;
        }
    }
    Ok(())
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

    fn wait_playlist(reg: &HlsSessionRegistry, id: &str) -> Vec<u8> {
        for _ in 0..100 {
            match reg.playlist(id, 0) {
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
    fn session_produces_playlist_and_stop_reaps() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture(&src);
        let reg = HlsSessionRegistry::with_cap(dir.path().join("hls"), 3).unwrap();
        let id = reg.start(1, &src).unwrap();
        let playlist = wait_playlist(&reg, &id);
        let text = String::from_utf8_lossy(&playlist);
        assert!(text.contains("#EXTM3U"), "{text}");
        assert!(text.contains("init.mp4") || text.contains(".m4s"), "{text}");
        assert!(reg.stop(&id));
        assert!(matches!(reg.playlist(&id, 0), Err(PlaylistError::NotFound)));
    }

    #[test]
    fn seek_restart_changes_window() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture(&src);
        let reg = HlsSessionRegistry::with_cap(dir.path().join("hls"), 3).unwrap();
        let id = reg.start(2, &src).unwrap();
        wait_playlist(&reg, &id);
        // Seek into the middle; should restart without failing.
        for _ in 0..100 {
            match reg.playlist(&id, 2000) {
                Ok(_) => {
                    reg.stop(&id);
                    return;
                }
                Err(PlaylistError::NotReady) => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => panic!("seek playlist error: {e:?}"),
            }
        }
        panic!("seek playlist not ready");
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
        let id = reg.start(1, &src).unwrap();
        assert!(matches!(
            reg.start(2, &src),
            Err(StartSessionError::CapFull)
        ));
        reg.stop(&id);
    }

    #[test]
    fn asset_name_allowlist() {
        assert!(is_safe_asset("init.mp4"));
        assert!(is_safe_asset("seg000.m4s"));
        assert!(is_safe_asset("seg12.m4s"));
        assert!(!is_safe_asset("../etc/passwd"));
        assert!(!is_safe_asset("seg.m4s"));
        assert!(!is_safe_asset("index.m3u8"));
    }
}
