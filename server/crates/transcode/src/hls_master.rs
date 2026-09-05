//! Master playlist and the rung identity it advertises.
//!
//! Split out of `hls.rs` on 2026-09-04. That file was 11,239 lines and every
//! slice touching it paid to re-read the whole thing; this is the region
//! ADR-0051's ladder work edits, so it is the one worth isolating first.
//! See `nightjar-meta/docs/plans/2026-09-04-splitting-hls-rs.md`.
//!
//! Nothing here knows about `Session` or the registry: a master playlist is a
//! function of a session id, its subtitle tracks, and the renditions offered.

use crate::hls::HlsSubtitleTrack;

/// Stable identity for a video rendition in the rung-scoped URI namespace.
///
/// This lives beside [`MasterRendition`] because the ladder will bind each
/// name to rendition and encoder state here. `SingleVideo` describes today's
/// sole 5 Mbps-advertised, resolution-unspecified rendition without claiming
/// it is ADR-0051's future high rung.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoRung {
    SingleVideo,
    #[cfg(test)]
    /// A second rung exists only under test until the ladder lands, so the
    /// per-rung isolation this slice introduces can be exercised at the
    /// session level rather than asserted.
    SecondVideo,
}

impl VideoRung {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SingleVideo => "single",
            #[cfg(test)]
            Self::SecondVideo => "second",
        }
    }
}

impl std::str::FromStr for VideoRung {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "single" => Ok(Self::SingleVideo),
            #[cfg(test)]
            "second" => Ok(Self::SecondVideo),
            _ => Err(()),
        }
    }
}

/// One video rendition in the master playlist (ADR-0051).
pub(crate) struct MasterRendition {
    /// The rung this rendition serves. The media playlist URI is derived from
    /// it (ADR-0051 amendment 1), so a rendition cannot advertise a URI in
    /// one rung's namespace while claiming another (Rule 4.9).
    pub rung: VideoRung,
    /// Advertised peak bitrate, the BANDWIDTH attribute.
    pub bandwidth: u64,
    /// `(width, height)` for RESOLUTION, or `None` to omit the attribute.
    pub resolution: Option<(u32, u32)>,
}

impl MasterRendition {
    fn single_video() -> Self {
        Self {
            rung: VideoRung::SingleVideo,
            bandwidth: 5_000_000,
            resolution: None,
        }
    }
}

/// Master playlist for the supplied video renditions and optional SUBTITLES group (ADR-0010).
/// Media and subtitle URIs are path-absolute under `/api/v0/sessions/…`
/// so run-directory depth cannot break client resolution (ADR-0008).
///
/// CODECS is omitted on purpose: a wrong value (we previously advertised
/// Main@L3.1 while VideoToolbox emits High@L4.0) makes Safari native HLS
/// refuse the variant outright. Better no hint than a lying one; the init
/// segment carries the real codec string.
///
/// An empty `renditions` slice emits no `#EXT-X-STREAM-INF` lines. Such a
/// master is useless, so callers must guarantee a non-empty slice.
pub(crate) fn build_master_with_renditions(
    session_id: &str,
    tracks: &[HlsSubtitleTrack],
    renditions: &[MasterRendition],
) -> Vec<u8> {
    use std::fmt::Write;
    let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:7\n");
    if !tracks.is_empty() {
        for t in tracks {
            let lang = t.language.as_deref().unwrap_or("und");
            let default = if t.is_default { "YES" } else { "NO" };
            let forced = if t.forced { "YES" } else { "NO" };
            let autoselect = if t.forced { "NO" } else { "YES" };
            let name = escape_hls_quoted(&t.name);
            let mut line = format!(
                "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"{name}\",\
                 LANGUAGE=\"{lang}\",DEFAULT={default},AUTOSELECT={autoselect},\
                 FORCED={forced},URI=\"/api/v0/sessions/{session_id}/subs/{}.m3u8\"",
                t.track_id
            );
            if t.sdh {
                line.push_str(
                    ",CHARACTERISTICS=\"public.accessibility.transcribes-spoken-dialog\"",
                );
            }
            let _ = writeln!(out, "{line}");
        }
    }
    for rendition in renditions {
        let mut line = format!("#EXT-X-STREAM-INF:BANDWIDTH={}", rendition.bandwidth);
        if let Some((width, height)) = rendition.resolution {
            let _ = write!(line, ",RESOLUTION={width}x{height}");
        }
        if !tracks.is_empty() {
            line.push_str(",SUBTITLES=\"subs\"");
        }
        let _ = writeln!(out, "{line}");
        // The master is the session's (ADR-0054 decision 5), and the media
        // playlist it points at is the rendition's rung's (ADR-0051 amendment
        // 1). The run survives in `EXT-X-MAP` inside that playlist and nowhere
        // else on the wire.
        let _ = writeln!(
            out,
            "/api/v0/sessions/{session_id}/v/{}/index.m3u8",
            rendition.rung.as_str()
        );
    }
    out.into_bytes()
}

/// Builds today's single-rendition master playlist through the generalized
/// rendition path.
pub(crate) fn build_master(session_id: &str, tracks: &[HlsSubtitleTrack]) -> Vec<u8> {
    let renditions = [MasterRendition::single_video()];
    build_master_with_renditions(session_id, tracks, &renditions)
}

pub(crate) fn escape_hls_quoted(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_rung_accepts_only_named_renditions() {
        assert_eq!(VideoRung::SingleVideo.as_str(), "single");
        assert_eq!("single".parse(), Ok(VideoRung::SingleVideo));
        assert_eq!(VideoRung::SecondVideo.as_str(), "second");
        assert_eq!("second".parse(), Ok(VideoRung::SecondVideo));
        for unknown in ["", "default", "high", "Single"] {
            assert_eq!(unknown.parse::<VideoRung>(), Err(()), "{unknown}");
        }
    }
    #[test]
    fn master_playlist_declares_subtitle_group() {
        let tracks = vec![
            HlsSubtitleTrack {
                track_id: "e2".into(),
                language: Some("en".into()),
                name: "SDH".into(),
                is_default: true,
                forced: false,
                sdh: true,
                item_id: 176,
                stream_index: Some(2),
                sidecar_path: None,
                codec: "subrip".into(),
                item_vtt_path: None,
            },
            HlsSubtitleTrack {
                track_id: "e3".into(),
                language: Some("en".into()),
                name: "en".into(),
                is_default: false,
                forced: false,
                sdh: false,
                item_id: 176,
                stream_index: Some(3),
                sidecar_path: None,
                codec: "subrip".into(),
                item_vtt_path: None,
            },
        ];
        let text = String::from_utf8(build_master("s1", &tracks)).unwrap();
        assert!(text.contains("#EXT-X-MEDIA:TYPE=SUBTITLES"));
        assert!(text.contains("GROUP-ID=\"subs\""));
        assert!(text.contains("URI=\"/api/v0/sessions/s1/subs/e2.m3u8\""));
        assert!(text.contains("SUBTITLES=\"subs\""));
        assert!(text.contains("\n/api/v0/sessions/s1/v/single/index.m3u8\n"));
        assert!(
            text.contains("CHARACTERISTICS=\"public.accessibility.transcribes-spoken-dialog\"")
        );
        assert!(!text.contains("CODECS="), "{text}");
        assert!(!text.contains("media.m3u8"));
    }
    #[test]
    fn master_without_tracks_has_no_subtitles_attr() {
        let text = String::from_utf8(build_master("s1", &[])).unwrap();
        assert!(!text.contains("EXT-X-MEDIA"));
        assert!(!text.contains("SUBTITLES="));
        assert!(!text.contains("CODECS="), "{text}");
        assert!(text.contains("\n/api/v0/sessions/s1/v/single/index.m3u8\n"));
    }

    /// A rendition advertises the media playlist of the rung it carries
    /// (ADR-0051 amendment 1), never a flat session URI.
    #[test]
    fn a_rendition_advertises_its_rungs_media_playlist() {
        let text = String::from_utf8(build_master("s1", &[])).unwrap();
        assert!(
            text.contains("\n/api/v0/sessions/s1/v/single/index.m3u8\n"),
            "{text}"
        );
        assert!(
            !text.contains("\n/api/v0/sessions/s1/index.m3u8\n"),
            "the flat media URI must no longer be advertised: {text}"
        );
    }

    #[test]
    fn master_playlist_renders_a_rendition_per_rung_with_subtitles() {
        let tracks = [HlsSubtitleTrack {
            track_id: "e2".into(),
            language: Some("en".into()),
            name: "English".into(),
            is_default: true,
            forced: false,
            sdh: false,
            item_id: 176,
            stream_index: Some(2),
            sidecar_path: None,
            codec: "subrip".into(),
            item_vtt_path: None,
        }];
        let renditions = [
            MasterRendition {
                rung: VideoRung::SingleVideo,
                bandwidth: 6_000_000,
                resolution: Some((1920, 1080)),
            },
            MasterRendition {
                rung: VideoRung::SecondVideo,
                bandwidth: 2_000_000,
                resolution: Some((1280, 720)),
            },
        ];
        let text =
            String::from_utf8(build_master_with_renditions("s1", &tracks, &renditions)).unwrap();
        let lines: Vec<_> = text.lines().collect();
        let stream_lines: Vec<_> = lines
            .iter()
            .copied()
            .filter(|line| line.starts_with("#EXT-X-STREAM-INF:"))
            .collect();

        assert_eq!(stream_lines.len(), 2);
        assert_eq!(
            stream_lines,
            [
                "#EXT-X-STREAM-INF:BANDWIDTH=6000000,RESOLUTION=1920x1080,SUBTITLES=\"subs\"",
                "#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720,SUBTITLES=\"subs\"",
            ]
        );
        assert!(!text.contains("CODECS="), "{text}");
        for (stream_line, uri) in stream_lines.iter().zip([
            "/api/v0/sessions/s1/v/single/index.m3u8",
            "/api/v0/sessions/s1/v/second/index.m3u8",
        ]) {
            let index = lines.iter().position(|line| line == stream_line).unwrap();
            assert_eq!(lines[index + 1], uri);
        }
    }

    /// Two rungs on one session advertise two different media playlist URIs.
    /// Collapse the rung in the writer and the second rendition advertises
    /// rung one's playlist.
    #[test]
    fn each_rung_advertises_its_own_media_playlist_uri() {
        let renditions = [
            MasterRendition {
                rung: VideoRung::SingleVideo,
                bandwidth: 5_000_000,
                resolution: None,
            },
            MasterRendition {
                rung: VideoRung::SecondVideo,
                bandwidth: 5_000_000,
                resolution: None,
            },
        ];
        let text = String::from_utf8(build_master_with_renditions("s1", &[], &renditions)).unwrap();
        assert!(
            text.contains("/api/v0/sessions/s1/v/single/index.m3u8"),
            "{text}"
        );
        assert!(
            text.contains("/api/v0/sessions/s1/v/second/index.m3u8"),
            "{text}"
        );
        assert_eq!(text.matches("/v/single/index.m3u8").count(), 1, "{text}");
        assert_eq!(text.matches("/v/second/index.m3u8").count(), 1, "{text}");
    }
}
