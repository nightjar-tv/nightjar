//! Media tree walk with optional directory-mtime cache (ADR-0013).
//!
//! Warm passes re-stat known directories and readdir only when a dir's mtime
//! moved. Those stats are SMB round-trips; the walk is latency-bound, so a
//! bounded worker pool issues them concurrently. That is the opposite of
//! parallel *file reads* (extract), which saturate the share — keep extract
//! serial (ADR-0013).

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::SystemTime;

const MEDIA_EXTS: &[&str] = &[
    "mp4", "m4v", "mkv", "avi", "mov", "webm", "ts", "m2ts", "wmv", "mpg", "mpeg", "ogv",
];

/// Default concurrent directory workers. Below the Wi-Fi SMB knee measured
/// 2026-07-27 (gains flatten ~16–32); override with `NIGHTJAR_WALK_CONCURRENCY`.
/// Not scaled by core count — that was the Jellyfin extract failure mode.
const DEFAULT_WALK_CONCURRENCY: usize = 8;

#[derive(Debug, Clone)]
pub struct MediaFile {
    pub path: PathBuf,
    pub mtime_ms: i64,
    pub size_bytes: i64,
}

#[derive(Debug, Clone, Default)]
struct CachedDir {
    /// Directory mtime in milliseconds since epoch.
    mtime_ms: i64,
    /// Media files directly in this directory (not recursive).
    files: Vec<MediaFile>,
    /// Child directories discovered on the last listing of this directory.
    children: Vec<PathBuf>,
}

/// Per-library walk memory so poll cycles can skip unchanged directories.
#[derive(Debug, Default, Clone)]
pub struct WalkCache {
    dirs: HashMap<PathBuf, CachedDir>,
}

impl WalkCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.dirs.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.dirs.is_empty()
    }

    pub fn dir_count(&self) -> usize {
        self.dirs.len()
    }
}

/// Result of a media walk, including which directories were actually re-listed.
#[derive(Debug, Default)]
pub struct WalkOutcome {
    pub files: Vec<MediaFile>,
    /// Directories whose contents were readdir'd this pass (cold, or mtime moved).
    /// Sidecar rediscovery is only needed for media whose parent is in this set.
    pub relisted_dirs: HashSet<PathBuf>,
    /// Metadata/readdir failures skipped during the walk (ADR-0014 doubt signal).
    pub listing_errors: u32,
}

/// Walk concurrency from `NIGHTJAR_WALK_CONCURRENCY`, default 16, clamped to 1..=256.
pub fn walk_concurrency() -> usize {
    std::env::var("NIGHTJAR_WALK_CONCURRENCY")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_WALK_CONCURRENCY)
        .clamp(1, 256)
}

/// Walk `root`, following directories but not symlink loops. Permission errors are skipped.
///
/// When `cache` is provided, directories whose mtime matches the previous walk are not
/// re-listed: their prior file list and child set are reused. That is the cheap
/// poll path (ADR-0013). Immediate-parent mtime updates when a file is added;
/// ancestors need not.
pub fn walk_media_files_cached(
    root: &Path,
    cache: Option<&mut WalkCache>,
) -> Result<WalkOutcome, String> {
    walk_media_files_cached_with_concurrency(root, cache, walk_concurrency())
}

/// Same as [`walk_media_files_cached`] with an explicit worker count (tests / measure).
pub fn walk_media_files_cached_with_concurrency(
    root: &Path,
    cache: Option<&mut WalkCache>,
    concurrency: usize,
) -> Result<WalkOutcome, String> {
    let concurrency = concurrency.clamp(1, 256);
    if concurrency == 1 {
        walk_serial(root, cache)
    } else {
        walk_parallel(root, cache, concurrency)
    }
}

fn walk_serial(root: &Path, mut cache: Option<&mut WalkCache>) -> Result<WalkOutcome, String> {
    let mut out = Vec::new();
    let mut relisted_dirs = HashSet::new();
    let mut listing_errors = 0u32;
    let mut stack = vec![root.to_path_buf()];
    let mut seen = HashSet::new();
    let mut next_dirs: HashMap<PathBuf, CachedDir> = HashMap::new();

    while let Some(dir) = stack.pop() {
        let canon = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if !seen.insert(canon) {
            continue;
        }
        match process_dir(&dir, cache.as_ref().map(|c| &c.dirs)) {
            DirVisit::Unreadable => {
                listing_errors += 1;
            }
            DirVisit::Cached { entry, children } => {
                out.extend(entry.files.iter().cloned());
                stack.extend(children);
                next_dirs.insert(dir, entry);
            }
            DirVisit::Listed {
                entry,
                errors,
                children,
            } => {
                listing_errors += errors;
                relisted_dirs.insert(dir.clone());
                out.extend(entry.files.iter().cloned());
                stack.extend(children);
                next_dirs.insert(dir, entry);
            }
        }
    }

    if let Some(cache) = cache.as_mut() {
        cache.dirs = next_dirs;
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(WalkOutcome {
        files: out,
        relisted_dirs,
        listing_errors,
    })
}

fn walk_parallel(
    root: &Path,
    mut cache: Option<&mut WalkCache>,
    workers: usize,
) -> Result<WalkOutcome, String> {
    let prev_dirs: Arc<HashMap<PathBuf, CachedDir>> =
        Arc::new(cache.as_ref().map(|c| c.dirs.clone()).unwrap_or_default());

    let state = Arc::new(ParallelState {
        pending: Mutex::new(VecDeque::from([root.to_path_buf()])),
        pending_cv: Condvar::new(),
        seen: Mutex::new(HashSet::new()),
        out: Mutex::new(Vec::new()),
        next_dirs: Mutex::new(HashMap::new()),
        relisted: Mutex::new(HashSet::new()),
        listing_errors: AtomicUsize::new(0),
        inflight: AtomicUsize::new(0),
    });

    let mut handles = Vec::with_capacity(workers);
    for i in 0..workers {
        let state = Arc::clone(&state);
        let prev_dirs = Arc::clone(&prev_dirs);
        handles.push(
            thread::Builder::new()
                .name(format!("walk-{i}"))
                .spawn(move || parallel_worker(state, prev_dirs))
                .map_err(|e| format!("spawn walk worker: {e}"))?,
        );
    }
    for h in handles {
        h.join().map_err(|_| "walk worker panicked".to_string())??;
    }

    let mut out = state.out.lock().unwrap_or_else(|e| e.into_inner());
    let next_dirs = std::mem::take(&mut *state.next_dirs.lock().unwrap_or_else(|e| e.into_inner()));
    let relisted_dirs =
        std::mem::take(&mut *state.relisted.lock().unwrap_or_else(|e| e.into_inner()));
    let listing_errors = state.listing_errors.load(Ordering::Relaxed) as u32;

    if let Some(cache) = cache.as_mut() {
        cache.dirs = next_dirs;
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(WalkOutcome {
        files: std::mem::take(&mut *out),
        relisted_dirs,
        listing_errors,
    })
}

struct ParallelState {
    pending: Mutex<VecDeque<PathBuf>>,
    pending_cv: Condvar,
    seen: Mutex<HashSet<PathBuf>>,
    out: Mutex<Vec<MediaFile>>,
    next_dirs: Mutex<HashMap<PathBuf, CachedDir>>,
    relisted: Mutex<HashSet<PathBuf>>,
    listing_errors: AtomicUsize,
    inflight: AtomicUsize,
}

fn parallel_worker(
    state: Arc<ParallelState>,
    prev_dirs: Arc<HashMap<PathBuf, CachedDir>>,
) -> Result<(), String> {
    loop {
        let dir = {
            let mut pending = state.pending.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(dir) = pending.pop_front() {
                    state.inflight.fetch_add(1, Ordering::SeqCst);
                    break dir;
                }
                if state.inflight.load(Ordering::SeqCst) == 0 {
                    // No work and nobody processing: wake others and exit.
                    state.pending_cv.notify_all();
                    return Ok(());
                }
                pending = state
                    .pending_cv
                    .wait(pending)
                    .unwrap_or_else(|e| e.into_inner());
            }
        };

        let canon = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        let first_visit = {
            let mut seen = state.seen.lock().unwrap_or_else(|e| e.into_inner());
            seen.insert(canon)
        };
        if !first_visit {
            state.inflight.fetch_sub(1, Ordering::SeqCst);
            state.pending_cv.notify_all();
            continue;
        }

        let visit = process_dir(&dir, Some(prev_dirs.as_ref()));
        let mut children: Vec<PathBuf> = Vec::new();
        match visit {
            DirVisit::Unreadable => {
                state.listing_errors.fetch_add(1, Ordering::Relaxed);
            }
            DirVisit::Cached {
                entry,
                children: ch,
            } => {
                children = ch;
                state
                    .out
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend(entry.files.iter().cloned());
                state
                    .next_dirs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(dir, entry);
            }
            DirVisit::Listed {
                entry,
                errors,
                children: ch,
            } => {
                children = ch;
                if errors > 0 {
                    state
                        .listing_errors
                        .fetch_add(errors as usize, Ordering::Relaxed);
                }
                state
                    .relisted
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(dir.clone());
                state
                    .out
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend(entry.files.iter().cloned());
                state
                    .next_dirs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(dir, entry);
            }
        }

        {
            let mut pending = state.pending.lock().unwrap_or_else(|e| e.into_inner());
            for child in children {
                pending.push_back(child);
            }
            state.inflight.fetch_sub(1, Ordering::SeqCst);
            state.pending_cv.notify_all();
        }
    }
}

enum DirVisit {
    Unreadable,
    Cached {
        entry: CachedDir,
        children: Vec<PathBuf>,
    },
    Listed {
        entry: CachedDir,
        errors: u32,
        children: Vec<PathBuf>,
    },
}

fn process_dir(dir: &Path, prev_dirs: Option<&HashMap<PathBuf, CachedDir>>) -> DirVisit {
    let meta = match fs::metadata(dir) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(path = %dir.display(), error = %e, "skip unreadable directory");
            return DirVisit::Unreadable;
        }
    };
    let mtime_ms = mtime_ms_from(&meta);

    if let Some(prev_dirs) = prev_dirs
        && let Some(prev) = prev_dirs.get(dir)
        && prev.mtime_ms == mtime_ms
    {
        return DirVisit::Cached {
            children: prev.children.clone(),
            entry: prev.clone(),
        };
    }

    let mut files = Vec::new();
    let mut children = Vec::new();
    let mut errors = 0u32;
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(path = %dir.display(), error = %e, "skip unreadable directory");
            return DirVisit::Unreadable;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                errors += 1;
                tracing::warn!(error = %e, "skip unreadable entry");
                continue;
            }
        };
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                errors += 1;
                tracing::warn!(path = %path.display(), error = %e, "skip unreadable metadata");
                continue;
            }
        };
        if meta.is_dir() {
            children.push(path);
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        if !is_media(&path) {
            continue;
        }
        files.push(MediaFile {
            path,
            mtime_ms: mtime_ms_from(&meta),
            size_bytes: meta.len() as i64,
        });
    }
    DirVisit::Listed {
        entry: CachedDir {
            mtime_ms,
            files,
            children: children.clone(),
        },
        errors,
        children,
    }
}

fn mtime_ms_from(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn is_media(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| MEDIA_EXTS.iter().any(|x| x.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::thread;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn finds_media_skips_other() {
        let dir = tempdir().unwrap();
        File::create(dir.path().join("a.mp4")).unwrap();
        File::create(dir.path().join("notes.txt")).unwrap();
        File::create(dir.path().join("Movie.en.srt")).unwrap();
        File::create(dir.path().join("Movie.vtt")).unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        File::create(dir.path().join("sub").join("b.mkv")).unwrap();
        let outcome = walk_media_files_cached_with_concurrency(dir.path(), None, 1).unwrap();
        assert_eq!(outcome.files.len(), 2);
        assert!(outcome.files.iter().all(|f| {
            let ext = f.path.extension().and_then(|e| e.to_str()).unwrap_or("");
            !matches!(
                ext.to_ascii_lowercase().as_str(),
                "srt" | "vtt" | "ass" | "ssa"
            )
        }));
    }

    #[test]
    fn dir_mtime_cache_skips_unchanged_and_sees_nested_add() {
        let root = tempdir().unwrap();
        let nested = root.path().join("A").join("B");
        fs::create_dir_all(&nested).unwrap();
        File::create(nested.join("one.mp4")).unwrap();

        let mut cache = WalkCache::new();
        let first =
            walk_media_files_cached_with_concurrency(root.path(), Some(&mut cache), 1).unwrap();
        assert_eq!(first.files.len(), 1);
        assert!(first.relisted_dirs.contains(&nested));

        // Unchanged tree: same files, cache hit path — no readdir.
        let second =
            walk_media_files_cached_with_concurrency(root.path(), Some(&mut cache), 1).unwrap();
        assert_eq!(second.files.len(), 1);
        assert!(second.relisted_dirs.is_empty());

        // Nested add updates only the immediate parent mtime; ancestors may not.
        thread::sleep(Duration::from_millis(1100));
        File::create(nested.join("two.mkv")).unwrap();
        let third =
            walk_media_files_cached_with_concurrency(root.path(), Some(&mut cache), 1).unwrap();
        assert_eq!(
            third.files.len(),
            2,
            "immediate-parent mtime change must surface the new file"
        );
        assert!(third.relisted_dirs.contains(&nested));
        assert!(
            !third.relisted_dirs.contains(root.path()),
            "unchanged ancestors must not be re-listed"
        );
    }

    #[test]
    fn sidecar_parent_relisted_when_srt_added() {
        let root = tempdir().unwrap();
        let movie = root.path().join("Movie");
        fs::create_dir_all(&movie).unwrap();
        File::create(movie.join("Movie.mkv")).unwrap();

        let mut cache = WalkCache::new();
        let _ = walk_media_files_cached_with_concurrency(root.path(), Some(&mut cache), 1).unwrap();

        thread::sleep(Duration::from_millis(1100));
        File::create(movie.join("Movie.en.srt")).unwrap();
        let after =
            walk_media_files_cached_with_concurrency(root.path(), Some(&mut cache), 1).unwrap();
        assert_eq!(after.files.len(), 1);
        assert!(
            after.relisted_dirs.contains(&movie),
            "new sidecar bumps parent mtime so rediscovery can run"
        );
    }

    #[test]
    fn concurrent_and_serial_same_change_lists() {
        let root = tempdir().unwrap();
        // Bushy tree so concurrency is exercised.
        for i in 0..12 {
            let d = root.path().join(format!("show{i}")).join("Season 1");
            fs::create_dir_all(&d).unwrap();
            File::create(d.join(format!("E01.mkv"))).unwrap();
            File::create(d.join(format!("E02.mp4"))).unwrap();
        }

        let mut serial_cache = WalkCache::new();
        let serial_cold =
            walk_media_files_cached_with_concurrency(root.path(), Some(&mut serial_cache), 1)
                .unwrap();
        let serial_warm =
            walk_media_files_cached_with_concurrency(root.path(), Some(&mut serial_cache), 1)
                .unwrap();

        let mut par_cache = WalkCache::new();
        let par_cold =
            walk_media_files_cached_with_concurrency(root.path(), Some(&mut par_cache), 8).unwrap();
        let par_warm =
            walk_media_files_cached_with_concurrency(root.path(), Some(&mut par_cache), 8).unwrap();

        let paths = |o: &WalkOutcome| -> Vec<String> {
            o.files
                .iter()
                .map(|f| f.path.to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(paths(&serial_cold), paths(&par_cold), "cold file lists");
        assert_eq!(
            serial_cold.relisted_dirs.len(),
            par_cold.relisted_dirs.len(),
            "cold relisted count"
        );
        assert_eq!(paths(&serial_warm), paths(&par_warm), "warm file lists");
        assert!(serial_warm.relisted_dirs.is_empty());
        assert!(par_warm.relisted_dirs.is_empty());

        thread::sleep(Duration::from_millis(1100));
        let target = root.path().join("show3").join("Season 1");
        File::create(target.join("E03.mkv")).unwrap();

        let serial_delta =
            walk_media_files_cached_with_concurrency(root.path(), Some(&mut serial_cache), 1)
                .unwrap();
        let par_delta =
            walk_media_files_cached_with_concurrency(root.path(), Some(&mut par_cache), 8).unwrap();
        assert_eq!(paths(&serial_delta), paths(&par_delta), "delta file lists");
        assert_eq!(
            serial_delta.relisted_dirs, par_delta.relisted_dirs,
            "delta relisted dirs"
        );
    }
}
