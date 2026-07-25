//! Text subtitle → WebVTT (ADR-0010): embedded extract + sidecar convert.

mod discover;
mod lang;
mod srt;

pub use discover::{DiscoveredSidecar, discover_sidecars};
pub use lang::normalize_language;
pub use srt::{decode_subtitle_bytes, srt_to_webvtt};

use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

/// Codecs we can convert to WebVTT without burn-in.
const TEXT_SUB_CODECS: &[&str] = &["subrip", "srt", "webvtt", "mov_text", "text"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleSourceKind {
    Embedded,
    Sidecar,
}

impl SubtitleSourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Embedded => "embedded",
            Self::Sidecar => "sidecar",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSubtitleStream {
    /// Absolute ffprobe stream index (`-map 0:N`).
    pub stream_index: u32,
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
}

impl TextSubtitleStream {
    pub fn track_id(&self) -> String {
        format!("e{}", self.stream_index)
    }
}

pub fn is_text_subtitle_codec(codec: &str) -> bool {
    let c = codec.to_ascii_lowercase();
    TEXT_SUB_CODECS.iter().any(|t| *t == c)
}

pub fn is_serveable_sidecar_format(format: &str) -> bool {
    matches!(format.to_ascii_lowercase().as_str(), "srt" | "vtt")
}

/// Lists text subtitle streams in `src`. Image/ASS tracks are skipped.
pub fn list_text_subtitles(src: &Path) -> Result<Vec<TextSubtitleStream>, String> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_streams",
            "-select_streams",
            "s",
        ])
        .arg(src)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "ffprobe not found on PATH".into()
            } else {
                format!("spawn ffprobe for {}: {e}", src.display())
            }
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "ffprobe failed for {}: {}",
            src.display(),
            stderr.trim()
        ));
    }
    let parsed: FfprobeSubs = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("parse ffprobe json for {}: {e}", src.display()))?;
    let mut out = Vec::new();
    for stream in parsed.streams.unwrap_or_default() {
        let codec = stream.codec_name.unwrap_or_default();
        if !is_text_subtitle_codec(&codec) {
            continue;
        }
        let Some(index) = stream.index else {
            continue;
        };
        let tags = stream.tags.unwrap_or_default();
        let language = tags
            .language
            .filter(|s| !s.is_empty())
            .and_then(|l| normalize_language(&l).or(Some(l.to_ascii_lowercase())));
        out.push(TextSubtitleStream {
            stream_index: index,
            codec,
            language,
            title: tags.title.filter(|s| !s.is_empty()),
        });
    }
    Ok(out)
}

/// Byte-capped WebVTT cache under `{data}/cache/subs` (ADR-0010).
pub struct SubsCache {
    cache_dir: PathBuf,
    cap_bytes: u64,
}

impl SubsCache {
    pub fn new(cache_dir: PathBuf, cap_bytes: u64) -> Result<Self, String> {
        fs::create_dir_all(&cache_dir)
            .map_err(|e| format!("create subs cache dir {}: {e}", cache_dir.display()))?;
        Ok(Self {
            cache_dir,
            cap_bytes,
        })
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    fn make_room(&self, needed: u64) -> Result<(), String> {
        if needed > self.cap_bytes {
            return Err(format!(
                "subtitle output is {needed} bytes but the subs cache cap ({} bytes) cannot hold it; raise NIGHTJAR_SUBS_CACHE_BYTES",
                self.cap_bytes
            ));
        }
        let mut evictable = Vec::new();
        let mut locked_bytes = 0u64;
        for entry in fs::read_dir(&self.cache_dir)
            .map_err(|e| format!("read subs cache dir {}: {e}", self.cache_dir.display()))?
            .flatten()
        {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".tmp.vtt") {
                locked_bytes += meta.len();
                continue;
            }
            if !name.ends_with(".vtt") {
                continue;
            }
            evictable.push(CacheFile {
                name,
                size: meta.len(),
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            });
        }
        let victims = select_evictions(evictable, locked_bytes, needed, self.cap_bytes)?;
        for name in victims {
            let path = self.cache_dir.join(&name);
            match fs::remove_file(&path) {
                Ok(()) => tracing::info!(path = %path.display(), "evicted subtitle cache file"),
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "subtitle eviction failed")
                }
            }
        }
        Ok(())
    }

    fn touch(&self, path: &Path) {
        let touched = fs::File::options()
            .append(true)
            .open(path)
            .and_then(|f| f.set_modified(SystemTime::now()));
        if let Err(e) = touched {
            tracing::warn!(path = %path.display(), error = %e, "subtitle cache touch failed");
        }
    }
}

struct CacheFile {
    name: String,
    size: u64,
    modified: SystemTime,
}

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
            "subs cache cap ({cap} bytes) cannot fit {needed} more bytes alongside {locked_bytes} bytes of in-flight output"
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

/// Ensures a WebVTT file exists for an embedded stream and returns its path.
pub fn ensure_embedded_webvtt(
    cache: &SubsCache,
    item_id: i64,
    mtime_ms: i64,
    size_bytes: i64,
    src: &Path,
    stream_index: u32,
) -> Result<PathBuf, String> {
    let track_id = format!("e{stream_index}");
    let dest = cache
        .cache_dir
        .join(format!("{item_id}-{mtime_ms}-{size_bytes}-{track_id}.vtt"));
    if dest.exists() && fs::metadata(&dest).map(|m| m.len()).unwrap_or(0) > 0 {
        cache.touch(&dest);
        return Ok(dest);
    }

    let streams = list_text_subtitles(src)?;
    if !streams.iter().any(|s| s.stream_index == stream_index) {
        return Err(format!(
            "stream {stream_index} is not a text subtitle in {}",
            src.display()
        ));
    }

    let tmp = cache.cache_dir.join(format!(
        "{item_id}-{mtime_ms}-{size_bytes}-{track_id}.tmp.vtt"
    ));
    let map = format!("0:{stream_index}");
    let output = Command::new("ffmpeg")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(src)
        .args(["-map", &map, "-c:s", "webvtt", "-f", "webvtt"])
        .arg(&tmp)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "ffmpeg not found on PATH".into()
            } else {
                format!("spawn ffmpeg for {}: {e}", src.display())
            }
        })?;
    if !output.status.success() {
        let _ = fs::remove_file(&tmp);
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "ffmpeg subtitle extract failed for {} stream {stream_index}: {} ({})",
            src.display(),
            output.status,
            err.trim()
        ));
    }
    let produced = fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
    cache.make_room(produced)?;
    fs::rename(&tmp, &dest).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("rename subtitle cache {}: {e}", dest.display())
    })?;
    Ok(dest)
}

/// Ensures a WebVTT file exists for a filesystem sidecar and returns its path.
pub fn ensure_sidecar_webvtt(
    cache: &SubsCache,
    item_id: i64,
    track_id: &str,
    sidecar_path: &Path,
    format: &str,
    mtime_ms: i64,
    size_bytes: i64,
) -> Result<PathBuf, String> {
    if !is_serveable_sidecar_format(format) {
        return Err(format!(
            "sidecar format {format} is not served as WebVTT ({})",
            sidecar_path.display()
        ));
    }
    let dest = cache
        .cache_dir
        .join(format!("{item_id}-{mtime_ms}-{size_bytes}-{track_id}.vtt"));
    if dest.exists() && fs::metadata(&dest).map(|m| m.len()).unwrap_or(0) > 0 {
        cache.touch(&dest);
        return Ok(dest);
    }

    let bytes = fs::read(sidecar_path)
        .map_err(|e| format!("read sidecar subtitle {}: {e}", sidecar_path.display()))?;
    let body = if format.eq_ignore_ascii_case("vtt") {
        let text = decode_subtitle_bytes(&bytes);
        if text.contains("WEBVTT") {
            text
        } else {
            format!("WEBVTT\n\n{text}")
        }
    } else {
        let text = decode_subtitle_bytes(&bytes);
        srt_to_webvtt(&text)
    };

    let tmp = cache.cache_dir.join(format!(
        "{item_id}-{mtime_ms}-{size_bytes}-{track_id}.tmp.vtt"
    ));
    let out_bytes = body.into_bytes();
    cache.make_room(out_bytes.len() as u64)?;
    fs::write(&tmp, &out_bytes)
        .map_err(|e| format!("write subtitle tmp {}: {e}", tmp.display()))?;
    fs::rename(&tmp, &dest).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("rename subtitle cache {}: {e}", dest.display())
    })?;
    Ok(dest)
}

#[derive(Debug, Deserialize)]
struct FfprobeSubs {
    streams: Option<Vec<FfSubStream>>,
}

#[derive(Debug, Deserialize)]
struct FfSubStream {
    index: Option<u32>,
    codec_name: Option<String>,
    tags: Option<FfTags>,
}

#[derive(Debug, Default, Deserialize)]
struct FfTags {
    language: Option<String>,
    title: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_codec_allowlist() {
        assert!(is_text_subtitle_codec("subrip"));
        assert!(is_text_subtitle_codec("SRT"));
        assert!(is_text_subtitle_codec("mov_text"));
        assert!(!is_text_subtitle_codec("ass"));
        assert!(!is_text_subtitle_codec("hdmv_pgs_subtitle"));
    }

    #[test]
    fn embedded_track_id() {
        let s = TextSubtitleStream {
            stream_index: 2,
            codec: "subrip".into(),
            language: Some("en".into()),
            title: None,
        };
        assert_eq!(s.track_id(), "e2");
    }

    fn ffmpeg_available() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn extracts_srt_from_corpus_fixture() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files/h264_aac_srt_mkv.mkv");
        if !corpus.exists() {
            eprintln!("skipping: missing {}", corpus.display());
            return;
        }
        let streams = list_text_subtitles(&corpus).expect("list");
        assert!(
            !streams.is_empty(),
            "expected at least one text sub on SRT fixture"
        );
        let idx = streams[0].stream_index;
        let dir = tempfile::tempdir().unwrap();
        let cache = SubsCache::new(dir.path().to_path_buf(), 64 * 1024 * 1024).unwrap();
        let vtt = ensure_embedded_webvtt(&cache, 1, 0, 0, &corpus, idx).expect("extract");
        let body = fs::read_to_string(&vtt).unwrap();
        assert!(
            body.contains("WEBVTT") || body.starts_with("\u{feff}WEBVTT"),
            "not webvtt: {body}"
        );
        let again = ensure_embedded_webvtt(&cache, 1, 0, 0, &corpus, idx).unwrap();
        assert_eq!(vtt, again);
    }

    #[test]
    fn converts_sidecar_srt_with_cache() {
        let dir = tempfile::tempdir().unwrap();
        let srt_path = dir.path().join("Movie.en.srt");
        fs::write(
            &srt_path,
            "1\n00:00:00,000 --> 00:00:01,000\nSidecar hello\n",
        )
        .unwrap();
        let cache_dir = dir.path().join("cache");
        let cache = SubsCache::new(cache_dir, 1024 * 1024).unwrap();
        let vtt = ensure_sidecar_webvtt(&cache, 7, "s-en", &srt_path, "srt", 1, 42).unwrap();
        let body = fs::read_to_string(&vtt).unwrap();
        assert!(body.contains("WEBVTT"));
        assert!(body.contains("Sidecar hello"));
        let again = ensure_sidecar_webvtt(&cache, 7, "s-en", &srt_path, "srt", 1, 42).unwrap();
        assert_eq!(vtt, again);
    }

    #[test]
    fn eviction_frees_oldest() {
        let victims = select_evictions(
            vec![
                CacheFile {
                    name: "old.vtt".into(),
                    size: 40,
                    modified: SystemTime::UNIX_EPOCH,
                },
                CacheFile {
                    name: "new.vtt".into(),
                    size: 40,
                    modified: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10),
                },
            ],
            0,
            50,
            100,
        )
        .unwrap();
        assert_eq!(victims, vec!["old.vtt"]);
    }
}
