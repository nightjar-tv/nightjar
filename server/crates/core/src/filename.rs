use crate::MediaKind;

/// Inclusive max span for a multi-episode file (`1x01-02-03` → 3). Dogfood
/// max is 3; cap rejects pathological `1x01-99` glued to a title numeral.
pub const MAX_EPISODE_RANGE: i32 = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedName {
    pub title: String,
    pub kind: MediaKind,
    pub year: Option<i32>,
    pub season: Option<i32>,
    pub episode: Option<i32>,
    /// Inclusive end when the basename encodes a contiguous range (`5x20-21`).
    /// `None` means a single episode (or not an episode).
    pub episode_end: Option<i32>,
}

impl ParsedName {
    /// Episodes covered by this file (start..=end). Empty when not an episode.
    pub fn episode_numbers(&self) -> Vec<i32> {
        let Some(start) = self.episode else {
            return Vec::new();
        };
        let end = self.episode_end.unwrap_or(start).max(start);
        (start..=end).collect()
    }
}

/// Parse a media filename (not a full path) into title / kind / episode fields.
pub fn parse_filename(file_name: &str) -> ParsedName {
    let stem = strip_extension(file_name);
    let normalized = stem.replace(['_', '.'], " ");
    let compact = stem.to_ascii_lowercase();

    if let Some((before, season, episode, episode_end)) = find_season_episode(&compact) {
        let title = clean_title(&stem[..before.min(stem.len())]);
        let end = if episode_end > episode {
            Some(episode_end)
        } else {
            None
        };
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
            episode_end: end,
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
        episode_end: None,
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

/// `(token_start, season, episode_start, episode_end)` — end inclusive.
fn find_season_episode(lower: &str) -> Option<(usize, i32, i32, i32)> {
    // S01E02 / s1e2 (no range forms in dogfood; single episode only)
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
                    return Some((i, season, episode, episode));
                }
            }
        }
        // 1x02 / 5x20-21 / 8x01-02-03
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
                    let end = extend_contiguous_dash_episodes(bytes, j, episode);
                    return Some((i, season, episode, end));
                }
            }
        }
        i += 1;
    }
    None
}

/// After `NxMM`, consume immediate `-NN` / `-NN-NN` when each NN is the next
/// contiguous episode and the span stays within [`MAX_EPISODE_RANGE`].
/// Does not consume spaced title numerals (` - 100 -`).
fn extend_contiguous_dash_episodes(bytes: &[u8], mut j: usize, start: i32) -> i32 {
    let mut end = start;
    while end - start + 1 < MAX_EPISODE_RANGE {
        if j >= bytes.len() || bytes[j] != b'-' {
            break;
        }
        let after_dash = j + 1;
        if after_dash >= bytes.len() || !bytes[after_dash].is_ascii_digit() {
            break;
        }
        let mut k = after_dash;
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
    end
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
        assert_eq!(p.episode_end, None);
        assert_eq!(p.title, "The Show");
    }

    #[test]
    fn parses_episode_nxnn() {
        let p = parse_filename("Show Name 1x02 Title.mp4");
        assert_eq!(p.kind, MediaKind::Episode);
        assert_eq!(p.season, Some(1));
        assert_eq!(p.episode, Some(2));
        assert_eq!(p.episode_end, None);
    }

    #[test]
    fn parses_nxnn_two_episode_range() {
        let p = parse_filename("Abbott Elementary - 3x01-02 - Career Day - WEBDL-1080p.mkv");
        assert_eq!(p.kind, MediaKind::Episode);
        assert_eq!(p.season, Some(3));
        assert_eq!(p.episode, Some(1));
        assert_eq!(p.episode_end, Some(2));
        assert_eq!(p.episode_numbers(), vec![1, 2]);
        assert_eq!(p.title, "Abbott Elementary");
    }

    #[test]
    fn parses_nxnn_three_episode_range() {
        let p = parse_filename("Red Dwarf - 8x01-02-03 - Back in the Red - Bluray-1080p.mkv");
        assert_eq!(p.season, Some(8));
        assert_eq!(p.episode, Some(1));
        assert_eq!(p.episode_end, Some(3));
        assert_eq!(p.episode_numbers(), vec![1, 2, 3]);
    }

    #[test]
    fn range_does_not_eat_spaced_title_numeral() {
        // "100" is the episode title, not episode 100.
        let p = parse_filename("30 Rock - 5x20-21 - 100 - Bluray-1080p.mkv");
        assert_eq!(p.season, Some(5));
        assert_eq!(p.episode, Some(20));
        assert_eq!(p.episode_end, Some(21));
        assert_eq!(p.episode_numbers(), vec![20, 21]);
    }

    #[test]
    fn non_contiguous_dash_is_not_a_range() {
        let p = parse_filename("Show - 1x01-03 - Title.mkv");
        assert_eq!(p.episode, Some(1));
        assert_eq!(p.episode_end, None);
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
