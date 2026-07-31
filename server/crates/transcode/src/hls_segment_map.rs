//! Session-global time-keyed HLS segment map (ADR-0020).
//!
//! Producer runs write `segNNN.m4s` under `run_<n>/`. This module parses each
//! run's honest `index.m3u8`, gates entries with `sidx.earliest_presentation_time`,
//! and stores them under title-absolute start milliseconds. Served URIs are
//! `seg_<ms:011>.m4s` (milliseconds, zero-padded).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Wire name for a segment whose media starts at `start_ms` (title-absolute).
pub fn time_keyed_segment_name(start_ms: u64) -> String {
    format!("seg_{start_ms:011}.m4s")
}

/// Parse `seg_00001277151.m4s` → start_ms. Rejects the old `segNNN.m4s` form.
pub fn parse_time_keyed_segment_name(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("seg_")?.strip_suffix(".m4s")?;
    if rest.len() != 11 || !rest.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedSegment {
    pub start_ms: u64,
    pub duration_ms: u64,
    pub run_id: u64,
    /// Relative to the session dir, e.g. `run_0/seg042.m4s`.
    pub rel_path: PathBuf,
}

#[derive(Debug, Default)]
pub struct SegmentMap {
    /// Title-absolute start_ms → segment. One entry per start; a newer run
    /// that produces a different packing at the same start replaces the
    /// prior entry (bytes remain under the old run until eviction).
    by_start: BTreeMap<u64, MappedSegment>,
}

impl SegmentMap {
    pub fn get(&self, start_ms: u64) -> Option<&MappedSegment> {
        self.by_start.get(&start_ms)
    }

    #[allow(dead_code)] // map API for eviction / future callers
    pub fn contains(&self, start_ms: u64) -> bool {
        self.by_start.contains_key(&start_ms)
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.by_start.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.by_start.is_empty()
    }

    /// Segments whose media interval intersects `[window_start, window_end)`.
    #[allow(dead_code)]
    pub fn overlapping(&self, window_start_ms: u64, window_end_ms: u64) -> Vec<&MappedSegment> {
        self.by_start
            .values()
            .filter(|s| {
                let end = s.start_ms.saturating_add(s.duration_ms);
                s.start_ms < window_end_ms && end > window_start_ms
            })
            .collect()
    }

    /// All segments in start-time order (for playlist assembly).
    pub fn iter_ordered(&self) -> impl DoubleEndedIterator<Item = &MappedSegment> {
        self.by_start.values()
    }

    /// Drop every map entry belonging to `run_id` (after that run dir is evicted).
    pub fn remove_run(&mut self, run_id: u64) {
        self.by_start.retain(|_, s| s.run_id != run_id);
    }

    /// Drop a single start key (file gone under an otherwise live run).
    pub fn remove_start(&mut self, start_ms: u64) {
        self.by_start.remove(&start_ms);
    }

    /// Run ids that still back at least one map entry (authoritative for eviction).
    pub fn referenced_run_ids(&self) -> std::collections::BTreeSet<u64> {
        self.by_start.values().map(|s| s.run_id).collect()
    }

    /// True when any map entry points at this run.
    pub fn run_is_referenced(&self, run_id: u64) -> bool {
        self.by_start.values().any(|s| s.run_id == run_id)
    }

    pub fn insert(&mut self, seg: MappedSegment) {
        self.by_start.insert(seg.start_ms, seg);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FfmpegIndexEntry {
    pub file_name: String,
    pub extinf_secs: f64,
}

/// Parse FFmpeg's HLS media playlist into ordered EXTINF + file pairs.
pub fn parse_ffmpeg_index(text: &str) -> Result<Vec<FfmpegIndexEntry>, String> {
    let mut out = Vec::new();
    let mut pending_extinf: Option<f64> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            let dur = rest
                .split(',')
                .next()
                .ok_or_else(|| format!("bad EXTINF line: {line}"))?;
            let secs: f64 = dur
                .parse()
                .map_err(|_| format!("bad EXTINF duration in: {line}"))?;
            pending_extinf = Some(secs);
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let secs = pending_extinf
            .take()
            .ok_or_else(|| format!("segment URI without EXTINF: {line}"))?;
        out.push(FfmpegIndexEntry {
            file_name: line.to_string(),
            extinf_secs: secs,
        });
    }
    Ok(out)
}

/// Read `sidx` earliest_presentation_time for track ref_id 1 (video), in ms.
pub fn sidx_video_earliest_ms(seg: &[u8]) -> Result<u64, String> {
    let mut off = 0usize;
    while off + 8 <= seg.len() {
        let size = u32::from_be_bytes(seg[off..off + 4].try_into().unwrap()) as usize;
        let typ = &seg[off + 4..off + 8];
        if size < 8 || off + size > seg.len() {
            break;
        }
        if typ == b"sidx" {
            let body = &seg[off + 8..off + size];
            if body.len() < 20 {
                return Err("sidx too short".into());
            }
            let version = body[0];
            let ref_id = u32::from_be_bytes(body[4..8].try_into().unwrap());
            if ref_id == 1 {
                let timescale = u32::from_be_bytes(body[8..12].try_into().unwrap());
                if timescale == 0 {
                    return Err("sidx timescale 0".into());
                }
                let earliest = if version == 0 {
                    if body.len() < 16 {
                        return Err("sidx v0 too short".into());
                    }
                    u64::from(u32::from_be_bytes(body[12..16].try_into().unwrap()))
                } else {
                    if body.len() < 20 {
                        return Err("sidx v1 too short".into());
                    }
                    u64::from_be_bytes(body[12..20].try_into().unwrap())
                };
                return Ok((earliest * 1000) / u64::from(timescale));
            }
        }
        off += size;
    }
    Err("no video sidx".into())
}

/// Ingest one producer run's `index.m3u8` into `map`.
///
/// For each EXTINF entry, reads the segment file, requires
/// `sidx.earliest ≈ cumulative EXTINF start` (within 1 ms), and inserts a
/// time-keyed map entry. Disagreement skips that segment (hard failure to
/// publish — never map wrong content).
pub fn ingest_run_index(
    map: &mut SegmentMap,
    session_dir: &Path,
    run_id: u64,
    index_text: &str,
) -> Result<usize, String> {
    let entries = parse_ffmpeg_index(index_text)?;
    let run_rel = PathBuf::from(format!("run_{run_id}"));
    let mut inserted = 0usize;
    // FFmpeg EXTINF starts are relative to the first packet after seek; with
    // -output_ts_offset the sidx carries title-absolute time. We trust sidx
    // for the key and EXTINF only for duration; gate checks sidx against the
    // running title-absolute timeline implied by successive sidx values.
    let mut prev_end_ms: Option<u64> = None;
    for entry in entries {
        let rel = run_rel.join(&entry.file_name);
        let abs = session_dir.join(&rel);
        let bytes = fs::read(&abs).map_err(|e| format!("read {}: {e}", abs.display()))?;
        let sidx_ms = sidx_video_earliest_ms(&bytes)?;
        let duration_ms = (entry.extinf_secs * 1000.0).round() as u64;
        if duration_ms == 0 {
            continue;
        }
        // Gate: after the first segment, sidx should equal the previous end
        // within one millisecond (contiguous producer output). The first
        // segment defines the land; its sidx is the authority for start_ms.
        if let Some(expect) = prev_end_ms {
            let delta = sidx_ms.abs_diff(expect);
            if delta > 1 {
                tracing::warn!(
                    run_id,
                    file = %entry.file_name,
                    sidx_ms,
                    expect_ms = expect,
                    delta_ms = delta,
                    "hls map-build gate: sidx disagrees with EXTINF timeline; skipping"
                );
                continue;
            }
        }
        let start_ms = sidx_ms;
        map.insert(MappedSegment {
            start_ms,
            duration_ms,
            run_id,
            rel_path: rel,
        });
        prev_end_ms = Some(start_ms.saturating_add(duration_ms));
        inserted += 1;
    }
    Ok(inserted)
}

/// Build an EVENT (or ENDLIST) media playlist from ordered map segments.
///
/// `init_uri` is the EXT-X-MAP URI (run-relative or session-absolute).
/// `#EXT-X-START` is window-relative (0) when `window_relative_start` is true.
pub fn build_map_playlist(segments: &[&MappedSegment], init_uri: &str, endlist: bool) -> Vec<u8> {
    use std::fmt::Write;
    let target = segments
        .iter()
        .map(|s| ((s.duration_ms as f64) / 1000.0).ceil() as u64)
        .max()
        .unwrap_or(2)
        .max(1);
    let mut out = format!(
        "#EXTM3U\n\
         #EXT-X-VERSION:7\n\
         #EXT-X-TARGETDURATION:{target}\n\
         #EXT-X-PLAYLIST-TYPE:EVENT\n\
         #EXT-X-MEDIA-SEQUENCE:0\n\
         #EXT-X-INDEPENDENT-SEGMENTS\n\
         #EXT-X-MAP:URI=\"{init_uri}\"\n\
         #EXT-X-START:TIME-OFFSET=0.000,PRECISE=YES\n"
    );
    for s in segments {
        let secs = s.duration_ms as f64 / 1000.0;
        let _ = writeln!(
            out,
            "#EXTINF:{secs:.6},\n{}",
            time_keyed_segment_name(s.start_ms)
        );
    }
    if endlist {
        out.push_str("#EXT-X-ENDLIST\n");
    }
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_keyed_round_trip() {
        let name = time_keyed_segment_name(1_277_151);
        assert_eq!(name, "seg_00001277151.m4s");
        assert_eq!(parse_time_keyed_segment_name(&name), Some(1_277_151));
        assert_eq!(parse_time_keyed_segment_name("seg042.m4s"), None);
        assert_eq!(parse_time_keyed_segment_name("seg_1277151.m4s"), None);
    }

    #[test]
    fn parse_ffmpeg_index_basic() {
        let text = "\
#EXTM3U
#EXT-X-TARGETDURATION:5
#EXTINF:4.004000,
seg005.m4s
#EXTINF:2.000000,
seg006.m4s
#EXT-X-ENDLIST
";
        let entries = parse_ffmpeg_index(text).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].file_name, "seg005.m4s");
        assert!((entries[0].extinf_secs - 4.004).abs() < 1e-6);
    }

    #[test]
    fn overlapping_and_remove_run() {
        let mut map = SegmentMap::default();
        map.insert(MappedSegment {
            start_ms: 1000,
            duration_ms: 1000,
            run_id: 0,
            rel_path: PathBuf::from("run_0/seg000.m4s"),
        });
        map.insert(MappedSegment {
            start_ms: 5000,
            duration_ms: 1000,
            run_id: 1,
            rel_path: PathBuf::from("run_1/seg000.m4s"),
        });
        assert_eq!(map.overlapping(0, 3000).len(), 1, "early window");
        assert_eq!(map.overlapping(4000, 7000).len(), 1, "late window");
        assert_eq!(map.overlapping(0, 7000).len(), 2, "full span");
        map.remove_run(0);
        assert_eq!(map.len(), 1);
        assert!(map.get(5000).is_some());
    }

    #[test]
    fn build_playlist_event_shape() {
        let segs = [
            MappedSegment {
                start_ms: 8008,
                duration_ms: 4004,
                run_id: 0,
                rel_path: PathBuf::from("run_0/a.m4s"),
            },
            MappedSegment {
                start_ms: 12_012,
                duration_ms: 4004,
                run_id: 0,
                rel_path: PathBuf::from("run_0/b.m4s"),
            },
        ];
        let refs: Vec<&MappedSegment> = segs.iter().collect();
        let bytes = build_map_playlist(&refs, "init.mp4", false);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("#EXT-X-PLAYLIST-TYPE:EVENT"));
        assert!(text.contains("#EXT-X-START:TIME-OFFSET=0.000,PRECISE=YES"));
        assert!(text.contains("seg_00000008008.m4s"));
        assert!(text.contains("seg_00000012012.m4s"));
        assert!(!text.contains("#EXT-X-ENDLIST"));
        let with_end = build_map_playlist(&refs, "init.mp4", true);
        assert!(
            String::from_utf8(with_end)
                .unwrap()
                .contains("#EXT-X-ENDLIST")
        );
    }
}
