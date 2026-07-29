//! Filesystem sidecar discovery beside a video file (ADR-0010).

use super::lang::normalize_language;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const SIDE_EXTS: &[&str] = &["srt", "vtt", "ass", "ssa"];
const SUB_DIRS: &[&str] = &["Subs", "Subtitles"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredSidecar {
    /// Full track id including `s` / `s-…` prefix.
    pub track_id: String,
    pub path: PathBuf,
    pub format: String,
    pub language: Option<String>,
    pub forced: bool,
    pub sdh: bool,
    pub mtime_ms: i64,
    pub size_bytes: i64,
}

/// Memoizes subtitle-extension listings per directory for one scan job.
///
/// Cold index of a flat library calls discovery once per media file. Without
/// this cache each call re-reads the parent (O(n²) on a 10k-file folder —
/// Gate 1 bench_10k hangs for minutes).
#[derive(Default)]
pub struct SidecarDirCache {
    dirs: HashMap<PathBuf, Vec<CachedSidecarFile>>,
    is_dir: HashMap<PathBuf, bool>,
}

#[derive(Debug, Clone)]
struct CachedSidecarFile {
    path: PathBuf,
    file_stem: String,
    format: String,
    mtime_ms: i64,
    size_bytes: i64,
}

/// Discover subtitle sidecars for `video_path` in its directory and in
/// `Subs/` / `Subtitles/` siblings. Does not create media items.
pub fn discover_sidecars(video_path: &Path) -> Result<Vec<DiscoveredSidecar>, String> {
    discover_sidecars_cached(video_path, None)
}

/// Same as [`discover_sidecars`], reusing directory listings in `cache`.
pub fn discover_sidecars_cached(
    video_path: &Path,
    mut cache: Option<&mut SidecarDirCache>,
) -> Result<Vec<DiscoveredSidecar>, String> {
    let parent = video_path
        .parent()
        .ok_or_else(|| format!("video has no parent: {}", video_path.display()))?;
    let stem = video_path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("video stem not utf-8: {}", video_path.display()))?;

    let mut out = Vec::new();
    scan_dir(parent, stem, None, &mut out, cache.as_deref_mut())?;
    for sub in SUB_DIRS {
        let dir = parent.join(sub);
        if cached_is_dir(&dir, cache.as_deref_mut()) {
            scan_dir(&dir, stem, Some(sub), &mut out, cache.as_deref_mut())?;
        }
    }
    // Movie.en.srt and Movie.en.vtt share track_id s-en (the id carries no
    // extension). One must win deterministically or the DB primary key on
    // (item, track_id) rejects the whole association. VTT wins: it serves
    // as-is.
    out.sort_by(|a, b| {
        a.track_id
            .cmp(&b.track_id)
            .then(format_rank(&a.format).cmp(&format_rank(&b.format)))
    });
    out.dedup_by(|loser, winner| {
        let dup = loser.track_id == winner.track_id;
        if dup {
            tracing::warn!(
                track_id = %winner.track_id,
                kept = %winner.path.display(),
                skipped = %loser.path.display(),
                "duplicate sidecar track id; keeping the servable format"
            );
        }
        dup
    });
    Ok(out)
}

fn format_rank(format: &str) -> u8 {
    match format {
        "vtt" => 0,
        "srt" => 1,
        "ass" => 2,
        _ => 3,
    }
}

fn cached_is_dir(dir: &Path, cache: Option<&mut SidecarDirCache>) -> bool {
    if let Some(cache) = cache {
        if let Some(v) = cache.is_dir.get(dir) {
            return *v;
        }
        let v = dir.is_dir();
        cache.is_dir.insert(dir.to_path_buf(), v);
        v
    } else {
        dir.is_dir()
    }
}

fn list_sidecar_files(
    dir: &Path,
    cache: Option<&mut SidecarDirCache>,
) -> Result<Vec<CachedSidecarFile>, String> {
    if let Some(cache) = cache.as_ref()
        && let Some(cached) = cache.dirs.get(dir)
    {
        return Ok(cached.clone());
    }
    let listed = read_sidecar_files(dir)?;
    if let Some(cache) = cache {
        cache.dirs.insert(dir.to_path_buf(), listed.clone());
    }
    Ok(listed)
}

fn read_sidecar_files(dir: &Path) -> Result<Vec<CachedSidecarFile>, String> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(path = %dir.display(), error = %e, "skip unreadable subtitle dir");
            return Ok(Vec::new());
        }
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(ext) = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
        else {
            continue;
        };
        if !SIDE_EXTS.iter().any(|x| *x == ext) {
            continue;
        }
        let Some(file_stem) = path
            .file_stem()
            .and_then(|s| s.to_str().map(|s| s.to_string()))
        else {
            continue;
        };
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "skip sidecar metadata");
                continue;
            }
        };
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        out.push(CachedSidecarFile {
            path,
            file_stem,
            format: ext,
            mtime_ms,
            size_bytes: meta.len() as i64,
        });
    }
    Ok(out)
}

fn scan_dir(
    dir: &Path,
    video_stem: &str,
    dir_token: Option<&str>,
    out: &mut Vec<DiscoveredSidecar>,
    cache: Option<&mut SidecarDirCache>,
) -> Result<(), String> {
    let listed = list_sidecar_files(dir, cache)?;
    for entry in listed {
        let Some(parsed) = parse_sidecar_stem(video_stem, &entry.file_stem, dir_token) else {
            continue;
        };
        out.push(DiscoveredSidecar {
            track_id: parsed.track_id,
            path: entry.path,
            format: entry.format,
            language: parsed.language,
            forced: parsed.forced,
            sdh: parsed.sdh,
            mtime_ms: entry.mtime_ms,
            size_bytes: entry.size_bytes,
        });
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedStem {
    track_id: String,
    language: Option<String>,
    forced: bool,
    sdh: bool,
}

fn parse_sidecar_stem(
    video_stem: &str,
    file_stem: &str,
    dir_token: Option<&str>,
) -> Option<ParsedStem> {
    if !file_stem.starts_with(video_stem) {
        return None;
    }
    let rest = &file_stem[video_stem.len()..];
    if !rest.is_empty() && !rest.starts_with('.') {
        return None;
    }
    let suffix = rest.trim_start_matches('.');
    let mut forced = false;
    let mut sdh = false;
    let mut lang_tokens = Vec::new();
    if !suffix.is_empty() {
        for tok in suffix.split('.') {
            let lower = tok.to_ascii_lowercase();
            if lower == "forced" {
                forced = true;
            } else if lower == "sdh" {
                sdh = true;
            } else {
                lang_tokens.push(tok.to_string());
            }
        }
    }
    let language = if lang_tokens.len() == 1 {
        normalize_language(&lang_tokens[0])
    } else {
        None
    };

    let mut id_parts: Vec<String> = Vec::new();
    if let Some(d) = dir_token {
        id_parts.push(d.to_string());
    }
    if !suffix.is_empty() {
        id_parts.push(suffix.to_string());
    }
    let track_id = if id_parts.is_empty() {
        "s".to_string()
    } else {
        format!("s-{}", id_parts.join("."))
    };

    Some(ParsedStem {
        track_id,
        language,
        forced,
        sdh,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;

    #[test]
    fn parse_examples() {
        assert_eq!(
            parse_sidecar_stem("Movie", "Movie", None).unwrap().track_id,
            "s"
        );
        assert_eq!(
            parse_sidecar_stem("Movie", "Movie.en", None)
                .unwrap()
                .track_id,
            "s-en"
        );
        assert_eq!(
            parse_sidecar_stem("Movie", "Movie.eng", None)
                .unwrap()
                .language
                .as_deref(),
            Some("en")
        );
        let forced = parse_sidecar_stem("Movie", "Movie.en.forced", None).unwrap();
        assert_eq!(forced.track_id, "s-en.forced");
        assert!(forced.forced);
        let sdh = parse_sidecar_stem("Movie", "Movie.en.sdh", None).unwrap();
        assert!(sdh.sdh);
        let subs = parse_sidecar_stem("Movie", "Movie.en", Some("Subs")).unwrap();
        assert_eq!(subs.track_id, "s-Subs.en");
        let unk = parse_sidecar_stem("Movie", "Movie.xx", None).unwrap();
        assert!(unk.language.is_none());
        assert_eq!(unk.track_id, "s-xx");
    }

    #[test]
    fn same_suffix_different_extension_dedupes_to_one_track() {
        let dir = tempdir().unwrap();
        let video = dir.path().join("Movie.mkv");
        File::create(&video).unwrap();
        File::create(dir.path().join("Movie.en.srt")).unwrap();
        File::create(dir.path().join("Movie.en.vtt")).unwrap();

        let found = discover_sidecars(&video).unwrap();
        let en: Vec<_> = found.iter().filter(|s| s.track_id == "s-en").collect();
        assert_eq!(
            en.len(),
            1,
            "duplicate track_id would break the PK: {found:?}"
        );
        assert_eq!(en[0].format, "vtt", "vtt serves as-is and must win");
    }

    #[test]
    fn normalisation_shares_language_but_not_identity() {
        let dir = tempdir().unwrap();
        let video = dir.path().join("Movie.mkv");
        File::create(&video).unwrap();
        File::create(dir.path().join("Movie.en.srt")).unwrap();
        File::create(dir.path().join("Movie.eng.srt")).unwrap();

        let found = discover_sidecars(&video).unwrap();
        let ids: Vec<_> = found.iter().map(|s| s.track_id.as_str()).collect();
        assert_eq!(ids, vec!["s-en", "s-eng"]);
        assert!(found.iter().all(|s| s.language.as_deref() == Some("en")));
    }

    #[test]
    fn discovers_beside_and_subs_dir() {
        let dir = tempdir().unwrap();
        let video = dir.path().join("Movie.mkv");
        File::create(&video).unwrap();
        File::create(dir.path().join("Movie.en.srt")).unwrap();
        File::create(dir.path().join("notes.txt")).unwrap();
        fs::create_dir(dir.path().join("Subs")).unwrap();
        File::create(dir.path().join("Subs").join("Movie.en.srt")).unwrap();
        File::create(dir.path().join("Other.srt")).unwrap();

        let found = discover_sidecars(&video).unwrap();
        let ids: Vec<_> = found.iter().map(|s| s.track_id.as_str()).collect();
        assert!(ids.contains(&"s-en"), "{ids:?}");
        assert!(ids.contains(&"s-Subs.en"), "{ids:?}");
        assert!(!ids.iter().any(|id| id.contains("Other")));
    }

    #[test]
    fn dir_cache_lists_each_parent_once() {
        let dir = tempdir().unwrap();
        // Flat folder like Gate 1 bench_10k: many videos, few sidecars.
        for i in 0..200 {
            File::create(dir.path().join(format!("item_{i:05}.mp4"))).unwrap();
        }
        File::create(dir.path().join("item_00001.en.srt")).unwrap();

        let mut cache = SidecarDirCache::default();
        for i in 0..200 {
            let video = dir.path().join(format!("item_{i:05}.mp4"));
            let found = discover_sidecars_cached(&video, Some(&mut cache)).unwrap();
            if i == 1 {
                assert_eq!(found.len(), 1);
                assert_eq!(found[0].track_id, "s-en");
            } else {
                assert!(found.is_empty(), "i={i} found={found:?}");
            }
        }
        assert_eq!(
            cache.dirs.len(),
            1,
            "parent dir must be listed once, not once per video"
        );
        // Subs/Subtitles probes are also cached (false here).
        assert_eq!(cache.is_dir.len(), 2);
    }
}
