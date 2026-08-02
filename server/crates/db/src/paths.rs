//! Library-relative path helpers (ADR-0030).

use std::path::{Path, PathBuf};

/// Strip trailing slashes from a library root (not `/` alone).
pub fn normalize_library_root(root: &str) -> String {
    let mut s = root.replace('\\', "/");
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    s
}

/// True when `stored` is still an absolute (unresolved) path.
///
/// On-disk discriminator (ADR-0030, Rule 4.9): `std::path::Path::is_absolute`
/// on the server host. Relpath writers must never produce a string that is
/// absolute under that predicate (no leading `/`, no Windows drive/UNC form).
/// Library root `/` is rejected for media libraries, so a leading `/` always
/// means transitional absolute leftover, not a relative segment.
pub fn is_absolute_stored(stored: &str) -> bool {
    Path::new(stored).is_absolute()
}

/// One helper for every open/display site (ADR-0030 §1, Rule 4.11).
/// Absolute stored values (migration leftovers) are used as-is; otherwise
/// join to the library root. Discrimination: [`is_absolute_stored`].
pub fn resolve_media_path(library_root: &str, stored: &str) -> PathBuf {
    if is_absolute_stored(stored) {
        PathBuf::from(stored)
    } else {
        Path::new(library_root).join(stored)
    }
}

/// Canonical relpath under `library_root`, or `None` if not under the root.
pub fn to_relpath(library_root: &str, absolute: &Path) -> Option<String> {
    let root = normalize_library_root(library_root);
    let abs = normalize_library_root(&absolute.to_string_lossy().replace('\\', "/"));
    if abs == root {
        return None;
    }
    let prefix = if root == "/" {
        "/".to_string()
    } else {
        format!("{root}/")
    };
    let rel = abs.strip_prefix(&prefix).or_else(|| {
        // ASCII-case-insensitive root (folding remounts).
        let abs_l = abs.to_ascii_lowercase();
        let pre_l = prefix.to_ascii_lowercase();
        abs_l
            .strip_prefix(&pre_l)
            .map(|_| &abs[prefix.len()..])
            .filter(|_| abs.len() >= prefix.len())
    })?;
    let rel = rel.replace('\\', "/");
    if rel.is_empty() || rel.starts_with('/') {
        return None;
    }
    if rel.split('/').any(|seg| seg == ".." || seg == ".") {
        return None;
    }
    Some(rel)
}

/// Case-fold each path segment for identity match (ADR-0030 §2).
pub fn fold_path(path: &str) -> String {
    path.replace('\\', "/")
        .split('/')
        .map(|seg| seg.to_lowercase())
        .collect::<Vec<_>>()
        .join("/")
}

pub fn paths_fold_equal(a: &str, b: &str) -> bool {
    fold_path(a) == fold_path(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relpath_strips_root() {
        assert_eq!(
            to_relpath("/media/TV", Path::new("/media/TV/Show/S01E01.mkv")).as_deref(),
            Some("Show/S01E01.mkv")
        );
        assert_eq!(
            to_relpath("/media/TV", Path::new("/media/Other/x.mkv")),
            None
        );
        assert_eq!(to_relpath("/media/TV", Path::new("/media/TV")), None);
    }

    #[test]
    fn resolve_mixed() {
        assert_eq!(
            resolve_media_path("/media/TV", "Show/ep.mkv"),
            PathBuf::from("/media/TV/Show/ep.mkv")
        );
        assert_eq!(
            resolve_media_path("/media/TV", "/old/abs/ep.mkv"),
            PathBuf::from("/old/abs/ep.mkv")
        );
    }

    #[test]
    fn fold_matches_case() {
        assert!(paths_fold_equal("Show/Ep.mkv", "show/ep.mkv"));
        assert!(!paths_fold_equal("Show/a.mkv", "Show/b.mkv"));
    }

    #[test]
    fn normalize_root_strips_slash() {
        assert_eq!(normalize_library_root("/media/TV/"), "/media/TV");
        assert_eq!(normalize_library_root("/"), "/");
    }
}
