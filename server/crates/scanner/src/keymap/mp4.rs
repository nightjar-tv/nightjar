//! MP4/ISO BMFF keyframe index reader (ADR-0023 §2). Index-first: parse the
//! video track's sample tables (`stss`/`stts`/`stsc`/`stsz`/`stco`/`co64`)
//! for sync sample byte offsets and PTS, without a packet walk.
//!
//! Sample offsets in `moov` are absolute in the file being read, so no
//! rewrite is needed here — that only matters at session-serve time when a
//! virtual `moov` is spliced (ADR-0023 §3b), which is out of scope for map
//! extraction.

use super::KeyframeEntry;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

enum SampleSizes {
    Uniform(u32, u32),
    PerSample(Vec<u32>),
}

impl SampleSizes {
    fn count(&self) -> u32 {
        match self {
            SampleSizes::Uniform(_, count) => *count,
            SampleSizes::PerSample(sizes) => sizes.len() as u32,
        }
    }

    fn size_for(&self, sample_number: u32) -> Result<u32, String> {
        match self {
            SampleSizes::Uniform(size, count) => {
                if sample_number == 0 || sample_number > *count {
                    return Err(format!("sample {sample_number} out of range"));
                }
                Ok(*size)
            }
            SampleSizes::PerSample(sizes) => sizes
                .get((sample_number - 1) as usize)
                .copied()
                .ok_or_else(|| format!("sample {sample_number} out of range")),
        }
    }
}

struct StblTables {
    stts: Vec<(u32, u32)>,
    /// `None` means every sample is a sync sample (no `stss` box).
    stss: Option<Vec<u32>>,
    stsc: Vec<(u32, u32)>,
    sample_sizes: SampleSizes,
    chunk_offsets: Vec<u64>,
    timescale: u32,
}

/// True if the file looks like ISO BMFF by top-level box type, without
/// reading box payloads (cheap even on multi-GB files).
pub(super) fn looks_like_mp4(path: &Path) -> Result<bool, String> {
    let mut file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut offset = 0u64;
    for _ in 0..64 {
        let Some((payload_offset, payload_len, kind)) = read_box_header(&mut file, offset)? else {
            return Ok(false);
        };
        if kind == *b"ftyp" || kind == *b"moov" || kind == *b"mdat" {
            return Ok(true);
        }
        offset = payload_offset + payload_len;
    }
    Ok(false)
}

pub fn build_from_sample_tables(path: &Path) -> Result<Vec<KeyframeEntry>, String> {
    let (moov_offset, moov_len) = find_top_level_box(path, b"moov")?
        .ok_or_else(|| format!("no moov box: {}", path.display()))?;
    let moov_len = usize::try_from(moov_len)
        .map_err(|_| format!("moov box too large to parse: {}", path.display()))?;

    let mut file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    file.seek(SeekFrom::Start(moov_offset))
        .map_err(|e| format!("seek to moov in {}: {e}", path.display()))?;
    let mut moov = vec![0u8; moov_len];
    file.read_exact(&mut moov)
        .map_err(|e| format!("read moov box in {}: {e}", path.display()))?;

    for (kind, payload) in child_boxes(&moov)? {
        if kind != *b"trak" {
            continue;
        }
        if let Some(table) = parse_video_trak(payload)? {
            return build_entries_from_tables(&table);
        }
    }
    Err(format!("no video track found in moov: {}", path.display()))
}

fn read_box_header(file: &mut File, offset: u64) -> Result<Option<(u64, u64, [u8; 4])>, String> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| format!("seek to {offset}: {e}"))?;
    let mut hdr = [0u8; 8];
    let n = read_up_to(file, &mut hdr)?;
    if n == 0 {
        return Ok(None);
    }
    if n < 8 {
        return Err(format!("truncated box header at offset {offset}"));
    }
    let mut size = u32::from_be_bytes(hdr[0..4].try_into().unwrap()) as u64;
    let kind: [u8; 4] = hdr[4..8].try_into().unwrap();
    let header_len = if size == 1 {
        let mut ext = [0u8; 8];
        file.read_exact(&mut ext)
            .map_err(|e| format!("read 64-bit box size at offset {offset}: {e}"))?;
        size = u64::from_be_bytes(ext);
        16u64
    } else {
        8u64
    };
    if size == 0 {
        let file_len = file
            .metadata()
            .map_err(|e| format!("stat while sizing box at offset {offset}: {e}"))?
            .len();
        size = file_len - offset;
    }
    if size < header_len {
        return Err(format!("invalid box size {size} at offset {offset}"));
    }
    Ok(Some((offset + header_len, size - header_len, kind)))
}

fn read_up_to(file: &mut File, buf: &mut [u8]) -> Result<usize, String> {
    let mut total = 0;
    while total < buf.len() {
        match file.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("read: {e}")),
        }
    }
    Ok(total)
}

fn find_top_level_box(path: &Path, want: &[u8; 4]) -> Result<Option<(u64, u64)>, String> {
    let mut file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut offset = 0u64;
    loop {
        match read_box_header(&mut file, offset)? {
            None => return Ok(None),
            Some((payload_offset, payload_len, kind)) => {
                if kind == *want {
                    return Ok(Some((payload_offset, payload_len)));
                }
                offset = payload_offset + payload_len;
            }
        }
    }
}

type ChildBox<'a> = ([u8; 4], &'a [u8]);

/// Immediate children of an in-memory box payload (e.g. `moov`, `trak`,
/// `mdia`, `minf`, `stbl` — all header-scale once located).
fn child_boxes(data: &[u8]) -> Result<Vec<ChildBox<'_>>, String> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let size = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        let kind: [u8; 4] = data[pos + 4..pos + 8].try_into().unwrap();
        let (header_len, box_size) = if size == 1 {
            if pos + 16 > data.len() {
                return Err("truncated 64-bit box header".to_string());
            }
            let large = u64::from_be_bytes(data[pos + 8..pos + 16].try_into().unwrap());
            let large = usize::try_from(large)
                .map_err(|_| "box too large to hold in memory".to_string())?;
            (16usize, large)
        } else if size == 0 {
            (8usize, data.len() - pos)
        } else {
            (8usize, size)
        };
        if box_size < header_len || pos + box_size > data.len() {
            return Err(format!("invalid child box size at offset {pos}"));
        }
        out.push((kind, &data[pos + header_len..pos + box_size]));
        pos += box_size;
    }
    Ok(out)
}

fn find_child<'a>(data: &'a [u8], want: &[u8; 4]) -> Result<Option<&'a [u8]>, String> {
    for (kind, payload) in child_boxes(data)? {
        if kind == *want {
            return Ok(Some(payload));
        }
    }
    Ok(None)
}

fn parse_video_trak(trak: &[u8]) -> Result<Option<StblTables>, String> {
    let Some(mdia) = find_child(trak, b"mdia")? else {
        return Ok(None);
    };
    let is_video = match find_child(mdia, b"hdlr")? {
        Some(hdlr) => hdlr_is_video(hdlr)?,
        None => false,
    };
    if !is_video {
        return Ok(None);
    }

    let mdhd = find_child(mdia, b"mdhd")?.ok_or("video track missing mdhd")?;
    let timescale = parse_mdhd_timescale(mdhd)?;

    let minf = find_child(mdia, b"minf")?.ok_or("video track missing minf")?;
    let stbl = find_child(minf, b"stbl")?.ok_or("video track missing stbl")?;

    let stts = parse_stts(find_child(stbl, b"stts")?.ok_or("video track missing stts")?)?;
    let stsc = parse_stsc(find_child(stbl, b"stsc")?.ok_or("video track missing stsc")?)?;
    let sample_sizes = parse_stsz(find_child(stbl, b"stsz")?.ok_or("video track missing stsz")?)?;
    let chunk_offsets = match find_child(stbl, b"co64")? {
        Some(co64) => parse_co64(co64)?,
        None => parse_stco(find_child(stbl, b"stco")?.ok_or("video track missing stco/co64")?)?,
    };
    let stss = find_child(stbl, b"stss")?.map(parse_stss).transpose()?;

    Ok(Some(StblTables {
        stts,
        stss,
        stsc,
        sample_sizes,
        chunk_offsets,
        timescale,
    }))
}

fn hdlr_is_video(hdlr: &[u8]) -> Result<bool, String> {
    // FullBox header (4 bytes) + pre_defined (4 bytes), then handler_type.
    let handler_type = hdlr.get(8..12).ok_or("truncated hdlr box")?;
    Ok(handler_type == b"vide")
}

fn parse_mdhd_timescale(mdhd: &[u8]) -> Result<u32, String> {
    let version = *mdhd.first().ok_or("empty mdhd box")?;
    let offset = 4 + if version == 1 { 16 } else { 8 };
    read_u32(mdhd, offset)
}

fn parse_stts(data: &[u8]) -> Result<Vec<(u32, u32)>, String> {
    let count = read_u32(data, 4)?;
    let mut out = Vec::with_capacity(count as usize);
    let mut pos = 8usize;
    for _ in 0..count {
        out.push((read_u32(data, pos)?, read_u32(data, pos + 4)?));
        pos += 8;
    }
    Ok(out)
}

fn parse_stss(data: &[u8]) -> Result<Vec<u32>, String> {
    let count = read_u32(data, 4)?;
    let mut out = Vec::with_capacity(count as usize);
    let mut pos = 8usize;
    for _ in 0..count {
        out.push(read_u32(data, pos)?);
        pos += 4;
    }
    out.sort_unstable();
    Ok(out)
}

fn parse_stsc(data: &[u8]) -> Result<Vec<(u32, u32)>, String> {
    let count = read_u32(data, 4)?;
    let mut out = Vec::with_capacity(count as usize);
    let mut pos = 8usize;
    for _ in 0..count {
        out.push((read_u32(data, pos)?, read_u32(data, pos + 4)?));
        pos += 12;
    }
    if out.is_empty() {
        return Err("empty stsc table".to_string());
    }
    Ok(out)
}

fn parse_stsz(data: &[u8]) -> Result<SampleSizes, String> {
    let sample_size = read_u32(data, 4)?;
    let count = read_u32(data, 8)?;
    if sample_size != 0 {
        return Ok(SampleSizes::Uniform(sample_size, count));
    }
    let mut sizes = Vec::with_capacity(count as usize);
    let mut pos = 12usize;
    for _ in 0..count {
        sizes.push(read_u32(data, pos)?);
        pos += 4;
    }
    Ok(SampleSizes::PerSample(sizes))
}

fn parse_stco(data: &[u8]) -> Result<Vec<u64>, String> {
    let count = read_u32(data, 4)?;
    let mut out = Vec::with_capacity(count as usize);
    let mut pos = 8usize;
    for _ in 0..count {
        out.push(read_u32(data, pos)? as u64);
        pos += 4;
    }
    Ok(out)
}

fn parse_co64(data: &[u8]) -> Result<Vec<u64>, String> {
    let count = read_u32(data, 4)?;
    let mut out = Vec::with_capacity(count as usize);
    let mut pos = 8usize;
    for _ in 0..count {
        out.push(read_u64(data, pos)?);
        pos += 8;
    }
    Ok(out)
}

fn read_u32(data: &[u8], pos: usize) -> Result<u32, String> {
    data.get(pos..pos + 4)
        .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
        .ok_or_else(|| format!("truncated box reading u32 at {pos}"))
}

fn read_u64(data: &[u8], pos: usize) -> Result<u64, String> {
    data.get(pos..pos + 8)
        .map(|b| u64::from_be_bytes(b.try_into().unwrap()))
        .ok_or_else(|| format!("truncated box reading u64 at {pos}"))
}

fn build_entries_from_tables(t: &StblTables) -> Result<Vec<KeyframeEntry>, String> {
    if t.chunk_offsets.is_empty() {
        return Err("no chunk offsets".to_string());
    }
    let total_samples = t.sample_sizes.count();
    if total_samples == 0 {
        return Err("no samples".to_string());
    }
    if t.timescale == 0 {
        return Err("zero timescale".to_string());
    }

    let sync = t.stss.as_deref();
    let mut sync_idx = 0usize;
    let mut entries = Vec::new();

    let mut stts_entry_idx = 0usize;
    let mut stts_remaining = 0u32;
    let mut stts_delta = 0u32;
    let mut decode_ticks: u64 = 0;

    let mut stsc_idx = 0usize;
    let mut sample_number = 1u32;

    'chunks: for (chunk_zero_idx, &chunk_offset) in t.chunk_offsets.iter().enumerate() {
        if sample_number > total_samples {
            break;
        }
        let chunk_number = chunk_zero_idx as u32 + 1;
        while stsc_idx + 1 < t.stsc.len() && t.stsc[stsc_idx + 1].0 <= chunk_number {
            stsc_idx += 1;
        }
        let samples_per_chunk = t.stsc[stsc_idx].1;
        let mut offset_in_chunk: u64 = 0;

        for _ in 0..samples_per_chunk {
            if sample_number > total_samples {
                break 'chunks;
            }
            if stts_remaining == 0 {
                let (count, delta) = t
                    .stts
                    .get(stts_entry_idx)
                    .copied()
                    .ok_or("stts exhausted before samples")?;
                stts_remaining = count;
                stts_delta = delta;
                stts_entry_idx += 1;
            }

            let is_sync = match sync {
                Some(list) => {
                    if sync_idx < list.len() && list[sync_idx] == sample_number {
                        sync_idx += 1;
                        true
                    } else {
                        false
                    }
                }
                None => true,
            };
            if is_sync {
                entries.push(KeyframeEntry {
                    pts_ms: ticks_to_ms(decode_ticks, t.timescale),
                    byte_offset: (chunk_offset + offset_in_chunk) as i64,
                });
            }

            offset_in_chunk += t.sample_sizes.size_for(sample_number)? as u64;
            decode_ticks += stts_delta as u64;
            stts_remaining -= 1;
            sample_number += 1;
        }
    }

    Ok(entries)
}

fn ticks_to_ms(ticks: u64, timescale: u32) -> i64 {
    let timescale = timescale as u64;
    (((ticks * 1000) + timescale / 2) / timescale) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn box_bytes(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn child_boxes_reads_sibling_boxes() {
        let mut data = box_bytes(b"ftyp", b"isom");
        data.extend(box_bytes(b"free", &[1, 2, 3, 4]));
        let children = child_boxes(&data).unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(&children[0].0, b"ftyp");
        assert_eq!(children[0].1, b"isom");
        assert_eq!(&children[1].0, b"free");
        assert_eq!(children[1].1, &[1, 2, 3, 4]);
    }

    #[test]
    fn child_boxes_rejects_truncated_size() {
        let mut data = box_bytes(b"free", &[0, 0]);
        // Corrupt the declared size to run past the buffer.
        data[3] = 0xFF;
        assert!(child_boxes(&data).is_err());
    }

    #[test]
    fn hdlr_is_video_reads_handler_type() {
        let mut payload = vec![0u8; 12];
        payload[8..12].copy_from_slice(b"vide");
        assert!(hdlr_is_video(&payload).unwrap());
        payload[8..12].copy_from_slice(b"soun");
        assert!(!hdlr_is_video(&payload).unwrap());
    }

    #[test]
    fn mdhd_timescale_version_0_and_1() {
        let mut v0 = vec![0u8; 20];
        v0[12..16].copy_from_slice(&1000u32.to_be_bytes());
        assert_eq!(parse_mdhd_timescale(&v0).unwrap(), 1000);

        let mut v1 = vec![0u8; 32];
        v1[0] = 1;
        v1[20..24].copy_from_slice(&90000u32.to_be_bytes());
        assert_eq!(parse_mdhd_timescale(&v1).unwrap(), 90000);
    }

    #[test]
    fn build_entries_from_tables_computes_sync_pts_and_offsets() {
        let table = StblTables {
            stts: vec![(4, 1000)],
            stss: Some(vec![1, 3]),
            stsc: vec![(1, 2)],
            sample_sizes: SampleSizes::Uniform(100, 4),
            chunk_offsets: vec![1000, 2000],
            timescale: 1000,
        };
        let entries = build_entries_from_tables(&table).unwrap();
        assert_eq!(
            entries,
            vec![
                KeyframeEntry {
                    pts_ms: 0,
                    byte_offset: 1000
                },
                KeyframeEntry {
                    pts_ms: 2000,
                    byte_offset: 2000
                },
            ]
        );
    }

    #[test]
    fn build_entries_from_tables_treats_missing_stss_as_all_sync() {
        let table = StblTables {
            stts: vec![(2, 500)],
            stss: None,
            stsc: vec![(1, 2)],
            sample_sizes: SampleSizes::Uniform(50, 2),
            chunk_offsets: vec![10],
            timescale: 500,
        };
        let entries = build_entries_from_tables(&table).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].pts_ms, 0);
        assert_eq!(entries[1].pts_ms, 1000);
    }

    fn testdata_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files")
            .join(name)
    }

    #[test]
    fn sample_table_index_builds_from_real_corpus_file() {
        let path = testdata_path("h264_aac_mp4.mp4");
        if !path.exists() {
            return;
        }
        let entries = build_from_sample_tables(&path).unwrap();
        assert!(!entries.is_empty());
        assert!(entries.windows(2).all(|w| w[0].pts_ms <= w[1].pts_ms));
    }

    #[test]
    fn looks_like_mp4_detects_real_corpus_file() {
        let path = testdata_path("h264_aac_mp4.mp4");
        if !path.exists() {
            return;
        }
        assert!(looks_like_mp4(&path).unwrap());
    }
}
