use crate::MediaKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedName {
    pub title: String,
    pub kind: MediaKind,
    pub year: Option<i32>,
    pub season: Option<i32>,
    pub episode: Option<i32>,
}

/// Parse a media filename (not a full path) into title / kind / episode fields.
pub fn parse_filename(file_name: &str) -> ParsedName {
    let stem = strip_extension(file_name);
    let normalized = stem.replace(['_', '.'], " ");
    let compact = stem.to_ascii_lowercase();

    if let Some((before, season, episode)) = find_season_episode(&compact) {
        let title = clean_title(&stem[..before.min(stem.len())]);
        return ParsedName {
            title: if title.is_empty() {
                stem.to_string()
            } else {
                title
            },
            kind: MediaKind::Episode,
            year: None,
            season: Some(season),
            episode: Some(episode),
        };
    }

    let year = find_year(&normalized);
    let title = match year {
        Some(y) => {
            let token = format!("({y})");
            let cut = stem
                .find(&token)
                .or_else(|| stem.to_ascii_lowercase().find(&y.to_string()));
            match cut {
                Some(i) if i > 0 => clean_title(&stem[..i]),
                _ => clean_title(stem),
            }
        }
        None => clean_title(stem),
    };

    ParsedName {
        title: if title.is_empty() {
            stem.to_string()
        } else {
            title
        },
        kind: MediaKind::Movie,
        year,
        season: None,
        episode: None,
    }
}

fn strip_extension(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

fn clean_title(s: &str) -> String {
    let mut out = s.replace(['_', '.'], " ");
    while out.contains("  ") {
        out = out.replace("  ", " ");
    }
    out.trim().trim_matches('-').trim().to_string()
}

fn find_season_episode(lower: &str) -> Option<(usize, i32, i32)> {
    // S01E02 / s1e2
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == b's' {
            let mut j = i + 1;
            let mut season = 0i32;
            let mut digits = 0;
            while j < bytes.len() && bytes[j].is_ascii_digit() && digits < 3 {
                season = season * 10 + (bytes[j] - b'0') as i32;
                j += 1;
                digits += 1;
            }
            if digits > 0 && j < bytes.len() && bytes[j] == b'e' {
                j += 1;
                let mut episode = 0i32;
                let mut edigits = 0;
                while j < bytes.len() && bytes[j].is_ascii_digit() && edigits < 3 {
                    episode = episode * 10 + (bytes[j] - b'0') as i32;
                    j += 1;
                    edigits += 1;
                }
                if edigits > 0 && season > 0 && episode > 0 {
                    return Some((i, season, episode));
                }
            }
        }
        // 1x02
        if bytes[i].is_ascii_digit() {
            let mut j = i;
            let mut season = 0i32;
            let mut digits = 0;
            while j < bytes.len() && bytes[j].is_ascii_digit() && digits < 2 {
                season = season * 10 + (bytes[j] - b'0') as i32;
                j += 1;
                digits += 1;
            }
            if digits > 0 && j < bytes.len() && bytes[j] == b'x' {
                j += 1;
                let mut episode = 0i32;
                let mut edigits = 0;
                while j < bytes.len() && bytes[j].is_ascii_digit() && edigits < 3 {
                    episode = episode * 10 + (bytes[j] - b'0') as i32;
                    j += 1;
                    edigits += 1;
                }
                if edigits > 0 && season > 0 && episode > 0 {
                    return Some((i, season, episode));
                }
            }
        }
        i += 1;
    }
    None
}

fn find_year(s: &str) -> Option<i32> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if bytes[i].is_ascii_digit()
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2].is_ascii_digit()
            && bytes[i + 3].is_ascii_digit()
        {
            let before_ok = i == 0 || !bytes[i - 1].is_ascii_digit();
            let after_ok = i + 4 == bytes.len() || !bytes[i + 4].is_ascii_digit();
            if before_ok && after_ok {
                let y = std::str::from_utf8(&bytes[i..i + 4])
                    .ok()?
                    .parse::<i32>()
                    .ok()?;
                if (1900..=2100).contains(&y) {
                    return Some(y);
                }
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_episode_sxxexx() {
        let p = parse_filename("The.Show.S02E05.720p.mkv");
        assert_eq!(p.kind, MediaKind::Episode);
        assert_eq!(p.season, Some(2));
        assert_eq!(p.episode, Some(5));
        assert_eq!(p.title, "The Show");
    }

    #[test]
    fn parses_episode_nxnn() {
        let p = parse_filename("Show Name 1x02 Title.mp4");
        assert_eq!(p.kind, MediaKind::Episode);
        assert_eq!(p.season, Some(1));
        assert_eq!(p.episode, Some(2));
    }

    #[test]
    fn parses_movie_with_year() {
        let p = parse_filename("Some Movie (2019).mkv");
        assert_eq!(p.kind, MediaKind::Movie);
        assert_eq!(p.year, Some(2019));
        assert_eq!(p.title, "Some Movie");
    }

    #[test]
    fn parses_movie_dot_year() {
        let p = parse_filename("Another.Movie.2021.BluRay.mp4");
        assert_eq!(p.kind, MediaKind::Movie);
        assert_eq!(p.year, Some(2021));
        assert!(p.title.starts_with("Another Movie"));
    }
}
