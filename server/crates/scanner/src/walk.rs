use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const MEDIA_EXTS: &[&str] = &[
    "mp4", "m4v", "mkv", "avi", "mov", "webm", "ts", "m2ts", "wmv", "mpg", "mpeg", "ogv",
];

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
}

/// Result of a media walk, including which directories were actually re-listed.
#[derive(Debug, Default)]
pub struct WalkOutcome {
    pub files: Vec<MediaFile>,
    /// Directories whose contents were readdir'd this pass (cold, or mtime moved).
    /// Sidecar rediscovery is only needed for media whose parent is in this set.
    pub relisted_dirs: HashSet<PathBuf>,
}

/// Walk `root`, following directories but not symlink loops. Permission errors are skipped.
///
/// When `cache` is provided, directories whose mtime matches the previous walk are not
/// re-listed: their prior file list and child set are reused. That is the cheap
/// poll path (ADR-0013). Immediate-parent mtime updates when a file is added;
/// ancestors need not.
pub fn walk_media_files_cached(
    root: &Path,
    mut cache: Option<&mut WalkCache>,
) -> Result<WalkOutcome, String> {
    let mut out = Vec::new();
    let mut relisted_dirs = HashSet::new();
    let mut stack = vec![root.to_path_buf()];
    let mut seen = HashSet::new();
    let mut next_dirs: HashMap<PathBuf, CachedDir> = HashMap::new();

    while let Some(dir) = stack.pop() {
        let canon = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if !seen.insert(canon) {
            continue;
        }
        let meta = match fs::metadata(&dir) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(path = %dir.display(), error = %e, "skip unreadable directory");
                continue;
            }
        };
        let mtime_ms = mtime_ms_from(&meta);

        if let Some(cache) = cache.as_mut()
            && let Some(prev) = cache.dirs.get(&dir)
            && prev.mtime_ms == mtime_ms
        {
            out.extend(prev.files.iter().cloned());
            stack.extend(prev.children.iter().cloned());
            next_dirs.insert(dir.clone(), prev.clone());
            continue;
        }

        relisted_dirs.insert(dir.clone());
        let mut files = Vec::new();
        let mut children = Vec::new();
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(path = %dir.display(), error = %e, "skip unreadable directory");
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "skip unreadable entry");
                    continue;
                }
            };
            let path = entry.path();
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "skip unreadable metadata");
                    continue;
                }
            };
            if meta.is_dir() {
                children.push(path.clone());
                stack.push(path);
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
        out.extend(files.iter().cloned());
        next_dirs.insert(
            dir,
            CachedDir {
                mtime_ms,
                files,
                children,
            },
        );
    }

    if let Some(cache) = cache.as_mut() {
        cache.dirs = next_dirs;
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(WalkOutcome {
        files: out,
        relisted_dirs,
    })
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
        let outcome = walk_media_files_cached(dir.path(), None).unwrap();
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
        let first = walk_media_files_cached(root.path(), Some(&mut cache)).unwrap();
        assert_eq!(first.files.len(), 1);
        assert!(first.relisted_dirs.contains(&nested));

        // Unchanged tree: same files, cache hit path — no readdir.
        let second = walk_media_files_cached(root.path(), Some(&mut cache)).unwrap();
        assert_eq!(second.files.len(), 1);
        assert!(second.relisted_dirs.is_empty());

        // Nested add updates only the immediate parent mtime; ancestors may not.
        thread::sleep(Duration::from_millis(1100));
        File::create(nested.join("two.mkv")).unwrap();
        let third = walk_media_files_cached(root.path(), Some(&mut cache)).unwrap();
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
        let _ = walk_media_files_cached(root.path(), Some(&mut cache)).unwrap();

        thread::sleep(Duration::from_millis(1100));
        File::create(movie.join("Movie.en.srt")).unwrap();
        let after = walk_media_files_cached(root.path(), Some(&mut cache)).unwrap();
        assert_eq!(after.files.len(), 1);
        assert!(
            after.relisted_dirs.contains(&movie),
            "new sidecar bumps parent mtime so rediscovery can run"
        );
    }
}
