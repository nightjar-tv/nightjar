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

/// Highest HDR the client accepts (ADR-0022). Source above this forces
/// transcode; the session applies a real tonemap graph when encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HdrCapability {
    None,
    Hdr10,
    DolbyVision,
}

impl HdrCapability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Hdr10 => "hdr10",
            Self::DolbyVision => "dolbyVision",
        }
    }

    /// Parse a stored source or wire value (`none` / `hdr10` / `dolbyVision`).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "hdr10" => Some(Self::Hdr10),
            "dolbyVision" | "dolby_vision" => Some(Self::DolbyVision),
            _ => None,
        }
    }

    fn accepts(self, source: HdrCapability) -> bool {
        match source {
            HdrCapability::None => true,
            HdrCapability::Hdr10 => matches!(self, Self::Hdr10 | Self::DolbyVision),
            HdrCapability::DolbyVision => matches!(self, Self::DolbyVision),
        }
    }
}

/// What a client can play natively (ADR-0006 / ADR-0022).
#[derive(Debug, Clone, Copy)]
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
    /// Source video bitrate above this forces transcode. `None` = no ceiling.
    pub max_bitrate_bps: Option<u64>,
    /// Source height above this forces transcode. `None` = no ceiling.
    pub max_height: Option<u32>,
    pub hdr: HdrCapability,
}

/// Phase 1 browser whitelist: H.264 family + AAC in MP4/M4V, stereo audio.
pub const BROWSER_V0: ClientCapabilityProfile = ClientCapabilityProfile {
    video_codecs: &["h264", "avc", "avc1"],
    audio_codecs: &["aac", "mp4a"],
    containers: &["mp4", "m4v", "mov"],
    extensions: &["mp4", "m4v"],
    max_audio_channels: Some(2),
    max_bitrate_bps: None,
    max_height: None,
    hdr: HdrCapability::None,
};

/// Android / Android TV Media3 floor (ADR-0022). Wide codecs; no bitrate /
/// height ceiling on LAN dogfood.
pub const MEDIA3_V0: ClientCapabilityProfile = ClientCapabilityProfile {
    video_codecs: &[
        "h264",
        "avc",
        "avc1",
        "hevc",
        "h265",
        "hev1",
        "av1",
        "vp9",
        "vp8",
        "mpeg2video",
        "mpeg4",
    ],
    audio_codecs: &[
        "aac", "mp4a", "ac3", "eac3", "truehd", "dts", "flac", "opus", "mp3", "vorbis",
    ],
    containers: &[
        "mp4", "m4v", "mov", "matroska", "webm", "avi", "mpegts", "mpeg",
    ],
    extensions: &["mp4", "m4v", "mkv", "webm", "avi", "ts", "m2ts", "mov"],
    max_audio_channels: None,
    max_bitrate_bps: None,
    max_height: None,
    hdr: HdrCapability::DolbyVision,
};

/// libmpv / media_kit floor (ADR-0022). Same wide accept list as Media3 for
/// v0; no remote bitrate ceiling.
pub const MPV_V0: ClientCapabilityProfile = ClientCapabilityProfile {
    video_codecs: &[
        "h264",
        "avc",
        "avc1",
        "hevc",
        "h265",
        "hev1",
        "av1",
        "vp9",
        "vp8",
        "mpeg2video",
        "mpeg4",
        "vc1",
    ],
    audio_codecs: &[
        "aac",
        "mp4a",
        "ac3",
        "eac3",
        "truehd",
        "dts",
        "dtshd",
        "flac",
        "opus",
        "mp3",
        "vorbis",
        "pcm_s16le",
    ],
    containers: &[
        "mp4", "m4v", "mov", "matroska", "webm", "avi", "mpegts", "mpeg",
    ],
    extensions: &["mp4", "m4v", "mkv", "webm", "avi", "ts", "m2ts", "mov"],
    max_audio_channels: None,
    max_bitrate_bps: None,
    max_height: None,
    hdr: HdrCapability::DolbyVision,
};

/// Apple Aether floor (ADR-0022): Matroska demux + AVPlayer loopback; client
/// bridges audio AVPlayer rejects. T1-scored against dogfood (same accept
/// shape as Media3 for decide_playback).
pub const AETHER_V0: ClientCapabilityProfile = ClientCapabilityProfile {
    video_codecs: &[
        "h264",
        "avc",
        "avc1",
        "hevc",
        "h265",
        "hev1",
        "av1",
        "vp9",
        "vp8",
        "mpeg2video",
        "mpeg4",
    ],
    audio_codecs: &[
        "aac", "mp4a", "ac3", "eac3", "truehd", "dts", "flac", "opus", "mp3", "vorbis", "alac",
    ],
    containers: &[
        "mp4", "m4v", "mov", "matroska", "webm", "avi", "mpegts", "mpeg",
    ],
    extensions: &["mp4", "m4v", "mkv", "webm", "avi", "ts", "m2ts", "mov"],
    max_audio_channels: None,
    max_bitrate_bps: None,
    max_height: None,
    hdr: HdrCapability::DolbyVision,
};

/// Named profile from a client `profileId`, or `None` when unknown.
pub fn known_profile(id: &str) -> Option<&'static ClientCapabilityProfile> {
    match id {
        "BROWSER_V0" => Some(&BROWSER_V0),
        "MEDIA3_V0" => Some(&MEDIA3_V0),
        "MPV_V0" => Some(&MPV_V0),
        "AETHER_V0" => Some(&AETHER_V0),
        _ => None,
    }
}

/// Resolve a client profile id. Omitted / empty / unknown → `BROWSER_V0`.
pub fn resolve_profile(id: Option<&str>) -> &'static ClientCapabilityProfile {
    match id {
        None | Some("") => &BROWSER_V0,
        Some(id) => known_profile(id).unwrap_or(&BROWSER_V0),
    }
}

/// Optional wire overrides on top of a named (or fallback) profile (ADR-0022
/// field bag). Unknown id without overrides stays `BROWSER_V0`.
pub fn resolve_profile_bag(
    id: Option<&str>,
    max_bitrate_bps: Option<u64>,
    max_height: Option<u32>,
    hdr: Option<&str>,
) -> ClientCapabilityProfile {
    let mut profile = *resolve_profile(id);
    if let Some(bps) = max_bitrate_bps {
        profile.max_bitrate_bps = Some(bps);
    }
    if let Some(h) = max_height {
        profile.max_height = Some(h);
    }
    if let Some(h) = hdr.and_then(HdrCapability::parse) {
        profile.hdr = h;
    }
    profile
}

/// Encode knobs for an HLS re-encode session (ADR-0022). Applied only when
/// `SessionMode::Transcode`; remux/copy never scales or tone-maps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VideoEncodePlan {
    /// Cap output height (FFmpeg `scale=-2:min(H,ih)`). `None` = keep source.
    pub max_height: Option<u32>,
    /// Target video bitrate. `None` = encoder default.
    pub max_bitrate_bps: Option<u64>,
    /// Source is HDR and the session encodes H.264 SDR → real tonemap graph.
    pub tone_map: bool,
}

/// Build the encode plan from source probe fields × profile ceilings.
///
/// Height/bitrate caps apply when the profile sets them and the source is
/// over (or height is unknown). HDR sources always tone-map on transcode:
/// session output is H.264 SDR regardless of profile passthrough (passthrough
/// is DirectPlay/Remux only).
pub fn video_encode_plan(
    source_height: Option<u32>,
    source_bitrate_bps: Option<u64>,
    source_hdr: Option<&str>,
    profile: &ClientCapabilityProfile,
) -> VideoEncodePlan {
    let max_height = match profile.max_height {
        Some(max) => match source_height {
            Some(h) if h > max => Some(max),
            None => Some(max),
            Some(_) => None,
        },
        None => None,
    };
    let max_bitrate_bps = match profile.max_bitrate_bps {
        Some(max) => match source_bitrate_bps {
            Some(b) if b > max => Some(max),
            None => Some(max),
            Some(_) => None,
        },
        None => None,
    };
    let tone_map = matches!(
        source_hdr
            .and_then(HdrCapability::parse)
            .unwrap_or(HdrCapability::None),
        HdrCapability::Hdr10 | HdrCapability::DolbyVision
    );
    VideoEncodePlan {
        max_height,
        max_bitrate_bps,
        tone_map,
    }
}

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
///
/// `height`, `bitrate_bps`, and `source_hdr` (`none` / `hdr10` /
/// `dolbyVision`) apply ADR-0022 ceilings: over ceiling forces **transcode**
/// (re-encode), not remux.
///
/// `tonemap_available` is the host FFmpeg `zscale` probe (ADR-0022). When a
/// session would re-encode HDR without it, the reason names the gap so
/// session start can refuse before spawn.
#[allow(clippy::too_many_arguments)]
pub fn decide_playback(
    path: &str,
    container: Option<&str>,
    video_codec: Option<&str>,
    audio_codec: Option<&str>,
    audio_channels: Option<u32>,
    height: Option<u32>,
    bitrate_bps: Option<u64>,
    source_hdr: Option<&str>,
    scan_error: Option<&str>,
    probe_status: &str,
    profile: &ClientCapabilityProfile,
    tonemap_available: bool,
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

    let mut decision = if video_ok && audio_ok {
        if let Some(reason) =
            profile_ceiling_transcode_reason(height, bitrate_bps, source_hdr, profile)
        {
            PlaybackDecision {
                method: PlaybackMethod::Transcode,
                reason,
                mime_type: "application/vnd.apple.mpegurl".into(),
            }
        } else if let Some(reason) = channel_ceiling_session_reason(audio_channels, profile) {
            PlaybackDecision {
                method: PlaybackMethod::Remux,
                reason,
                mime_type: "application/vnd.apple.mpegurl".into(),
            }
        } else if container_ok {
            PlaybackDecision {
                method: PlaybackMethod::DirectPlay,
                reason: "codecs and container supported by client".into(),
                mime_type: mime_for_path(path),
            }
        } else {
            PlaybackDecision {
                method: PlaybackMethod::Remux,
                reason: "codecs supported; container needs a stream-copy session".into(),
                mime_type: "application/vnd.apple.mpegurl".into(),
            }
        }
    } else {
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
    };

    if decision.method == PlaybackMethod::Transcode
        && source_needs_tonemap(source_hdr)
        && !tonemap_available
    {
        decision.reason = format!(
            "host FFmpeg lacks zscale/libzimg; cannot tone-map HDR ({})",
            decision.reason
        );
    }

    decision
}

fn source_needs_tonemap(source_hdr: Option<&str>) -> bool {
    matches!(
        source_hdr
            .and_then(HdrCapability::parse)
            .unwrap_or(HdrCapability::None),
        HdrCapability::Hdr10 | HdrCapability::DolbyVision
    )
}

/// ADR-0022 bitrate / height / HDR ceilings force a re-encode session.
fn profile_ceiling_transcode_reason(
    height: Option<u32>,
    bitrate_bps: Option<u64>,
    source_hdr: Option<&str>,
    profile: &ClientCapabilityProfile,
) -> Option<String> {
    if let Some(max_h) = profile.max_height
        && let Some(h) = height
        && h > max_h
    {
        return Some(format!(
            "source height {h} exceeds profile maxHeight {max_h}"
        ));
    }
    if let Some(max_b) = profile.max_bitrate_bps
        && let Some(b) = bitrate_bps
        && b > max_b
    {
        return Some(format!(
            "source bitrate {b} exceeds profile maxBitrateBps {max_b}"
        ));
    }
    if let Some(raw) = source_hdr.filter(|s| !s.is_empty()) {
        let source = HdrCapability::parse(raw).unwrap_or(HdrCapability::None);
        if !profile.hdr.accepts(source) {
            return Some(format!(
                "source HDR {} exceeds profile hdr {}",
                source.as_str(),
                profile.hdr.as_str()
            ));
        }
    }
    None
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
            None,
            None,
            Some("none"),
            scan_error,
            probe_status,
            &BROWSER_V0,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn decide_channels(
        path: &str,
        container: Option<&str>,
        video: Option<&str>,
        audio: Option<&str>,
        channels: Option<u32>,
        height: Option<u32>,
        bitrate_bps: Option<u64>,
        source_hdr: Option<&str>,
        scan_error: Option<&str>,
        probe_status: &str,
        profile: &ClientCapabilityProfile,
    ) -> PlaybackDecision {
        decide_playback(
            path,
            container,
            video,
            audio,
            channels,
            height,
            bitrate_bps,
            source_hdr,
            scan_error,
            probe_status,
            profile,
            true,
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
                    None,
                    Some("none"),
                    None,
                    "probed",
                    &BROWSER_V0,
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
                    None,
                    Some("none"),
                    None,
                    "probed",
                    &BROWSER_V0,
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
                    None,
                    Some("none"),
                    None,
                    "probed",
                    &BROWSER_V0,
                ),
                PlaybackMethod::Remux,
            ),
        ];
        for (name, decision, expected) in cases {
            assert_eq!(decision.method, expected, "{name}: {}", decision.reason);
        }
    }

    #[test]
    fn channel_ceiling_session_reports_hls_mime_and_names_the_layout() {
        let d = decide_channels(
            "/a/b.mp4",
            Some("mov,mp4,m4a"),
            Some("h264"),
            Some("aac"),
            Some(8),
            None,
            None,
            Some("none"),
            None,
            "probed",
            &BROWSER_V0,
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
            None,
            Some("none"),
            None,
            "probed",
            &BROWSER_V0,
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
    fn height_ceiling_forces_transcode() {
        let capped = ClientCapabilityProfile {
            max_height: Some(1080),
            hdr: HdrCapability::None,
            ..BROWSER_V0
        };
        let d = decide_channels(
            "/a/b.mp4",
            Some("mov,mp4,m4a"),
            Some("h264"),
            Some("aac"),
            Some(2),
            Some(2160),
            None,
            Some("none"),
            None,
            "probed",
            &capped,
        );
        assert_eq!(d.method, PlaybackMethod::Transcode);
        assert!(d.reason.contains("maxHeight"), "{}", d.reason);
    }

    #[test]
    fn bitrate_ceiling_forces_transcode() {
        let capped = ClientCapabilityProfile {
            max_bitrate_bps: Some(5_000_000),
            ..BROWSER_V0
        };
        let d = decide_channels(
            "/a/b.mp4",
            Some("mov,mp4,m4a"),
            Some("h264"),
            Some("aac"),
            Some(2),
            Some(1080),
            Some(40_000_000),
            Some("none"),
            None,
            "probed",
            &capped,
        );
        assert_eq!(d.method, PlaybackMethod::Transcode);
        assert!(d.reason.contains("maxBitrateBps"), "{}", d.reason);
    }

    #[test]
    fn hdr_mismatch_forces_transcode_on_browser() {
        let d = decide_channels(
            "/a/b.mp4",
            Some("mov,mp4,m4a"),
            Some("h264"),
            Some("aac"),
            Some(2),
            None,
            None,
            Some("hdr10"),
            None,
            "probed",
            &BROWSER_V0,
        );
        assert_eq!(d.method, PlaybackMethod::Transcode);
        assert!(d.reason.contains("HDR"), "{}", d.reason);
    }

    #[test]
    fn mpv_direct_plays_mkv_hevc() {
        let d = decide_channels(
            "/a/b.mkv",
            Some("matroska,webm"),
            Some("hevc"),
            Some("aac"),
            Some(2),
            None,
            None,
            Some("none"),
            None,
            "probed",
            &MPV_V0,
        );
        assert_eq!(d.method, PlaybackMethod::DirectPlay, "{}", d.reason);
    }

    #[test]
    fn resolve_profile_unknown_falls_back_to_browser() {
        assert_eq!(
            resolve_profile(None).max_audio_channels,
            BROWSER_V0.max_audio_channels
        );
        assert_eq!(
            resolve_profile(Some("nope")).video_codecs,
            BROWSER_V0.video_codecs
        );
        assert_eq!(
            resolve_profile(Some("MPV_V0")).video_codecs,
            MPV_V0.video_codecs
        );
        assert!(known_profile("MEDIA3_V0").is_some());
        assert!(known_profile("AETHER_V0").is_some());
        assert!(known_profile("ghost").is_none());
    }

    #[test]
    fn encode_plan_scales_and_caps_bitrate_when_over_ceiling() {
        let capped = ClientCapabilityProfile {
            max_height: Some(1080),
            max_bitrate_bps: Some(5_000_000),
            ..BROWSER_V0
        };
        let plan = video_encode_plan(Some(2160), Some(40_000_000), Some("none"), &capped);
        assert_eq!(plan.max_height, Some(1080));
        assert_eq!(plan.max_bitrate_bps, Some(5_000_000));
        assert!(!plan.tone_map);
    }

    #[test]
    fn encode_plan_skips_scale_when_already_under() {
        let capped = ClientCapabilityProfile {
            max_height: Some(1080),
            ..BROWSER_V0
        };
        let plan = video_encode_plan(Some(720), Some(2_000_000), Some("none"), &capped);
        assert_eq!(plan.max_height, None);
        assert!(!plan.tone_map);
    }

    #[test]
    fn encode_plan_tone_maps_hdr_sources() {
        let plan = video_encode_plan(Some(1080), None, Some("hdr10"), &BROWSER_V0);
        assert!(plan.tone_map);
        let dv = video_encode_plan(Some(1080), None, Some("dolby_vision"), &MEDIA3_V0);
        assert!(dv.tone_map);
    }

    #[test]
    fn missing_host_tonemap_names_zscale_in_reason() {
        let d = decide_playback(
            "/a/b.mp4",
            Some("mov,mp4,m4a"),
            Some("hevc"),
            Some("aac"),
            Some(2),
            Some(1080),
            None,
            Some("hdr10"),
            None,
            "probed",
            &BROWSER_V0,
            false,
        );
        assert_eq!(d.method, PlaybackMethod::Transcode);
        assert!(d.reason.contains("lacks zscale"), "{}", d.reason);
    }

    #[test]
    fn profile_bag_overrides_ceilings_on_unknown_id() {
        let p = resolve_profile_bag(
            Some("TIZEN_FUTURE"),
            Some(3_000_000),
            Some(720),
            Some("hdr10"),
        );
        assert_eq!(p.max_bitrate_bps, Some(3_000_000));
        assert_eq!(p.max_height, Some(720));
        assert_eq!(p.hdr, HdrCapability::Hdr10);
        // Unknown id keeps browser codec floor until the bag grows codecs.
        assert_eq!(p.video_codecs, BROWSER_V0.video_codecs);
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
