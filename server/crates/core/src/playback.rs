use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackDecision {
    pub direct_play: bool,
    pub needs_transcode: bool,
    pub reason: String,
    pub mime_type: String,
}

/// Phase 1 browser-safe whitelist. Seed of the Phase 2 capability-profile system.
///
/// Direct play only when the file is H.264 (8-bit family) + AAC in an MP4/M4V
/// container. Everything else needs remux/transcode (Phase 2). Probe failures
/// and pending probes block direct play. We will not claim browser playback
/// without a successful probe.
pub fn decide_direct_play(
    path: &str,
    container: Option<&str>,
    video_codec: Option<&str>,
    audio_codec: Option<&str>,
    scan_error: Option<&str>,
    probe_status: &str,
) -> PlaybackDecision {
    let mime = mime_for_path(path);

    if probe_status == "indexed" {
        return PlaybackDecision {
            direct_play: false,
            needs_transcode: true,
            reason: "probe pending".into(),
            mime_type: mime,
        };
    }

    if let Some(err) = scan_error.filter(|e| !e.is_empty()) {
        return PlaybackDecision {
            direct_play: false,
            needs_transcode: true,
            reason: format!("probe failed: {err}"),
            mime_type: mime,
        };
    }

    let video_ok = matches_video(video_codec);
    let audio_ok = matches_audio(audio_codec);
    let container_ok = matches_container(path, container);

    if video_ok && audio_ok && container_ok {
        return PlaybackDecision {
            direct_play: true,
            needs_transcode: false,
            reason: "H.264 + AAC in MP4; Phase 1 browser direct play".into(),
            mime_type: mime,
        };
    }

    let mut why = Vec::new();
    if !container_ok {
        why.push("container not MP4/M4V");
    }
    if !video_ok {
        why.push("video not H.264");
    }
    if !audio_ok {
        why.push("audio not AAC");
    }
    PlaybackDecision {
        direct_play: false,
        needs_transcode: true,
        reason: format!("needs transcode: {}", why.join(", ")),
        mime_type: mime,
    }
}

fn matches_video(codec: Option<&str>) -> bool {
    matches!(
        codec.map(|c| c.to_ascii_lowercase()).as_deref(),
        Some("h264" | "avc" | "avc1")
    )
}

fn matches_audio(codec: Option<&str>) -> bool {
    matches!(
        codec.map(|c| c.to_ascii_lowercase()).as_deref(),
        Some("aac" | "mp4a")
    )
}

fn matches_container(path: &str, container: Option<&str>) -> bool {
    let path_l = path.to_ascii_lowercase();
    if path_l.ends_with(".mp4") || path_l.ends_with(".m4v") {
        return true;
    }
    let c = container.unwrap_or("").to_ascii_lowercase();
    // ffprobe format_name is often "mov,mp4,m4a,3gp,3g2,mj2"
    c.split(',')
        .any(|p| matches!(p.trim(), "mp4" | "m4v" | "mov"))
}

pub fn mime_for_path(path: &str) -> String {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "ts" | "m2ts" => "video/mp2t",
        "ogv" => "video/ogg",
        _ => "application/octet-stream",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_aac_mp4_direct_plays() {
        let d = decide_direct_play(
            "/a/b.mp4",
            Some("mov,mp4,m4a"),
            Some("h264"),
            Some("aac"),
            None,
            "probed",
        );
        assert!(d.direct_play);
        assert!(!d.needs_transcode);
    }

    #[test]
    fn h264_ac3_needs_transcode() {
        let d = decide_direct_play(
            "/a/b.mp4",
            Some("mp4"),
            Some("h264"),
            Some("ac3"),
            None,
            "probed",
        );
        assert!(!d.direct_play);
        assert!(d.needs_transcode);
        assert!(d.reason.contains("audio not AAC"));
    }

    #[test]
    fn mkv_needs_transcode() {
        let d = decide_direct_play(
            "/a/b.mkv",
            Some("matroska,webm"),
            Some("h264"),
            Some("aac"),
            None,
            "probed",
        );
        assert!(!d.direct_play);
        assert!(d.needs_transcode);
    }

    #[test]
    fn hevc_needs_transcode() {
        let d = decide_direct_play(
            "/a/b.mp4",
            Some("mp4"),
            Some("hevc"),
            Some("aac"),
            None,
            "probed",
        );
        assert!(!d.direct_play);
    }

    #[test]
    fn probe_error_blocks() {
        let d = decide_direct_play(
            "/a/b.mp4",
            None,
            None,
            None,
            Some("ffprobe missing"),
            "error",
        );
        assert!(!d.direct_play);
        assert!(d.needs_transcode);
    }

    #[test]
    fn indexed_blocks_until_probed() {
        let d = decide_direct_play("/a/b.mp4", None, None, None, None, "indexed");
        assert!(!d.direct_play);
        assert!(d.reason.contains("probe pending"));
    }
}
