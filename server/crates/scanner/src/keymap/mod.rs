//! Keyframe map extraction (ADR-0023: keyframe map and byte-offset session
//! start). Builds a per-video-track table of keyframe presentation time to
//! file byte offset, later used for byte-offset session start instead of a
//! cold `-ss` seek.
//!
//! Index-first: read the container's own index (Matroska Cues, MP4 sync
//! sample tables) — a header-scale read. Only when that index is missing or
//! empty does the build fall back to an ffprobe packet walk, which demuxes
//! the whole file and is reserved for the minority of damaged or
//! index-less sources (ADR-0023 §2).

mod matroska;
mod mp4;
mod packet_walk;

use std::fs::File;
use std::io::Read;
use std::path::Path;

/// One keyframe: presentation time and the file byte offset playback can
/// start from. Byte offset meaning is container-kind-specific (ADR-0023 §1):
/// a Matroska Cluster start for `matroska`, a sync sample start for `mp4`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyframeEntry {
    pub pts_ms: i64,
    pub byte_offset: i64,
}

#[derive(Debug, Clone)]
pub struct KeyframeMapBuild {
    pub container_kind: &'static str,
    pub entries: Vec<KeyframeEntry>,
    /// DEF-8519-class damage signal: set when the last mapped keyframe sits
    /// materially short of the probed duration (truncated index or file).
    pub usable_extent_ms: Option<i64>,
    pub source: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainerKind {
    Matroska,
    Mp4,
}

impl ContainerKind {
    fn as_str(self) -> &'static str {
        match self {
            ContainerKind::Matroska => "matroska",
            ContainerKind::Mp4 => "mp4",
        }
    }
}

/// Floor of the truncated-index damage window (ADR-0023 §2 / DEF-8519 family).
const MAP_SHORTFALL_FLOOR_MS: i64 = 5_000;
/// Fraction-of-duration alternative to the floor, for long titles.
const MAP_SHORTFALL_PCT: f64 = 0.02;

/// Build the keyframe map for the video stream at `path`. `duration_ms` is
/// the probed title duration, if known, used only to derive
/// [`KeyframeMapBuild::usable_extent_ms`].
pub fn build_keyframe_map(
    path: &Path,
    duration_ms: Option<i64>,
) -> Result<KeyframeMapBuild, String> {
    let kind = detect_container_kind(path)?;
    let (entries, source) = match kind {
        ContainerKind::Matroska => index_then_packet_walk(path, matroska::build_from_cues),
        ContainerKind::Mp4 => index_then_packet_walk(path, mp4::build_from_sample_tables),
    }?;

    Ok(KeyframeMapBuild {
        container_kind: kind.as_str(),
        usable_extent_ms: usable_extent(&entries, duration_ms),
        entries,
        source,
    })
}

fn index_then_packet_walk(
    path: &Path,
    build_index: impl Fn(&Path) -> Result<Vec<KeyframeEntry>, String>,
) -> Result<(Vec<KeyframeEntry>, &'static str), String> {
    match build_index(path) {
        Ok(entries) if !entries.is_empty() => return Ok((entries, "index")),
        Ok(_) => {}
        Err(e) => tracing::debug!(
            path = %path.display(),
            error = %e,
            "keyframe index read failed; falling back to packet walk"
        ),
    }
    let entries = packet_walk::walk(path)?;
    if entries.is_empty() {
        return Err(format!(
            "no keyframes found via index or packet walk: {}",
            path.display()
        ));
    }
    Ok((entries, "packet_walk"))
}

fn detect_container_kind(path: &Path) -> Result<ContainerKind, String> {
    let mut file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut magic = [0u8; 4];
    let read = file
        .read(&mut magic)
        .map_err(|e| format!("read magic for {}: {e}", path.display()))?;
    if read == 4 && u32::from_be_bytes(magic) == matroska::EBML_HEADER_ID {
        return Ok(ContainerKind::Matroska);
    }
    if mp4::looks_like_mp4(path)? {
        return Ok(ContainerKind::Mp4);
    }
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("mkv" | "mka" | "webm") => Ok(ContainerKind::Matroska),
        Some("mp4" | "m4v" | "mov") => Ok(ContainerKind::Mp4),
        _ => Err(format!(
            "cannot determine container kind for {}",
            path.display()
        )),
    }
}

fn usable_extent(entries: &[KeyframeEntry], duration_ms: Option<i64>) -> Option<i64> {
    let duration_ms = duration_ms?;
    let last = entries.last()?.pts_ms;
    let threshold =
        MAP_SHORTFALL_FLOOR_MS.max((duration_ms as f64 * MAP_SHORTFALL_PCT).round() as i64);
    (duration_ms - last > threshold).then_some(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn usable_extent_none_when_within_floor() {
        let entries = vec![KeyframeEntry {
            pts_ms: 596_000,
            byte_offset: 0,
        }];
        assert_eq!(usable_extent(&entries, Some(600_000)), None);
    }

    #[test]
    fn usable_extent_flags_flat_floor_shortfall() {
        let entries = vec![KeyframeEntry {
            pts_ms: 10_000,
            byte_offset: 0,
        }];
        assert_eq!(usable_extent(&entries, Some(20_000)), Some(10_000));
    }

    #[test]
    fn usable_extent_flags_percentage_shortfall_on_long_titles() {
        // 2% of 3_600_000 = 72_000, bigger than the 5_000 ms floor.
        let entries = vec![KeyframeEntry {
            pts_ms: 3_500_000,
            byte_offset: 0,
        }];
        assert_eq!(usable_extent(&entries, Some(3_600_000)), Some(3_500_000));
    }

    #[test]
    fn usable_extent_none_without_duration_or_entries() {
        assert_eq!(usable_extent(&[], Some(1000)), None);
        let entries = vec![KeyframeEntry {
            pts_ms: 0,
            byte_offset: 0,
        }];
        assert_eq!(usable_extent(&entries, None), None);
    }

    #[test]
    fn detect_container_kind_falls_back_to_extension() {
        let dir = tempfile::tempdir().unwrap();
        let mkv = dir.path().join("weird.mkv");
        std::fs::write(&mkv, b"not really ebml but named mkv").unwrap();
        assert_eq!(
            detect_container_kind(&mkv).unwrap(),
            ContainerKind::Matroska
        );

        let mp4 = dir.path().join("weird.mp4");
        std::fs::write(&mp4, b"not really iso bmff but named mp4").unwrap();
        assert_eq!(detect_container_kind(&mp4).unwrap(), ContainerKind::Mp4);
    }

    #[test]
    fn detect_container_kind_errors_on_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let unknown = dir.path().join("weird.bin");
        std::fs::write(&unknown, b"nothing recognizable here").unwrap();
        assert!(detect_container_kind(&unknown).is_err());
    }

    fn testdata_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files")
            .join(name)
    }

    #[test]
    fn builds_map_for_real_mkv_corpus_file() {
        let path = testdata_path("h264_aac_mkv.mkv");
        if !path.exists() {
            return;
        }
        let build = build_keyframe_map(&path, Some(2000)).unwrap();
        assert_eq!(build.container_kind, "matroska");
        assert!(!build.entries.is_empty());
        assert!(build.entries.windows(2).all(|w| w[0].pts_ms <= w[1].pts_ms));
    }

    #[test]
    fn builds_map_for_real_mp4_corpus_file() {
        let path = testdata_path("h264_aac_mp4.mp4");
        if !path.exists() {
            return;
        }
        let build = build_keyframe_map(&path, Some(2000)).unwrap();
        assert_eq!(build.container_kind, "mp4");
        assert!(!build.entries.is_empty());
        assert!(build.entries.windows(2).all(|w| w[0].pts_ms <= w[1].pts_ms));
    }
}
