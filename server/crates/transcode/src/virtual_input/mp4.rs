//! MP4 virtual faststart (ADR-0023 §3b).
//!
//! End-moov sources make FFmpeg hunt the tail for `moov` before it can seek.
//! The virtual file presents `[ftyp…][moov'][mdat]` instead, with every
//! `stco`/`co64` chunk offset shifted by the size of the relocated `moov`
//! (the classic qt-faststart delta), and `mdat` served by Range onto the
//! original extent — no media copy.
//!
//! The rewrite is load-bearing, not polish. A splice that moves `moov` in
//! front without rewriting chunk offsets still emits a segment whose `sidx`
//! matches the requested land while audio decodes into garbage (observed:
//! AAC `channel element … not allocated`). The metric looks right and the
//! stream is broken, which is why the Matroska splice must never be reused
//! here.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Box types whose payload is a list of child boxes on the path to `stbl`.
const CONTAINER_BOXES: [&[u8; 4]; 5] = [b"moov", b"trak", b"mdia", b"minf", b"stbl"];

pub(crate) enum Layout {
    /// `moov` already precedes `mdat`: FFmpeg's own `-ss` is an index seek
    /// on the real path, so no virtual file is bound.
    Faststart,
    /// End-moov: serve `[prefix][moov'][mdat]`.
    Relocate {
        /// Bytes `[0, mdat_offset)` of the real file (ftyp and any free/wide).
        prefix_len: u64,
        /// Rewritten `moov`.
        moov: Vec<u8>,
        mdat_offset: u64,
        mdat_len: u64,
    },
}

struct BoxHeader {
    kind: [u8; 4],
    offset: u64,
    size: u64,
}

/// Reads the top-level box list and decides which layout this file needs.
///
/// Reads box headers only until it knows the order; the `moov` payload is
/// read (and rewritten) only for end-moov sources.
pub(crate) fn plan(path: &Path) -> Result<Layout, String> {
    let mut file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let file_len = file
        .metadata()
        .map_err(|e| format!("stat {}: {e}", path.display()))?
        .len();
    let boxes = read_top_level_boxes(&mut file, file_len)?;

    let moov = boxes
        .iter()
        .find(|b| &b.kind == b"moov")
        .ok_or_else(|| format!("no moov box in {}", path.display()))?;
    let mut mdats = boxes.iter().filter(|b| &b.kind == b"mdat");
    let mdat = mdats
        .next()
        .ok_or_else(|| format!("no mdat box in {}", path.display()))?;
    if mdats.next().is_some() {
        // Chunk offsets would need a per-mdat delta; this is not that file.
        return Err(format!("multiple mdat boxes in {}", path.display()));
    }
    if moov.offset < mdat.offset {
        return Ok(Layout::Faststart);
    }

    let mut moov_bytes = vec![0u8; usize::try_from(moov.size).map_err(|_| "moov too large")?];
    file.seek(SeekFrom::Start(moov.offset))
        .map_err(|e| format!("seek to moov in {}: {e}", path.display()))?;
    file.read_exact(&mut moov_bytes)
        .map_err(|e| format!("read moov in {}: {e}", path.display()))?;
    let delta = moov.size;
    let rewritten = rewrite_chunk_offsets(&mut moov_bytes, delta)?;
    if rewritten == 0 {
        return Err(format!("no stco/co64 offsets in {}", path.display()));
    }
    Ok(Layout::Relocate {
        prefix_len: mdat.offset,
        moov: moov_bytes,
        mdat_offset: mdat.offset,
        mdat_len: mdat.size,
    })
}

fn read_top_level_boxes(file: &mut File, file_len: u64) -> Result<Vec<BoxHeader>, String> {
    let mut boxes = Vec::new();
    let mut pos = 0u64;
    while pos + 8 <= file_len {
        let mut header = [0u8; 16];
        file.seek(SeekFrom::Start(pos))
            .map_err(|e| format!("seek to box at {pos}: {e}"))?;
        let read = read_up_to(file, &mut header)?;
        if read < 8 {
            break;
        }
        let mut kind = [0u8; 4];
        kind.copy_from_slice(&header[4..8]);
        let short_size = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        let (size, header_len) = match short_size {
            1 if read >= 16 => (
                u64::from_be_bytes(header[8..16].try_into().unwrap_or_default()),
                16u64,
            ),
            0 => (file_len - pos, 8),
            n if n >= 8 => (u64::from(n), 8),
            _ => break,
        };
        if size < header_len || pos + size > file_len {
            break;
        }
        boxes.push(BoxHeader {
            kind,
            offset: pos,
            size,
        });
        pos += size;
    }
    Ok(boxes)
}

fn read_up_to(file: &mut File, buf: &mut [u8]) -> Result<usize, String> {
    let mut total = 0;
    while total < buf.len() {
        match file.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("read box header: {e}")),
        }
    }
    Ok(total)
}

/// Adds `delta` to every chunk offset in `moov`, walking the box tree rather
/// than scanning for the four-byte type, so a `stco` pattern inside sample
/// data can never be "rewritten" into corrupt offsets.
///
/// Returns the number of offsets touched.
pub(crate) fn rewrite_chunk_offsets(moov: &mut [u8], delta: u64) -> Result<usize, String> {
    rewrite_in_children(moov, delta)
}

fn rewrite_in_children(data: &mut [u8], delta: u64) -> Result<usize, String> {
    let mut rewritten = 0;
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let size = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap_or_default()) as usize;
        let mut kind = [0u8; 4];
        kind.copy_from_slice(&data[pos + 4..pos + 8]);
        let (size, header_len) = match size {
            1 => {
                if pos + 16 > data.len() {
                    break;
                }
                let large =
                    u64::from_be_bytes(data[pos + 8..pos + 16].try_into().unwrap_or_default());
                (usize::try_from(large).map_err(|_| "box too large")?, 16)
            }
            0 => (data.len() - pos, 8),
            n if n >= 8 => (n, 8),
            _ => break,
        };
        if size < header_len || pos + size > data.len() {
            break;
        }
        let body = &mut data[pos + header_len..pos + size];
        if CONTAINER_BOXES.contains(&&kind) {
            rewritten += rewrite_in_children(body, delta)?;
        } else if &kind == b"stco" {
            rewritten += shift_offsets(body, delta, 4)?;
        } else if &kind == b"co64" {
            rewritten += shift_offsets(body, delta, 8)?;
        }
        pos += size;
    }
    Ok(rewritten)
}

/// `stco`/`co64` body: version+flags (4), entry_count (4), then entries of
/// `width` bytes each.
fn shift_offsets(body: &mut [u8], delta: u64, width: usize) -> Result<usize, String> {
    if body.len() < 8 {
        return Err("chunk offset box too short".into());
    }
    let count = u32::from_be_bytes(body[4..8].try_into().unwrap_or_default()) as usize;
    let mut shifted = 0;
    for index in 0..count {
        let start = 8 + index * width;
        let Some(entry) = body.get_mut(start..start + width) else {
            return Err(format!(
                "chunk offset entry {index} of {count} runs past the box"
            ));
        };
        if width == 4 {
            let value = u32::from_be_bytes(entry.try_into().unwrap_or_default());
            let shifted_value = u64::from(value) + delta;
            let narrowed = u32::try_from(shifted_value)
                .map_err(|_| "chunk offset overflows stco; source needs co64".to_string())?;
            entry.copy_from_slice(&narrowed.to_be_bytes());
        } else {
            let value = u64::from_be_bytes(entry.try_into().unwrap_or_default());
            entry.copy_from_slice(&(value + delta).to_be_bytes());
        }
        shifted += 1;
    }
    Ok(shifted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    fn stco(offsets: &[u32]) -> Vec<u8> {
        let mut body = vec![0u8; 4];
        body.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
        for offset in offsets {
            body.extend_from_slice(&offset.to_be_bytes());
        }
        mp4_box(b"stco", &body)
    }

    fn moov_with(stbl_children: Vec<u8>) -> Vec<u8> {
        let stbl = mp4_box(b"stbl", &stbl_children);
        let minf = mp4_box(b"minf", &stbl);
        let mdia = mp4_box(b"mdia", &minf);
        let trak = mp4_box(b"trak", &mdia);
        mp4_box(b"moov", &trak)
    }

    fn read_stco(moov: &[u8]) -> Vec<u32> {
        let at = find(moov, b"stco").expect("stco present");
        let count = u32::from_be_bytes(moov[at + 12..at + 16].try_into().unwrap()) as usize;
        (0..count)
            .map(|i| {
                let s = at + 16 + i * 4;
                u32::from_be_bytes(moov[s..s + 4].try_into().unwrap())
            })
            .collect()
    }

    /// Start of the box whose type is `needle`, last match first so a decoy
    /// pattern earlier in the tree does not answer for the real box.
    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .rposition(|w| w == needle)
            .map(|p| p - 4)
    }

    #[test]
    fn every_chunk_offset_shifts_by_the_moov_size() {
        let mut moov = moov_with(stco(&[100, 200, 300]));
        assert_eq!(rewrite_chunk_offsets(&mut moov, 1000).unwrap(), 3);
        assert_eq!(read_stco(&moov), vec![1100, 1200, 1300]);
    }

    #[test]
    fn co64_entries_shift_as_64_bit() {
        let mut body = vec![0u8; 4];
        body.extend_from_slice(&1u32.to_be_bytes());
        body.extend_from_slice(&(u64::from(u32::MAX) + 10).to_be_bytes());
        let mut moov = moov_with(mp4_box(b"co64", &body));
        assert_eq!(rewrite_chunk_offsets(&mut moov, 5).unwrap(), 1);
        let at = find(&moov, b"co64").unwrap();
        let value = u64::from_be_bytes(moov[at + 16..at + 24].try_into().unwrap());
        assert_eq!(value, u64::from(u32::MAX) + 15);
    }

    /// A `stco` pattern inside sample-description payload is data, not a box:
    /// shifting it would move chunk offsets that describe nothing.
    #[test]
    fn pattern_outside_the_box_tree_is_left_alone() {
        let mut decoy = b"junkstco".to_vec();
        decoy.extend_from_slice(&[0u8; 16]);
        let mut children = mp4_box(b"stsd", &decoy);
        children.extend(stco(&[64]));
        let mut moov = moov_with(children);
        assert_eq!(rewrite_chunk_offsets(&mut moov, 8).unwrap(), 1);
        assert_eq!(read_stco(&moov), vec![72]);
        let decoy_at = moov
            .windows(8)
            .position(|w| w == b"junkstco")
            .expect("decoy present");
        assert!(
            moov[decoy_at + 8..decoy_at + 24].iter().all(|b| *b == 0),
            "sample-description payload must survive the rewrite"
        );
    }

    #[test]
    fn truncated_entry_list_is_an_error_not_a_partial_rewrite() {
        let mut body = vec![0u8; 4];
        body.extend_from_slice(&4u32.to_be_bytes());
        body.extend_from_slice(&7u32.to_be_bytes());
        let mut moov = moov_with(mp4_box(b"stco", &body));
        assert!(rewrite_chunk_offsets(&mut moov, 1).is_err());
    }
}
