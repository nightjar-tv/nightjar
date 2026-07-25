//! Text subtitle → WebVTT sidecars (ADR-0010).

use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Codecs we can convert to WebVTT without burn-in.
const TEXT_SUB_CODECS: &[&str] = &["subrip", "srt", "webvtt", "mov_text", "text"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSubtitleStream {
    /// Absolute ffprobe stream index (`-map 0:N`).
    pub stream_index: u32,
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
}

pub fn is_text_subtitle_codec(codec: &str) -> bool {
    let c = codec.to_ascii_lowercase();
    TEXT_SUB_CODECS.iter().any(|t| *t == c)
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
        out.push(TextSubtitleStream {
            stream_index: index,
            codec,
            language: tags.language.filter(|s| !s.is_empty()),
            title: tags.title.filter(|s| !s.is_empty()),
        });
    }
    Ok(out)
}

/// Ensures a WebVTT file exists for `stream_index` and returns its path.
pub fn ensure_webvtt(
    cache_dir: &Path,
    item_id: i64,
    mtime_ms: i64,
    size_bytes: i64,
    src: &Path,
    stream_index: u32,
) -> Result<PathBuf, String> {
    fs::create_dir_all(cache_dir)
        .map_err(|e| format!("create subs cache dir {}: {e}", cache_dir.display()))?;
    let dest = cache_dir.join(format!(
        "{item_id}-{mtime_ms}-{size_bytes}-{stream_index}.vtt"
    ));
    if dest.exists() && fs::metadata(&dest).map(|m| m.len()).unwrap_or(0) > 0 {
        return Ok(dest);
    }

    let streams = list_text_subtitles(src)?;
    if !streams.iter().any(|s| s.stream_index == stream_index) {
        return Err(format!(
            "stream {stream_index} is not a text subtitle in {}",
            src.display()
        ));
    }

    let tmp = cache_dir.join(format!(
        "{item_id}-{mtime_ms}-{size_bytes}-{stream_index}.tmp.vtt"
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
        let vtt = ensure_webvtt(dir.path(), 1, 0, 0, &corpus, idx).expect("extract");
        let body = fs::read_to_string(&vtt).unwrap();
        assert!(
            body.contains("WEBVTT") || body.starts_with("\u{feff}WEBVTT"),
            "not webvtt: {body}"
        );
        // Second call hits cache.
        let again = ensure_webvtt(dir.path(), 1, 0, 0, &corpus, idx).unwrap();
        assert_eq!(vtt, again);
    }
}
