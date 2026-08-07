//! Probe / subtitle / map status strings (ADR-0014, ADR-0023). CHECKs dropped
//! in migration 006; writers validate here instead of rebuilding media_items.

pub const PROBE_STATUSES: &[&str] = &["indexed", "probed", "error", "unavailable"];
pub const SUBTITLE_STATUSES: &[&str] = &[
    "pending",
    "ready",
    "none",
    "eligible",
    "error",
    "unavailable",
];
pub const MAP_STATUSES: &[&str] = &["pending", "ready", "error", "unavailable"];
pub const MAP_CONTAINER_KINDS: &[&str] = &["matroska", "mp4"];

pub fn parse_probe_status(s: &str) -> Result<&str, String> {
    if PROBE_STATUSES.contains(&s) {
        Ok(s)
    } else {
        Err(format!("invalid probe_status: {s}"))
    }
}

pub fn parse_subtitle_status(s: &str) -> Result<&str, String> {
    if SUBTITLE_STATUSES.contains(&s) {
        Ok(s)
    } else {
        Err(format!("invalid subtitle_status: {s}"))
    }
}

pub fn parse_map_status(s: &str) -> Result<&str, String> {
    if MAP_STATUSES.contains(&s) {
        Ok(s)
    } else {
        Err(format!("invalid map_status: {s}"))
    }
}

pub fn parse_map_container_kind(s: &str) -> Result<&str, String> {
    if MAP_CONTAINER_KINDS.contains(&s) {
        Ok(s)
    } else {
        Err(format!("invalid map container_kind: {s}"))
    }
}

/// ADR-0026 §3 backoff schedule: 1 day, then 7, then 30, then 90, capped at
/// 90. Shared by the metadata negative-result cache and the subtitle-extract
/// `unavailable` retry gate so one schedule cannot drift (Rule 4.11).
pub fn backoff_days(attempt_count: i64) -> i64 {
    match attempt_count {
        1 => 1,
        2 => 7,
        3 => 30,
        _ => 90,
    }
}

/// ADR-0041 Decision 1: the derived kind of one subtitle stream row in
/// `media_item_subtitle_tracks` (the migration CHECK admits exactly these).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleTrackKind {
    Text,
    Ass,
    Image,
    Unknown,
}

impl SubtitleTrackKind {
    /// SQL value for the `kind` CHECK in migration 017.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Ass => "ass",
            Self::Image => "image",
            Self::Unknown => "unknown",
        }
    }
}

/// Sidecar input to the classifier (ADR-0041 Decision 2). The probe builds
/// this with nightjar-transcode's existing sidecar-format logic
/// (`is_serveable_sidecar_format` / `is_burn_in_sidecar_format`); the db
/// crate stays free of that dependency, so the verdict arrives precomputed
/// and "does this sidecar convert" keeps a single implementation (Rule 4.11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidecarPresence {
    /// No `media_item_sidecars` row for the item.
    None,
    /// Sidecar the existing pipeline turns into WebVTT (srt/vtt).
    ConvertsToWebVtt,
    /// Sidecar present but burn-in-owned (ass/ssa, ADR-0019): no WebVTT.
    BurnInOnly,
}

/// Classify `subtitle_status` from the persisted track inventory plus sidecar
/// presence (ADR-0041 Decision 2). Pure: no I/O, no ffmpeg, no DB reads.
///
/// Returns one of `SUBTITLE_STATUSES` (`none` | `eligible`); every other
/// value (`pending | ready | error | unavailable`) is written by the
/// pipeline, never by this classification.
///
/// - no text/ass track and no sidecar → `none`
/// - image-only, no text/ass, no sidecar → `none`
/// - sidecar that converts to WebVTT → `eligible`
/// - sidecar present but burn-in-owned (ass/ssa) → not WebVTT-eligible
/// - one or more embedded `text`/`ass` rows → `eligible`
/// - any `unknown` row is counted, never silently dropped: it can be neither
///   proven text (extractable) nor proven image (burn-in), so classifying it
///   as `none` would drop it as harmless. It stays `eligible` instead
///   (Decision 1: the measured library has n_unknown = 0, so this path is
///   exercised only by future or unusual sources and must not be a silent
///   no-op when it is).
pub fn classify_subtitle_status(
    tracks: &[SubtitleTrackKind],
    sidecar: SidecarPresence,
) -> &'static str {
    let has_text_or_ass = tracks
        .iter()
        .any(|t| matches!(t, SubtitleTrackKind::Text | SubtitleTrackKind::Ass));
    let has_unknown = tracks.contains(&SubtitleTrackKind::Unknown);
    if has_text_or_ass || has_unknown {
        return "eligible";
    }
    if sidecar == SidecarPresence::ConvertsToWebVtt {
        return "eligible";
    }
    "none"
}

#[cfg(test)]
mod tests {
    use super::*;
    use SidecarPresence as S;
    use SubtitleTrackKind as K;

    #[test]
    fn accepts_unavailable() {
        assert_eq!(parse_probe_status("unavailable").unwrap(), "unavailable");
        assert_eq!(parse_subtitle_status("unavailable").unwrap(), "unavailable");
        assert_eq!(parse_map_status("unavailable").unwrap(), "unavailable");
    }

    #[test]
    fn accepts_eligible() {
        assert_eq!(parse_subtitle_status("eligible").unwrap(), "eligible");
    }

    #[test]
    fn accepts_map_kinds() {
        assert_eq!(parse_map_container_kind("matroska").unwrap(), "matroska");
        assert_eq!(parse_map_container_kind("mp4").unwrap(), "mp4");
    }

    /// ADR-0026 §3 shape: 1d, 7d, 30d, then capped at 90d. One schedule, one
    /// function (Rule 4.11); metadata and the subtitle retry gate both call it.
    #[test]
    fn backoff_schedule_matches_adr_0026_s3() {
        assert_eq!(backoff_days(1), 1);
        assert_eq!(backoff_days(2), 7);
        assert_eq!(backoff_days(3), 30);
        assert_eq!(backoff_days(4), 90);
        assert_eq!(backoff_days(5), 90, "capped at 90");
    }

    #[test]
    fn rejects_unknown() {
        assert!(parse_probe_status("nope").is_err());
        assert!(parse_subtitle_status("nope").is_err());
        assert!(parse_map_status("nope").is_err());
        assert!(parse_map_container_kind("avi").is_err());
    }

    /// ADR-0041 Decision 2, table-driven over every classification branch.
    #[test]
    fn classifies_subtitle_status() {
        let cases: &[(&str, &[K], S, &str)] = &[
            // zero tracks + no sidecar → none
            ("zero tracks, no sidecar", &[], S::None, "none"),
            // no text/ass track and no sidecar → none
            ("image-only, no sidecar", &[K::Image], S::None, "none"),
            (
                "image-only multi, no sidecar",
                &[K::Image, K::Image, K::Image],
                S::None,
                "none",
            ),
            // sidecar text present → eligible if it converts
            (
                "sidecar converts, no tracks",
                &[],
                S::ConvertsToWebVtt,
                "eligible",
            ),
            (
                "sidecar converts over image-only",
                &[K::Image],
                S::ConvertsToWebVtt,
                "eligible",
            ),
            // sidecar present but not convertible → per existing sidecar
            // convert logic (ass/ssa is burn-in owned, ADR-0019): no WebVTT
            ("sidecar burn-in only", &[], S::BurnInOnly, "none"),
            (
                "image-only with burn-in sidecar",
                &[K::Image],
                S::BurnInOnly,
                "none",
            ),
            // one or more embedded text/ass rows → eligible
            ("one embedded text", &[K::Text], S::None, "eligible"),
            ("one embedded ass", &[K::Ass], S::None, "eligible"),
            (
                "embedded text plus sidecar",
                &[K::Text],
                S::ConvertsToWebVtt,
                "eligible",
            ),
            // unknown rows are counted, never silently dropped
            (
                "unknown-only is not dropped",
                &[K::Unknown],
                S::None,
                "eligible",
            ),
            (
                "unknown among image rows is not dropped",
                &[K::Image, K::Unknown],
                S::None,
                "eligible",
            ),
            (
                "unknown with burn-in sidecar",
                &[K::Unknown],
                S::BurnInOnly,
                "eligible",
            ),
            // multi-track (≥3, one unknown codec) → eligible, unknown counted
            (
                "multi-track with one unknown",
                &[K::Text, K::Ass, K::Image, K::Unknown],
                S::None,
                "eligible",
            ),
        ];
        for (name, tracks, sidecar, expected) in cases {
            assert_eq!(
                classify_subtitle_status(tracks, *sidecar),
                *expected,
                "{name}"
            );
        }
    }

    #[test]
    fn classifier_output_is_a_writable_status() {
        // The classifier's output must be a value `set_subtitle_status`
        // accepts, i.e. a member of SUBTITLE_STATUSES (parse_subtitle_status).
        let tracks = [K::Text, K::Unknown];
        let status = classify_subtitle_status(&tracks, S::None);
        assert_eq!(status, "eligible");
        assert!(SUBTITLE_STATUSES.contains(&status));
        assert_eq!(parse_subtitle_status(status).unwrap(), "eligible");
    }
}
