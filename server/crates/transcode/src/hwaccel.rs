//! Hardware H.264 encode detection by session-shaped verification (ADR-0009).
//!
//! Probe and HLS session share one encode-leg description (`EncodeLeg`). Backend
//! differences are data rows, not a second spawn path.

use std::fs;
use std::path::{Path, PathBuf};
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

/// Session-shaped H.264 encode leg (ADR-0009 field budget).
///
/// Probe and HLS use the same values. Software prefilters (scale, tonemap, ASS)
/// sit outside this struct and are composed with `upload_vf` by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodeLeg {
    pub encoder: String,
    pub backend: String,
    /// Args before `-i` (e.g. `-vaapi_device`, path).
    pub pre_input: Vec<String>,
    /// Filter suffix for encoder surfaces after software filters, if any.
    pub upload_vf: Option<String>,
    /// When set, pass `-pix_fmt`. When `None`, omit (VAAPI owns surfaces via upload).
    pub pix_fmt: Option<String>,
    /// Extra args after `-c:v` and optional pix_fmt (e.g. libx264 `-preset`).
    pub encoder_extra: Vec<String>,
    /// Render node or device path recorded by probe (`None` when unused).
    pub device: Option<String>,
}

impl EncodeLeg {
    pub fn software() -> Self {
        Self {
            encoder: "libx264".into(),
            backend: "software".into(),
            pre_input: Vec::new(),
            upload_vf: None,
            pix_fmt: Some("yuv420p".into()),
            encoder_extra: vec!["-preset".into(), "veryfast".into()],
            device: None,
        }
    }

    pub fn videotoolbox() -> Self {
        Self {
            encoder: "h264_videotoolbox".into(),
            backend: "videotoolbox".into(),
            pre_input: Vec::new(),
            upload_vf: None,
            pix_fmt: Some("yuv420p".into()),
            encoder_extra: Vec::new(),
            device: None,
        }
    }

    /// Intel QSV system-memory path (measured: no upload on N150 host).
    pub fn qsv_sysmem() -> Self {
        Self {
            encoder: "h264_qsv".into(),
            backend: "qsv".into(),
            pre_input: Vec::new(),
            upload_vf: None,
            pix_fmt: Some("nv12".into()),
            encoder_extra: Vec::new(),
            device: None,
        }
    }

    /// VAAPI encode leg for a verified render node (spike 2026-08-03).
    pub fn vaapi(device: impl Into<String>) -> Self {
        let device = device.into();
        Self {
            encoder: "h264_vaapi".into(),
            backend: "vaapi".into(),
            pre_input: vec!["-vaapi_device".into(), device.clone()],
            upload_vf: Some("format=nv12,hwupload".into()),
            pix_fmt: None,
            encoder_extra: Vec::new(),
            device: Some(device),
        }
    }

    /// Name-only HW leg (NVENC / V4L2 / MF) — system memory + yuv420p until measured.
    pub fn generic_hw(encoder: &str, backend: &str) -> Self {
        Self {
            encoder: encoder.into(),
            backend: backend.into(),
            pre_input: Vec::new(),
            upload_vf: None,
            pix_fmt: Some("yuv420p".into()),
            encoder_extra: Vec::new(),
            device: None,
        }
    }

    /// Compose software filter chain with upload suffix (encode-leg contract).
    pub fn compose_video_filter(&self, software: Option<&str>) -> Option<String> {
        match (software, self.upload_vf.as_deref()) {
            (Some(sw), Some(up)) if !sw.is_empty() => Some(format!("{sw},{up}")),
            (Some(sw), None) if !sw.is_empty() => Some(sw.to_string()),
            (None, Some(up)) | (Some(""), Some(up)) => Some(up.to_string()),
            _ => None,
        }
    }

    /// Append `-c:v`, optional pix_fmt, and encoder_extra to a command.
    pub fn push_encoder_args(&self, cmd: &mut Command) {
        cmd.args(["-c:v", &self.encoder]);
        if let Some(ref pf) = self.pix_fmt {
            cmd.args(["-pix_fmt", pf]);
        }
        for a in &self.encoder_extra {
            cmd.arg(a);
        }
    }

    /// Append pre-input device args.
    pub fn push_pre_input(&self, cmd: &mut Command) {
        for a in &self.pre_input {
            cmd.arg(a);
        }
    }
}

impl From<&str> for EncodeLeg {
    /// Test / name-only reconstruction. **Not** the product path: VAAPI hardcodes
    /// `renderD128` and drops the probed device. Production must use
    /// `TranscodeCapabilities::preferred_encode_leg` from probe.
    fn from(name: &str) -> Self {
        match name {
            "libx264" | "copy" => Self::software(),
            "h264_videotoolbox" => Self::videotoolbox(),
            "h264_qsv" => Self::qsv_sysmem(),
            "h264_vaapi" => Self::vaapi("/dev/dri/renderD128"),
            other => Self::generic_hw(other, "hardware"),
        }
    }
}

impl From<String> for EncodeLeg {
    fn from(name: String) -> Self {
        EncodeLeg::from(name.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct TranscodeCapabilities {
    pub ffmpeg_version: Option<String>,
    pub preferred_h264_encoder: String,
    /// Full encode leg for the preferred backend (shared by probe and sessions).
    pub preferred_encode_leg: EncodeLeg,
    pub encoders: Vec<EncoderCandidate>,
}

impl TranscodeCapabilities {
    /// Software-only result used when FFmpeg is missing or every verify fails
    /// before `libx264` can be checked. Preferred encoder is still `libx264`.
    pub fn software_only(ffmpeg_version: Option<String>, libx264_reason: &str) -> Self {
        let leg = EncodeLeg::software();
        Self {
            ffmpeg_version,
            preferred_h264_encoder: leg.encoder.clone(),
            preferred_encode_leg: leg,
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
/// wins; `libx264` is always last.
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

/// DRM render nodes to try for VAAPI (ADR-0009: probe records which worked).
pub fn list_render_nodes() -> Vec<PathBuf> {
    let mut nodes = Vec::new();
    let dri = Path::new("/dev/dri");
    if let Ok(rd) = fs::read_dir(dri) {
        for ent in rd.flatten() {
            let name = ent.file_name();
            let s = name.to_string_lossy();
            if s.starts_with("renderD") {
                nodes.push(ent.path());
            }
        }
    }
    nodes.sort();
    if nodes.is_empty() {
        // Still try the conventional default so reasons stay informative.
        nodes.push(PathBuf::from("/dev/dri/renderD128"));
    }
    nodes
}

/// Encode-leg variants to try for one advertised encoder name.
pub fn encode_legs_for_candidate(name: &str, backend: &str) -> Vec<EncodeLeg> {
    match name {
        "libx264" => vec![EncodeLeg::software()],
        "h264_videotoolbox" => vec![EncodeLeg::videotoolbox()],
        "h264_qsv" => vec![EncodeLeg::qsv_sysmem()],
        "h264_vaapi" => list_render_nodes()
            .into_iter()
            .map(|p| EncodeLeg::vaapi(p.to_string_lossy()))
            .collect(),
        other => vec![EncodeLeg::generic_hw(other, backend)],
    }
}

/// Enumerate, session-shaped verify, and select. Call once at process startup
/// (or later from `nightjar doctor`); never from a playback session path.
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
    let mut preferred_leg: Option<EncodeLeg> = None;

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

        let legs = encode_legs_for_candidate(name, backend);
        let mut last_reason = String::from("no encode-leg variants");
        let mut verified_leg: Option<EncodeLeg> = None;

        for (i, leg) in legs.into_iter().enumerate() {
            let dir = probe_root.join(format!("{name}_{i}"));
            let _ = fs::remove_dir_all(&dir);
            if let Err(e) = fs::create_dir_all(&dir) {
                last_reason = format!("probe dir: {e}");
                continue;
            }
            match verify_encode_leg(&leg, &dir) {
                Ok(()) => {
                    verified_leg = Some(leg);
                    let _ = fs::remove_dir_all(&dir);
                    break;
                }
                Err(reason) => {
                    last_reason = reason;
                    let _ = fs::remove_dir_all(&dir);
                }
            }
        }

        if let Some(leg) = verified_leg {
            tracing::info!(
                encoder = %leg.encoder,
                backend = %leg.backend,
                device = ?leg.device,
                "h264 encode leg verified"
            );
            if preferred_leg.is_none() {
                preferred_leg = Some(leg.clone());
            }
            encoders.push(EncoderCandidate {
                name: name.into(),
                backend: backend.into(),
                status: EncoderStatus::Verified,
                reason: None,
            });
        } else {
            tracing::warn!(
                encoder = name,
                backend,
                reason = %last_reason,
                "h264 encode leg verify failed"
            );
            encoders.push(EncoderCandidate {
                name: name.into(),
                backend: backend.into(),
                status: EncoderStatus::Failed,
                reason: Some(last_reason),
            });
        }
    }

    let preferred_encode_leg = preferred_leg.unwrap_or_else(EncodeLeg::software);
    let preferred = select_preferred(&encoders);
    // First verified in preference order owns preferred_encode_leg; keep encoder
    // name aligned when at least one leg verified.
    let preferred_encode_leg = if preferred == preferred_encode_leg.encoder {
        preferred_encode_leg
    } else if preferred == "libx264" {
        EncodeLeg::software()
    } else {
        preferred_encode_leg
    };

    tracing::info!(
        preferred = %preferred,
        device = ?preferred_encode_leg.device,
        ffmpeg = ?ffmpeg_version,
        "transcode capability probe complete"
    );

    TranscodeCapabilities {
        ffmpeg_version,
        preferred_h264_encoder: preferred,
        preferred_encode_leg,
        encoders,
    }
}

pub fn probe_h264_encoders_arc(work_dir: &Path) -> Arc<TranscodeCapabilities> {
    Arc::new(probe_h264_encoders(work_dir))
}

/// Software prefilter stub for probe (ADR-0009 encode-leg proof).
///
/// Sessions compose scale / tonemap / SDR retag then `upload_vf`. Surface-
/// changing tails end in `format=yuv420p` (HDR tonemap; optional scale+format).
/// Probe uses that surface step + `compose_video_filter` so verify is not a
/// strict subset that skips the yuv420p→nv12 handoff before `hwupload`.
const PROBE_SOFTWARE_CHAIN: &str = "format=yuv420p";

/// Session-shaped verify for one encode leg (ADR-0009 encode-leg proof).
fn verify_encode_leg(leg: &EncodeLeg, dir: &Path) -> Result<(), String> {
    let out_path = dir.join("probe.mp4");
    let mut cmd = Command::new("ffmpeg");
    cmd.current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"]);
    leg.push_pre_input(&mut cmd);
    cmd.args([
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=320x240:rate=24:duration=2",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:duration=2",
    ]);
    // Same compose path as HLS Transcode: software stub then encode-leg upload.
    if let Some(vf) = leg.compose_video_filter(Some(PROBE_SOFTWARE_CHAIN)) {
        cmd.args(["-vf", &vf]);
    }
    leg.push_encoder_args(&mut cmd);
    cmd.args(["-c:a", "aac", "-ac", "2", "-shortest", "probe.mp4"]);

    let mut child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;
    wait_child(&mut child, VERIFY_TIMEOUT)?;
    if !out_path.exists() || fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0) == 0 {
        return Err("encoder produced no output file".into());
    }
    demux_check(&out_path)
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
    fn software_leg_owns_pix_fmt_and_preset() {
        let leg = EncodeLeg::software();
        assert_eq!(leg.encoder, "libx264");
        assert_eq!(leg.pix_fmt.as_deref(), Some("yuv420p"));
        assert!(leg.upload_vf.is_none());
        assert!(leg.pre_input.is_empty());
        assert!(
            leg.encoder_extra
                .windows(2)
                .any(|w| w == ["-preset", "veryfast"])
        );
    }

    #[test]
    fn vaapi_leg_device_upload_no_sw_pix_fmt() {
        let leg = EncodeLeg::vaapi("/dev/dri/renderD128");
        assert_eq!(leg.pre_input, ["-vaapi_device", "/dev/dri/renderD128"]);
        assert_eq!(leg.upload_vf.as_deref(), Some("format=nv12,hwupload"));
        assert!(leg.pix_fmt.is_none());
        assert_eq!(leg.device.as_deref(), Some("/dev/dri/renderD128"));
    }

    #[test]
    fn qsv_sysmem_no_upload() {
        let leg = EncodeLeg::qsv_sysmem();
        assert!(leg.pre_input.is_empty());
        assert!(leg.upload_vf.is_none());
        assert_eq!(leg.pix_fmt.as_deref(), Some("nv12"));
    }

    #[test]
    fn compose_appends_upload_after_software() {
        let leg = EncodeLeg::vaapi("/dev/dri/renderD128");
        let vf = leg
            .compose_video_filter(Some("scale=-2:720,format=yuv420p"))
            .unwrap();
        assert_eq!(vf, "scale=-2:720,format=yuv420p,format=nv12,hwupload");
        assert_eq!(
            leg.compose_video_filter(None).as_deref(),
            Some("format=nv12,hwupload")
        );
        let soft = EncodeLeg::software();
        assert_eq!(
            soft.compose_video_filter(Some("scale=-2:720")).as_deref(),
            Some("scale=-2:720")
        );
    }

    #[test]
    fn probe_compose_matches_surface_format_before_upload() {
        // Probe must not be upload-only: session software chains can end in
        // format=yuv420p before VAAPI format=nv12,hwupload.
        let leg = EncodeLeg::vaapi("/dev/dri/renderD129");
        assert_eq!(
            leg.compose_video_filter(Some(PROBE_SOFTWARE_CHAIN))
                .as_deref(),
            Some("format=yuv420p,format=nv12,hwupload")
        );
        let nvenc = EncodeLeg::generic_hw("h264_nvenc", "nvenc");
        assert_eq!(
            nvenc
                .compose_video_filter(Some(PROBE_SOFTWARE_CHAIN))
                .as_deref(),
            Some("format=yuv420p")
        );
    }

    #[test]
    fn from_str_vaapi_is_name_only_fallback_not_probed_device() {
        let leg = EncodeLeg::from("h264_vaapi");
        assert_eq!(leg.device.as_deref(), Some("/dev/dri/renderD128"));
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
        assert_eq!(
            caps.preferred_encode_leg.encoder,
            caps.preferred_h264_encoder
        );
    }
}
