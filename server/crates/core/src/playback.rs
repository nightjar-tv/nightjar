use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackMethod {
    DirectPlay,
    Remux,
    Transcode,
}

impl PlaybackMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DirectPlay => "directPlay",
            Self::Remux => "remux",
            Self::Transcode => "transcode",
        }
    }
}

/// What a client can play natively. The compatibility contract for Phase 2
/// (ADR-0006); richer per-client profiles arrive additively later.
pub struct ClientCapabilityProfile {
    pub video_codecs: &'static [&'static str],
    pub audio_codecs: &'static [&'static str],
    /// ffprobe format_name parts accepted for direct play.
    pub containers: &'static [&'static str],
    /// File extensions treated as an accepted container without a probe match.
    pub extensions: &'static [&'static str],
}

/// Phase 1 browser whitelist: H.264 family + AAC in MP4/M4V.
pub const BROWSER_V0: ClientCapabilityProfile = ClientCapabilityProfile {
    video_codecs: &["h264", "avc", "avc1"],
    audio_codecs: &["aac", "mp4a"],
    containers: &["mp4", "m4v", "mov"],
    extensions: &["mp4", "m4v"],
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackDecision {
    pub method: PlaybackMethod,
    pub reason: String,
    pub mime_type: String,
}

/// (file streams × client capability profile) → directPlay | remux | transcode.
///
/// Codecs the client plays in an accepted container direct play; the same
/// codecs in any other container remux (stream copy to MP4); everything else,
/// including pending and failed probes, is transcode. We will not claim
/// browser playback without a successful probe.
pub fn decide_playback(
    path: &str,
    container: Option<&str>,
    video_codec: Option<&str>,
    audio_codec: Option<&str>,
    scan_error: Option<&str>,
    probe_status: &str,
    profile: &ClientCapabilityProfile,
) -> PlaybackDecision {
    if probe_status == "indexed" {
        return PlaybackDecision {
            method: PlaybackMethod::Transcode,
            reason: "probe pending".into(),
            mime_type: mime_for_path(path),
        };
    }

    if let Some(err) = scan_error.filter(|e| !e.is_empty()) {
        return PlaybackDecision {
            method: PlaybackMethod::Transcode,
            reason: format!("probe failed: {err}"),
            mime_type: mime_for_path(path),
        };
    }

    let video_ok = matches_codec(video_codec, profile.video_codecs);
    let audio_ok = matches_codec(audio_codec, profile.audio_codecs);
    let container_ok = matches_container(path, container, profile);

    if video_ok && audio_ok {
        if container_ok {
            return PlaybackDecision {
                method: PlaybackMethod::DirectPlay,
                reason: "codecs and container supported by client".into(),
                mime_type: mime_for_path(path),
            };
        }
        return PlaybackDecision {
            method: PlaybackMethod::Remux,
            reason: "codecs supported; container needs repackaging to MP4".into(),
            mime_type: "video/mp4".into(),
        };
    }

    let mut why = Vec::new();
    if !video_ok {
        why.push("video codec unsupported");
    }
    if !audio_ok {
        why.push("audio codec unsupported");
    }
    PlaybackDecision {
        method: PlaybackMethod::Transcode,
        reason: format!("needs transcode: {}", why.join(", ")),
        mime_type: mime_for_path(path),
    }
}

fn matches_codec(codec: Option<&str>, accepted: &[&str]) -> bool {
    match codec {
        Some(c) => {
            let c = c.to_ascii_lowercase();
            accepted.contains(&c.as_str())
        }
        None => false,
    }
}

fn matches_container(
    path: &str,
    container: Option<&str>,
    profile: &ClientCapabilityProfile,
) -> bool {
    let path_l = path.to_ascii_lowercase();
    if profile
        .extensions
        .iter()
        .any(|ext| path_l.ends_with(&format!(".{ext}")))
    {
        return true;
    }
    let c = container.unwrap_or("").to_ascii_lowercase();
    // ffprobe format_name is often "mov,mp4,m4a,3gp,3g2,mj2"
    c.split(',').any(|p| profile.containers.contains(&p.trim()))
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

    fn decide(
        path: &str,
        container: Option<&str>,
        video: Option<&str>,
        audio: Option<&str>,
        scan_error: Option<&str>,
        probe_status: &str,
    ) -> PlaybackDecision {
        decide_playback(
            path,
            container,
            video,
            audio,
            scan_error,
            probe_status,
            &BROWSER_V0,
        )
    }

    #[test]
    fn table_of_expected_methods() {
        let cases = [
            (
                "h264 aac mp4 direct plays",
                decide(
                    "/a/b.mp4",
                    Some("mov,mp4,m4a"),
                    Some("h264"),
                    Some("aac"),
                    None,
                    "probed",
                ),
                PlaybackMethod::DirectPlay,
            ),
            (
                "h264 aac mkv remuxes",
                decide(
                    "/a/b.mkv",
                    Some("matroska,webm"),
                    Some("h264"),
                    Some("aac"),
                    None,
                    "probed",
                ),
                PlaybackMethod::Remux,
            ),
            (
                "h264 ac3 mp4 transcodes",
                decide(
                    "/a/b.mp4",
                    Some("mp4"),
                    Some("h264"),
                    Some("ac3"),
                    None,
                    "probed",
                ),
                PlaybackMethod::Transcode,
            ),
            (
                "hevc aac mp4 transcodes",
                decide(
                    "/a/b.mp4",
                    Some("mp4"),
                    Some("hevc"),
                    Some("aac"),
                    None,
                    "probed",
                ),
                PlaybackMethod::Transcode,
            ),
            (
                "hevc ac3 mkv transcodes",
                decide(
                    "/a/b.mkv",
                    Some("matroska,webm"),
                    Some("hevc"),
                    Some("ac3"),
                    None,
                    "probed",
                ),
                PlaybackMethod::Transcode,
            ),
        ];
        for (name, decision, expected) in cases {
            assert_eq!(decision.method, expected, "{name}: {}", decision.reason);
        }
    }

    #[test]
    fn remux_reports_mp4_mime_not_source_mime() {
        let d = decide(
            "/a/b.mkv",
            Some("matroska,webm"),
            Some("h264"),
            Some("aac"),
            None,
            "probed",
        );
        assert_eq!(d.method, PlaybackMethod::Remux);
        assert_eq!(d.mime_type, "video/mp4");
    }

    #[test]
    fn probe_error_is_transcode() {
        let d = decide(
            "/a/b.mp4",
            None,
            None,
            None,
            Some("ffprobe missing"),
            "error",
        );
        assert_eq!(d.method, PlaybackMethod::Transcode);
        assert!(d.reason.contains("probe failed"));
    }

    #[test]
    fn indexed_is_transcode_until_probed() {
        let d = decide("/a/b.mp4", None, None, None, None, "indexed");
        assert_eq!(d.method, PlaybackMethod::Transcode);
        assert!(d.reason.contains("probe pending"));
    }

    #[test]
    fn transcode_reason_names_the_offending_stream() {
        let d = decide(
            "/a/b.mkv",
            Some("matroska,webm"),
            Some("h264"),
            Some("dts"),
            None,
            "probed",
        );
        assert_eq!(d.method, PlaybackMethod::Transcode);
        assert!(d.reason.contains("audio codec unsupported"));
        assert!(!d.reason.contains("video codec unsupported"));
    }
}
