//! Media-file content identity (ADR-0023).
//!
//! `content_id` = `{size_bytes}-{sha256_hex(first 64 KiB)}-{sha256_hex(last 64 KiB)}`.
//! Prefix/suffix are truncated when the file is smaller than 64 KiB (hash the
//! bytes that exist). Invalidation is string equality against stored stamps —
//! not a re-read of the file.
//!
//! Hashing the file bytes is the writer's job (needs a digest crate). This
//! module owns only the on-disk string shape so schema and writers cannot drift.

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

/// True when `derived` was built under the live `content_id`.
///
/// Missing either side is stale: no map / no probe stamp must not look valid.
pub fn content_id_matches(live: Option<&str>, derived: Option<&str>) -> bool {
    match (live, derived) {
        (Some(a), Some(b)) if !a.is_empty() && !b.is_empty() => a == b,
        _ => false,
    }
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
}
