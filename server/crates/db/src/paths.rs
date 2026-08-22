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

/// Validate and normalise a media library root before write (ADR-0030).
pub fn require_library_root(root: &str) -> Result<String, String> {
    let root = normalize_library_root(root);
    if root.is_empty() || root == "/" {
        return Err("library root must be an absolute path other than /".into());
    }
    if looks_windows_path_form(&root) {
        return Err(format!(
            "library root rejects Windows path form (v1 is POSIX/Docker only): {root}"
        ));
    }
    if !Path::new(&root).is_absolute() {
        return Err(format!("library root must be absolute: {root}"));
    }
    Ok(root)
}

/// True when `stored` is still an absolute (unresolved) path.
///
/// On-disk discriminator (ADR-0030, Rule 4.9): `std::path::Path::is_absolute`
/// on the server host. Relpath writers must never produce a string that is
/// absolute under that predicate — enforced at the write boundary by
/// [`require_relpath`], not by call-site discipline alone.
pub fn is_absolute_stored(stored: &str) -> bool {
    Path::new(stored).is_absolute()
}

/// Write-boundary check for library-relative path strings (ADR-0030).
/// Call before every INSERT/UPDATE that stores a *relpath* (not migration
/// leftovers that intentionally remain absolute).
pub fn require_relpath(path: &str) -> Result<&str, String> {
    if path.is_empty() {
        return Err("relpath must not be empty".into());
    }
    if path.contains('\\') {
        return Err(format!("relpath must use / separators only: {path}"));
    }
    if looks_windows_path_form(path) {
        return Err(format!(
            "relpath rejects Windows path form (v1 is POSIX/Docker only): {path}"
        ));
    }
    if is_absolute_stored(path) {
        return Err(format!("relpath must not be absolute: {path}"));
    }
    if path
        .split('/')
        .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return Err(format!("relpath has empty, '.', or '..' segment: {path}"));
    }
    Ok(path)
}

/// Drive letter, drive-relative (`C:foo`), or UNC — out of scope for v1.
fn looks_windows_path_form(path: &str) -> bool {
    if path.starts_with("\\\\") {
        return true;
    }
    let b = path.as_bytes();
    b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic()
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
    require_relpath(&rel).ok()?;
    Some(rel)
}

/// Show folder relpath for an episode file (ADR-0033 Q2): the **deepest**
/// directory holding the file that is not itself a season directory. `Season
/// N/` and `Specials/` inherit the show folder's series row. Returns `""` when
/// the library root is itself the show folder.
///
/// Path-walk only: walk up from the file and stop at the first directory
/// component that is not season-named. The first line of this doc used to read
/// "the highest directory under the library root", which describes the opposite
/// walk — under it, every show inside a genre or first-letter folder would share
/// one show folder. Both the migration that retro-derives series rows and the
/// queue's group formation call this, so the two always agree on the folder key.
pub fn show_folder_relpath(stored: &str, library_root: &str) -> String {
    let rel = if is_absolute_stored(stored) {
        to_relpath(library_root, Path::new(stored)).unwrap_or_else(|| stored.to_string())
    } else {
        stored.to_string()
    };
    let mut parts: Vec<&str> = rel.split('/').collect();
    parts.pop(); // filename
    while parts.last().is_some_and(|seg| is_season_directory(seg)) {
        parts.pop();
    }
    parts.join("/")
}

/// A directory segment naming a **numbered** season — `Season 3`, `S03`.
///
/// **Not `Specials`, `Special`, `Extras` or `Extra`.** Those are season
/// directories for the purpose of [`show_folder_relpath`], which must walk up
/// past all of them, and they are *not* numbered for the purpose of deciding
/// what a file inside one can be. TMDB models `Top Gear: Polar Special` as a
/// standalone **movie**, so a file in a `Specials/` folder may honestly bind to
/// a film; a file in `Season 3/` may not.
///
/// Exposed here, beside the rule it is half of, rather than rewritten in the
/// scanner. A second predicate about the same naming convention written
/// somewhere else is the reimplemented-`norm_key` trap — it reported 25 folders
/// against the shipped chain's 12.
pub fn is_numbered_season_directory(seg: &str) -> bool {
    let s = seg.trim().to_ascii_lowercase();
    if let Some(rest) = s.strip_prefix("season ") {
        return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit());
    }
    let b = s.as_bytes();
    b.len() >= 2 && b[0] == b's' && b[1..].iter().all(u8::is_ascii_digit)
}

fn is_season_directory(seg: &str) -> bool {
    let s = seg.trim().to_ascii_lowercase();
    matches!(s.as_str(), "specials" | "special" | "extras" | "extra")
        || is_numbered_season_directory(seg)
}

/// Does this stored path sit inside a **numbered** season directory?
///
/// The same walk [`show_folder_relpath`] performs — up from the file, through
/// the season-directory tail — asking of that tail whether any segment is
/// numbered. One walk, two questions, so the two cannot disagree about where
/// the show folder starts.
///
/// `Show/Specials/x.mkv` is **false**. `Show/Season 03/x.mkv` is true, and so is
/// `Show/Season 03/Extras/x.mkv`: the tail holds a numbered season.
pub fn under_numbered_season_directory(stored: &str, library_root: &str) -> bool {
    let rel = if is_absolute_stored(stored) {
        to_relpath(library_root, Path::new(stored)).unwrap_or_else(|| stored.to_string())
    } else {
        stored.to_string()
    };
    let mut parts: Vec<&str> = rel.split('/').collect();
    parts.pop(); // filename
    let mut numbered = false;
    while parts.last().is_some_and(|seg| is_season_directory(seg)) {
        numbered |= is_numbered_season_directory(parts.pop().unwrap_or_default());
    }
    numbered
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

    /// **`Specials/` is a season directory and is not a numbered one**, and the
    /// whole `wrong.kind` rule turns on the difference. TMDB models `Top Gear:
    /// Polar Special` as a standalone movie record, so a file in a `Specials/`
    /// folder may honestly bind to a film; a file in `Season 3/` may not.
    ///
    /// The earlier attempt at this rule did not draw the distinction, scored as
    /// a free win on the generated oracle, and destroyed five correct bindings
    /// in the real library.
    #[test]
    fn specials_is_a_season_directory_but_not_a_numbered_one() {
        for seg in ["Season 3", "Season 03", "S03", "s3", "SEASON 12"] {
            assert!(is_season_directory(seg), "{seg}");
            assert!(is_numbered_season_directory(seg), "{seg}");
        }
        for seg in ["Specials", "specials", "Special", "Extras", "extra"] {
            assert!(is_season_directory(seg), "{seg}");
            assert!(
                !is_numbered_season_directory(seg),
                "{seg} must not read as a numbered season — a film may live here"
            );
        }
        for seg in ["Top Gear", "Season", "Sxx", "Season two"] {
            assert!(!is_numbered_season_directory(seg), "{seg}");
        }
    }

    /// The walk is the one `show_folder_relpath` performs, asked a second
    /// question — so the two cannot disagree about where the show folder starts.
    #[test]
    fn under_numbered_season_reads_the_whole_season_tail() {
        let root = "/media/TV";
        for p in [
            "Top Gear/Season 16/Top Gear - 16x00 - Special.mkv",
            "Top Gear/S16/x.mkv",
            "/media/TV/Top Gear/Season 16/x.mkv",
            // A numbered season anywhere in the season tail counts.
            "Top Gear/Season 16/Extras/x.mkv",
        ] {
            assert!(under_numbered_season_directory(p, root), "{p}");
        }
        for p in [
            "Top Gear/Specials/Polar Special.mkv",
            "Top Gear/Extras/x.mkv",
            "Top Gear/x.mkv",
            "x.mkv",
        ] {
            assert!(
                !under_numbered_season_directory(p, root),
                "{p} — nothing here says the file cannot be a film"
            );
        }
    }

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

    #[test]
    fn require_relpath_rejects_absolute_and_windows_forms() {
        assert!(require_relpath("Show/ep.mkv").is_ok());
        assert!(require_relpath("/Show/ep.mkv").is_err());
        assert!(require_relpath("C:foo.mkv").is_err());
        assert!(require_relpath("C:/Movies/a.mkv").is_err());
        assert!(require_relpath("\\\\server\\share\\a.mkv").is_err());
        assert!(require_relpath("a\\b.mkv").is_err());
        assert!(require_relpath("../x.mkv").is_err());
        assert!(require_relpath("").is_err());
    }

    #[test]
    fn require_library_root_rejects_slash_and_relative() {
        assert!(require_library_root("/media/TV").is_ok());
        assert!(require_library_root("/").is_err());
        assert!(require_library_root("media/TV").is_err());
        assert!(require_library_root("C:\\Media").is_err());
    }

    #[test]
    fn show_folder_skips_season_dirs_and_specials() {
        // Standard layout: the show folder is the first path component.
        assert_eq!(
            show_folder_relpath("Shameless (US)/Season 1/ep.mkv", "/media/TV"),
            "Shameless (US)"
        );
        assert_eq!(
            show_folder_relpath("Shameless (UK)/Season 01/ep.mkv", "/media/TV"),
            "Shameless (UK)"
        );
        // Specials/ inherits the show folder.
        assert_eq!(
            show_folder_relpath("Alpha/Specials/S00E01.mkv", "/media/TV"),
            "Alpha"
        );
        // Season 1 and Season 2 land on the same folder.
        assert_eq!(
            show_folder_relpath("Alpha/Season 2/ep.mkv", "/media/TV"),
            "Alpha"
        );
        // A flat show folder (episodes directly under it).
        assert_eq!(show_folder_relpath("Alpha/ep.mkv", "/media/TV"), "Alpha");
        // Deep nesting: the folder under the top-level grouping dir.
        assert_eq!(
            show_folder_relpath("Anime/One Piece/Season 1/ep.mkv", "/media/TV"),
            "Anime/One Piece"
        );
        // Library root is the show folder: season dirs walk up to nothing.
        assert_eq!(show_folder_relpath("Season 1/ep.mkv", "/media/TV"), "");
        assert_eq!(show_folder_relpath("ep.mkv", "/media/TV"), "");
        // Absolute leftover paths are root-stripped before the walk.
        assert_eq!(
            show_folder_relpath("/media/TV/Shameless (US)/Season 1/ep.mkv", "/media/TV"),
            "Shameless (US)"
        );
    }

    #[test]
    fn show_folder_distinguishes_fold_colliding_siblings() {
        // The D2 class: two siblings that fold to one matcher key are still
        // different folders with different series rows.
        let us = show_folder_relpath("Shameless (US)/Season 1/ep.mkv", "/TV");
        let uk = show_folder_relpath("Shameless (UK)/Season 1/ep.mkv", "/TV");
        assert_ne!(us, uk);
        assert_eq!(us, "Shameless (US)");
        assert_eq!(uk, "Shameless (UK)");
    }
}
