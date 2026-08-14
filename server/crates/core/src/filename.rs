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

/// Release-junk tokens that end a title. Quality, source and codec, matched as
/// whole words so an ordinary word is never cut: `Ac3` the band is not `AC3`
/// the codec only because the boundary check requires a separator on both
/// sides, and `Web` is deliberately absent for the same reason `Charlotte's
/// Web` exists.
///
/// The episode *title* extractor in `nightjar-metadata` has had this list since
/// the dogfood measurement; `parse_filename` has never had one on either
/// branch. The movie branch appeared to, because cutting at the year removes
/// whatever follows it — which works only when a year is found, and 62 of the
/// 63 measured failures are names with no year at all.
const TITLE_JUNK: &[&str] = &[
    "bluray", "blu-ray", "webdl", "web-dl", "webrip", "hdtv", "pdtv", "dvdrip", "bdrip", "hdrip",
    "tvrip", "sdtv", "remux", "2160p", "1080p", "1080i", "720p", "480p", "x264", "x265", "h264",
    "h265", "hevc", "xvid", "divx", "aac", "ac3", "dts", "truehd", "atmos", "flac", "10bit",
    "8bit", "hdr10", "proper", "repack",
];

/// Cut a title at the first whole-word release-junk token.
///
/// Whole-word only: the token must start at a word boundary, so `x264` in
/// `Matrix264` is not a token and `1080p` glued to a word is not either. When
/// the cut would leave nothing, the title is returned whole — a name that is
/// *only* junk carries no identity, and an empty title is worse than a junk one
/// because the caller substitutes the entire stem for it.
fn cut_at_title_junk(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut cut = None;
    for tok in TITLE_JUNK {
        let mut from = 0;
        while let Some(rel) = lower[from..].find(tok) {
            let i = from + rel;
            let end = i + tok.len();
            let left_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
            let right_ok = end >= bytes.len() || !bytes[end].is_ascii_alphanumeric();
            if left_ok && right_ok {
                cut = Some(cut.map_or(i, |c: usize| c.min(i)));
                break;
            }
            from = end;
        }
    }
    let Some(i) = cut else {
        return s.to_string();
    };
    let head = s[..i]
        .trim()
        .trim_matches([' ', '-', '_', '.', '(', '['])
        .trim();
    if head.is_empty() {
        s.to_string()
    } else {
        head.to_string()
    }
}

/// Strip a leading `[group]` release tag.
///
/// **Guarded on what is left, not on what is stripped.** `[REC] (2007)` is a
/// real film and its whole title is the bracket, so the rule only fires when
/// the remainder still carries a letter — `[Commie] Show - 07` keeps `Show`,
/// `[REC] (2007)` keeps everything. Without that guard the rule destroys a
/// title to clean one, which is the trade the orthography measurement rejected.
fn strip_leading_group(stem: &str) -> &str {
    let t = stem.trim_start();
    if !t.starts_with('[') {
        return stem;
    }
    let Some(close) = t.find(']') else {
        return stem;
    };
    let rest = t[close + 1..]
        .trim_start_matches([' ', '_', '.', '-'])
        .trim();
    if rest.chars().any(|c| c.is_alphabetic()) {
        rest
    } else {
        stem
    }
}

/// Parse a media filename (not a full path) into title / kind / episode fields.
pub fn parse_filename(file_name: &str) -> ParsedName {
    let stem = strip_leading_group(strip_extension(file_name));
    let normalized = stem.replace(['_', '.'], " ");
    let compact = stem.to_ascii_lowercase();

    if let Some((before, season, episode, episode_end)) = find_season_episode(&compact) {
        let title = cut_at_title_junk(&clean_title(&stem[..before.min(stem.len())]));
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
                _ => cut_at_title_junk(&clean_title(stem)),
            }
        }
        None => cut_at_title_junk(&clean_title(stem)),
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
                    let end = extend_episode_span(bytes, j, season, episode);
                    return Some((i, season, episode, end));
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
                    let end = extend_episode_span(bytes, j, season, episode);
                    return Some((i, season, episode, end));
                }
            }
        }
        i += 1;
    }
    None
}

/// Consume the episode tokens that follow `S<season>E<start>` or `N x <start>`
/// and return the last episode the file covers.
///
/// **One rule over a separator and repetition set**, because the forms are not
/// four additions to a fifth. The set is `E01E02`, `E01-E02`, `x01x02`,
/// `01-x03`, `E1-S6E2` and the bare `-02` — and the grammar behind all of them
/// is the same: an optional separator, an optional repeat of the season, an
/// optional episode marker, then a number.
///
/// **A separator means a range end; a repetition means the next episode.**
/// `S15E06-08` is one file holding episodes 6, 7 and 8, so it emits the
/// inclusive run — ADR-0025's amendment requires the item list to show one
/// entry spanning the range rather than a gap that reads as missing media, and
/// emitting `[6]` would leave 7 and 8 looking absent. Without a separator the
/// tokens are an explicit list and each must be the next number.
///
/// **A repeated season must agree.** `S6E1-S6E2` is a range; `S6E1-S7E2` is not
/// one, and stops here rather than falling out somewhere later.
///
/// [`MAX_EPISODE_RANGE`] bounds the result either way, which is what stops a
/// malformed or adversarial name generating an arbitrary run.
fn extend_episode_span(bytes: &[u8], mut j: usize, season: i32, start: i32) -> i32 {
    let mut end = start;
    loop {
        let mut k = j;
        let separated = k < bytes.len() && bytes[k] == b'-';
        if separated {
            k += 1;
        }
        // An optional repeat of the season, in either spelling it appears in:
        // `s06` before an `e`, or `6x` before the number.
        if let Some((repeated, after)) = read_repeated_season(bytes, k) {
            if repeated != season {
                break;
            }
            k = after;
        }
        if k < bytes.len() && (bytes[k] == b'e' || bytes[k] == b'x') {
            k += 1;
        }
        // Something must separate this token from the last, or a stray trailing
        // number would read as an episode.
        if k == j {
            break;
        }
        let mut next = 0i32;
        let mut digits = 0;
        while k < bytes.len() && bytes[k].is_ascii_digit() && digits < 3 {
            next = next * 10 + (bytes[k] - b'0') as i32;
            k += 1;
            digits += 1;
        }
        if digits == 0 {
            break;
        }
        let ok = if separated {
            next > end
        } else {
            next == end + 1
        };
        if !ok || next - start + 1 > MAX_EPISODE_RANGE {
            break;
        }
        end = next;
        j = k;
    }
    end
}

/// `s06` or `6x` immediately at `i`, returning the season and the offset after
/// it. Only the spellings a season is actually repeated in.
fn read_repeated_season(bytes: &[u8], i: usize) -> Option<(i32, usize)> {
    if i >= bytes.len() {
        return None;
    }
    if bytes[i] == b's' {
        let mut k = i + 1;
        let mut n = 0i32;
        let mut digits = 0;
        while k < bytes.len() && bytes[k].is_ascii_digit() && digits < 3 {
            n = n * 10 + (bytes[k] - b'0') as i32;
            k += 1;
            digits += 1;
        }
        // Must be followed by an episode marker, else `s` is part of a word.
        if digits > 0 && k < bytes.len() && bytes[k] == b'e' {
            return Some((n, k));
        }
        return None;
    }
    if bytes[i].is_ascii_digit() {
        let mut k = i;
        let mut n = 0i32;
        let mut digits = 0;
        while k < bytes.len() && bytes[k].is_ascii_digit() && digits < 2 {
            n = n * 10 + (bytes[k] - b'0') as i32;
            k += 1;
            digits += 1;
        }
        if digits > 0 && k < bytes.len() && bytes[k] == b'x' {
            return Some((n, k));
        }
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

    /// Rule 1 — a leading release-group bracket is not part of the title.
    #[test]
    fn a_leading_group_bracket_is_stripped() {
        let a = parse_filename("[AnonGroup] Anon Show - 1x02 - A Title.mkv");
        assert_eq!(a.title, "Anon Show");
        assert_eq!(a.season, Some(1));
        assert_eq!(a.episode, Some(2));

        let b = parse_filename("[AnonGroup]Anon Film (2019).mkv");
        assert_eq!(b.title, "Anon Film");
        assert_eq!(b.year, Some(2019));
    }

    /// Rule 1, the guard — the rule fires on what is **left**, not on what is
    /// stripped. A film whose whole title is a bracket keeps it; stripping to
    /// clean would destroy the title to tidy it.
    #[test]
    fn a_bracket_that_is_the_title_is_kept() {
        let a = parse_filename("[REC] (2007).mkv");
        assert!(a.title.contains("[REC]"), "got {:?}", a.title);
        assert_eq!(a.year, Some(2007));

        // A bracket that is not leading is not a group tag either.
        let b = parse_filename("Anon Show - 1x02 - A Title [AnonGroup].mkv");
        assert_eq!(b.title, "Anon Show");
        let c = parse_filename("Anon [Bracketed] Film (2011).mkv");
        assert_eq!(c.title, "Anon [Bracketed] Film");
    }

    /// Rule 2 — one case per junk class, on the episode branch's title and on
    /// the movie branch's, since neither had a strip before.
    #[test]
    fn release_junk_is_cut_from_the_title_on_both_branches() {
        for junk in [
            "Bluray-1080p",
            "WEBRip-720p",
            "HDTV",
            "x264",
            "HEVC",
            "AAC",
            "DTS",
            "REPACK",
        ] {
            let e = parse_filename(&format!("Anon Show {junk} - 1x02 - A Title.mkv"));
            assert_eq!(e.title, "Anon Show", "episode branch kept {junk}");
            let m = parse_filename(&format!("Anon Film {junk}.mkv"));
            assert_eq!(m.title, "Anon Film", "movie branch kept {junk}");
        }
    }

    /// Rule 2, the guards — whole words only, and never cut to nothing.
    #[test]
    fn junk_words_glued_into_a_title_are_not_cut() {
        assert_eq!(
            parse_filename("Anon Matrix264 Film (2011).mkv").title,
            "Anon Matrix264 Film"
        );
        assert_eq!(
            parse_filename("Anon Aacorn Film (2011).mkv").title,
            "Anon Aacorn Film"
        );
        // A name that is only junk carries no identity; cutting to empty makes
        // the caller substitute the whole stem, which is worse than the junk.
        let only = parse_filename("1080p.x264.mkv");
        assert!(!only.title.is_empty());
    }

    /// The control: the form 93% of the measured library is written in parses
    /// identically before and after both rules.
    #[test]
    fn the_common_form_is_unchanged_by_the_title_rules() {
        let p = parse_filename("Anon Show - 4x11 - An Episode Title - Bluray-1080p.mkv");
        assert_eq!(p.title, "Anon Show");
        assert_eq!(p.kind, MediaKind::Episode);
        assert_eq!(p.season, Some(4));
        assert_eq!(p.episode, Some(11));
        let m = parse_filename("Anon Film (2019) Bluray-1080p.mkv");
        assert_eq!(m.title, "Anon Film");
        assert_eq!(m.year, Some(2019));
    }

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

    /// **Superseded 2026-08-14 and kept as the record of what changed.** This
    /// asserted `1x01-03 -> [1]`, on the reading that a dash repeats an episode
    /// and so must land on the next number. A dash is a **range end**: the file
    /// holds episodes 1, 2 and 3, and emitting `[1]` leaves two of them looking
    /// absent — the gap ADR-0025's amendment exists to prevent.
    ///
    /// What survives is the part that was really being tested: a dash followed
    /// by something that is not a larger episode number is not a range.
    #[test]
    fn a_dash_span_covers_the_run_and_a_backwards_one_is_not_a_range() {
        let p = parse_filename("Show - 1x01-03 - Title.mkv");
        assert_eq!(p.episode, Some(1));
        assert_eq!(p.episode_end, Some(3));
        assert_eq!(p.episode_numbers(), vec![1, 2, 3]);

        let back = parse_filename("Show - 1x05-03 - Title.mkv");
        assert_eq!(back.episode, Some(5));
        assert_eq!(back.episode_end, None);
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

#[cfg(test)]
mod multi_episode_spellings {
    use crate::parse_filename;

    /// Measured on `origin/main` at `3d776ec` before the rule was written.
    /// Five spellings return the first episode and report success, which is
    /// worse than returning nothing: a caller cannot tell a single-episode
    /// file from a range whose tail was dropped.
    ///
    /// **Three of the five are on the `SxxExx` branch, which never calls
    /// `extend_contiguous_dash_episodes` at all.** A change confined to that
    /// function fixes the `NxNN` two and moves the corpus enough to look done.
    #[test]
    fn every_multi_episode_spelling_yields_the_whole_span() {
        for (name, want) in [
            ("Show.S01E01E02.mkv", vec![1, 2]),
            ("Show.S01E01-E02.mkv", vec![1, 2]),
            ("Show.S6E1-S6E2.mkv", vec![1, 2]),
            ("Show.[02x01x02].mkv", vec![1, 2]),
            ("Show.1x01-x03.mkv", vec![1, 2, 3]),
            ("Show.S01E01-02-03.mkv", vec![1, 2, 3]),
            // The two that already worked, unchanged.
            ("Show.1x01-02.mkv", vec![1, 2]),
            ("Show.5x20-21.mkv", vec![20, 21]),
        ] {
            let p = parse_filename(name);
            assert_eq!(p.episode_numbers(), want, "{name}");
        }
    }

    /// A repeated season token must agree. `S6E1-S6E2` is a range; `S6E1-S7E2`
    /// crosses seasons and is not one, and returning the first episode alone is
    /// the correct outcome there rather than something to fall out by accident.
    #[test]
    fn a_cross_season_span_is_not_a_range() {
        let p = parse_filename("Show.S6E1-S7E2.mkv");
        assert_eq!(p.season, Some(6));
        assert_eq!(p.episode_numbers(), vec![1]);
    }

    /// A separator introduces a **range end**, so `A-B` emits the inclusive
    /// run. Written first against the superseded reading of decision 2, which
    /// asserted `1x01-05 -> [1]`; that reading is amended 2026-08-14 because it
    /// contradicts this slice's own ADR-0025 amendment — one file holding five
    /// episodes must not leave four of them looking absent.
    #[test]
    fn a_separator_emits_the_inclusive_run() {
        assert_eq!(
            parse_filename("Show.1x01-05.mkv").episode_numbers(),
            vec![1, 2, 3, 4, 5]
        );
        assert_eq!(
            parse_filename("Show.S15E06-08.mkv").episode_numbers(),
            vec![6, 7, 8]
        );
    }

    /// The cap is what keeps a range from being arbitrary, and it is unchanged.
    /// `S01E91-E100` is 10 wide against a cap of 8 — refused before and after.
    #[test]
    fn the_cap_still_bounds_the_span() {
        assert_eq!(
            parse_filename("Show.S01E01-E20.mkv").episode_numbers(),
            vec![1]
        );
        assert_eq!(
            parse_filename("Series.S01E91-E100.mkv").episode_numbers(),
            vec![91]
        );
    }

    /// A descending or equal second number is not a range.
    #[test]
    fn a_backwards_span_is_not_a_range() {
        assert_eq!(
            parse_filename("Show.S01E05-E02.mkv").episode_numbers(),
            vec![5]
        );
        assert_eq!(
            parse_filename("Show.S01E05-E05.mkv").episode_numbers(),
            vec![5]
        );
    }

    /// Season 0 is a season and episode 0 stays refused — both load-bearing.
    #[test]
    fn season_zero_and_episode_zero_are_unchanged() {
        assert_eq!(parse_filename("Show.S00E01-E02.mkv").season, Some(0));
        assert_eq!(parse_filename("Show.S01E00-E01.mkv").episode, None);
    }
}
