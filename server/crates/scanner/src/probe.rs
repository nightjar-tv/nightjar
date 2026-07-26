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
}

#[derive(Debug, Deserialize)]
struct FfStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    channels: Option<i64>,
    width: Option<i32>,
    height: Option<i32>,
}

pub fn ffprobe(path: &Path) -> Result<ProbeResult, String> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "ffprobe not found on PATH".into()
            } else {
                format!("spawn ffprobe for {}: {e}", path.display())
            }
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "ffprobe failed for {}: {}",
            path.display(),
            stderr.trim()
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

    let container = parsed
        .format
        .and_then(|f| f.format_name)
        .map(|s| s.split(',').next().unwrap_or("unknown").trim().to_string());

    let mut video_codec = None;
    let mut audio_codec = None;
    let mut audio_channels = None;
    let mut width = None;
    let mut height = None;
    for stream in parsed.streams.unwrap_or_default() {
        match stream.codec_type.as_deref() {
            Some("video") if video_codec.is_none() => {
                video_codec = stream.codec_name;
                width = stream.width;
                height = stream.height;
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
    })
}
