//! Session-scoped virtual media files for byte-offset session start
//! (ADR-0023). One binding model, two mechanisms that are not
//! interchangeable:
//!
//! - Matroska splices `[0, first Cluster)` onto `[land Cluster, EOF)`, and
//!   FFmpeg opens that with **no `-ss`**.
//! - MP4 presents a faststart layout so FFmpeg **keeps its `-ss`** at the
//!   map PTS, seeking through an index instead of hunting the tail for
//!   `moov`.
//!
//! Never feed an MP4 into the Matroska splice: sample offsets in `moov` are
//! absolute in the original file (see [`mp4`]).
//!
//! Bytes reach FFmpeg over HTTP Range on loopback ([`range_server`]), bound
//! to the session that opened it. Identity is re-checked at every bind, so a
//! replacement landing mid-session degrades to `-ss` on the real file rather
//! than serving map offsets into new bytes.

mod matroska;
mod mp4;
mod range_server;

use range_server::{Piece, RangeServer, VirtualFile};
use std::ffi::OsString;
use std::path::Path;
use std::time::Instant;

/// Which byte offsets a map carries (ADR-0023 §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapContainerKind {
    /// Cluster absolute byte offset plus Cluster PTS.
    Matroska,
    /// Sync sample byte offset plus sample PTS.
    Mp4,
}

impl MapContainerKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "matroska" => Some(Self::Matroska),
            "mp4" => Some(Self::Mp4),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Matroska => "matroska",
            Self::Mp4 => "mp4",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyframeEntry {
    pub pts_ms: u64,
    pub byte_offset: u64,
}

/// The item's keyframe map as it stood at session create. Entries are
/// ordered by `pts_ms`; `content_id` is the identity they were built under.
#[derive(Debug, Clone)]
pub struct KeyframeMap {
    pub container_kind: MapContainerKind,
    pub content_id: String,
    pub entries: Vec<KeyframeEntry>,
}

impl KeyframeMap {
    /// Map as read from the item store (ADR-0023 §7). Bind-time identity
    /// revalidation against the bytes on disk still runs at every bind (§4).
    pub fn from_db_rows(rows: &nightjar_db::KeyframeMapRows) -> Option<Self> {
        let container_kind = MapContainerKind::parse(&rows.container_kind)?;
        Some(Self {
            container_kind,
            content_id: rows.content_id.clone(),
            entries: rows
                .entries
                .iter()
                .filter_map(|&(pts_ms, byte_offset)| {
                    Some(KeyframeEntry {
                        pts_ms: u64::try_from(pts_ms).ok()?,
                        byte_offset: u64::try_from(byte_offset).ok()?,
                    })
                })
                .collect(),
        })
    }

    /// Greatest entry at or before `pts_ms` — the land this session snaps to.
    pub fn entry_at_or_before(&self, pts_ms: u64) -> Option<KeyframeEntry> {
        let index = match self.entries.binary_search_by_key(&pts_ms, |e| e.pts_ms) {
            Ok(exact) => exact,
            Err(0) => return None,
            Err(after) => after - 1,
        };
        self.entries.get(index).copied()
    }

    /// End of the Matroska header: the byte offset of the Cluster holding
    /// the first keyframe. Only a map whose first entry is the title start
    /// pins that offset; anything else would splice playable Clusters into
    /// the header.
    fn header_end(&self) -> Option<u64> {
        let first = self.entries.first()?;
        (first.pts_ms == 0).then_some(first.byte_offset)
    }
}

/// A bound virtual file. Dropping it stops the server for that session.
pub(crate) struct VirtualInput {
    server: RangeServer,
    kind: MapContainerKind,
    /// Land the splice was built for. Matroska rebinds when the land moves;
    /// the MP4 layout is land-independent (§3c) and stays for the session.
    land_offset: u64,
}

impl VirtualInput {
    fn serves(&self, kind: MapContainerKind, land_offset: u64) -> bool {
        self.kind == kind && (kind == MapContainerKind::Mp4 || self.land_offset == land_offset)
    }
}

/// What FFmpeg should open, and how it is timed.
pub(crate) struct Bind {
    /// `-i` argument: a virtual-file URL, or the real path when the source
    /// is already faststart.
    pub input: OsString,
    /// Whether FFmpeg seeks inside the input (`-ss`). False for the Matroska
    /// splice, which already starts at the land.
    pub seek_input: bool,
    /// Snapped land (map PTS): `-output_ts_offset`, `landedMs`.
    pub land_ms: u64,
    pub virtual_input: Option<VirtualInput>,
}

/// Re-reads the live identity windows and compares them to the map stamp
/// (ADR-0023 §4). Returns the read cost so every bind can log it.
pub(crate) fn verify_identity(src: &Path, content_id: &str) -> Result<u128, String> {
    let started = Instant::now();
    let live = nightjar_db::content_id_for_path(src)?;
    let cost_ms = started.elapsed().as_millis();
    if live != content_id {
        return Err(format!(
            "content_id changed under {} (map {content_id}, live {live})",
            src.display()
        ));
    }
    Ok(cost_ms)
}

/// Binds the virtual file for `want_ms`, reusing `bound` when it already
/// serves this land.
pub(crate) fn bind(
    src: &Path,
    map: &KeyframeMap,
    want_ms: u64,
    bound: Option<VirtualInput>,
) -> Result<Bind, String> {
    let entry = map
        .entry_at_or_before(want_ms)
        .ok_or_else(|| format!("no map entry at or before {want_ms}ms"))?;
    match map.container_kind {
        MapContainerKind::Matroska => bind_matroska(src, map, entry, bound),
        MapContainerKind::Mp4 => bind_mp4(src, entry, bound),
    }
}

fn bind_matroska(
    src: &Path,
    map: &KeyframeMap,
    entry: KeyframeEntry,
    bound: Option<VirtualInput>,
) -> Result<Bind, String> {
    if let Some(input) = bound.filter(|b| b.serves(MapContainerKind::Matroska, entry.byte_offset)) {
        return Ok(Bind {
            input: input.server.url().into(),
            seek_input: false,
            land_ms: entry.pts_ms,
            virtual_input: Some(input),
        });
    }
    let header_end = map
        .header_end()
        .ok_or_else(|| "map has no title-start entry to end the header at".to_string())?;
    // A packet-walk map records block positions inside a Cluster; splicing
    // one of those hands FFmpeg garbage that still parses as a header.
    if !matroska::is_cluster_start(src, header_end)? {
        return Err(format!("map header offset {header_end} is not a Cluster"));
    }
    if !matroska::is_cluster_start(src, entry.byte_offset)? {
        return Err(format!(
            "map land offset {} is not a Cluster",
            entry.byte_offset
        ));
    }
    let file_len = std::fs::metadata(src)
        .map_err(|e| format!("stat {}: {e}", src.display()))?
        .len();
    if entry.byte_offset >= file_len {
        return Err(format!(
            "map land offset {} past end of {}",
            entry.byte_offset,
            src.display()
        ));
    }
    let pieces = vec![
        Piece::FileRange {
            offset: 0,
            len: header_end,
        },
        Piece::FileRange {
            offset: entry.byte_offset,
            len: file_len - entry.byte_offset,
        },
    ];
    let server = RangeServer::start(VirtualFile::new(src, pieces, "video/x-matroska", "mkv"))?;
    Ok(Bind {
        input: server.url().into(),
        seek_input: false,
        land_ms: entry.pts_ms,
        virtual_input: Some(VirtualInput {
            server,
            kind: MapContainerKind::Matroska,
            land_offset: entry.byte_offset,
        }),
    })
}

fn bind_mp4(src: &Path, entry: KeyframeEntry, bound: Option<VirtualInput>) -> Result<Bind, String> {
    if let Some(input) = bound.filter(|b| b.serves(MapContainerKind::Mp4, entry.byte_offset)) {
        return Ok(Bind {
            input: input.server.url().into(),
            seek_input: true,
            land_ms: entry.pts_ms,
            virtual_input: Some(input),
        });
    }
    match mp4::plan(src)? {
        // Already faststart: the map snap is the whole win, FFmpeg seeks the
        // real path through its own index.
        mp4::Layout::Faststart => Ok(Bind {
            input: src.as_os_str().to_owned(),
            seek_input: true,
            land_ms: entry.pts_ms,
            virtual_input: None,
        }),
        mp4::Layout::Relocate {
            prefix_len,
            moov,
            mdat_offset,
            mdat_len,
        } => {
            let pieces = vec![
                Piece::FileRange {
                    offset: 0,
                    len: prefix_len,
                },
                Piece::Bytes(moov),
                Piece::FileRange {
                    offset: mdat_offset,
                    len: mdat_len,
                },
            ];
            let server = RangeServer::start(VirtualFile::new(src, pieces, "video/mp4", "mp4"))?;
            Ok(Bind {
                input: server.url().into(),
                seek_input: true,
                land_ms: entry.pts_ms,
                virtual_input: Some(VirtualInput {
                    server,
                    kind: MapContainerKind::Mp4,
                    land_offset: entry.byte_offset,
                }),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(u64, u64)]) -> KeyframeMap {
        KeyframeMap {
            container_kind: MapContainerKind::Matroska,
            content_id: "stamp".into(),
            entries: entries
                .iter()
                .map(|&(pts_ms, byte_offset)| KeyframeEntry {
                    pts_ms,
                    byte_offset,
                })
                .collect(),
        }
    }

    #[test]
    fn snaps_back_to_the_entry_at_or_before() {
        let map = map(&[(0, 100), (2000, 500), (5000, 900)]);
        assert_eq!(map.entry_at_or_before(0).unwrap().byte_offset, 100);
        assert_eq!(map.entry_at_or_before(1999).unwrap().byte_offset, 100);
        assert_eq!(map.entry_at_or_before(2000).unwrap().byte_offset, 500);
        assert_eq!(map.entry_at_or_before(9999).unwrap().pts_ms, 5000);
    }

    #[test]
    fn header_end_needs_a_title_start_entry() {
        assert_eq!(map(&[(0, 100), (2000, 500)]).header_end(), Some(100));
        assert_eq!(map(&[(2000, 500)]).header_end(), None);
        assert_eq!(map(&[]).entry_at_or_before(0), None);
    }

    #[test]
    fn identity_mismatch_is_reported_not_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.mkv");
        std::fs::write(&path, b"original bytes").unwrap();
        let live = nightjar_db::content_id_for_path(&path).unwrap();
        assert!(verify_identity(&path, &live).is_ok());
        std::fs::write(&path, b"replaced bytes").unwrap();
        assert!(verify_identity(&path, &live).is_err());
    }

    /// NAS-gated smoke: real end-moov through virtual faststart + FFmpeg
    /// copy `-ss`. Catches range-server truncation (nonblocking accept
    /// inherit) that synthetic fixtures never filled a send buffer hard
    /// enough to surface. AAC interleave coverage lives in `hls` tests.
    #[test]
    fn greys_end_moov_bind_survives_ffmpeg_copy_seek() {
        let src = std::path::Path::new(
            "/Volumes/media/TV Shows/Greys Anatomy/Season 6/\
             Grey's Anatomy - 6x14 - Valentine's Day Massacre - WEBRip-1080p.mp4",
        );
        if !src.is_file() {
            eprintln!("skipping: dogfood end-moov not mounted");
            return;
        }
        let bind = bind_mp4(
            src,
            KeyframeEntry {
                pts_ms: 58_976,
                byte_offset: 18_012_697,
            },
            None,
        )
        .expect("bind");
        assert!(bind.virtual_input.is_some());
        let url = bind.input.to_string_lossy().into_owned();
        let dir = tempfile::tempdir().unwrap();
        let err = dir.path().join("err");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-ss",
                "58.976",
                "-i",
                &url,
                "-t",
                "4",
                "-c",
                "copy",
            ])
            .arg(dir.path().join("out.mp4"))
            .stderr(std::fs::File::create(&err).unwrap())
            .status()
            .unwrap();
        let err_text = std::fs::read_to_string(&err).unwrap();
        assert!(
            status.success(),
            "ffmpeg against rust virtual faststart failed: {err_text}"
        );
        drop(bind);
    }
}
