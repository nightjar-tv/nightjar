use serde::Deserialize;
use std::path::Path;
use std::process::Command;

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
    /// `none` | `hdr10` | `dolby_vision` (ADR-0022).
    pub hdr: Option<String>,
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
    codec_type: Option<String>,
    codec_name: Option<String>,
    channels: Option<i64>,
    width: Option<i32>,
    height: Option<i32>,
    bit_rate: Option<String>,
    color_transfer: Option<String>,
    #[serde(default)]
    side_data_list: Vec<FfSideData>,
}

#[derive(Debug, Deserialize)]
struct FfSideData {
    side_data_type: Option<String>,
}

const STDERR_TAIL: usize = 512;

pub fn ffprobe(path: &Path) -> Result<ProbeResult, String> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "spawn ffprobe: not found on PATH".into()
            } else {
                format!("spawn ffprobe for {}: {e}", path.display())
            }
        })?;

    if !output.status.success() {
        let code = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into());
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail = stderr_tail(stderr.trim());
        return Err(format!(
            "ffprobe failed for {} (exit {code}): {tail}",
            path.display()
        ));
    }

    let parsed: FfprobeJson = serde_json::from_slice(&output.stdout)
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
    })
}

fn classify_hdr(color_transfer: Option<&str>, side_data: &[FfSideData]) -> String {
    for side in side_data {
        let Some(t) = side.side_data_type.as_deref() else {
            continue;
        };
        let lower = t.to_ascii_lowercase();
        if lower.contains("dovi") || lower.contains("dolby vision") {
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
            let r = ffprobe(Path::new("/tmp/x.mkv"));
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
        let err = ffprobe(&path).unwrap_err();
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
                }]
            ),
            "dolby_vision"
        );
    }
}
