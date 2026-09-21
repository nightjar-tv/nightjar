//! Media tree walk with optional directory-mtime cache (ADR-0013).
//!
//! Warm passes re-stat known directories and readdir only when a dir's mtime
//! moved. Those stats are SMB round-trips; the walk is latency-bound, so a
//! bounded worker pool issues them concurrently. That is the opposite of
//! parallel *file reads* (extract), which saturate the share — keep extract
//! serial (ADR-0013).
//!
//! Cancellation is cooperative and observed at directory boundaries only
//! (ADR-0014 §2 pause). A `readdir` or `stat` already inside the kernel cannot
//! be interrupted, so a cancelled walk finishes the directory it started and
//! stops before the next one.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    /// True when the last listing of this directory was partial: a readdir
    /// entry or a child stat failed, so `files`/`children` may be missing
    /// entries. A partial listing must never authorize `delete_missing`
    /// (ADR-0014 §2), so a later pass re-lists instead of reusing it. A clean
    /// listing clears the flag.
    incomplete: bool,
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
    /// True when the cancellation check returned true at a directory boundary
    /// and the walk stopped early. `files` and `relisted_dirs` are then a
    /// partial listing, so the caller must not treat them as the whole tree:
    /// no `delete_missing`, no sidecar reconciliation (ADR-0014 §2).
    pub cancelled: bool,
}

/// Cooperative cancellation check for one walk pass.
///
/// The scan path passes the ADR-0014 §2 reachability pause:
/// `Availability::pause.is_paused(library_id)`. The check runs once per
/// directory, before that directory is listed; a `readdir` or `stat` already
/// in the kernel cannot be interrupted, so cancellation is cooperative at
/// directory boundaries only. It is shared with the worker threads, so it
/// owns its state and carries no borrow.
pub type CancelCheck = Arc<dyn Fn() -> bool + Send + Sync>;

/// Cancellation check for callers with no cancellation signal: the repoint
/// dry-run and unit tests. It never cancels.
fn never_cancelled() -> CancelCheck {
    Arc::new(|| false)
}

/// Walk concurrency from `NIGHTJAR_WALK_CONCURRENCY`, default 8, clamped to 1..=256.
pub fn walk_concurrency() -> usize {
    std::env::var("NIGHTJAR_WALK_CONCURRENCY")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_WALK_CONCURRENCY)
        .clamp(1, 256)
}

/// Walk `root`, following directories (and file symlinks via [`fs::metadata`]) but
/// not symlink loops: each directory's canonical path is visited at most once.
/// Permission errors are skipped.
///
/// When `cache` is provided, directories whose mtime matches the previous walk are not
/// re-listed: their prior file list and child set are reused. That mtime shortcut
/// cannot see an in-place edit or a sidecar change under an unchanged parent
/// mtime, so the scan path does not use it: [`walk_media_files_fresh`] passes an
/// empty cache and re-lists every directory (CHK-FC). This cache-aware primitive
/// remains for the repoint dry-run and for tests.
///
/// A cached directory is reused only when its previous listing was complete. A
/// listing that lost an entry to a readdir/stat failure is re-listed, so a
/// partial file set never authorizes `delete_missing` on a later pass
/// (ADR-0014 §2).
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
    walk_with_concurrency(root, cache, concurrency, never_cancelled())
}

/// Both walk modes behind one dispatcher, with an explicit cancellation check.
fn walk_with_concurrency(
    root: &Path,
    cache: Option<&mut WalkCache>,
    concurrency: usize,
    cancel: CancelCheck,
) -> Result<WalkOutcome, String> {
    let concurrency = concurrency.clamp(1, 256);
    if concurrency == 1 {
        walk_serial(root, cache, &cancel)
    } else {
        walk_parallel(root, cache, concurrency, cancel)
    }
}

/// Walk `root` re-listing every directory, ignoring the per-directory mtime
/// cache, and replace `cache` with the fresh listing.
///
/// Every full scan uses this — automatic poll, explicit manual scan, library
/// create, and internal follow-up (CHK-FC). The operator and the poll both need
/// the tree as it is now, even where a directory's mtime did not move: an
/// in-place file edit or a sidecar change leaves the parent mtime alone, so a
/// cached listing would never re-read it. One fresh enumeration path is the
/// authority; the refreshed cache is kept for the repoint dry-run and tests,
/// not to let a later scan skip a directory.
///
/// Every directory this pass readdir'd is in `relisted_dirs`, because the fresh
/// walk lists each one. Sidecar rediscovery keys off that set, so a pass
/// reconciles the supported sidecars beside every media file it saw — including
/// unchanged parents — through the caller's shared per-directory listing cache
/// (ADR-0013 §3.5 amendment).
///
/// `cancel` is the cooperative stop check (CHK-WC). The scan path passes the
/// ADR-0014 §2 reachability pause, so a library that goes unreachable mid-walk
/// stops before the next directory. A cancelled outcome is a partial listing:
/// the caller must abandon the pass rather than reconcile against it
/// (ADR-0014 §2). A partial listing is also not written back as the library's
/// cache — it is not the tree.
pub fn walk_media_files_fresh(
    root: &Path,
    cache: &mut WalkCache,
    cancel: CancelCheck,
) -> Result<WalkOutcome, String> {
    let mut fresh = WalkCache::new();
    // The empty cache makes the inner walk list every directory, so its
    // `relisted_dirs` already holds each one.
    let outcome = walk_with_concurrency(root, Some(&mut fresh), walk_concurrency(), cancel)?;
    if !outcome.cancelled {
        *cache = fresh;
    }
    Ok(outcome)
}

fn walk_serial(
    root: &Path,
    mut cache: Option<&mut WalkCache>,
    cancel: &CancelCheck,
) -> Result<WalkOutcome, String> {
    let mut out = Vec::new();
    let mut relisted_dirs = HashSet::new();
    let mut listing_errors = 0u32;
    let mut stack = vec![root.to_path_buf()];
    let mut seen = HashSet::new();
    let mut next_dirs: HashMap<PathBuf, CachedDir> = HashMap::new();
    let mut cancelled = false;

    while let Some(dir) = stack.pop() {
        // Cooperative cancellation at a directory boundary (ADR-0014 §2): the
        // popped directory is dropped unlisted and no further directory work
        // starts. The in-kernel readdir/stat of an earlier directory already
        // ran to completion.
        if cancel() {
            cancelled = true;
            break;
        }
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

    // A cancelled pass listed only part of the tree: its directory set is not
    // the library's listing, so it must not replace the cache.
    if !cancelled && let Some(cache) = cache.as_mut() {
        cache.dirs = next_dirs;
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(WalkOutcome {
        files: out,
        relisted_dirs,
        listing_errors,
        cancelled,
    })
}

fn walk_parallel(
    root: &Path,
    mut cache: Option<&mut WalkCache>,
    workers: usize,
    cancel: CancelCheck,
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
        cancelled: AtomicBool::new(false),
    });

    let mut handles = Vec::with_capacity(workers);
    for i in 0..workers {
        let state = Arc::clone(&state);
        let prev_dirs = Arc::clone(&prev_dirs);
        let cancel = Arc::clone(&cancel);
        handles.push(
            thread::Builder::new()
                .name(format!("walk-{i}"))
                .spawn(move || parallel_worker(state, prev_dirs, cancel))
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
    let cancelled = state.cancelled.load(Ordering::SeqCst);

    // A cancelled pass listed only part of the tree: its directory set is not
    // the library's listing, so it must not replace the cache.
    if !cancelled && let Some(cache) = cache.as_mut() {
        cache.dirs = next_dirs;
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(WalkOutcome {
        files: std::mem::take(&mut *out),
        relisted_dirs,
        listing_errors,
        cancelled,
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
    /// Set when a worker observes the cancellation check and stops.
    cancelled: AtomicBool,
}

/// Release one claimed directory slot and wake the other workers.
///
/// The pending lock is held across the decrement and the notify, so a worker
/// between its empty-queue check and its `wait` cannot miss the wakeup. `children`
/// are queued first: the pop path uses this to publish the directories it found.
fn release_slot(state: &ParallelState, children: Vec<PathBuf>) {
    let mut pending = state.pending.lock().unwrap_or_else(|e| e.into_inner());
    for child in children {
        pending.push_back(child);
    }
    state.inflight.fetch_sub(1, Ordering::SeqCst);
    state.pending_cv.notify_all();
}

fn parallel_worker(
    state: Arc<ParallelState>,
    prev_dirs: Arc<HashMap<PathBuf, CachedDir>>,
    cancel: CancelCheck,
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

        // Cooperative cancellation at a directory boundary (ADR-0014 §2): the
        // claimed directory is released unlisted, no further directory work
        // starts, and the other workers stop at their own next claim. The
        // in-kernel readdir/stat of an earlier directory already ran to
        // completion.
        if cancel() {
            state.cancelled.store(true, Ordering::SeqCst);
            release_slot(&state, Vec::new());
            return Ok(());
        }

        let canon = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        let first_visit = {
            let mut seen = state.seen.lock().unwrap_or_else(|e| e.into_inner());
            seen.insert(canon)
        };
        if !first_visit {
            release_slot(&state, Vec::new());
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

        release_slot(&state, children);
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

    // Reuse the prior listing only when its mtime is unchanged AND that
    // listing was complete. An incomplete entry is re-listed: reusing it would
    // report zero listing errors on this pass, which would let a partial file
    // set authorize `delete_missing` (ADR-0014 §2, Rule 4.15).
    if let Some(prev_dirs) = prev_dirs
        && let Some(prev) = prev_dirs.get(dir)
        && prev.mtime_ms == mtime_ms
        && !prev.incomplete
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
        // Follow symlinks (fs::metadata, not DirEntry::metadata/lstat) so a
        // symlink-to-file is visible to the under-root check (ADR-0030).
        let meta = match fs::metadata(&path) {
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
            incomplete: errors > 0,
        },
        errors,
        children,
    }
}

pub(crate) fn mtime_ms_from(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// True when the path extension is a known media container (not sidecar/text).
pub fn is_media(path: &Path) -> bool {
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

    /// SCAN-D2A: an in-place edit keeps the parent directory mtime, so the
    /// cached walk never re-reads the file. A fresh walk must re-list anyway
    /// and report the new size, and it must leave the cache warm for the next
    /// automatic pass. The parent mtime is restored explicitly so the scenario
    /// holds on filesystems that do bump it on a content write.
    #[cfg(unix)]
    #[test]
    fn fresh_walk_relist_unchanged_dir_and_sees_in_place_edit() {
        let root = tempdir().unwrap();
        let file = root.path().join("clip.mp4");
        File::create(&file).unwrap();

        let mut cache = WalkCache::new();
        let first =
            walk_media_files_cached_with_concurrency(root.path(), Some(&mut cache), 1).unwrap();
        assert_eq!(first.files.len(), 1);
        assert_eq!(first.files[0].size_bytes, 0, "empty file is zero bytes");

        let dir_mtime = fs::metadata(root.path()).unwrap().modified().unwrap();
        thread::sleep(Duration::from_millis(1100));
        fs::write(&file, b"longer content").unwrap();
        File::open(root.path())
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(dir_mtime))
            .unwrap();

        // Cached walk: parent mtime unchanged, so the stale listing is reused.
        let cached =
            walk_media_files_cached_with_concurrency(root.path(), Some(&mut cache), 1).unwrap();
        assert!(
            cached.relisted_dirs.is_empty(),
            "unchanged parent mtime must reuse the cached listing"
        );
        assert_eq!(cached.files[0].size_bytes, 0, "cached listing stays stale");

        // Fresh walk: re-lists the directory and reports the new size. The
        // directory is in `relisted_dirs` even though its mtime did not move,
        // because a fresh/manual walk re-lists every directory and so triggers
        // sidecar rediscovery beside each media file it saw (ADR-0013 §3.5).
        let fresh = walk_media_files_fresh(root.path(), &mut cache, never_cancelled()).unwrap();
        assert!(
            fresh.relisted_dirs.contains(root.path()),
            "a fresh walk lists every directory, so each is a rediscovery trigger"
        );
        assert_eq!(
            fresh.files[0].size_bytes,
            fs::metadata(&file).unwrap().len() as i64,
            "fresh walk must observe the in-place edit"
        );

        // The fresh listing replaced the cache, so the next cached walk is warm.
        let after =
            walk_media_files_cached_with_concurrency(root.path(), Some(&mut cache), 1).unwrap();
        assert!(
            after.relisted_dirs.is_empty(),
            "the fresh listing must leave the cache warm"
        );
        assert_eq!(after.files[0].size_bytes, fresh.files[0].size_bytes);
    }

    /// R4 storage bounds (SCAN-D1): a listing that lost an entry to a stat
    /// failure must not become authoritative on the next unchanged-directory
    /// pass. B is a dangling symlink on the first pass, so its stat fails and
    /// only A is listed. The unchanged pass must re-list (not reuse) that
    /// partial entry and report the doubt again; a clean listing clears it.
    /// Both walk modes run the same scenario, so their completeness agrees.
    #[cfg(unix)]
    #[test]
    fn incomplete_listing_is_not_reused_in_either_walk_mode() {
        use std::os::unix::fs::symlink;

        for concurrency in [1usize, 8] {
            let root = tempdir().unwrap();
            fs::write(root.path().join("A.mp4"), b"a").unwrap();
            let b = root.path().join("B.mkv");
            symlink("missing-target.mkv", &b).unwrap();

            let mut cache = WalkCache::new();
            let partial = walk_media_files_cached_with_concurrency(
                root.path(),
                Some(&mut cache),
                concurrency,
            )
            .unwrap();
            assert_eq!(
                partial.files.len(),
                1,
                "conc={concurrency}: only A is listed while B's stat fails"
            );
            assert_eq!(
                partial.listing_errors, 1,
                "conc={concurrency}: the failed stat must be counted"
            );

            // Directory mtime is unchanged: a complete cache would be reused,
            // but a partial one must be re-listed.
            let cached_pass = walk_media_files_cached_with_concurrency(
                root.path(),
                Some(&mut cache),
                concurrency,
            )
            .unwrap();
            assert!(
                cached_pass.relisted_dirs.contains(root.path()),
                "conc={concurrency}: an incomplete listing must be re-listed, not reused"
            );
            assert_eq!(
                cached_pass.listing_errors, 1,
                "conc={concurrency}: the doubt must persist while B is unreadable"
            );

            // B becomes readable: a successful listing must clear the doubt.
            fs::remove_file(&b).unwrap();
            fs::write(&b, b"b").unwrap();
            let clean = walk_media_files_cached_with_concurrency(
                root.path(),
                Some(&mut cache),
                concurrency,
            )
            .unwrap();
            assert_eq!(
                clean.files.len(),
                2,
                "conc={concurrency}: both files are listed once B is readable"
            );
            assert_eq!(
                clean.listing_errors, 0,
                "conc={concurrency}: a clean listing must clear the doubt"
            );

            // And the clean entry is reusable again.
            let clean_cached = walk_media_files_cached_with_concurrency(
                root.path(),
                Some(&mut cache),
                concurrency,
            )
            .unwrap();
            assert!(
                clean_cached.relisted_dirs.is_empty(),
                "conc={concurrency}: a complete entry is still reused"
            );
            assert_eq!(clean_cached.listing_errors, 0);
        }
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
            File::create(d.join("E01.mkv")).unwrap();
            File::create(d.join("E02.mp4")).unwrap();
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
        // Presence for the emptiness assertions below: each warm walk must have
        // found the 24 files the tree holds before "relisted nothing" means
        // anything (Rule 4.15). Twelve shows times two files each.
        assert_eq!(
            serial_warm.files.len(),
            24,
            "serial warm walk must still find every file"
        );
        assert!(serial_warm.relisted_dirs.is_empty());
        assert_eq!(
            par_warm.files.len(),
            24,
            "parallel warm walk must still find every file"
        );
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

    /// R4 storage bounds: a directory the process cannot read is counted as a
    /// listing error and its media is not claimed. The walk never opens media,
    /// so a permission failure here is a readdir failure, not a probe failure.
    /// The recovered pass is the positive control (Rule 4.15): the error count
    /// returns to zero only because the file is genuinely found.
    #[cfg(unix)]
    #[test]
    fn unreadable_directory_is_counted_and_recovers_when_readable() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().unwrap();
        let open = root.path().join("open");
        let locked = root.path().join("locked");
        fs::create_dir(&open).unwrap();
        fs::create_dir(&locked).unwrap();
        File::create(open.join("visible.mp4")).unwrap();
        File::create(locked.join("hidden.mkv")).unwrap();

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let denied = walk_media_files_cached_with_concurrency(root.path(), None, 1).unwrap();
        assert_eq!(denied.files.len(), 1, "only the readable file is listed");
        assert!(
            denied.files[0].path.ends_with("visible.mp4"),
            "got {:?}",
            denied.files[0].path
        );
        assert_eq!(
            denied.listing_errors, 1,
            "an unreadable directory must be counted, not silently skipped"
        );

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        let recovered = walk_media_files_cached_with_concurrency(root.path(), None, 1).unwrap();
        assert_eq!(
            recovered.files.len(),
            2,
            "the walk must find the hidden file once the directory is readable again"
        );
        assert_eq!(recovered.listing_errors, 0);
    }

    #[test]
    fn directory_symlink_cycle_does_not_spin() {
        let root = tempdir().unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        File::create(a.join("keep.mp4")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&b, a.join("to_b")).unwrap();
            std::os::unix::fs::symlink(&a, b.join("to_a")).unwrap();
        }
        #[cfg(not(unix))]
        {
            return;
        }
        // Must finish; canonical `seen` set breaks the a↔b directory loop.
        let outcome = walk_media_files_cached_with_concurrency(root.path(), None, 1).unwrap();
        assert_eq!(outcome.files.len(), 1);
        let outcome_par = walk_media_files_cached_with_concurrency(root.path(), None, 4).unwrap();
        assert_eq!(outcome_par.files.len(), 1);
    }

    /// A cancellation check that answers false five times and true after that,
    /// so the walk it is given claims at most five directories. The counter is
    /// the positive control: it proves the check was consulted.
    fn cancel_after_five() -> (CancelCheck, Arc<AtomicUsize>) {
        let checks = Arc::new(AtomicUsize::new(0));
        let cancel: CancelCheck = {
            let checks = Arc::clone(&checks);
            Arc::new(move || checks.fetch_add(1, Ordering::SeqCst) >= 5)
        };
        (cancel, checks)
    }

    /// A bushy tree: `shows` show directories under the root, one file each.
    fn show_tree(root: &Path, shows: usize) {
        for i in 0..shows {
            let d = root.join(format!("show{i:02}"));
            fs::create_dir_all(&d).unwrap();
            File::create(d.join("E01.mkv")).unwrap();
        }
    }

    /// CHK-WC: a cancellation check that trips partway stops the walk at the
    /// next directory boundary, in both walk modes. The cancelled pass lists at
    /// least one directory (positive control: the walk was genuinely active)
    /// and no more than the five directories its check allowed (negative
    /// control: it stopped). The clean control lists the whole tree, so the
    /// stop is the check's doing, not an empty fixture.
    #[test]
    fn cancellation_stops_walk_at_a_directory_boundary() {
        let root = tempdir().unwrap();
        show_tree(root.path(), 24);
        // Root plus 24 show directories; 24 media files.
        let total_dirs = 25;

        let clean = walk_with_concurrency(root.path(), None, 1, never_cancelled()).unwrap();
        assert!(
            !clean.cancelled,
            "an uncancelled walk must not report cancellation"
        );
        assert_eq!(
            clean.files.len(),
            24,
            "control: the tree holds 24 media files"
        );
        assert_eq!(
            clean.relisted_dirs.len(),
            total_dirs,
            "control: the tree holds {total_dirs} listable directories"
        );

        for concurrency in [1usize, 8] {
            let (cancel, checks) = cancel_after_five();
            let outcome =
                walk_with_concurrency(root.path(), None, concurrency, Arc::clone(&cancel)).unwrap();

            assert!(
                outcome.cancelled,
                "conc={concurrency}: a tripped check must be reported"
            );
            assert!(
                checks.load(Ordering::SeqCst) >= 6,
                "conc={concurrency}: the check must have been consulted"
            );
            assert!(
                !outcome.relisted_dirs.is_empty(),
                "conc={concurrency}: positive control: the cancelled walk listed a directory"
            );
            assert!(
                outcome.relisted_dirs.len() <= 5,
                "conc={concurrency}: the walk may only list the directories its check allowed, listed {}",
                outcome.relisted_dirs.len()
            );
            assert!(
                outcome.files.len() < clean.files.len(),
                "conc={concurrency}: the walk must stop before every file, found {}",
                outcome.files.len()
            );
            // Every worker holds a clone of the check, so a worker left running
            // would keep the count above one. The cancelled walk must join them.
            assert_eq!(
                Arc::strong_count(&cancel),
                1,
                "conc={concurrency}: the cancelled walk must join every worker"
            );
        }
    }

    /// CHK-WC: a cancelled pass is a partial listing, so it must not replace
    /// the library's cache. Both the cached walk (which owns the per-pass
    /// directory map) and the fresh entry point (which publishes that map) keep
    /// the previous complete listing.
    #[test]
    fn cancelled_walk_does_not_publish_a_partial_cache() {
        let root = tempdir().unwrap();
        show_tree(root.path(), 24);
        let total_dirs = 25;

        for concurrency in [1usize, 8] {
            let mut cache = WalkCache::new();
            let complete = walk_with_concurrency(
                root.path(),
                Some(&mut cache),
                concurrency,
                never_cancelled(),
            )
            .unwrap();
            assert!(!complete.cancelled);
            assert_eq!(
                cache.dir_count(),
                total_dirs,
                "conc={concurrency}: the complete pass primes the cache"
            );

            let (cancel, _) = cancel_after_five();
            let outcome =
                walk_with_concurrency(root.path(), Some(&mut cache), concurrency, cancel).unwrap();
            assert!(outcome.cancelled, "conc={concurrency}");
            assert_eq!(
                cache.dir_count(),
                total_dirs,
                "conc={concurrency}: a cancelled pass must not replace the cache"
            );
        }

        // The fresh entry point keeps the previous listing on a cancelled pass
        // and replaces it on a clean one.
        let mut fresh_cache = WalkCache::new();
        let clean =
            walk_media_files_fresh(root.path(), &mut fresh_cache, never_cancelled()).unwrap();
        assert!(!clean.cancelled);
        assert_eq!(fresh_cache.dir_count(), total_dirs);
        let (cancel, _) = cancel_after_five();
        let cancelled = walk_media_files_fresh(root.path(), &mut fresh_cache, cancel).unwrap();
        assert!(cancelled.cancelled);
        assert_eq!(
            fresh_cache.dir_count(),
            total_dirs,
            "a cancelled fresh walk must keep the previous listing"
        );
    }
}
