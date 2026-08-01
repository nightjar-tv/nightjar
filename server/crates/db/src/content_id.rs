//! Media-file content identity (ADR-0023).
//!
//! `content_id` = `{size_bytes}-{sha256_hex(first 64 KiB)}-{sha256_hex(last 64 KiB)}`.
//! Prefix/suffix are truncated when the file is smaller than 64 KiB (hash the
//! bytes that exist). Stored comparison is string equality against derived
//! stamps. Bind-time revalidation re-reads the windows (§4 / §8 in the ADR).
//!
//! This is an identity fingerprint, not a security boundary. SHA-256 is used
//! because it is already in the tree; a lighter non-cryptographic hash is a
//! fair future argument, but must not be hand-rolled.

use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Bytes hashed from the head and tail of the media file.
pub const CONTENT_ID_WINDOW: usize = 64 * 1024;

/// Build a content_id from size and already-hex-encoded SHA-256 digests.
///
/// `first_sha256_hex` / `last_sha256_hex` must be lowercase 64-char hex.
pub fn format_content_id(
    size_bytes: u64,
    first_sha256_hex: &str,
    last_sha256_hex: &str,
) -> Result<String, String> {
    validate_sha256_hex(first_sha256_hex, "first")?;
    validate_sha256_hex(last_sha256_hex, "last")?;
    Ok(format!("{size_bytes}-{first_sha256_hex}-{last_sha256_hex}"))
}

/// Compute `content_id` for a media file on disk.
pub fn content_id_for_path(path: &Path) -> Result<String, String> {
    let mut file =
        File::open(path).map_err(|e| format!("open for content_id {}: {e}", path.display()))?;
    let size = file
        .metadata()
        .map_err(|e| format!("stat for content_id {}: {e}", path.display()))?
        .len();
    content_id_from_reader(&mut file, size)
        .map_err(|e| format!("content_id {}: {e}", path.display()))
}

/// Compute `content_id` from an already-open readable+seekable handle.
pub fn content_id_from_reader<R: Read + Seek>(
    reader: &mut R,
    size_bytes: u64,
) -> Result<String, String> {
    let (first, last) = read_windows(reader, size_bytes)?;
    format_content_id(size_bytes, &sha256_hex(&first), &sha256_hex(&last))
}

/// True when `derived` was built under the live `content_id`.
///
/// Missing either side is stale: no map / no probe stamp must not look valid.
pub fn content_id_matches(live: Option<&str>, derived: Option<&str>) -> bool {
    match (live, derived) {
        (Some(a), Some(b)) if !a.is_empty() && !b.is_empty() => a == b,
        _ => false,
    }
}

fn read_windows<R: Read + Seek>(
    reader: &mut R,
    size_bytes: u64,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    let win = CONTENT_ID_WINDOW as u64;
    let first_len = size_bytes.min(win) as usize;
    let mut first = vec![0u8; first_len];
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|e| format!("seek start: {e}"))?;
    reader
        .read_exact(&mut first)
        .map_err(|e| format!("read first window: {e}"))?;

    if size_bytes <= win {
        return Ok((first.clone(), first));
    }

    let mut last = vec![0u8; CONTENT_ID_WINDOW];
    reader
        .seek(SeekFrom::End(-(win as i64)))
        .map_err(|e| format!("seek end window: {e}"))?;
    reader
        .read_exact(&mut last)
        .map_err(|e| format!("read last window: {e}"))?;
    Ok((first, last))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn validate_sha256_hex(s: &str, which: &str) -> Result<(), String> {
    if s.len() != 64 {
        return Err(format!(
            "{which} sha256 hex must be 64 chars, got {}",
            s.len()
        ));
    }
    if !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(format!("{which} sha256 hex must be lowercase hex"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn formats_size_and_two_digests() {
        let id = format_content_id(1_024, A, B).unwrap();
        assert_eq!(id, format!("1024-{A}-{B}"));
    }

    #[test]
    fn rejects_bad_hex_length() {
        assert!(format_content_id(1, "abcd", B).is_err());
    }

    #[test]
    fn rejects_uppercase_hex() {
        let upper = A.to_uppercase();
        assert!(format_content_id(1, &upper, B).is_err());
    }

    #[test]
    fn match_requires_both_sides() {
        assert!(!content_id_matches(None, Some(A)));
        assert!(!content_id_matches(Some(A), None));
        assert!(!content_id_matches(Some(""), Some(A)));
        assert!(content_id_matches(Some("x"), Some("x")));
        assert!(!content_id_matches(Some("x"), Some("y")));
    }

    #[test]
    fn small_file_first_and_last_are_same_window() {
        let bytes = b"hello-content-id";
        let mut cur = Cursor::new(bytes.as_slice());
        let id = content_id_from_reader(&mut cur, bytes.len() as u64).unwrap();
        let digest = sha256_hex(bytes);
        assert_eq!(id, format!("{}-{digest}-{digest}", bytes.len()));
    }

    #[test]
    fn large_file_hashes_distinct_windows() {
        let mut bytes = vec![0u8; CONTENT_ID_WINDOW + 8];
        bytes[..4].copy_from_slice(b"HEAD");
        let n = bytes.len();
        bytes[n - 4..].copy_from_slice(b"TAIL");
        let mut cur = Cursor::new(bytes.as_slice());
        let id = content_id_from_reader(&mut cur, n as u64).unwrap();
        let first = sha256_hex(&bytes[..CONTENT_ID_WINDOW]);
        let last = sha256_hex(&bytes[n - CONTENT_ID_WINDOW..]);
        assert_ne!(first, last);
        assert_eq!(id, format!("{n}-{first}-{last}"));
    }

    #[test]
    fn path_helper_matches_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.bin");
        std::fs::write(&path, b"path-helper").unwrap();
        let from_path = content_id_for_path(&path).unwrap();
        let mut f = File::open(&path).unwrap();
        let size = f.metadata().unwrap().len();
        let from_reader = content_id_from_reader(&mut f, size).unwrap();
        assert_eq!(from_path, from_reader);
    }
}
