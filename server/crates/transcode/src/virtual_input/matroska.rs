//! Matroska Cluster splice (ADR-0023 §3a).
//!
//! The virtual file is `[0, first Cluster)` followed by
//! `[land Cluster, EOF)`. A Cluster is self-contained, so that splice is a
//! valid Matroska body and FFmpeg opens directly at the land with no `-ss`.
//!
//! Both offsets come from the keyframe map and are verified here: a map
//! built by packet walk records block positions, which sit inside a Cluster
//! and would demux as garbage. Bad offset means no splice, not a bad splice.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const CLUSTER_ID: [u8; 4] = [0x1F, 0x43, 0xB6, 0x75];

/// Confirms `offset` starts a Cluster element in `path`.
pub(crate) fn is_cluster_start(path: &Path, offset: u64) -> Result<bool, String> {
    let mut file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| format!("seek to {offset} in {}: {e}", path.display()))?;
    let mut id = [0u8; 4];
    match file.read_exact(&mut id) {
        Ok(()) => Ok(id == CLUSTER_ID),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(format!("read cluster id at {offset}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_id_is_checked_at_the_exact_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.mkv");
        let mut bytes = vec![0u8; 32];
        bytes[8..12].copy_from_slice(&CLUSTER_ID);
        std::fs::write(&path, &bytes).unwrap();
        assert!(is_cluster_start(&path, 8).unwrap());
        // One byte into the Cluster header is where a block position lands.
        assert!(!is_cluster_start(&path, 9).unwrap());
        assert!(!is_cluster_start(&path, 0).unwrap());
        assert!(!is_cluster_start(&path, 1024).unwrap());
    }
}
