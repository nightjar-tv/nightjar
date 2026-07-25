//! Hardware H.264 encode detection by verification (ADR-0009).

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const VERIFY_TIMEOUT: Duration = Duration::from_secs(20);

/// Outcome of one encoder candidate after startup probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderStatus {
    Verified,
    Failed,
    Unavailable,
}

impl EncoderStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Failed => "failed",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderCandidate {
    pub name: String,
    pub backend: String,
    pub status: EncoderStatus,
    pub reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TranscodeCapabilities {
    pub ffmpeg_version: Option<String>,
    pub preferred_h264_encoder: String,
    pub encoders: Vec<EncoderCandidate>,
}

impl TranscodeCapabilities {
    /// Software-only result used when FFmpeg is missing or every verify fails
    /// before `libx264` can be checked. Preferred encoder is still `libx264`.
    pub fn software_only(ffmpeg_version: Option<String>, libx264_reason: &str) -> Self {
        Self {
            ffmpeg_version,
            preferred_h264_encoder: "libx264".into(),
            encoders: vec![EncoderCandidate {
                name: "libx264".into(),
                backend: "software".into(),
                status: EncoderStatus::Failed,
                reason: Some(libx264_reason.into()),
            }],
        }
    }
}

/// Platform preference order for H.264 encode (ADR-0009 §3). First verified
/// wins; `libx264` is always last. Throughput over quality for this slice;
/// VideoToolbox-at-low-bitrate amendments go in the ADR, not a silent reorder.
fn candidates_for_host() -> Vec<(&'static str, &'static str)> {
    let mut out = Vec::new();
    #[cfg(target_os = "macos")]
    {
        out.push(("h264_videotoolbox", "videotoolbox"));
    }
    #[cfg(target_os = "linux")]
    {
        out.push(("h264_nvenc", "nvenc"));
        out.push(("h264_qsv", "qsv"));
        out.push(("h264_vaapi", "vaapi"));
        out.push(("h264_v4l2m2m", "v4l2m2m"));
    }
    #[cfg(target_os = "windows")]
    {
        out.push(("h264_nvenc", "nvenc"));
        out.push(("h264_qsv", "qsv"));
        out.push(("h264_mf", "mediafoundation"));
    }
    out.push(("libx264", "software"));
    out
}

/// Pure selection: first verified candidate in platform order, else libx264.
pub fn select_preferred(encoders: &[EncoderCandidate]) -> String {
    for enc in encoders {
        if enc.status == EncoderStatus::Verified {
            return enc.name.clone();
        }
    }
    "libx264".into()
}

/// Enumerate, verify, and select. Call once at process startup (or later from
/// `nightjar doctor`); never from a playback session path (ADR-0009 §2).
pub fn probe_h264_encoders(work_dir: &Path) -> TranscodeCapabilities {
    let ffmpeg_version = ffmpeg_version_line();
    let advertised = match listed_encoders() {
        Ok(set) => set,
        Err(e) => {
            tracing::warn!(error = %e, "ffmpeg encoder list failed; software fallback");
            return TranscodeCapabilities::software_only(ffmpeg_version, &e);
        }
    };

    let probe_root = work_dir.join("hwaccel-probe");
    if let Err(e) = fs::create_dir_all(&probe_root) {
        let msg = format!("create probe dir {}: {e}", probe_root.display());
        tracing::warn!(error = %msg, "hwaccel probe setup failed");
        return TranscodeCapabilities::software_only(ffmpeg_version, &msg);
    }

    let mut encoders = Vec::new();
    for (name, backend) in candidates_for_host() {
        if !advertised.iter().any(|a| a == name) {
            encoders.push(EncoderCandidate {
                name: name.into(),
                backend: backend.into(),
                status: EncoderStatus::Unavailable,
                reason: Some(format!("{name} not listed by this ffmpeg build")),
            });
            continue;
        }
        let dir = probe_root.join(name);
        let _ = fs::remove_dir_all(&dir);
        if let Err(e) = fs::create_dir_all(&dir) {
            encoders.push(EncoderCandidate {
                name: name.into(),
                backend: backend.into(),
                status: EncoderStatus::Failed,
                reason: Some(format!("probe dir: {e}")),
            });
            continue;
        }
        match verify_encoder(name, &dir) {
            Ok(()) => {
                tracing::info!(encoder = name, backend, "h264 encoder verified");
                encoders.push(EncoderCandidate {
                    name: name.into(),
                    backend: backend.into(),
                    status: EncoderStatus::Verified,
                    reason: None,
                });
            }
            Err(reason) => {
                tracing::warn!(encoder = name, backend, %reason, "h264 encoder verify failed");
                encoders.push(EncoderCandidate {
                    name: name.into(),
                    backend: backend.into(),
                    status: EncoderStatus::Failed,
                    reason: Some(reason),
                });
            }
        }
        let _ = fs::remove_dir_all(&dir);
    }

    let preferred = select_preferred(&encoders);
    tracing::info!(
        preferred = %preferred,
        ffmpeg = ?ffmpeg_version,
        "transcode capability probe complete"
    );

    TranscodeCapabilities {
        ffmpeg_version,
        preferred_h264_encoder: preferred,
        encoders,
    }
}

pub fn probe_h264_encoders_arc(work_dir: &Path) -> Arc<TranscodeCapabilities> {
    Arc::new(probe_h264_encoders(work_dir))
}

fn ffmpeg_version_line() -> Option<String> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-version"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(|l| l.trim().to_string())
}

fn listed_encoders() -> Result<Vec<String>, String> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "ffmpeg not found on PATH".into()
            } else {
                format!("ffmpeg -encoders: {e}")
            }
        })?;
    if !out.status.success() {
        return Err(format!("ffmpeg -encoders exited {}", out.status));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // After the `------` legend, rows look like: " V....D libx264   ..."
    let mut names = Vec::new();
    let mut past_legend = false;
    for line in text.lines() {
        if line.trim_start().starts_with("------") {
            past_legend = true;
            continue;
        }
        if !past_legend {
            continue;
        }
        let rest = line.get(8..).unwrap_or("").trim_start();
        let name = rest.split_whitespace().next().unwrap_or("");
        if !name.is_empty() {
            names.push(name.to_string());
        }
    }
    Ok(names)
}

fn verify_encoder(encoder: &str, dir: &Path) -> Result<(), String> {
    let out_path = dir.join("probe.mp4");
    if encoder == "h264_vaapi" {
        return verify_vaapi(&out_path);
    }

    let mut child = Command::new("ffmpeg")
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=24:duration=2",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-c:v",
            encoder,
            "-c:a",
            "aac",
            "-ac",
            "2",
            "-shortest",
            "probe.mp4",
        ])
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;
    wait_child(&mut child, VERIFY_TIMEOUT)?;
    if !out_path.exists() || fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0) == 0 {
        return Err("encoder produced no output file".into());
    }
    demux_check(&out_path)
}

fn verify_vaapi(out_path: &Path) -> Result<(), String> {
    let mut child = Command::new("ffmpeg")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-vaapi_device",
            "/dev/dri/renderD128",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=24:duration=2",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-vf",
            "format=nv12,hwupload",
            "-c:v",
            "h264_vaapi",
            "-c:a",
            "aac",
            "-ac",
            "2",
            "-shortest",
            out_path.to_str().unwrap_or("probe.mp4"),
        ])
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;
    wait_child(&mut child, VERIFY_TIMEOUT)?;
    if !out_path.exists() || fs::metadata(out_path).map(|m| m.len()).unwrap_or(0) == 0 {
        return Err("encoder produced no output file".into());
    }
    demux_check(out_path)
}

fn demux_check(path: &Path) -> Result<(), String> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .map_err(|e| format!("ffprobe: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("ffprobe failed: {}", err.trim()));
    }
    let codec = String::from_utf8_lossy(&out.stdout)
        .trim()
        .to_ascii_lowercase();
    if codec.contains("h264") || codec.contains("avc") {
        Ok(())
    } else {
        Err(format!("unexpected video codec from probe: {codec:?}"))
    }
}

fn wait_child(child: &mut std::process::Child, timeout: Duration) -> Result<(), String> {
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
                        std::io::Read::read_to_string(s, &mut buf).ok()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferred_picks_first_verified() {
        let encoders = vec![
            EncoderCandidate {
                name: "h264_nvenc".into(),
                backend: "nvenc".into(),
                status: EncoderStatus::Failed,
                reason: Some("no gpu".into()),
            },
            EncoderCandidate {
                name: "h264_videotoolbox".into(),
                backend: "videotoolbox".into(),
                status: EncoderStatus::Verified,
                reason: None,
            },
            EncoderCandidate {
                name: "libx264".into(),
                backend: "software".into(),
                status: EncoderStatus::Verified,
                reason: None,
            },
        ];
        assert_eq!(select_preferred(&encoders), "h264_videotoolbox");
    }

    #[test]
    fn preferred_falls_back_to_libx264() {
        let encoders = vec![EncoderCandidate {
            name: "h264_nvenc".into(),
            backend: "nvenc".into(),
            status: EncoderStatus::Unavailable,
            reason: Some("missing".into()),
        }];
        assert_eq!(select_preferred(&encoders), "libx264");
    }

    #[test]
    fn probe_verifies_libx264_when_ffmpeg_present() {
        let ok = Command::new("ffmpeg")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            if std::env::var_os("NIGHTJAR_TEST_REQUIRE_FFMPEG").is_some() {
                panic!("NIGHTJAR_TEST_REQUIRE_FFMPEG set but ffmpeg missing");
            }
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let caps = probe_h264_encoders(dir.path());
        let soft = caps
            .encoders
            .iter()
            .find(|e| e.name == "libx264")
            .expect("libx264 candidate");
        assert_eq!(soft.status, EncoderStatus::Verified, "{soft:?}");
        assert!(
            caps.encoders
                .iter()
                .any(|e| e.status == EncoderStatus::Verified
                    && e.name == caps.preferred_h264_encoder),
            "preferred must be a verified encoder: {caps:?}"
        );
    }
}
