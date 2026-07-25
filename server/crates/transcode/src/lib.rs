//! FFmpeg orchestration: remux cache (ADR-0006), HLS transcode sessions
//! (ADR-0007), hardware encode detection (ADR-0009), and text subtitle
//! WebVTT sidecars (ADR-0010).

mod hls;
mod hwaccel;
mod subs;

pub use hls::{
    EncoderKind, HlsSessionRegistry, PlaylistError, SessionEncoder, StartAction, StartSessionError,
    WindowAction, decide_start, decide_window_action,
};
pub use hwaccel::{
    EncoderCandidate, EncoderStatus, TranscodeCapabilities, probe_h264_encoders,
    probe_h264_encoders_arc, select_preferred,
};
pub use subs::{TextSubtitleStream, ensure_webvtt, is_text_subtitle_codec, list_text_subtitles};

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

const MAX_CONCURRENT_REMUXES: usize = 2;

/// Identity of one remux output. mtime and size are part of the name so a
/// changed source file naturally misses the cache and the stale output ages
/// out via LRU.
#[derive(Debug, Clone)]
pub struct RemuxKey {
    pub item_id: i64,
    pub mtime_ms: i64,
    pub size_bytes: i64,
}

impl RemuxKey {
    fn file_name(&self) -> String {
        format!("{}-{}-{}.mp4", self.item_id, self.mtime_ms, self.size_bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemuxState {
    NotStarted { reason: Option<String> },
    Preparing,
    Ready,
    Failed(String),
}

enum JobEntry {
    Preparing,
    Failed(String),
}

pub struct RemuxRegistry {
    cache_dir: PathBuf,
    cap_bytes: u64,
    jobs: Mutex<HashMap<String, JobEntry>>,
}

impl RemuxRegistry {
    /// Creates the cache directory and sweeps orphaned `.tmp` files left by a
    /// killed remux; those never match the ready check and would silently eat
    /// the cap.
    pub fn new(cache_dir: PathBuf, cap_bytes: u64) -> Result<Self, String> {
        fs::create_dir_all(&cache_dir)
            .map_err(|e| format!("create remux cache dir {}: {e}", cache_dir.display()))?;
        for entry in fs::read_dir(&cache_dir)
            .map_err(|e| format!("read remux cache dir {}: {e}", cache_dir.display()))?
            .flatten()
        {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "tmp") {
                if let Err(e) = fs::remove_file(&path) {
                    tracing::warn!(path = %path.display(), error = %e, "orphaned tmp sweep failed");
                } else {
                    tracing::info!(path = %path.display(), "swept orphaned remux tmp file");
                }
            }
        }
        Ok(Self {
            cache_dir,
            cap_bytes,
            jobs: Mutex::new(HashMap::new()),
        })
    }

    pub fn cache_path(&self, key: &RemuxKey) -> PathBuf {
        self.cache_dir.join(key.file_name())
    }

    pub fn status(&self, key: &RemuxKey) -> RemuxState {
        let name = key.file_name();
        let jobs = match self.jobs.lock() {
            Ok(g) => g,
            Err(_) => return RemuxState::Failed("remux registry lock poisoned".into()),
        };
        match jobs.get(&name) {
            Some(JobEntry::Preparing) => RemuxState::Preparing,
            Some(JobEntry::Failed(reason)) => RemuxState::Failed(reason.clone()),
            None => {
                if self.cache_dir.join(&name).exists() {
                    RemuxState::Ready
                } else {
                    RemuxState::NotStarted { reason: None }
                }
            }
        }
    }

    /// Starts a background remux if a slot is free, or reports the current
    /// state. When all slots are busy this returns `NotStarted` with a busy
    /// reason; clients re-POST while they see that (ADR-0006 §3). A `Failed`
    /// entry is retried when a slot is free.
    pub fn start(self: &Arc<Self>, key: &RemuxKey, src: &Path) -> RemuxState {
        let name = key.file_name();
        let dest = self.cache_dir.join(&name);
        if dest.exists() {
            return RemuxState::Ready;
        }

        let mut jobs = match self.jobs.lock() {
            Ok(g) => g,
            Err(_) => return RemuxState::Failed("remux registry lock poisoned".into()),
        };
        if matches!(jobs.get(&name), Some(JobEntry::Preparing)) {
            return RemuxState::Preparing;
        }
        let preparing = jobs
            .values()
            .filter(|e| matches!(e, JobEntry::Preparing))
            .count();
        if preparing >= MAX_CONCURRENT_REMUXES {
            return RemuxState::NotStarted {
                reason: Some("all remux slots busy; retry shortly".into()),
            };
        }

        let in_flight: Vec<String> = jobs
            .iter()
            .filter(|(_, e)| matches!(e, JobEntry::Preparing))
            .map(|(n, _)| n.clone())
            .collect();
        if let Err(reason) = self.make_room(key.size_bytes as u64, &in_flight) {
            jobs.insert(name, JobEntry::Failed(reason.clone()));
            return RemuxState::Failed(reason);
        }

        jobs.insert(name.clone(), JobEntry::Preparing);
        drop(jobs);

        let registry = Arc::clone(self);
        let src = src.to_path_buf();
        let tmp = self.cache_dir.join(format!("{name}.tmp"));
        let spawn = std::thread::Builder::new()
            .name(format!("remux-{}", key.item_id))
            .spawn(move || {
                let result = run_remux(&src, &tmp, &dest);
                let mut jobs = match registry.jobs.lock() {
                    Ok(g) => g,
                    Err(_) => return,
                };
                match result {
                    Ok(()) => {
                        jobs.remove(&name);
                        tracing::info!(path = %dest.display(), "remux ready");
                    }
                    Err(e) => {
                        tracing::warn!(src = %src.display(), error = %e, "remux failed");
                        jobs.insert(name, JobEntry::Failed(e));
                    }
                }
            });
        if let Err(e) = spawn {
            let reason = format!("spawn remux thread: {e}");
            if let Ok(mut jobs) = self.jobs.lock() {
                jobs.insert(key.file_name(), JobEntry::Failed(reason.clone()));
            }
            return RemuxState::Failed(reason);
        }
        RemuxState::Preparing
    }

    /// Bumps the cache file's mtime so LRU eviction sees it as recently served.
    pub fn touch(&self, key: &RemuxKey) {
        let path = self.cache_path(key);
        let touched = fs::File::options()
            .append(true)
            .open(&path)
            .and_then(|f| f.set_modified(SystemTime::now()));
        if let Err(e) = touched {
            tracing::warn!(path = %path.display(), error = %e, "remux cache touch failed");
        }
    }

    /// Evicts oldest-served ready files until `needed` bytes fit under the
    /// cap. In-flight outputs are never evicted.
    fn make_room(&self, needed: u64, in_flight: &[String]) -> Result<(), String> {
        if needed > self.cap_bytes {
            return Err(format!(
                "source is {needed} bytes but the remux cache cap ({} bytes) cannot hold it; raise NIGHTJAR_REMUX_CACHE_BYTES",
                self.cap_bytes
            ));
        }
        let mut evictable = Vec::new();
        let mut locked_bytes = 0u64;
        for entry in fs::read_dir(&self.cache_dir)
            .map_err(|e| format!("read remux cache dir {}: {e}", self.cache_dir.display()))?
            .flatten()
        {
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let pinned = in_flight.contains(&file_name)
                || in_flight.iter().any(|n| file_name == format!("{n}.tmp"));
            if pinned {
                locked_bytes += meta.len();
            } else {
                evictable.push(CacheFile {
                    name: file_name,
                    size: meta.len(),
                    modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                });
            }
        }
        let victims = select_evictions(evictable, locked_bytes, needed, self.cap_bytes)?;
        for name in victims {
            let path = self.cache_dir.join(&name);
            match fs::remove_file(&path) {
                Ok(()) => tracing::info!(path = %path.display(), "evicted remux cache file"),
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "remux eviction failed")
                }
            }
        }
        Ok(())
    }
}

struct CacheFile {
    name: String,
    size: u64,
    modified: SystemTime,
}

/// Pure eviction decision: which of `evictable` to delete (oldest mtime
/// first) so `locked_bytes + remaining + needed <= cap`.
fn select_evictions(
    mut evictable: Vec<CacheFile>,
    locked_bytes: u64,
    needed: u64,
    cap: u64,
) -> Result<Vec<String>, String> {
    let all_evictable: u64 = evictable.iter().map(|f| f.size).sum();
    let floor = locked_bytes.saturating_add(needed);
    if floor > cap {
        return Err(format!(
            "remux cache cap ({cap} bytes) cannot fit {needed} more bytes alongside {locked_bytes} bytes of in-flight output"
        ));
    }
    let budget_for_ready = cap - floor;
    if all_evictable <= budget_for_ready {
        return Ok(Vec::new());
    }
    evictable.sort_by_key(|f| f.modified);
    let mut to_free = all_evictable - budget_for_ready;
    let mut victims = Vec::new();
    for file in evictable {
        if to_free == 0 {
            break;
        }
        to_free = to_free.saturating_sub(file.size);
        victims.push(file.name);
    }
    Ok(victims)
}

/// Stream-copies `src` into an fMP4-friendly MP4 at `dest`, via `tmp` so a
/// partial output is never mistaken for a ready file. `-f mp4` is explicit
/// because the `.tmp` extension defeats FFmpeg's container inference.
pub fn run_remux(src: &Path, tmp: &Path, dest: &Path) -> Result<(), String> {
    let output = Command::new("ffmpeg")
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(src)
        .args([
            "-map",
            "0:v:0",
            "-map",
            "0:a:0?",
            "-c",
            "copy",
            "-movflags",
            "+faststart",
            "-f",
            "mp4",
        ])
        .arg(tmp)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "ffmpeg not found on PATH".into()
            } else {
                format!("spawn ffmpeg for {}: {e}", src.display())
            }
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_file(tmp).inspect_err(|e| {
            tracing::warn!(path = %tmp.display(), error = %e, "remux tmp cleanup failed");
        });
        return Err(format!(
            "ffmpeg remux failed for {}: {}",
            src.display(),
            stderr.trim()
        ));
    }

    fs::rename(tmp, dest).map_err(|e| {
        format!(
            "rename remux output {} -> {}: {e}",
            tmp.display(),
            dest.display()
        )
    })
}

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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

    fn make_h264_aac_mkv(path: &Path) {
        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=64x64:d=0.2",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=48000:cl=stereo",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-shortest",
                path.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(status.success(), "fixture mkv generation failed");
    }

    #[test]
    fn remux_produces_probeable_mp4() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("fixture.mkv");
        make_h264_aac_mkv(&src);

        let tmp = dir.path().join("out.mp4.tmp");
        let dest = dir.path().join("out.mp4");
        run_remux(&src, &tmp, &dest).unwrap();
        assert!(dest.exists());
        assert!(!tmp.exists());

        let probe = Command::new("ffprobe")
            .args([
                "-v",
                "quiet",
                "-show_entries",
                "format=format_name",
                "-of",
                "csv=p=0",
            ])
            .arg(&dest)
            .output()
            .unwrap();
        assert!(probe.status.success());
        let format = String::from_utf8_lossy(&probe.stdout);
        assert!(format.contains("mp4"), "unexpected container: {format}");
    }

    #[test]
    fn registry_lifecycle_ready_after_start() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("fixture.mkv");
        make_h264_aac_mkv(&src);
        let src_len = fs::metadata(&src).unwrap().len() as i64;

        let registry =
            Arc::new(RemuxRegistry::new(dir.path().join("cache"), 1024 * 1024 * 1024).unwrap());
        let key = RemuxKey {
            item_id: 1,
            mtime_ms: 42,
            size_bytes: src_len,
        };
        assert_eq!(
            registry.status(&key),
            RemuxState::NotStarted { reason: None }
        );

        let state = registry.start(&key, &src);
        assert!(matches!(state, RemuxState::Preparing | RemuxState::Ready));
        for _ in 0..200 {
            match registry.status(&key) {
                RemuxState::Ready => break,
                RemuxState::Failed(e) => panic!("remux failed: {e}"),
                _ => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        assert_eq!(registry.status(&key), RemuxState::Ready);
        assert!(registry.cache_path(&key).exists());
        // Second start is a cache hit.
        assert_eq!(registry.start(&key, &src), RemuxState::Ready);
    }

    #[test]
    fn source_over_cap_fails_with_reason() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(RemuxRegistry::new(dir.path().join("cache"), 100).unwrap());
        let key = RemuxKey {
            item_id: 7,
            mtime_ms: 1,
            size_bytes: 1000,
        };
        let src = dir.path().join("big.mkv");
        fs::write(&src, vec![0u8; 1000]).unwrap();
        match registry.start(&key, &src) {
            RemuxState::Failed(reason) => {
                assert!(reason.contains("cap"), "reason: {reason}")
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(matches!(registry.status(&key), RemuxState::Failed(_)));
    }

    #[test]
    fn new_sweeps_orphaned_tmp_files() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("9-1-1.mp4.tmp"), b"partial").unwrap();
        fs::write(cache.join("9-1-1.mp4"), b"ready").unwrap();
        let _registry = RemuxRegistry::new(cache.clone(), 1024).unwrap();
        assert!(!cache.join("9-1-1.mp4.tmp").exists());
        assert!(cache.join("9-1-1.mp4").exists());
    }

    #[test]
    fn eviction_table() {
        let t = |secs: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
        let file = |name: &str, size: u64, secs: u64| CacheFile {
            name: name.into(),
            size,
            modified: t(secs),
        };

        // Fits without eviction.
        let victims = select_evictions(vec![file("a.mp4", 30, 1)], 0, 50, 100).unwrap();
        assert!(victims.is_empty());

        // Oldest goes first, and only as many as needed.
        let victims = select_evictions(
            vec![file("new.mp4", 40, 10), file("old.mp4", 40, 1)],
            0,
            50,
            100,
        )
        .unwrap();
        assert_eq!(victims, vec!["old.mp4".to_string()]);

        // Evicts several when one is not enough.
        let victims = select_evictions(
            vec![
                file("c.mp4", 30, 3),
                file("a.mp4", 30, 1),
                file("b.mp4", 30, 2),
            ],
            0,
            60,
            100,
        )
        .unwrap();
        assert_eq!(victims, vec!["a.mp4".to_string(), "b.mp4".to_string()]);

        // In-flight bytes are untouchable; error when they crowd out the request.
        let err = select_evictions(vec![file("a.mp4", 10, 1)], 90, 20, 100).unwrap_err();
        assert!(err.contains("in-flight"));
    }
}
