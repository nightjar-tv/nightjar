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

/// Walk `root`, following directories but not symlink loops. Permission errors are skipped.
pub fn walk_media_files(root: &Path) -> Result<Vec<MediaFile>, String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut seen = std::collections::HashSet::new();

    while let Some(dir) = stack.pop() {
        let canon = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if !seen.insert(canon) {
            continue;
        }
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
                // Do not follow symlinked dirs into cycles; canonicalize+seen handles loops.
                stack.push(path);
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            if !is_media(&path) {
                continue;
            }
            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            out.push(MediaFile {
                path,
                mtime_ms,
                size_bytes: meta.len() as i64,
            });
        }
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
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
        let files = walk_media_files(dir.path()).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| {
            let ext = f.path.extension().and_then(|e| e.to_str()).unwrap_or("");
            !matches!(
                ext.to_ascii_lowercase().as_str(),
                "srt" | "vtt" | "ass" | "ssa"
            )
        }));
    }
}
