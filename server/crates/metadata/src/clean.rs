//! Filename / folder title cleaning for search inputs (spike parity).

use std::path::Path;

/// Prefer folder `Title (Year)` over probe year.
pub fn year_from_path(path: &str) -> Option<i32> {
    let parent = Path::new(path).parent()?.file_name()?.to_str()?;
    year_in_parens(parent)
}

fn year_in_parens(s: &str) -> Option<i32> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 5 < bytes.len() {
        if bytes[i] == b'('
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2].is_ascii_digit()
            && bytes[i + 3].is_ascii_digit()
            && bytes[i + 4].is_ascii_digit()
            && bytes[i + 5] == b')'
        {
            let y: i32 = std::str::from_utf8(&bytes[i + 1..i + 5])
                .ok()?
                .parse()
                .ok()?;
            if (1920..=2035).contains(&y) {
                return Some(y);
            }
        }
        i += 1;
    }
    None
}

fn strip_year_parens(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 5 < bytes.len()
            && bytes[i] == b'('
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2].is_ascii_digit()
            && bytes[i + 3].is_ascii_digit()
            && bytes[i + 4].is_ascii_digit()
            && bytes[i + 5] == b')'
        {
            out.push(' ');
            i += 6;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn first_year_in_parens(s: &str) -> Option<i32> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 5 < bytes.len() {
        if bytes[i] == b'('
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2].is_ascii_digit()
            && bytes[i + 3].is_ascii_digit()
            && bytes[i + 4].is_ascii_digit()
            && bytes[i + 5] == b')'
        {
            return std::str::from_utf8(&bytes[i + 1..i + 5]).ok()?.parse().ok();
        }
        i += 1;
    }
    None
}

const JUNK_TOKENS: &[&str] = &[
    "bluray", "blu-ray", "blu ray", "web-dl", "webdl", "webrip", "hdtv", "dvdrip", "remux",
    "2160p", "1080p", "720p", "480p", "x264", "x265", "h264", "h265", "hevc", "aac", "dts",
    "truehd", "atmos", "hdr10", "dv", "proper", "repack", "extended", "unrated", "multi", "subbed",
    "dual", "internal",
];

fn strip_junk(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let mut cut = lower.len();
    for tok in JUNK_TOKENS {
        if let Some(idx) = lower.find(tok) {
            // Only treat as release junk when preceded by separator-ish chars.
            let ok_boundary = idx == 0
                || matches!(
                    lower.as_bytes()[idx - 1],
                    b'.' | b' ' | b'_' | b'-' | b'[' | b'('
                );
            if ok_boundary && idx < cut {
                cut = idx;
            }
        }
    }
    s[..cut.min(s.len())].to_string()
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn clean_movie_title(raw: &str, year: Option<i32>) -> (String, Option<i32>) {
    let mut y = year.or_else(|| year_in_parens(raw));
    let mut t = strip_year_parens(raw.trim());
    t = strip_junk(&t);
    t = collapse_ws(&t)
        .trim_matches(|c: char| " .-_".contains(c))
        .to_string();
    if let Some(yy) = y
        && (!(1920..=2035).contains(&yy))
        && !raw.trim().starts_with(&yy.to_string())
    {
        y = None;
    }
    if matches!(year, Some(1080 | 2160 | 720 | 480)) && y == year {
        y = None;
    }
    (t, y)
}

pub fn clean_show_title(raw: &str) -> (String, Option<i32>) {
    let y = first_year_in_parens(raw.trim());
    let mut t = strip_year_parens(raw.trim());
    t = collapse_ws(&t)
        .trim_matches(|c: char| " .-_".contains(c))
        .to_string();
    (t, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_release_junk() {
        let (t, y) = clean_movie_title("Fight Club (1999) Bluray-1080p", Some(1999));
        assert_eq!(t, "Fight Club");
        assert_eq!(y, Some(1999));
    }

    #[test]
    fn show_year_tag() {
        let (t, y) = clean_show_title("Heartland (2007) (CA)");
        // Spike cleaner strips `(YYYY)` only; region tags remain.
        assert_eq!(t, "Heartland (CA)");
        assert_eq!(y, Some(2007));
    }

    #[test]
    fn year_from_folder() {
        assert_eq!(
            year_from_path("/Volumes/media/Movies/Fight Club (1999)/Fight Club.mkv"),
            Some(1999)
        );
    }
}
