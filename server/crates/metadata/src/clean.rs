//! Filename / folder title cleaning for search inputs.

use std::path::Path;

/// Prefer folder `Title (Year)` over probe year (movies: parent of the file).
pub fn year_from_path(path: &str) -> Option<i32> {
    let parent = Path::new(path).parent()?.file_name()?.to_str()?;
    year_in_parens(parent)
}

/// Show-root folder year for `…/Show Name (2001)/Season 1/…`.
pub fn year_from_show_folder(path: &str) -> Option<i32> {
    let show = Path::new(path).parent()?.parent()?.file_name()?.to_str()?;
    year_in_parens(show)
}

/// Series premiere year from library: earliest non-null episode `year`, else
/// show-folder `(YYYY)`.
pub fn series_library_year(
    episode_years: impl IntoIterator<Item = Option<i32>>,
    episode_path: &str,
) -> Option<i32> {
    let mut min_y: Option<i32> = None;
    for y in episode_years.into_iter().flatten() {
        if (1920..=2035).contains(&y) {
            min_y = Some(match min_y {
                Some(m) => m.min(y),
                None => y,
            });
        }
    }
    min_y.or_else(|| year_from_show_folder(episode_path))
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
    year_in_parens(s).or_else(|| {
        // year_in_parens requires 1920–2035; first_year keeps any four digits
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
    })
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

fn is_combining_mark(c: char) -> bool {
    matches!(c, '\u{0300}'..='\u{036F}')
}

/// Map common Latin diacritics to ASCII (dogfood set; no unicode-norm crate).
fn strip_diacritics(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if is_combining_mark(c) {
            continue;
        }
        let repl = match c {
            'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' => "A",
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => "a",
            'È' | 'É' | 'Ê' | 'Ë' => "E",
            'è' | 'é' | 'ê' | 'ë' => "e",
            'Ì' | 'Í' | 'Î' | 'Ï' => "I",
            'ì' | 'í' | 'î' | 'ï' => "i",
            'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø' => "O",
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' => "o",
            'Ù' | 'Ú' | 'Û' | 'Ü' => "U",
            'ù' | 'ú' | 'û' | 'ü' => "u",
            'Ý' => "Y",
            'ý' | 'ÿ' => "y",
            'Ñ' => "N",
            'ñ' => "n",
            'Ç' => "C",
            'ç' => "c",
            'Æ' => "AE",
            'æ' => "ae",
            'Œ' => "OE",
            'œ' => "oe",
            'ß' => "ss",
            '²' => "2",
            'Ð' => "D",
            'ð' => "d",
            other => {
                out.push(other);
                continue;
            }
        };
        out.push_str(repl);
    }
    out
}

/// Fold punctuation / orthography so library titles match TMDB:
/// `and`↔`&`, apostrophes (ASCII + U+2019), colons, diacritics.
pub fn fold_title_orthography(s: &str) -> String {
    let s = strip_diacritics(s);
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str(" and "),
            '\'' | '\u{2018}' | '\u{2019}' | '`' => {}
            ':' | '/' | '.' | ',' | '!' | '?' | '…' | '-' | '—' | '–' => out.push(' '),
            other => out.push(other),
        }
    }
    collapse_ws(&out)
}

pub fn clean_movie_title(raw: &str, year: Option<i32>) -> (String, Option<i32>) {
    let mut y = year.or_else(|| year_in_parens(raw));
    let mut t = strip_year_parens(raw.trim());
    t = strip_junk(&t);
    t = fold_title_orthography(&t);
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
    t = fold_title_orthography(&t);
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

    #[test]
    fn series_library_year_prefers_episode_then_show_folder() {
        let path = "/Volumes/media/TV Shows/Scrubs (2001)/Season 1/Scrubs - 1x01.mkv";
        assert_eq!(series_library_year([None, None], path), Some(2001));
        assert_eq!(
            series_library_year([Some(2005), Some(2001), None], path),
            Some(2001)
        );
        assert_eq!(
            series_library_year([None], "/Volumes/media/TV Shows/Bones/Season 1/x.mkv"),
            None
        );
    }

    #[test]
    fn folds_ampersand_apostrophe_colon_diacritic() {
        assert_eq!(
            fold_title_orthography("Angels & Demons"),
            "Angels and Demons"
        );
        assert_eq!(
            fold_title_orthography("Angels and Demons"),
            "Angels and Demons"
        );
        assert_eq!(fold_title_orthography("A Bug's Life"), "A Bugs Life");
        assert_eq!(fold_title_orthography("A Bug\u{2019}s Life"), "A Bugs Life");
        assert_eq!(
            fold_title_orthography("Joker: Folie à Deux"),
            "Joker Folie a Deux"
        );
        assert_eq!(
            fold_title_orthography("Léon The Professional"),
            "Leon The Professional"
        );
    }
}
