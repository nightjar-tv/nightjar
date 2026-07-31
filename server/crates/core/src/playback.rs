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
    /// Highest audio channel count the client renders usefully; tracks above
    /// it are downmixed by a session (ADR-0012). `None` means no ceiling.
    pub max_audio_channels: Option<u32>,
}

/// Phase 1 browser whitelist: H.264 family + AAC in MP4/M4V, stereo audio.
pub const BROWSER_V0: ClientCapabilityProfile = ClientCapabilityProfile {
    video_codecs: &["h264", "avc", "avc1"],
    audio_codecs: &["aac", "mp4a"],
    containers: &["mp4", "m4v", "mov"],
    extensions: &["mp4", "m4v"],
    max_audio_channels: Some(2),
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
/// codecs in any other container remux (a stream-copy HLS session, ADR-0011);
/// everything else, including pending and failed probes, is transcode. We will
/// not claim browser playback without a successful probe.
///
/// `audio_channels` is the first-audio channel count. A track above the
/// profile ceiling loses direct play even when its codec and container are
/// fine: the session copies video and encodes a stereo downmix (ADR-0012).
/// `None` also loses direct play when the profile has a ceiling: an upgraded
/// database may still have NULL after migration 004 until the next probe, and
/// treating that as "within ceiling" would keep direct-playing 5.1 to browsers.
#[allow(clippy::too_many_arguments)]
pub fn decide_playback(
    path: &str,
    container: Option<&str>,
    video_codec: Option<&str>,
    audio_codec: Option<&str>,
    audio_channels: Option<u32>,
    scan_error: Option<&str>,
    probe_status: &str,
    profile: &ClientCapabilityProfile,
) -> PlaybackDecision {
    if probe_status == "indexed" || probe_status == "unavailable" {
        return PlaybackDecision {
            method: PlaybackMethod::Transcode,
            reason: if probe_status == "unavailable" {
                "library unavailable".into()
            } else {
                "probe pending".into()
            },
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
        if let Some(reason) = channel_ceiling_session_reason(audio_channels, profile) {
            return PlaybackDecision {
                method: PlaybackMethod::Remux,
                reason,
                mime_type: "application/vnd.apple.mpegurl".into(),
            };
        }
        if container_ok {
            return PlaybackDecision {
                method: PlaybackMethod::DirectPlay,
                reason: "codecs and container supported by client".into(),
                mime_type: mime_for_path(path),
            };
        }
        return PlaybackDecision {
            method: PlaybackMethod::Remux,
            reason: "codecs supported; container needs a stream-copy session".into(),
            mime_type: "application/vnd.apple.mpegurl".into(),
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

/// Why this title cannot DirectPlay under the profile's channel ceiling.
/// Known over-ceiling counts and unknown (`None`) both force a session when
/// the profile sets `max_audio_channels`: NULL must not pass as safe.
fn channel_ceiling_session_reason(
    audio_channels: Option<u32>,
    profile: &ClientCapabilityProfile,
) -> Option<String> {
    let max = profile.max_audio_channels?;
    match audio_channels {
        Some(c) if c > max => Some(format!(
            "codecs supported; {c}-channel audio exceeds the client ceiling \
             and is downmixed by a session"
        )),
        None => Some(
            "codecs supported; audio channel count not yet stored, \
             session downmix until probed"
                .into(),
        ),
        Some(_) => None,
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

/// Map a `testdata/manifest.json` `expect` string to the Gate 2 decision
/// method, or `None` when the row is not a playback-method claim (sidecars,
/// pending sources, Range-only fixtures).
pub fn method_from_manifest_expect(expect: &str) -> Option<PlaybackMethod> {
    let e = expect.to_ascii_lowercase();
    if e.contains("not a media item")
        || e.contains("once sourced")
        || e.contains("open-ended range")
        || e.contains("range past")
        || e.contains("pending source")
    {
        return None;
    }
    // Session / remux before "direct play" so "session, not direct play" wins.
    if e.contains("remux") || e.contains("session, not direct play") || e.contains("session with") {
        return Some(PlaybackMethod::Remux);
    }
    if e.contains("needs transcode") || e.contains("structured scan_error") {
        return Some(PlaybackMethod::Transcode);
    }
    if e.contains("direct play") {
        return Some(PlaybackMethod::DirectPlay);
    }
    None
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
        decide_channels(
            path,
            container,
            video,
            audio,
            Some(2),
            scan_error,
            probe_status,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn decide_channels(
        path: &str,
        container: Option<&str>,
        video: Option<&str>,
        audio: Option<&str>,
        channels: Option<u32>,
        scan_error: Option<&str>,
        probe_status: &str,
    ) -> PlaybackDecision {
        decide_playback(
            path,
            container,
            video,
            audio,
            channels,
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
            (
                "7.1 aac mp4 needs a session for the downmix",
                decide_channels(
                    "/a/b.mp4",
                    Some("mov,mp4,m4a"),
                    Some("h264"),
                    Some("aac"),
                    Some(8),
                    None,
                    "probed",
                ),
                PlaybackMethod::Remux,
            ),
            (
                "mono aac mp4 stays direct play",
                decide_channels(
                    "/a/b.mp4",
                    Some("mov,mp4,m4a"),
                    Some("h264"),
                    Some("aac"),
                    Some(1),
                    None,
                    "probed",
                ),
                PlaybackMethod::DirectPlay,
            ),
            (
                "unknown channel count forces a session, not unsafe direct play",
                decide_channels(
                    "/a/b.mp4",
                    Some("mov,mp4,m4a"),
                    Some("h264"),
                    Some("aac"),
                    None,
                    None,
                    "probed",
                ),
                PlaybackMethod::Remux,
            ),
        ];
        for (name, decision, expected) in cases {
            assert_eq!(decision.method, expected, "{name}: {}", decision.reason);
        }
    }

    /// ADR-0012: the 7.1 downgrade is user-visible, so the reason has to name
    /// the layout rather than blame the container.
    #[test]
    fn channel_ceiling_session_reports_hls_mime_and_names_the_layout() {
        let d = decide_channels(
            "/a/b.mp4",
            Some("mov,mp4,m4a"),
            Some("h264"),
            Some("aac"),
            Some(8),
            None,
            "probed",
        );
        assert_eq!(d.method, PlaybackMethod::Remux);
        assert_eq!(d.mime_type, "application/vnd.apple.mpegurl");
        assert!(d.reason.contains("8-channel"), "{}", d.reason);
        assert!(!d.reason.contains("container"), "{}", d.reason);
    }

    #[test]
    fn null_channel_count_session_names_the_gap() {
        let d = decide_channels(
            "/a/b.mp4",
            Some("mov,mp4,m4a"),
            Some("h264"),
            Some("aac"),
            None,
            None,
            "probed",
        );
        assert_eq!(d.method, PlaybackMethod::Remux);
        assert!(d.reason.contains("not yet stored"), "{}", d.reason);
    }

    #[test]
    fn remux_reports_hls_mime_not_source_mime() {
        let d = decide(
            "/a/b.mkv",
            Some("matroska,webm"),
            Some("h264"),
            Some("aac"),
            None,
            "probed",
        );
        assert_eq!(d.method, PlaybackMethod::Remux);
        assert_eq!(d.mime_type, "application/vnd.apple.mpegurl");
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

    #[test]
    fn manifest_expect_strings_map_to_methods() {
        assert_eq!(
            method_from_manifest_expect("browser direct play"),
            Some(PlaybackMethod::DirectPlay)
        );
        assert_eq!(
            method_from_manifest_expect("remux as an HLS copy session (container only; ADR-0011)"),
            Some(PlaybackMethod::Remux)
        );
        assert_eq!(
            method_from_manifest_expect(
                "session, not direct play: 8 channels exceed the browser ceiling"
            ),
            Some(PlaybackMethod::Remux)
        );
        assert_eq!(
            method_from_manifest_expect("needs transcode (AC3)"),
            Some(PlaybackMethod::Transcode)
        );
        assert_eq!(
            method_from_manifest_expect("structured scan_error; no crash"),
            Some(PlaybackMethod::Transcode)
        );
        assert_eq!(
            method_from_manifest_expect("not a media item; associated to Movie.mp4"),
            None
        );
    }
}
