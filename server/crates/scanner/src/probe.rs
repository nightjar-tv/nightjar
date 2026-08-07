use serde::Deserialize;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

#[derive(Debug, Default, Clone)]
pub struct ProbeResult {
    pub duration_ms: Option<i64>,
    pub container: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    /// Channel count of the first audio stream (ADR-0012 channel ceiling).
    pub audio_channels: Option<i64>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    /// Video stream bitrate when ffprobe reports it (ADR-0022).
    pub video_bitrate_bps: Option<i64>,
    /// `none` | `hdr10` | `dolby_vision` | `dolby_vision_p5` (ADR-0022).
    /// Profile 5 is distinct: IPT-PQ has no zscale tonemap path.
    pub hdr: Option<String>,
    /// Subtitle streams the container carries (ADR-0041 Decision 1). The
    /// parser no longer drops them; the probe persists one row per stream.
    pub subtitle_streams: Vec<ProbeSubtitleStream>,
}

/// One ffprobe-reported subtitle stream (ADR-0041 Decision 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeSubtitleStream {
    /// Absolute stream index (`index` from ffprobe).
    pub stream_index: u32,
    /// `codec_name`; `unknown` when ffprobe reports an unmapped codec.
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub forced: bool,
    pub sdh: bool,
}

#[derive(Debug, Deserialize)]
struct FfprobeJson {
    format: Option<FfFormat>,
    streams: Option<Vec<FfStream>>,
}

#[derive(Debug, Deserialize)]
struct FfFormat {
    format_name: Option<String>,
    duration: Option<String>,
    bit_rate: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FfStream {
    index: Option<u32>,
    codec_type: Option<String>,
    codec_name: Option<String>,
    channels: Option<i64>,
    width: Option<i32>,
    height: Option<i32>,
    bit_rate: Option<String>,
    color_transfer: Option<String>,
    #[serde(default)]
    tags: Option<FfTags>,
    #[serde(default)]
    disposition: Option<FfDisposition>,
    #[serde(default)]
    side_data_list: Vec<FfSideData>,
}

#[derive(Debug, Default, Deserialize)]
struct FfTags {
    language: Option<String>,
    title: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct FfDisposition {
    forced: Option<i64>,
    hearing_impaired: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct FfSideData {
    side_data_type: Option<String>,
    /// Present on DOVI configuration records (ffprobe).
    dv_profile: Option<u64>,
}

const STDERR_TAIL: usize = 512;
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// Probe a media file with ffprobe. `should_cancel` is the library
/// reachability signal (ADR-0014 / ADR-0041 Decision 8.7 amendment): when it
/// turns true the ffprobe child is killed and the probe reports
/// `unavailable`, never `probed` or `error`.
///
/// The output pipes are drained concurrently on reader threads, like
/// `Command::output()` does: a stream-heavy title's JSON can exceed the pipe
/// buffer, and a drain-after-exit loop would let ffprobe block on a full pipe
/// forever (same shape as `keymap::packet_walk::walk`).
pub fn ffprobe(
    path: &Path,
    should_cancel: Option<&dyn Fn() -> bool>,
) -> Result<ProbeResult, String> {
    let mut child = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "spawn ffprobe: not found on PATH".into()
            } else {
                format!("spawn ffprobe for {}: {e}", path.display())
            }
        })?;

    // Drain both pipes now, not after exit: ffprobe blocks once a pipe fills,
    // and the JSON of a many-stream title is far larger than the pipe buffer.
    // The threads finish at EOF, which is guaranteed once the child exits or
    // is killed below.
    let stdout_reader = spawn_pipe_reader("ffprobe-stdout", child.stdout.take())?;
    let stderr_reader = spawn_pipe_reader("ffprobe-stderr", child.stderr.take())?;

    let status = loop {
        // Cancel wins over a just-finished probe: once the library is
        // unreachable the run is aborted, never reported probed (ADR-0041
        // Decision 8.7). Stamped "unavailable:" for the pool's classifier.
        if should_cancel.is_some_and(|c| c()) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("unavailable: ffprobe cancelled (library unreachable)".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(CANCEL_POLL),
            Err(e) => {
                return Err(format!("wait ffprobe for {}: {e}", path.display()));
            }
        }
    };

    // The child has exited, so both pipes are at EOF and the readers are done.
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();

    if !status.success() {
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into());
        let tail = stderr_tail(stderr.trim());
        return Err(format!(
            "ffprobe failed for {} (exit {code}): {tail}",
            path.display()
        ));
    }

    let parsed: FfprobeJson = serde_json::from_slice(stdout.as_bytes())
        .map_err(|e| format!("parse ffprobe json for {}: {e}", path.display()))?;

    let duration_ms = parsed
        .format
        .as_ref()
        .and_then(|f| f.duration.as_ref())
        .and_then(|d| d.parse::<f64>().ok())
        .map(|s| (s * 1000.0).round() as i64);

    let format_bitrate = parsed
        .format
        .as_ref()
        .and_then(|f| f.bit_rate.as_ref())
        .and_then(|b| b.parse::<i64>().ok())
        .filter(|&b| b > 0);

    let container = parsed
        .format
        .and_then(|f| f.format_name)
        .map(|s| s.split(',').next().unwrap_or("unknown").trim().to_string());

    let mut video_codec = None;
    let mut audio_codec = None;
    let mut audio_channels = None;
    let mut width = None;
    let mut height = None;
    let mut video_bitrate_bps = None;
    let mut hdr = None;
    let mut subtitle_streams = Vec::new();
    for stream in parsed.streams.unwrap_or_default() {
        match stream.codec_type.as_deref() {
            Some("video") if video_codec.is_none() => {
                video_codec = stream.codec_name;
                width = stream.width;
                height = stream.height;
                video_bitrate_bps = stream
                    .bit_rate
                    .as_ref()
                    .and_then(|b| b.parse::<i64>().ok())
                    .filter(|&b| b > 0)
                    .or(format_bitrate);
                hdr = Some(classify_hdr(
                    stream.color_transfer.as_deref(),
                    &stream.side_data_list,
                ));
            }
            Some("audio") if audio_codec.is_none() => {
                audio_codec = stream.codec_name;
                audio_channels = stream.channels;
            }
            Some("subtitle") => {
                // A stream without an index cannot be keyed in the inventory
                // table; ffprobe always reports `index` for -show_streams, so
                // this is defensive only.
                let Some(index) = stream.index else {
                    continue;
                };
                let tags = stream.tags.unwrap_or_default();
                let disp = stream.disposition.unwrap_or_default();
                subtitle_streams.push(ProbeSubtitleStream {
                    stream_index: index,
                    codec: stream.codec_name.unwrap_or_else(|| "unknown".to_string()),
                    language: tags.language,
                    title: tags.title.filter(|t| !t.is_empty()),
                    forced: disp.forced == Some(1),
                    sdh: disp.hearing_impaired == Some(1),
                });
            }
            _ => {}
        }
    }

    Ok(ProbeResult {
        duration_ms,
        container,
        video_codec,
        audio_codec,
        audio_channels,
        width,
        height,
        video_bitrate_bps,
        hdr,
        subtitle_streams,
    })
}

/// Drain one of ffprobe's pipes on a reader thread so the child can never
/// block on a full pipe while the probe polls for exit or cancel.
fn spawn_pipe_reader<R: std::io::Read + Send + 'static>(
    name: &str,
    pipe: Option<R>,
) -> Result<std::thread::JoinHandle<String>, String> {
    let Some(mut pipe) = pipe else {
        return Err(format!("{name}: child pipe missing"));
    };
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let mut buf = String::new();
            let _ = pipe.read_to_string(&mut buf);
            buf
        })
        .map_err(|e| format!("spawn {name} reader: {e}"))
}

fn classify_hdr(color_transfer: Option<&str>, side_data: &[FfSideData]) -> String {
    for side in side_data {
        let Some(t) = side.side_data_type.as_deref() else {
            continue;
        };
        let lower = t.to_ascii_lowercase();
        if lower.contains("dovi") || lower.contains("dolby vision") {
            // Profile 5 is IPT-PQ; zscale+hable cannot map it (no colourspace path).
            if side.dv_profile == Some(5) {
                return "dolby_vision_p5".into();
            }
            return "dolby_vision".into();
        }
    }
    match color_transfer.map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("smpte2084") | Some("arib-std-b67") => "hdr10".into(),
        _ => "none".into(),
    }
}

fn stderr_tail(s: &str) -> String {
    if s.is_empty() {
        return "(no stderr)".into();
    }
    if s.len() <= STDERR_TAIL {
        return s.to_string();
    }
    let start = s.len() - STDERR_TAIL;
    format!("…{}", &s[start..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn spawn_failure_message_is_distinct() {
        // Point PATH away so spawn fails distinctly from process exit.
        let err = {
            let old = std::env::var_os("PATH");
            unsafe { std::env::set_var("PATH", "/var/empty-nightjar-no-ffprobe") };
            let r = ffprobe(Path::new("/tmp/x.mkv"), None);
            match old {
                Some(v) => unsafe { std::env::set_var("PATH", v) },
                None => unsafe { std::env::remove_var("PATH") },
            }
            r.unwrap_err()
        };
        assert!(
            err.starts_with("spawn ffprobe"),
            "expected spawn message, got {err}"
        );
        assert!(!err.starts_with("ffprobe failed"));
    }

    #[test]
    fn process_failure_includes_exit_code() {
        if std::env::var_os("NIGHTJAR_TEST_REQUIRE_FFMPEG").is_none()
            && !Command::new("ffprobe")
                .arg("-version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        {
            return;
        }
        let path = PathBuf::from("/tmp/nightjar-definitely-missing-probe-target.mkv");
        let err = ffprobe(&path, None).unwrap_err();
        assert!(
            err.contains("exit ") || err.starts_with("spawn ffprobe"),
            "expected exit code or spawn, got {err}"
        );
        if err.starts_with("ffprobe failed") {
            assert!(!err.ends_with(": "), "empty body after colon: {err}");
        }
    }

    #[test]
    fn classify_hdr_dovi_and_pq() {
        assert_eq!(classify_hdr(None, &[]), "none");
        assert_eq!(classify_hdr(Some("smpte2084"), &[]), "hdr10");
        assert_eq!(
            classify_hdr(
                Some("bt709"),
                &[FfSideData {
                    side_data_type: Some("DOVI configuration record".into()),
                    dv_profile: Some(8),
                }]
            ),
            "dolby_vision"
        );
        assert_eq!(
            classify_hdr(
                None,
                &[FfSideData {
                    side_data_type: Some("DOVI configuration record".into()),
                    dv_profile: Some(5),
                }]
            ),
            "dolby_vision_p5"
        );
    }
}
