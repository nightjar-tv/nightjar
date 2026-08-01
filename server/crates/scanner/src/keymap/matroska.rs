//! Matroska/WebM keyframe index reader (ADR-0023 §2). Index-first: read the
//! Cues element for Cluster absolute byte offsets and Cluster PTS, without a
//! packet walk.
//!
//! SeekHead entries (when present) give direct-seek positions for
//! Info/Tracks/Cues so most of the file is never touched. Without a usable
//! SeekHead entry for Cues, this falls back to scanning the last few
//! megabytes, since muxers commonly write Cues after every Cluster.

use super::KeyframeEntry;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

pub(super) const EBML_HEADER_ID: u32 = 0x1A45_DFA3;
const SEGMENT_ID: u32 = 0x1853_8067;
const SEEKHEAD_ID: u32 = 0x114D_9B74;
const SEEK_ID: u32 = 0x4DBB;
const SEEK_ID_ID: u32 = 0x53AB;
const SEEK_POSITION_ID: u32 = 0x53AC;
const INFO_ID: u32 = 0x1549_A966;
const TIMESTAMP_SCALE_ID: u32 = 0x002A_D7B1;
const CLUSTER_ID: u32 = 0x1F43_B675;
const TRACKS_ID: u32 = 0x1654_AE6B;
const TRACK_ENTRY_ID: u32 = 0xAE;
const TRACK_NUMBER_ID: u32 = 0xD7;
const TRACK_TYPE_ID: u32 = 0x83;
const TRACK_TYPE_VIDEO: u64 = 1;
const CUES_ID: u32 = 0x1C53_BB6B;
const CUE_POINT_ID: u32 = 0xBB;
const CUE_TIME_ID: u32 = 0xB3;
const CUE_TRACK_POSITIONS_ID: u32 = 0xB7;
const CUE_TRACK_ID: u32 = 0xF7;
const CUE_CLUSTER_POSITION_ID: u32 = 0xF1;

/// Default per the Matroska spec when \Segment\Info\TimestampScale is absent.
const DEFAULT_TIMESTAMP_SCALE_NS: u64 = 1_000_000;
/// Tail region scanned for a Cues element with no SeekHead entry.
const TAIL_SCAN_BYTES: u64 = 4 * 1024 * 1024;

pub fn build_from_cues(path: &Path) -> Result<Vec<KeyframeEntry>, String> {
    let mut file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let file_len = file
        .metadata()
        .map_err(|e| format!("stat {}: {e}", path.display()))?
        .len();

    let (ebml_id, ebml_header_len, ebml_size, ebml_unknown) = read_element_header(&mut file, 0)?
        .ok_or_else(|| format!("empty file: {}", path.display()))?;
    if ebml_id != EBML_HEADER_ID {
        return Err(format!("not an EBML file: {}", path.display()));
    }
    if ebml_unknown {
        return Err(format!("EBML header has unknown size: {}", path.display()));
    }
    let segment_offset = ebml_header_len + ebml_size;

    let (seg_id, seg_header_len, seg_size, seg_unknown) =
        read_element_header(&mut file, segment_offset)?
            .ok_or_else(|| format!("missing Segment element: {}", path.display()))?;
    if seg_id != SEGMENT_ID {
        return Err(format!("expected Segment element: {}", path.display()));
    }
    let segment_data_start = segment_offset + seg_header_len;
    let segment_data_end = if seg_unknown {
        file_len
    } else {
        segment_data_start + seg_size
    };

    let mut seek_positions: Vec<(u32, u64)> = Vec::new();
    let mut timescale = DEFAULT_TIMESTAMP_SCALE_NS;
    let mut timescale_found = false;
    let mut video_track: Option<u64> = None;
    let mut cues: Option<(u64, u64)> = None;

    walk_children(
        &mut file,
        segment_data_start,
        segment_data_end,
        |file, id, start, len| {
            match id {
                SEEKHEAD_ID => seek_positions = parse_seekhead(file, start, start + len)?,
                INFO_ID => {
                    if let Some(ts) = parse_timescale(file, start, start + len)? {
                        timescale = ts;
                    }
                    timescale_found = true;
                }
                TRACKS_ID => video_track = find_video_track(file, start, start + len)?,
                CUES_ID => cues = Some((start, len)),
                // Clusters dominate the segment; stop the sequential walk here
                // and rely on SeekHead / a tail scan for anything not seen yet.
                CLUSTER_ID => return Ok(false),
                _ => {}
            }
            Ok(true)
        },
    )?;

    if cues.is_none()
        && let Some(resolved) = resolve_via_seekhead(
            &mut file,
            &seek_positions,
            CUES_ID,
            segment_data_start,
            segment_data_end,
        )?
    {
        cues = Some(resolved);
    }
    if video_track.is_none()
        && let Some((data_start, data_len)) = resolve_via_seekhead(
            &mut file,
            &seek_positions,
            TRACKS_ID,
            segment_data_start,
            segment_data_end,
        )?
    {
        video_track = find_video_track(&mut file, data_start, data_start + data_len)?;
    }
    if !timescale_found
        && let Some((data_start, data_len)) = resolve_via_seekhead(
            &mut file,
            &seek_positions,
            INFO_ID,
            segment_data_start,
            segment_data_end,
        )?
        && let Some(ts) = parse_timescale(&mut file, data_start, data_start + data_len)?
    {
        timescale = ts;
    }

    let cues = match cues {
        Some(c) => c,
        None => find_cues_via_tail_scan(&mut file, file_len)?
            .ok_or_else(|| format!("no Cues element found: {}", path.display()))?,
    };

    let raw = parse_cues(&mut file, cues.0, cues.0 + cues.1, video_track)?;
    let mut entries: Vec<KeyframeEntry> = raw
        .into_iter()
        .map(|(cue_time, cluster_pos)| KeyframeEntry {
            pts_ms: ns_ticks_to_ms(cue_time, timescale),
            byte_offset: (segment_data_start + cluster_pos) as i64,
        })
        .collect();
    entries.sort_by_key(|e| e.pts_ms);
    Ok(entries)
}

/// Look up `want` in SeekHead entries and read its element header at the
/// resolved absolute offset, returning its data range.
fn resolve_via_seekhead(
    file: &mut File,
    seek_positions: &[(u32, u64)],
    want: u32,
    segment_data_start: u64,
    segment_data_end: u64,
) -> Result<Option<(u64, u64)>, String> {
    let Some(&(_, relative_pos)) = seek_positions.iter().find(|(id, _)| *id == want) else {
        return Ok(None);
    };
    let abs = segment_data_start + relative_pos;
    let Some((id, header_len, size, unknown)) = read_element_header(file, abs)? else {
        return Ok(None);
    };
    if id != want {
        return Ok(None);
    }
    let data_start = abs + header_len;
    let data_len = if unknown {
        segment_data_end.saturating_sub(data_start)
    } else {
        size
    };
    Ok(Some((data_start, data_len)))
}

fn parse_seekhead(file: &mut File, start: u64, end: u64) -> Result<Vec<(u32, u64)>, String> {
    let mut out = Vec::new();
    walk_children(file, start, end, |file, id, s, l| {
        if id == SEEK_ID {
            let mut target_id = None;
            let mut position = None;
            walk_children(file, s, s + l, |file, cid, cs, cl| {
                match cid {
                    SEEK_ID_ID => target_id = Some(read_uint(file, cs, cl)? as u32),
                    SEEK_POSITION_ID => position = Some(read_uint(file, cs, cl)?),
                    _ => {}
                }
                Ok(true)
            })?;
            if let (Some(id), Some(pos)) = (target_id, position) {
                out.push((id, pos));
            }
        }
        Ok(true)
    })?;
    Ok(out)
}

fn parse_timescale(file: &mut File, start: u64, end: u64) -> Result<Option<u64>, String> {
    let mut result = None;
    walk_children(file, start, end, |file, id, s, l| {
        if id == TIMESTAMP_SCALE_ID {
            result = Some(read_uint(file, s, l)?);
        }
        Ok(true)
    })?;
    Ok(result)
}

fn find_video_track(file: &mut File, start: u64, end: u64) -> Result<Option<u64>, String> {
    let mut found = None;
    walk_children(file, start, end, |file, id, s, l| {
        if id == TRACK_ENTRY_ID && found.is_none() {
            let mut number = None;
            let mut track_type = None;
            walk_children(file, s, s + l, |file, cid, cs, cl| {
                match cid {
                    TRACK_NUMBER_ID => number = Some(read_uint(file, cs, cl)?),
                    TRACK_TYPE_ID => track_type = Some(read_uint(file, cs, cl)?),
                    _ => {}
                }
                Ok(true)
            })?;
            if track_type == Some(TRACK_TYPE_VIDEO) {
                found = number;
            }
        }
        Ok(true)
    })?;
    Ok(found)
}

fn parse_cues(
    file: &mut File,
    start: u64,
    end: u64,
    video_track: Option<u64>,
) -> Result<Vec<(u64, u64)>, String> {
    let mut out = Vec::new();
    walk_children(file, start, end, |file, id, s, l| {
        if id == CUE_POINT_ID {
            let mut cue_time = None;
            let mut chosen_position = None;
            let mut fallback_position = None;
            walk_children(file, s, s + l, |file, cid, cs, cl| {
                match cid {
                    CUE_TIME_ID => cue_time = Some(read_uint(file, cs, cl)?),
                    CUE_TRACK_POSITIONS_ID => {
                        let mut track = None;
                        let mut position = None;
                        walk_children(file, cs, cs + cl, |file, tid, ts, tl| {
                            match tid {
                                CUE_TRACK_ID => track = Some(read_uint(file, ts, tl)?),
                                CUE_CLUSTER_POSITION_ID => {
                                    position = Some(read_uint(file, ts, tl)?)
                                }
                                _ => {}
                            }
                            Ok(true)
                        })?;
                        if fallback_position.is_none() {
                            fallback_position = position;
                        }
                        if let (Some(want), Some(got), Some(pos)) = (video_track, track, position)
                            && want == got
                        {
                            chosen_position = Some(pos);
                        }
                    }
                    _ => {}
                }
                Ok(true)
            })?;
            let position = if video_track.is_some() {
                chosen_position
            } else {
                fallback_position
            };
            if let (Some(time), Some(pos)) = (cue_time, position) {
                out.push((time, pos));
            }
        }
        Ok(true)
    })?;
    Ok(out)
}

fn walk_children<F>(
    file: &mut File,
    data_start: u64,
    data_end: u64,
    mut visit: F,
) -> Result<(), String>
where
    F: FnMut(&mut File, u32, u64, u64) -> Result<bool, String>,
{
    let mut pos = data_start;
    while pos < data_end {
        let Some((id, header_len, size, unknown)) = read_element_header(file, pos)? else {
            break;
        };
        let child_data_start = pos + header_len;
        let child_data_len = if unknown {
            data_end.saturating_sub(child_data_start)
        } else {
            size
        };
        if !visit(file, id, child_data_start, child_data_len)? {
            return Ok(());
        }
        if unknown {
            break;
        }
        pos = child_data_start + child_data_len;
    }
    Ok(())
}

fn read_element_header(
    file: &mut File,
    offset: u64,
) -> Result<Option<(u32, u64, u64, bool)>, String> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| format!("seek to {offset}: {e}"))?;
    let mut buf = [0u8; 12];
    let n = read_up_to(file, &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    let (id, id_len) =
        read_vint_id(&buf[..n]).ok_or_else(|| format!("invalid EBML id at offset {offset}"))?;
    let (size, size_len, unknown) = read_vint_size(&buf[..n], id_len)
        .ok_or_else(|| format!("invalid EBML size at offset {offset}"))?;
    Ok(Some((id, (id_len + size_len) as u64, size, unknown)))
}

fn read_up_to(file: &mut File, buf: &mut [u8]) -> Result<usize, String> {
    let mut total = 0;
    while total < buf.len() {
        match file.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("read: {e}")),
        }
    }
    Ok(total)
}

fn read_uint(file: &mut File, start: u64, len: u64) -> Result<u64, String> {
    if len == 0 || len > 8 {
        return Err(format!(
            "unsupported EBML uint length {len} at offset {start}"
        ));
    }
    let mut buf = [0u8; 8];
    file.seek(SeekFrom::Start(start))
        .map_err(|e| format!("seek to {start}: {e}"))?;
    file.read_exact(&mut buf[8 - len as usize..])
        .map_err(|e| format!("read uint at {start}: {e}"))?;
    Ok(u64::from_be_bytes(buf))
}

fn vint_len(first_byte: u8) -> Option<usize> {
    if first_byte == 0 {
        return None;
    }
    Some(first_byte.leading_zeros() as usize + 1)
}

/// Element ID vints keep their marker bit (it is part of the ID's identity).
fn read_vint_id(data: &[u8]) -> Option<(u32, usize)> {
    let first = *data.first()?;
    let len = vint_len(first)?;
    if len > 4 || data.len() < len {
        return None;
    }
    let mut v = 0u32;
    for &b in &data[..len] {
        v = (v << 8) | b as u32;
    }
    Some((v, len))
}

/// Size vints strip the marker bit; an all-ones value means "unknown size".
fn read_vint_size(data: &[u8], offset: usize) -> Option<(u64, usize, bool)> {
    let rest = data.get(offset..)?;
    let first = *rest.first()?;
    let len = vint_len(first)?;
    if len > 8 || rest.len() < len {
        return None;
    }
    let mask = if len < 8 { 0xFFu8 >> len } else { 0u8 };
    let mut v = (rest[0] & mask) as u64;
    for &b in &rest[1..len] {
        v = (v << 8) | b as u64;
    }
    let all_ones = v == (1u64 << (7 * len)) - 1;
    Some((v, len, all_ones))
}

fn find_cues_via_tail_scan(file: &mut File, file_len: u64) -> Result<Option<(u64, u64)>, String> {
    let tail_len = TAIL_SCAN_BYTES.min(file_len);
    let tail_start = file_len - tail_len;
    let mut buf = vec![0u8; tail_len as usize];
    file.seek(SeekFrom::Start(tail_start))
        .map_err(|e| format!("seek to tail scan region at {tail_start}: {e}"))?;
    file.read_exact(&mut buf)
        .map_err(|e| format!("read tail scan region: {e}"))?;

    let pattern = CUES_ID.to_be_bytes();
    let mut search_end = buf.len();
    while let Some(rel_pos) = rfind(&buf[..search_end], &pattern) {
        let abs = tail_start + rel_pos as u64;
        if let Some((id, header_len, size, unknown)) = read_element_header(file, abs)?
            && id == CUES_ID
        {
            let data_start = abs + header_len;
            let data_len = if unknown {
                file_len.saturating_sub(data_start)
            } else {
                size
            };
            if data_start + data_len <= file_len {
                return Ok(Some((data_start, data_len)));
            }
        }
        search_end = rel_pos;
    }
    Ok(None)
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len())
        .rev()
        .find(|&i| &haystack[i..i + needle.len()] == needle)
}

fn ns_ticks_to_ms(ticks: u64, timescale_ns: u64) -> i64 {
    let ns = ticks as u128 * timescale_ns as u128;
    ((ns + 500_000) / 1_000_000) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write_vint_id(out: &mut Vec<u8>, id: u32, len: usize) {
        let bytes = id.to_be_bytes();
        out.extend_from_slice(&bytes[4 - len..]);
    }

    fn write_vint_size(out: &mut Vec<u8>, value: u64, len: usize) {
        let marker = 1u8 << (8 - len);
        let mut bytes = value.to_be_bytes();
        bytes[8 - len] |= marker;
        out.extend_from_slice(&bytes[8 - len..]);
    }

    fn element(id: u32, id_len: usize, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        write_vint_id(&mut out, id, id_len);
        // Always emit an 8-byte size vint so payload length never overflows
        // the marker-bit budget of a shorter encoding.
        write_vint_size(&mut out, payload.len() as u64, 8);
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn vint_id_keeps_marker_bit() {
        // CueTime: 1-byte id 0xB3.
        assert_eq!(read_vint_id(&[0xB3]), Some((0xB3, 1)));
        // Cues: 4-byte id 0x1C53BB6B.
        assert_eq!(
            read_vint_id(&[0x1C, 0x53, 0xBB, 0x6B]),
            Some((0x1C53_BB6B, 4))
        );
    }

    #[test]
    fn vint_size_strips_marker_and_flags_unknown() {
        // 1-byte size, value 5: 0x80 | 5 = 0x85.
        assert_eq!(read_vint_size(&[0x85], 0), Some((5, 1, false)));
        // 1-byte unknown size: 0xFF (all data bits set).
        assert_eq!(read_vint_size(&[0xFF], 0), Some((0x7F, 1, true)));
    }

    #[test]
    fn rfind_locates_last_occurrence() {
        let hay = [1u8, 2, 3, 1, 2, 3, 9];
        assert_eq!(rfind(&hay, &[1, 2, 3]), Some(3));
        assert_eq!(rfind(&hay, &[9]), Some(6));
        assert_eq!(rfind(&hay, &[8]), None);
    }

    #[test]
    fn ns_ticks_to_ms_uses_timescale() {
        // 1 tick at the default 1_000_000 ns scale is 1 ms.
        assert_eq!(ns_ticks_to_ms(1, DEFAULT_TIMESTAMP_SCALE_NS), 1);
        assert_eq!(ns_ticks_to_ms(2500, DEFAULT_TIMESTAMP_SCALE_NS), 2500);
    }

    fn build_synthetic_mkv() -> Vec<u8> {
        // EBML header (empty payload is fine; only the id/size are read).
        let mut out = element(EBML_HEADER_ID, 4, &[]);

        // \Segment\Info\TimestampScale = 1_000_000 (default, spelled out).
        let mut info_payload = Vec::new();
        info_payload.extend(element(TIMESTAMP_SCALE_ID, 3, &{
            let mut v = Vec::new();
            v.extend_from_slice(&1_000_000u32.to_be_bytes());
            v
        }));
        let info = element(INFO_ID, 4, &info_payload);

        // \Segment\Tracks\TrackEntry{TrackNumber=1, TrackType=1 (video)}.
        let mut track_entry_payload = Vec::new();
        track_entry_payload.extend(element(TRACK_NUMBER_ID, 1, &[1]));
        track_entry_payload.extend(element(TRACK_TYPE_ID, 1, &[1]));
        let tracks = element(
            TRACKS_ID,
            4,
            &element(TRACK_ENTRY_ID, 1, &track_entry_payload),
        );

        // \Segment\Cues\CuePoint{CueTime=0, CueTrackPositions{CueTrack=1, CueClusterPosition=100}}
        // and a second CuePoint at CueTime=2000 (ns ticks) -> pos 500.
        let cue_point = |time: u64, track: u8, pos: u64| {
            let mut ctp = Vec::new();
            ctp.extend(element(CUE_TRACK_ID, 1, &[track]));
            ctp.extend(element(CUE_CLUSTER_POSITION_ID, 1, &[pos as u8]));
            let mut cp = Vec::new();
            cp.extend(element(CUE_TIME_ID, 1, &[time as u8]));
            cp.extend(element(CUE_TRACK_POSITIONS_ID, 1, &ctp));
            element(CUE_POINT_ID, 1, &cp)
        };
        let mut cues_payload = Vec::new();
        cues_payload.extend(cue_point(2, 1, 100));
        cues_payload.extend(cue_point(0, 1, 50));
        let cues = element(CUES_ID, 4, &cues_payload);

        let mut segment_payload = Vec::new();
        segment_payload.extend(info);
        segment_payload.extend(tracks);
        segment_payload.extend(cues);
        out.extend(element(SEGMENT_ID, 4, &segment_payload));
        out
    }

    fn write_temp(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("synthetic.mkv");
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn build_from_cues_reads_synthetic_segment_without_seekhead() {
        let bytes = build_synthetic_mkv();
        let (_dir, path) = write_temp(&bytes);
        let entries = build_from_cues(&path).unwrap();
        assert_eq!(
            entries,
            vec![
                KeyframeEntry {
                    pts_ms: 0,
                    byte_offset: entries[0].byte_offset,
                },
                KeyframeEntry {
                    pts_ms: 2,
                    byte_offset: entries[1].byte_offset,
                },
            ]
        );
        assert!(entries[1].byte_offset > entries[0].byte_offset);
    }

    fn testdata_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files")
            .join(name)
    }

    #[test]
    fn cues_index_builds_from_real_corpus_file() {
        let path = testdata_path("h264_aac_mkv.mkv");
        if !path.exists() {
            return;
        }
        let entries = build_from_cues(&path).unwrap();
        assert!(!entries.is_empty());
        assert!(entries.windows(2).all(|w| w[0].pts_ms <= w[1].pts_ms));
        assert_eq!(entries[0].pts_ms, 0);
    }
}
