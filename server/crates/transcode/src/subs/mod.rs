//! Text subtitle → WebVTT (ADR-0010 / ADR-0013).
//!
//! Extraction runs at scan time into derived library data under
//! `{NIGHTJAR_DATA_DIR}/subs/{itemId}/{trackId}.vtt`. Playback only reads.

mod discover;
mod lang;
mod srt;

pub use discover::{DiscoveredSidecar, discover_sidecars};
pub use lang::{container_stream_language, normalize_language};
pub use srt::{decode_subtitle_bytes, srt_to_webvtt};

use serde::Deserialize;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Codecs we can convert to WebVTT without burn-in.
const TEXT_SUB_CODECS: &[&str] = &["subrip", "srt", "webvtt", "mov_text", "text"];

/// Kill a runaway extract rather than leave ffmpeg demuxing forever on a NAS.
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(300);

/// Refuse extract when the data volume has less free space than this.
const MIN_FREE_BYTES: u64 = 256 * 1024 * 1024;

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

/// Filesystem sidecar input for a scan-time extract job.
#[derive(Debug, Clone)]
pub struct SidecarInput {
    pub track_id: String,
    pub path: PathBuf,
    pub format: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractOutcome {
    /// No serveable text tracks (embedded or sidecar).
    None,
    /// All serveable tracks written under the item directory.
    Ready,
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
        out.push(TextSubtitleStream {
            language: container_stream_language(tags.language),
            stream_index: index,
            codec,
            title: tags.title.filter(|s| !s.is_empty()),
        });
    }
    Ok(out)
}

/// Derived-library subtitle store under `{data}/subs` (ADR-0013). Not a cache.
pub struct SubsStore {
    root: PathBuf,
    /// Serialises extracts so two workers never share the same item tmp paths.
    extract_lock: Mutex<()>,
}

impl SubsStore {
    pub fn new(root: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(&root)
            .map_err(|e| format!("create subs dir {}: {e}", root.display()))?;
        Ok(Self {
            root,
            extract_lock: Mutex::new(()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn item_dir(&self, item_id: i64) -> PathBuf {
        self.root.join(item_id.to_string())
    }

    pub fn vtt_path(&self, item_id: i64, track_id: &str) -> PathBuf {
        self.item_dir(item_id).join(format!("{track_id}.vtt"))
    }

    pub fn has_vtt(&self, item_id: i64, track_id: &str) -> bool {
        let path = self.vtt_path(item_id, track_id);
        path.exists() && fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > 0
    }

    pub fn remove_item(&self, item_id: i64) -> Result<(), String> {
        let dir = self.item_dir(item_id);
        if !dir.exists() {
            return Ok(());
        }
        fs::remove_dir_all(&dir)
            .map_err(|e| format!("remove subs for item {item_id} ({}): {e}", dir.display()))
    }

    /// Delete `subs/{id}/` directories whose id is not in `keep_ids`.
    pub fn cleanup_orphans(&self, keep_ids: &[i64]) -> Result<usize, String> {
        let mut keep: std::collections::HashSet<i64> = keep_ids.iter().copied().collect();
        let mut removed = 0usize;
        let entries = match fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => {
                return Err(format!("read subs root {}: {e}", self.root.display()));
            }
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if !ft.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|s| s.parse::<i64>().ok()) else {
                continue;
            };
            if keep.remove(&id) {
                continue;
            }
            match fs::remove_dir_all(entry.path()) {
                Ok(()) => {
                    tracing::info!(item_id = id, "removed orphan subtitle directory");
                    removed += 1;
                }
                Err(e) => tracing::warn!(
                    item_id = id,
                    path = %entry.path().display(),
                    error = %e,
                    "orphan subtitle cleanup failed"
                ),
            }
        }
        Ok(removed)
    }
}

fn write_webvtt(dest: &Path, body: &str) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("create subtitle dir {}: {e}", parent.display()))?;
    }
    let tmp = dest.with_extension("tmp.vtt");
    fs::write(&tmp, body.as_bytes())
        .map_err(|e| format!("write subtitle tmp {}: {e}", tmp.display()))?;
    fs::rename(&tmp, dest).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("rename subtitle {}: {e}", dest.display())
    })?;
    Ok(())
}

fn srt_bytes_to_webvtt(bytes: &[u8]) -> String {
    srt_to_webvtt(&decode_subtitle_bytes(bytes))
}

/// `-c:s copy` for native subrip; `-c:s srt` remuxes mov_text/webvtt/text into
/// SRT packets. Never `-c:s webvtt` — that muxer was the measured bottleneck.
fn srt_encoder_for_codec(codec: &str) -> &'static str {
    match codec.to_ascii_lowercase().as_str() {
        "subrip" | "srt" => "copy",
        _ => "srt",
    }
}

/// Extract/convert every serveable track for one item (ADR-0013). One FFmpeg
/// demux fills all embedded text tracks; sidecars convert in-process.
pub fn extract_item_subtitles(
    store: &SubsStore,
    item_id: i64,
    src: &Path,
    sidecars: &[SidecarInput],
) -> Result<ExtractOutcome, String> {
    let _guard = store
        .extract_lock
        .lock()
        .map_err(|_| "subtitle extract lock poisoned".to_string())?;

    ensure_free_space(store.root())?;

    let serveable_sidecars: Vec<&SidecarInput> = sidecars
        .iter()
        .filter(|s| is_serveable_sidecar_format(&s.format))
        .collect();
    // Sidecar-only titles still extract when the container probe fails (corrupt
    // video, permission), so external .srt next to a bad file is not stranded.
    let embedded = match list_text_subtitles(src) {
        Ok(v) => v,
        Err(e) if !serveable_sidecars.is_empty() => {
            tracing::warn!(
                path = %src.display(),
                error = %e,
                "embedded subtitle probe failed; continuing with sidecars"
            );
            Vec::new()
        }
        Err(e) => return Err(e),
    };
    if embedded.is_empty() && serveable_sidecars.is_empty() {
        store.remove_item(item_id)?;
        return Ok(ExtractOutcome::None);
    }

    // Fresh directory so a prior generation cannot leave a stale track.
    store.remove_item(item_id)?;
    fs::create_dir_all(store.item_dir(item_id))
        .map_err(|e| format!("create subtitle dir for item {item_id}: {e}"))?;

    if !embedded.is_empty() {
        let refs: Vec<&TextSubtitleStream> = embedded.iter().collect();
        extract_embedded_srt_batch(store, item_id, src, &refs)?;
    }

    for s in serveable_sidecars {
        write_sidecar_webvtt(store, item_id, s)?;
    }

    Ok(ExtractOutcome::Ready)
}

/// Path of an already-extracted WebVTT, or an error if missing.
pub fn stored_webvtt(store: &SubsStore, item_id: i64, track_id: &str) -> Result<PathBuf, String> {
    let path = store.vtt_path(item_id, track_id);
    if store.has_vtt(item_id, track_id) {
        Ok(path)
    } else {
        Err(format!(
            "subtitle {track_id} for item {item_id} is not extracted yet"
        ))
    }
}

fn write_sidecar_webvtt(
    store: &SubsStore,
    item_id: i64,
    sidecar: &SidecarInput,
) -> Result<(), String> {
    let bytes = fs::read(&sidecar.path)
        .map_err(|e| format!("read sidecar subtitle {}: {e}", sidecar.path.display()))?;
    let body = if sidecar.format.eq_ignore_ascii_case("vtt") {
        let text = decode_subtitle_bytes(&bytes);
        if text.contains("WEBVTT") {
            text
        } else {
            format!("WEBVTT\n\n{text}")
        }
    } else {
        srt_bytes_to_webvtt(&bytes)
    };
    write_webvtt(&store.vtt_path(item_id, &sidecar.track_id), &body)
}

fn extract_embedded_srt_batch(
    store: &SubsStore,
    item_id: i64,
    src: &Path,
    streams: &[&TextSubtitleStream],
) -> Result<(), String> {
    let item_dir = store.item_dir(item_id);
    let mut tmp_srts: Vec<(u32, PathBuf)> = Vec::with_capacity(streams.len());
    let mut cmd = Command::new("ffmpeg");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(src);
    for s in streams {
        let tmp = item_dir.join(format!("e{}.tmp.srt", s.stream_index));
        let map = format!("0:{}", s.stream_index);
        let encoder = srt_encoder_for_codec(&s.codec);
        cmd.args(["-map", &map, "-c:s", encoder, "-f", "srt"])
            .arg(&tmp);
        tmp_srts.push((s.stream_index, tmp));
    }

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "ffmpeg not found on PATH".into()
        } else {
            format!("spawn ffmpeg for {}: {e}", src.display())
        }
    })?;
    if let Err(e) = wait_extract_child(&mut child, EXTRACT_TIMEOUT) {
        for (_, tmp) in &tmp_srts {
            let _ = fs::remove_file(tmp);
        }
        tracing::warn!(
            path = %src.display(),
            error = %e,
            "subtitle extract failed or timed out"
        );
        return Err(format!(
            "ffmpeg subtitle extract failed for {}: {e}",
            src.display()
        ));
    }

    for (stream_index, tmp_srt) in tmp_srts {
        let track_id = format!("e{stream_index}");
        let dest = store.vtt_path(item_id, &track_id);
        let result = (|| {
            let bytes = fs::read(&tmp_srt).map_err(|e| {
                format!(
                    "read extracted srt for stream {stream_index} ({}): {e}",
                    tmp_srt.display()
                )
            })?;
            let body = srt_bytes_to_webvtt(&bytes);
            write_webvtt(&dest, &body)
        })();
        let _ = fs::remove_file(&tmp_srt);
        result?;
    }
    Ok(())
}

fn wait_extract_child(child: &mut std::process::Child, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                let err = child
                    .stderr
                    .as_mut()
                    .and_then(|s| {
                        let mut buf = String::new();
                        s.read_to_string(&mut buf).ok()?;
                        Some(buf)
                    })
                    .unwrap_or_default();
                let err = err.trim();
                if err.is_empty() {
                    return Err(format!("ffmpeg exited {status}"));
                }
                return Err(format!("ffmpeg exited {status}: {err}"));
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("ffmpeg timed out after {timeout:?}"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("wait: {e}")),
        }
    }
}

fn ensure_free_space(path: &Path) -> Result<(), String> {
    let available = available_bytes(path)?;
    if available < MIN_FREE_BYTES {
        return Err(format!(
            "subtitle extract refused: data volume has {available} free bytes; need at least {MIN_FREE_BYTES}"
        ));
    }
    Ok(())
}

/// Free bytes on the volume containing `path`, via `df -k` (no new crate).
fn available_bytes(path: &Path) -> Result<u64, String> {
    let probe = if path.exists() {
        path.to_path_buf()
    } else {
        path.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    };
    let output = Command::new("df")
        .args(["-k"])
        .arg(&probe)
        .output()
        .map_err(|e| format!("df for {}: {e}", probe.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "df failed for {}: {}",
            probe.display(),
            stderr.trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Header + one data line; Available is the 4th field on both macOS and Linux.
    let line = stdout
        .lines()
        .nth(1)
        .ok_or_else(|| format!("df produced no data for {}", probe.display()))?;
    let avail_k = line
        .split_whitespace()
        .nth(3)
        .ok_or_else(|| format!("df line missing available column: {line}"))?
        .parse::<u64>()
        .map_err(|e| format!("parse df available: {e}"))?;
    Ok(avail_k.saturating_mul(1024))
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

    fn ffmpeg_available() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn skip_without_ffmpeg() -> bool {
        if ffmpeg_available() {
            return false;
        }
        if std::env::var_os("NIGHTJAR_TEST_REQUIRE_FFMPEG").is_some() {
            panic!("ffmpeg required (NIGHTJAR_TEST_REQUIRE_FFMPEG set) but not on PATH");
        }
        eprintln!("skipping: ffmpeg not on PATH");
        true
    }

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

    #[test]
    fn vtt_path_keys_on_item_and_track_not_media_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = SubsStore::new(dir.path().to_path_buf()).unwrap();
        let p = store.vtt_path(42, "e2");
        assert_eq!(p, dir.path().join("42").join("e2.vtt"));
        // Reorganising media must not change the stored path.
        assert!(!p.to_string_lossy().contains("Movies"));
    }

    #[test]
    fn extracts_srt_from_corpus_fixture() {
        if skip_without_ffmpeg() {
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
        let dir = tempfile::tempdir().unwrap();
        let store = SubsStore::new(dir.path().to_path_buf()).unwrap();
        let outcome = extract_item_subtitles(&store, 1, &corpus, &[]).expect("extract");
        assert_eq!(outcome, ExtractOutcome::Ready);
        let track_id = streams[0].track_id();
        let vtt = stored_webvtt(&store, 1, &track_id).unwrap();
        let body = fs::read_to_string(&vtt).unwrap();
        assert!(
            body.contains("WEBVTT") || body.starts_with("\u{feff}WEBVTT"),
            "not webvtt: {body}"
        );
        assert!(
            body.contains("Nightjar SRT sample"),
            "converted cue missing: {body}"
        );
    }

    #[test]
    fn srt_encoder_copies_subrip_encodes_others() {
        assert_eq!(srt_encoder_for_codec("subrip"), "copy");
        assert_eq!(srt_encoder_for_codec("SRT"), "copy");
        assert_eq!(srt_encoder_for_codec("mov_text"), "srt");
        assert_eq!(srt_encoder_for_codec("webvtt"), "srt");
        assert_eq!(srt_encoder_for_codec("text"), "srt");
    }

    #[test]
    fn one_pass_fills_all_missing_tracks() {
        if skip_without_ffmpeg() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let srt_a = dir.path().join("a.srt");
        let srt_b = dir.path().join("b.srt");
        fs::write(&srt_a, "1\n00:00:00,000 --> 00:00:01,000\nTrack A\n").unwrap();
        fs::write(&srt_b, "1\n00:00:00,000 --> 00:00:01,000\nTrack B\n").unwrap();
        let mkv = dir.path().join("two_subs.mkv");
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=64x64:d=1",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=48000:cl=stereo:d=1",
                "-i",
            ])
            .arg(&srt_a)
            .arg("-i")
            .arg(&srt_b)
            .args([
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-c:s",
                "srt",
                "-map",
                "0:v:0",
                "-map",
                "1:a:0",
                "-map",
                "2:0",
                "-map",
                "3:0",
                "-shortest",
            ])
            .arg(&mkv)
            .status();
        let Ok(status) = status else {
            eprintln!("skipping: could not spawn ffmpeg");
            return;
        };
        if !status.success() {
            eprintln!("skipping: ffmpeg multi-sub mux failed");
            return;
        }
        let streams = list_text_subtitles(&mkv).expect("list");
        assert!(
            streams.len() >= 2,
            "expected two text subs, got {streams:?}"
        );
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        extract_item_subtitles(&store, 9, &mkv, &[]).expect("extract");
        assert!(store.has_vtt(9, &streams[0].track_id()));
        assert!(store.has_vtt(9, &streams[1].track_id()));
        let a = fs::read_to_string(store.vtt_path(9, &streams[0].track_id())).unwrap();
        let b = fs::read_to_string(store.vtt_path(9, &streams[1].track_id())).unwrap();
        assert!(a.contains("Track A") || b.contains("Track A"));
        assert!(a.contains("Track B") || b.contains("Track B"));
    }

    #[test]
    fn sidecar_addition_does_not_shadow_embedded_store_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        let item_id = 7i64;
        fs::create_dir_all(store.item_dir(item_id)).unwrap();
        write_webvtt(
            &store.vtt_path(item_id, "e2"),
            "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nEmb\n",
        )
        .unwrap();
        let embedded_before = fs::read_to_string(store.vtt_path(item_id, "e2")).unwrap();

        let srt_path = dir.path().join("Movie.en.srt");
        fs::write(
            &srt_path,
            "1\n00:00:00,000 --> 00:00:01,000\nSidecar hello\n",
        )
        .unwrap();
        // Simulate only writing the new sidecar track into an existing item dir
        // the way a mistaken ordinal scheme would collide; our namespaces must not.
        write_sidecar_webvtt(
            &store,
            item_id,
            &SidecarInput {
                track_id: "s-en".into(),
                path: srt_path,
                format: "srt".into(),
            },
        )
        .unwrap();

        assert!(store.has_vtt(item_id, "e2"));
        assert!(store.has_vtt(item_id, "s-en"));
        assert_eq!(
            fs::read_to_string(store.vtt_path(item_id, "e2")).unwrap(),
            embedded_before,
            "adding a sidecar must not renumber or overwrite embedded e2"
        );
        let side = fs::read_to_string(store.vtt_path(item_id, "s-en")).unwrap();
        assert!(side.contains("Sidecar hello"));
    }

    #[test]
    fn converts_sidecar_srt() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("Movie.mp4");
        fs::write(&video, b"not a real mp4").unwrap();
        let srt_path = dir.path().join("Movie.en.srt");
        fs::write(
            &srt_path,
            "1\n00:00:00,000 --> 00:00:01,000\nSidecar hello\n",
        )
        .unwrap();
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        let outcome = extract_item_subtitles(
            &store,
            7,
            &video,
            &[SidecarInput {
                track_id: "s-en".into(),
                path: srt_path,
                format: "srt".into(),
            }],
        )
        .expect("sidecar-only extract");
        assert_eq!(outcome, ExtractOutcome::Ready);
        let body = fs::read_to_string(store.vtt_path(7, "s-en")).unwrap();
        assert!(body.contains("WEBVTT"));
        assert!(body.contains("Sidecar hello"));
    }

    #[test]
    fn cleanup_removes_orphan_item_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let store = SubsStore::new(dir.path().to_path_buf()).unwrap();
        fs::create_dir_all(store.item_dir(1)).unwrap();
        fs::create_dir_all(store.item_dir(2)).unwrap();
        write_webvtt(&store.vtt_path(1, "e2"), "WEBVTT\n").unwrap();
        write_webvtt(&store.vtt_path(2, "e2"), "WEBVTT\n").unwrap();
        let n = store.cleanup_orphans(&[1]).unwrap();
        assert_eq!(n, 1);
        assert!(store.item_dir(1).exists());
        assert!(!store.item_dir(2).exists());
    }

    #[test]
    fn invalidation_rewrites_on_fresh_extract() {
        if skip_without_ffmpeg() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files/h264_aac_srt_mkv.mkv");
        if !corpus.exists() {
            eprintln!("skipping: missing {}", corpus.display());
            return;
        }
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        extract_item_subtitles(&store, 3, &corpus, &[]).unwrap();
        let streams = list_text_subtitles(&corpus).unwrap();
        let track = streams[0].track_id();
        let first = fs::read_to_string(store.vtt_path(3, &track)).unwrap();
        // Stale marker file that must disappear when we re-extract into a fresh dir.
        write_webvtt(&store.vtt_path(3, "e999"), "WEBVTT\n\nstale\n").unwrap();
        assert!(store.has_vtt(3, "e999"));
        extract_item_subtitles(&store, 3, &corpus, &[]).unwrap();
        assert!(
            !store.has_vtt(3, "e999"),
            "re-extract must clear prior generation"
        );
        let second = fs::read_to_string(store.vtt_path(3, &track)).unwrap();
        assert!(second.contains("WEBVTT"));
        assert_eq!(first.lines().next(), second.lines().next());
    }
}
