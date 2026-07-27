//! Shared allowlists for path-segment IDs (item / session subtitle assets).
//!
//! These IDs are joined under a store or session directory. Reject anything
//! that is not a single normal path component so `..`, separators, and
//! absolute forms cannot escape that directory (Phase 3 streaming criterion).

use std::path::{Component, Path};

/// Embedded `e{N}` or sidecar `s-…` track id used in URLs and on disk.
pub(crate) fn is_valid_track_id(id: &str) -> bool {
    if id.is_empty() || id.contains('\0') {
        return false;
    }
    let mut components = Path::new(id).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => {}
        _ => return false,
    }
    let mut chars = id.chars();
    match chars.next() {
        Some('e') | Some('s') => {
            chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        }
        _ => false,
    }
}

/// `{trackId}.m3u8` or `{trackId}/segNNN.vtt` under `/sessions/.../subs/{*asset}`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SessionSubtitleAsset {
    Playlist { track_id: String },
    Segment { track_id: String, index: u64 },
}

pub(crate) fn parse_session_subtitle_asset(asset: &str) -> Option<SessionSubtitleAsset> {
    if asset.contains('\0') {
        return None;
    }
    if let Some(track_id) = asset.strip_suffix(".m3u8") {
        if !is_valid_track_id(track_id) {
            return None;
        }
        return Some(SessionSubtitleAsset::Playlist {
            track_id: track_id.to_string(),
        });
    }

    let (track_id, seg_name) = asset.split_once('/')?;
    if seg_name.contains('/') || seg_name.contains('\\') {
        return None;
    }
    if !is_valid_track_id(track_id) {
        return None;
    }
    let index = seg_name
        .strip_prefix("seg")?
        .strip_suffix(".vtt")
        .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))?
        .parse()
        .ok()?;
    Some(SessionSubtitleAsset::Segment {
        track_id: track_id.to_string(),
        index,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_real_track_ids() {
        assert!(is_valid_track_id("e2"));
        assert!(is_valid_track_id("s-en"));
        assert!(is_valid_track_id("s-en.forced"));
        assert!(is_valid_track_id("s-Subs.en"));
    }

    #[test]
    fn rejects_path_traversal_track_ids() {
        for id in [
            "..",
            "../e2",
            "e2/..",
            "e2/../e3",
            "/e2",
            "e2\\x",
            "",
            "x2",
            "e2/seg000.vtt",
        ] {
            assert!(!is_valid_track_id(id), "expected reject: {id:?}");
        }
    }

    #[test]
    fn parses_playlist_and_segment() {
        assert_eq!(
            parse_session_subtitle_asset("e2.m3u8"),
            Some(SessionSubtitleAsset::Playlist {
                track_id: "e2".into()
            })
        );
        assert_eq!(
            parse_session_subtitle_asset("s-en.forced/seg012.vtt"),
            Some(SessionSubtitleAsset::Segment {
                track_id: "s-en.forced".into(),
                index: 12
            })
        );
    }

    #[test]
    fn rejects_traversal_and_arbitrary_paths() {
        // Axum URL-decodes path params once before extract; these are the
        // post-decode forms that must 404 rather than touch the filesystem.
        for asset in [
            "../seg000.vtt",
            "e2/../../../etc/passwd",
            "e2/seg000.vtt/../../passwd",
            "e2/../seg000.vtt",
            "../../etc/passwd.m3u8",
            "..m3u8",
            "e2/seg000.m4s",
            "e2/full.vtt",
            "e2/../full.vtt",
            "seg000.vtt",
            "e2/seg00a.vtt",
            "e2//seg000.vtt",
        ] {
            assert!(
                parse_session_subtitle_asset(asset).is_none(),
                "expected reject: {asset:?}"
            );
        }
    }

    #[test]
    fn segment_index_is_not_a_filesystem_join() {
        // Only the integer is kept; a forged name cannot become a read path.
        let Some(SessionSubtitleAsset::Segment { index, .. }) =
            parse_session_subtitle_asset("e2/seg000.vtt")
        else {
            panic!("expected segment");
        };
        assert_eq!(index, 0);
    }
}
