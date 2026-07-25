//! Filesystem sidecar discovery beside a video file (ADR-0010).

use super::lang::normalize_language;
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

/// Discover subtitle sidecars for `video_path` in its directory and in
/// `Subs/` / `Subtitles/` siblings. Does not create media items.
pub fn discover_sidecars(video_path: &Path) -> Result<Vec<DiscoveredSidecar>, String> {
    let parent = video_path
        .parent()
        .ok_or_else(|| format!("video has no parent: {}", video_path.display()))?;
    let stem = video_path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("video stem not utf-8: {}", video_path.display()))?;

    let mut out = Vec::new();
    scan_dir(parent, stem, None, &mut out)?;
    for sub in SUB_DIRS {
        let dir = parent.join(sub);
        if dir.is_dir() {
            scan_dir(&dir, stem, Some(sub), &mut out)?;
        }
    }
    out.sort_by(|a, b| a.track_id.cmp(&b.track_id));
    Ok(out)
}

fn scan_dir(
    dir: &Path,
    video_stem: &str,
    dir_token: Option<&str>,
    out: &mut Vec<DiscoveredSidecar>,
) -> Result<(), String> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(path = %dir.display(), error = %e, "skip unreadable subtitle dir");
            return Ok(());
        }
    };
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
        let Some(file_stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(parsed) = parse_sidecar_stem(video_stem, file_stem, dir_token) else {
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
        out.push(DiscoveredSidecar {
            track_id: parsed.track_id,
            path,
            format: ext,
            language: parsed.language,
            forced: parsed.forced,
            sdh: parsed.sdh,
            mtime_ms,
            size_bytes: meta.len() as i64,
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
}
