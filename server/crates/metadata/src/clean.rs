//! Filename / folder title cleaning for search inputs.

use std::path::Path;

/// Prefer folder `Title (Year)` over probe year (movies: parent of the file).
pub fn year_from_path(path: &str) -> Option<i32> {
    let parent = Path::new(path).parent()?.file_name()?.to_str()?;
    year_in_parens(parent)
}

/// Show-root folder year for `…/Show Name (2001)/Season 1/…`.
///
/// Walks exactly two parents above the file. That is path-form sensitive when
/// the **library root is the show folder**: absolute
/// `…/Show (2001)/Season 1/ep.mkv` still yields `(2001)`, but the same file
/// stored as relpath `Season 1/ep.mkv` has no show-folder component left.
/// Normal `library/Show (YYYY)/Season N/…` keeps the same answer under both
/// absolute and relative storage. Pin path form when reproing
/// `series_library_year` / collision-pin year misses — do not conflate with
/// a media_items.path → relpath migration (ADR-0030).
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
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
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
///
/// New folds need a row in `tests/fixtures/fold_corpus.json` (same discipline
/// as a playback bug: corpus fixture before the rule).
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

fn strip_regional_parentheticals(s: &str) -> String {
    // Matching soft key only; stored/parser titles keep (US)/(UK)/….
    const TAGS: &[&str] = &["us", "uk", "au", "ca", "nz"];
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'(' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b')' {
                j += 1;
            }
            if j < bytes.len() {
                let inner = s[i + 1..j].trim();
                if TAGS.iter().any(|t| inner.eq_ignore_ascii_case(t)) {
                    out.push(' ');
                    i = j + 1;
                    continue;
                }
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Turn remaining `(…)` into spaces so mid-title parens do not split the soft key
/// (`(Impractical)` ↔ `impractical`). Year and regional tags are already gone.
fn parens_to_spaces(s: &str) -> String {
    s.chars()
        .map(|c| if c == '(' || c == ')' { ' ' } else { c })
        .collect()
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

/// Show soft key for matcher grouping / search (ADR-0026 matcher path).
///
/// Folds `&`↔`and`, hyphen/en-dash/em-dash, case, and strips regional
/// `(US)`/`(UK)`/`(AU)`/`(CA)`/`(NZ)` for matching only. Does not change the
/// on-disk parser title.
pub fn clean_show_title(raw: &str) -> (String, Option<i32>) {
    let y = first_year_in_parens(raw.trim());
    let mut t = strip_year_parens(raw.trim());
    t = strip_regional_parentheticals(&t);
    t = fold_title_orthography(&t);
    t = parens_to_spaces(&t);
    t = t.to_ascii_lowercase();
    t = collapse_ws(&t)
        .trim_matches(|c: char| " .-_".contains(c))
        .to_string();
    (t, y)
}

/// ADR-0032 rejection list for reference episode titles. When uncertain,
/// treat as rejected (decline rather than guess).
pub fn episode_title_rejected(after_token: &str, show_soft_key: &str) -> bool {
    let t = after_token.trim();
    if t.is_empty() {
        return true;
    }
    let collapsed = fold_alnum_words(t);
    if collapsed.is_empty() {
        return true;
    }
    if let Some(rest) = collapsed.strip_prefix("episode ")
        && rest.chars().all(|c| c.is_ascii_digit())
    {
        return true;
    }
    if let Some(rest) = collapsed.strip_prefix("ep ")
        && rest.chars().all(|c| c.is_ascii_digit())
    {
        return true;
    }
    if let Some(rest) = collapsed.strip_prefix("ep")
        && !rest.is_empty()
        && rest.chars().all(|c| c.is_ascii_digit())
    {
        return true;
    }
    let show = fold_alnum_words(show_soft_key);
    if !show.is_empty() && collapsed == show {
        return true;
    }
    false
}

fn fold_alnum_words(s: &str) -> String {
    s.to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

const RELEASE_JUNK: &[&str] = &[
    "bluray", "blu-ray", "blu ray", "web-dl", "webdl", "webrip", "hdtv", "dvdrip", "remux",
    "2160p", "1080p", "720p", "480p", "x264", "x265", "h264", "h265", "hevc", "aac", "dts",
    "truehd", "atmos", "hdr10", "proper", "repack", "extended", "unrated", "multi", "subbed",
    "dual", "internal",
];

fn strip_release_junk_fragment(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let mut cut = lower.len();
    for tok in RELEASE_JUNK {
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
    s[..cut.min(s.len())]
        .trim()
        .trim_matches([' ', '-', '_', '.', '–', '—'])
        .to_string()
}

/// Text after SxxExx / NxNN in a basename, release junk stripped.
pub fn after_token_episode_title(basename: &str, season: i32, episode: i32) -> Option<String> {
    let stem = match basename.rfind('.') {
        Some(i) if i > 0 => &basename[..i],
        _ => basename,
    };
    let lower = stem.to_ascii_lowercase();
    let end = find_episode_token_end(&lower, season, episode)?;
    if end >= stem.len() {
        return Some(String::new());
    }
    let rest = stem[end..]
        .trim_start_matches([' ', '-', '_', '.', '–', '—'])
        .trim();
    Some(strip_release_junk_fragment(rest))
}

fn find_episode_token_end(lower: &str, season: i32, episode: i32) -> Option<usize> {
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == b's' {
            let mut j = i + 1;
            let mut s = 0i32;
            let mut sd = 0;
            while j < bytes.len() && bytes[j].is_ascii_digit() && sd < 3 {
                s = s * 10 + (bytes[j] - b'0') as i32;
                j += 1;
                sd += 1;
            }
            if sd > 0 && j < bytes.len() && bytes[j] == b'e' {
                j += 1;
                let mut e = 0i32;
                let mut ed = 0;
                while j < bytes.len() && bytes[j].is_ascii_digit() && ed < 3 {
                    e = e * 10 + (bytes[j] - b'0') as i32;
                    j += 1;
                    ed += 1;
                }
                if ed > 0 && s == season && e == episode {
                    return Some(j);
                }
            }
        }
        i += 1;
    }
    i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            let mut s = 0i32;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                s = s * 10 + (bytes[i] - b'0') as i32;
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'x' {
                i += 1;
                let mut e = 0i32;
                let mut ed = 0;
                while i < bytes.len() && bytes[i].is_ascii_digit() && ed < 3 {
                    e = e * 10 + (bytes[i] - b'0') as i32;
                    i += 1;
                    ed += 1;
                }
                if ed > 0 && s == season && e == episode && start > 0 {
                    // Skip contiguous `-NN` range tail so it is not title text.
                    return Some(skip_contiguous_dash_episodes(bytes, i, e));
                }
            }
        } else {
            i += 1;
        }
    }
    None
}

fn skip_contiguous_dash_episodes(bytes: &[u8], mut j: usize, start: i32) -> usize {
    let mut end = start;
    while end - start + 1 < nightjar_core::MAX_EPISODE_RANGE {
        if j >= bytes.len() || bytes[j] != b'-' {
            break;
        }
        let after = j + 1;
        if after >= bytes.len() || !bytes[after].is_ascii_digit() {
            break;
        }
        let mut k = after;
        let mut next = 0i32;
        let mut nd = 0;
        while k < bytes.len() && bytes[k].is_ascii_digit() && nd < 3 {
            next = next * 10 + (bytes[k] - b'0') as i32;
            k += 1;
            nd += 1;
        }
        if nd == 0 || next != end + 1 {
            break;
        }
        end = next;
        j = k;
    }
    j
}

/// ADR-0032 reference pick: usable mid-season preferred; S01E01 only if usable.
pub fn pick_reference_episode(
    episodes: &[(i32, i32, &str)],
    show_soft_key: &str,
) -> Option<(i32, i32, String)> {
    let mut usable: Vec<(i32, i32, String)> = Vec::new();
    for &(season, episode, basename) in episodes {
        let Some(title) = after_token_episode_title(basename, season, episode) else {
            continue;
        };
        if episode_title_rejected(&title, show_soft_key) {
            continue;
        }
        usable.push((season, episode, title));
    }
    if usable.is_empty() {
        return None;
    }
    // Prefer non-pilot (not S01E01 / 1x01).
    if let Some(p) = usable
        .iter()
        .find(|(s, e, _)| !(*s == 1 && *e == 1))
        .cloned()
    {
        return Some(p);
    }
    usable.into_iter().next()
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
        assert_eq!(t, "heartland");
        assert_eq!(y, Some(2007));
    }

    #[test]
    fn show_soft_key_near_dups_and_sample_below_floor() {
        // Pairs that must share a soft key. Near-dup groups from
        // nightjar-meta/notes/metadata-parse-baseline-2026-08-03.md; below-floor rows from
        // nightjar-meta/notes/tmdb-show-coverage-sample-2026-08-03.md.
        let maul_ascii = "Star Wars - Maul - Shadow Lord";
        let maul_en = format!(
            "Star Wars - Maul {} Shadow Lord",
            char::from_u32(0x2013).unwrap()
        );
        let pairs: &[(&str, &str)] = &[
            (
                "The Inspired Unemployed (Impractical) Jokers",
                "the inspired unemployed impractical jokers",
            ),
            (maul_ascii, maul_en.as_str()),
            ("INVINCIBLE (2021)", "Invincible (2021)"),
            ("Will and Grace", "Will & Grace"),
            ("Shameless (US)", "Shameless"),
            ("Top Gear", "Top Gear"),
            ("Shameless (UK)", "Shameless"),
            ("Show Name (AU)", "Show Name"),
            ("Show Name (NZ)", "Show Name"),
        ];
        assert!(
            maul_en.contains(char::from_u32(0x2013).unwrap()),
            "test fixture must contain U+2013 en-dash"
        );
        for (a, b) in pairs {
            let (ka, _) = clean_show_title(a);
            let (kb, _) = clean_show_title(b);
            assert_eq!(ka, kb, "soft key diverge for {a:?} vs {b:?}");
            assert!(!ka.is_empty(), "empty soft key for {a:?}");
        }

        // Soft key is lowercased; year still extracted; regional tag gone.
        let (t, y) = clean_show_title("INVINCIBLE (2021)");
        assert_eq!(t, "invincible");
        assert_eq!(y, Some(2021));
        let (t, _) = clean_show_title("Will & Grace");
        assert_eq!(t, "will and grace");
        let (t, _) = clean_show_title("Shameless (US)");
        assert_eq!(t, "shameless");
    }

    #[test]
    fn episode_title_rejection_list_and_reference_pick() {
        assert!(episode_title_rejected("Episode 1", "shameless"));
        assert!(episode_title_rejected("Ep 02", "show"));
        assert!(episode_title_rejected("Shameless", "shameless"));
        assert!(!episode_title_rejected("7.1", "9 1 1"));
        assert!(!episode_title_rejected(
            "I Hate You, Stephen Hawking",
            "shameless"
        ));

        let eps = [
            (1, 1, "Show - 1x01 - Episode 1 - Bluray-1080p.mkv"),
            (1, 5, "Show - 1x05 - The Crossing - Bluray-1080p.mkv"),
        ];
        let (s, e, title) = pick_reference_episode(&eps, "show").unwrap();
        assert_eq!((s, e), (1, 5));
        assert_eq!(title, "The Crossing");

        assert_eq!(
            after_token_episode_title(
                "Abbott Elementary - 3x01-02 - Career Day - WEBDL-1080p.mkv",
                3,
                1
            )
            .as_deref(),
            Some("Career Day")
        );
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
        // Normal layout: absolute and library-relative agree.
        assert_eq!(
            year_from_show_folder("Scrubs (2001)/Season 1/Scrubs - 1x01.mkv"),
            Some(2001)
        );
        // Library root == show folder: absolute still sees the show component;
        // relpath does not. Recorded so a 0030 migration is not misread as the
        // series_library_year bug.
        assert_eq!(
            year_from_show_folder("/mnt/Show (2001)/Season 1/x.mkv"),
            Some(2001)
        );
        assert_eq!(year_from_show_folder("Season 1/x.mkv"), None);
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

    #[test]
    fn fold_corpus_fixture() {
        let path = format!(
            "{}/tests/fixtures/fold_corpus.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        let rows: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap();
        assert!(!rows.is_empty(), "fold corpus must not be empty");
        for row in rows {
            let input = row["in"].as_str().expect("in");
            let expected = row["out"].as_str().expect("out");
            assert_eq!(
                fold_title_orthography(input),
                expected,
                "fold corpus row for {input:?}"
            );
        }
    }
}
