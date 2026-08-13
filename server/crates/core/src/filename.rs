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
///
/// Season 0 is a season. Every provider models specials as season 0, and the
/// coverage predicate already excludes it from the fit check by name
/// (`queue.rs`, "a `Specials` folder exists independently of whether a
/// provider models season 0") rather than by relying on the parser to refuse
/// it. Episode 0 is still refused: nothing measured asserts a real `E00`, and
/// the one corpus case that expects episode 0 is Sonarr's sentinel for "this
/// name carries no standard episode number", not an episode called zero.
fn find_season_episode(lower: &str) -> Option<(usize, i32, i32, i32)> {
    // S01E02 / s1e2 (no range forms in dogfood; single episode only)
    let bytes = lower.as_bytes();
    let mut i = 0;
    // `i + 2` because the shortest token is three bytes (`1x1`). `i + 3`
    // stopped at `len - 4` and so could never read a token that ended the
    // string — and the extension is stripped before this runs, so
    // `Show 1x1.mkv` reaches here as `show 1x1` and was missed too.
    while i + 2 < bytes.len() {
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
                if edigits > 0 && episode > 0 {
                    return Some((i, season, episode, episode));
                }
            }
        }
        // 1x02 / 5x20-21 / 8x01-02-03
        //
        // The season digits must be a **whole** run: bounded on the left by a
        // non-digit, and no longer than two. Without both halves the scan
        // starts mid-number and reads a resolution as an episode —
        // `1080x1920` matched at the `8`, giving season 80 episode 192, which
        // turned any movie carrying a resolution into an episode. Codec and
        // bit-depth tokens are unaffected because they have no digits before
        // the `x` at all (`x264`, `x265`) or no `x` (`h.264`).
        if bytes[i].is_ascii_digit() && (i == 0 || !bytes[i - 1].is_ascii_digit()) {
            let mut j = i;
            let mut season = 0i32;
            let mut digits = 0;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                if digits < 2 {
                    season = season * 10 + (bytes[j] - b'0') as i32;
                }
                j += 1;
                digits += 1;
            }
            if (1..=2).contains(&digits) && j < bytes.len() && bytes[j] == b'x' {
                j += 1;
                let mut episode = 0i32;
                let mut edigits = 0;
                while j < bytes.len() && bytes[j].is_ascii_digit() && edigits < 3 {
                    episode = episode * 10 + (bytes[j] - b'0') as i32;
                    j += 1;
                    edigits += 1;
                }
                if edigits > 0 && episode > 0 {
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

    /// Defect 1 — a resolution token is not an episode. Titles are invented;
    /// the token shapes are what is under test.
    #[test]
    fn a_resolution_token_is_not_an_episode() {
        for name in [
            "A Movie Name.1080x1920.mkv",
            "A Movie Name (1920x1080).mkv",
            "Anon Release - 07 (1280x720 x264-AAC) [ABCD1234].mkv",
            "Anon Release - 06 [848x480 H.264 AAC][DEADBEEF].mkv",
        ] {
            let p = parse_filename(name);
            assert_eq!(p.season, None, "{name} invented a season");
            assert_eq!(p.episode, None, "{name} invented an episode");
            assert_eq!(p.kind, MediaKind::Movie, "{name} was called an episode");
        }
    }

    /// Defect 1, the other side — codec and bit-depth tokens are digit-adjacent
    /// too and must keep parsing exactly as they did.
    #[test]
    fn codec_tokens_still_parse_as_before() {
        let p = parse_filename("Anon Show - 2x03 - An Episode.1080p.x264.H.264.10bit.mkv");
        assert_eq!(p.season, Some(2));
        assert_eq!(p.episode, Some(3));
        let m = parse_filename("Anon Film.2019.1080p.BluRay.x265.10bit.mkv");
        assert_eq!(m.season, None);
        assert_eq!(m.episode, None);
        assert_eq!(m.kind, MediaKind::Movie);
        assert_eq!(m.year, Some(2019));
    }

    /// Defect 2 — season 0 is a season. Episode 0 stays refused, and that is a
    /// decision with evidence behind it rather than an oversight: no provider
    /// episode in the measured library carries episode 0, so a parsed `E00`
    /// could not bind to anything.
    #[test]
    fn season_zero_parses_and_episode_zero_does_not() {
        let a = parse_filename("Anon Show - S00E16 - A Special.mkv");
        assert_eq!(a.season, Some(0));
        assert_eq!(a.episode, Some(16));
        assert_eq!(a.kind, MediaKind::Episode);

        let b = parse_filename("Anon Show - 0x01 - Another Special.mkv");
        assert_eq!(b.season, Some(0));
        assert_eq!(b.episode, Some(1));
        assert_eq!(b.kind, MediaKind::Episode);

        let c = parse_filename("Anon Show - S01E00 - A Pilot.mkv");
        assert_eq!(c.episode, None, "episode 0 is still refused");
        assert_eq!(c.kind, MediaKind::Movie);
    }

    /// Defect 3 — the token may end the name. The extension is stripped before
    /// the scan, so this reaches the scan as `anon show 1x1` and the old
    /// `i + 3 < len` bound could never read it.
    #[test]
    fn a_name_ending_in_the_episode_token_parses() {
        let a = parse_filename("Anon Show 1x1.mkv");
        assert_eq!(a.season, Some(1));
        assert_eq!(a.episode, Some(1));

        let b = parse_filename("1x1");
        assert_eq!(b.season, Some(1));
        assert_eq!(b.episode, Some(1));

        let c = parse_filename("Anon Show S01E01.mkv");
        assert_eq!(c.season, Some(1));
        assert_eq!(c.episode, Some(1));
    }

    /// The control: the form 93% of the measured library is written in must
    /// parse identically before and after all three fixes.
    #[test]
    fn the_common_form_is_unchanged() {
        let p = parse_filename("Anon Show - 4x11 - An Episode Title - Bluray-1080p.mkv");
        assert_eq!(p.kind, MediaKind::Episode);
        assert_eq!(p.season, Some(4));
        assert_eq!(p.episode, Some(11));
        assert_eq!(p.episode_end, None);
        assert_eq!(p.title, "Anon Show");
    }

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
