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
    let head = s[..back_up_to_open_bracket(s, i)]
        .trim()
        .trim_matches([' ', '-', '_', '.', '(', '['])
        .trim();
    if head.is_empty() {
        s.to_string()
    } else {
        head.to_string()
    }
}

/// Move a cut at `i` back to the opening bracket of the group it fell inside.
///
/// **A bracket group is atomic.** `[BD 1080p FLAC]` holds one junk token, so
/// the whole group is release metadata and the title ends before it — cutting
/// at `1080p` leaves `Goblin Slayer - Goblin's Crown [BD`, a title with half a
/// bracket on the end. 18 corpus cases end this way.
///
/// Only a group left **open** at `i` counts. A bracket the title closes before
/// the cut is part of the title, so `Anon [Bracketed] Film 1080p` still cuts at
/// the junk token and keeps the bracket.
fn back_up_to_open_bracket(s: &str, i: usize) -> usize {
    let mut open: Vec<usize> = Vec::new();
    for (at, c) in s[..i].char_indices() {
        match c {
            '[' | '(' | '{' => open.push(at),
            ']' | ')' | '}' => {
                open.pop();
            }
            _ => {}
        }
    }
    open.first().copied().unwrap_or(i)
}

/// Cut the stem at `i` and clean it, backing the cut out of any bracket group
/// it landed inside.
///
/// Every cut site goes through here. Bracket atomicity was first written into
/// `cut_at_title_junk` alone and moved exactly **one** corpus case: 17 of the
/// 18 titles ending in a half-open bracket come from the episode-token cut
/// (`Series Title [1x05] Episode Title`) or the year cut
/// (`[GM-Team][国漫][Anime Title][2019]`), neither of which is the junk cut.
/// A fix that removes one route is not a fix for the mechanism.
fn cut_stem_at(stem: &str, i: usize) -> String {
    clean_title(&stem[..back_up_to_open_bracket(stem, i.min(stem.len()))])
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

/// Cut a title at a separated absolute-episode number — `Show - 12 [Group]`.
///
/// Anime releases number episodes absolutely and separate the number from the
/// title with a spaced dash. Nothing else in the name says where the title
/// ends, so the dash is the terminator: 97 of the corpus's title-only failures
/// are this one form.
///
/// **Spaced only.** A glued dash is part of the title and always has been —
/// `Stargate SG-1`, `Storage 24-7`. Measured over the 25,043-file dogfood
/// library, matching a glued dash changes 215 basenames, 213 of them bound
/// today, and turns `Stargate SG-1` into `Stargate SG`. The spaced form changes
/// two, both unmatched. That gap is the whole reason this rule is narrow.
///
/// **A four-digit number in the year range is a year, not an episode.** The
/// movie branch has already cut at any year it found before this runs, so the
/// guard bites only on the episode branch, which never parses a year at all.
///
/// **The head must still carry a letter**, and when it does not the scan moves
/// on to the next candidate rather than giving up. `5x09 - 100` has no title
/// before the number; cutting there would leave a title that agreed with
/// nothing.
fn cut_at_absolute_episode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        if !(bytes[i] == b' ' && bytes[i + 1] == b'-' && bytes[i + 2] == b' ') {
            i += 1;
            continue;
        }
        let start = i + 3;
        let mut j = start;
        while j < bytes.len() && bytes[j].is_ascii_digit() && j - start < 4 {
            j += 1;
        }
        let digits = j - start;
        // A run longer than four digits is not an episode number, and a letter
        // or digit straight after it means the token is something else
        // (`- 07v2`, `- 12th`).
        let bounded =
            j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j].is_ascii_alphabetic());
        if digits == 0 || bounded {
            i += 1;
            continue;
        }
        if digits == 4 {
            let n: i32 = s[start..j].parse().unwrap_or(0);
            if (1900..=2100).contains(&n) {
                i += 1;
                continue;
            }
        }
        let head = s[..i].trim().trim_matches('-').trim();
        if head.chars().any(char::is_alphabetic) {
            return head.to_string();
        }
        i += 1;
    }
    s.to_string()
}

/// Season words this recognises, and only these. Each is one the corpus
/// actually contains: `season` (23 occurrences), `temporada` (6), `stagione`
/// (2), `saison` (1).
///
/// **`series` is deliberately absent.** The English "Series 4" spelling earns
/// one corpus case and costs a daily show: `Tree_Series_2018_06_22_Seth_Meyers`
/// becomes season 2018. The word also appears 392 times in the corpus without
/// a number after it, which is a measure of how ordinary it is inside a title.
const SEASON_WORDS: &[&str] = &["season", "saison", "stagione", "temporada"];

/// A season token carrying no episode — `Series.S01.720p`, `30 Series Season 04`.
///
/// Returns `(token_start, season)`. A season pack is a real shape: 77 corpus
/// cases fail on season while the number is sitting in the basename, because
/// [`find_season_episode`] requires an episode marker after the season digits
/// and a pack has none.
///
/// Scanned on the **normalised** stem, where `_` and `.` are spaces. That
/// replacement is byte-for-byte, so the index returned is an index into the
/// original stem as well.
///
/// Three guards, each one paid for by a passing corpus case it would otherwise
/// break:
///
/// 1. **A spaced dash-number anywhere means the token belongs to the title.**
///    `Some Anime Show S3 - 12` is the anime absolute form and Sonarr keeps the
///    `S3`; twelve corpus cases pass today because Nightjar keeps it too. The
///    dash is matched loosely — one dash, any run of spaces — so an odd
///    spelling makes the rule decline rather than fire.
/// 2. **A year straight after the number means the `s` is not a marker.**
///    `V.H.S.2.2013.LIMITED` is the film V/H/S/2. A season number is followed
///    by release metadata, not by the work's year.
/// 3. **The head must carry a letter**, so a name that is only a token has no
///    title to end.
fn find_bare_season(normalized: &str) -> Option<(usize, i32)> {
    if has_spaced_dash_number(normalized) {
        return None;
    }
    let lower = normalized.to_ascii_lowercase();
    let bytes = lower.as_bytes();

    let mut best: Option<(usize, i32)> = None;
    let mut consider = |i: usize, marker_len: usize| {
        // One optional separator between the marker and the digits.
        let mut j = i + marker_len;
        if j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'.' || bytes[j] == b'_') {
            j += 1;
        }
        let start = j;
        while j < bytes.len() && bytes[j].is_ascii_digit() && j - start < 4 {
            j += 1;
        }
        if j == start
            || (j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j].is_ascii_alphabetic()))
        {
            return;
        }
        if year_follows(bytes, j) {
            return;
        }
        if !normalized[..i].chars().any(char::is_alphabetic) {
            return;
        }
        let Ok(season) = lower[start..j].parse::<i32>() else {
            return;
        };
        if best.is_none_or(|(b, _)| i < b) {
            best = Some((i, season));
        }
    };

    let mut i = 0;
    while i < bytes.len() {
        if i == 0 || is_token_boundary(bytes[i - 1]) {
            if bytes[i] == b's' {
                consider(i, 1);
            }
            for w in SEASON_WORDS {
                if lower[i..].starts_with(w) {
                    consider(i, w.len());
                }
            }
        }
        i += 1;
    }
    best
}

/// A separator a season marker may start after. An apostrophe is not one:
/// without that, `Ocean's 11` is season 11.
fn is_token_boundary(b: u8) -> bool {
    matches!(b, b' ' | b'.' | b'_' | b'-' | b'[' | b'(')
}

/// A four-digit year at `j`, after an optional single separator.
fn year_follows(bytes: &[u8], mut j: usize) -> bool {
    if j < bytes.len() && matches!(bytes[j], b' ' | b'.' | b'_' | b'-') {
        j += 1;
    }
    if j + 4 > bytes.len() || !bytes[j..j + 4].iter().all(u8::is_ascii_digit) {
        return false;
    }
    if j + 4 < bytes.len() && bytes[j + 4].is_ascii_digit() {
        return false;
    }
    let y = std::str::from_utf8(&bytes[j..j + 4])
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(0);
    (1900..=2100).contains(&y)
}

/// ` - 12` anywhere, with any run of spaces around the dash.
fn has_spaced_dash_number(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'-' && i > 0 && bytes[i - 1] == b' ' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] == b' ' {
                j += 1;
            }
            if j > i + 1 && j < bytes.len() && bytes[j].is_ascii_digit() {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Cut a title at a bare episode marker — `Anon Show Ep01`, `Anon Show E1135`.
///
/// A release that numbers episodes absolutely often marks the number with `E`
/// or `Ep` and no season at all, so [`find_season_episode`] declines and the
/// title runs on through the marker and the episode title behind it.
///
/// **The number is not parsed.** It is an absolute episode number and
/// [`ParsedName`] has nowhere to put one. The title is the half that is
/// scorable and the half the matcher searches on.
///
/// **Two digits minimum.** `E06` is a marker; `E3` is as likely to be a title
/// word, and taking one digit earns nothing measurable. The marker must also
/// start at a separator, so `HEVC` and `EAC3` are untouched — the `e` in
/// `HEVC` follows a letter, and the `a` after `EAC3`'s `E` is not a digit.
fn cut_at_episode_marker(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'e' && (i == 0 || is_token_boundary(bytes[i - 1])) {
            let mut j = i + 1;
            if j < bytes.len() && bytes[j] == b'p' {
                j += 1;
            }
            if j < bytes.len() && matches!(bytes[j], b' ' | b'.' | b'_') {
                j += 1;
            }
            let start = j;
            while j < bytes.len() && bytes[j].is_ascii_digit() && j - start < 4 {
                j += 1;
            }
            let digits = j - start;
            let bounded =
                j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j].is_ascii_alphabetic());
            if (2..=4).contains(&digits) && !bounded {
                let head = s[..i].trim().trim_matches([' ', '-', '_', '.']).trim();
                if head.chars().any(char::is_alphabetic) {
                    return head.to_string();
                }
            }
        }
        i += 1;
    }
    s.to_string()
}

/// Parse a media filename (not a full path) into title / kind / episode fields.
pub fn parse_filename(file_name: &str) -> ParsedName {
    let stem = strip_leading_group(strip_extension(file_name));
    let normalized = stem.replace(['_', '.'], " ");
    let compact = stem.to_ascii_lowercase();

    if let Some((before, season, episode, episode_end)) = find_season_episode(&compact) {
        let title = cut_at_episode_marker(&cut_at_absolute_episode(&cut_at_title_junk(
            &cut_stem_at(stem, before),
        )));
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

    // A season token with no episode is a season pack. It runs after the
    // season/episode scan, which owns every name that carries both, and before
    // the year branch, which would otherwise call a pack a movie.
    if let Some((before, season)) = find_bare_season(&normalized) {
        let title = cut_at_episode_marker(&cut_at_title_junk(&cut_stem_at(stem, before)));
        return ParsedName {
            title: if title.is_empty() {
                stem.to_string()
            } else {
                title
            },
            kind: MediaKind::Episode,
            year: None,
            season: Some(season),
            episode: None,
            episode_end: None,
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
                Some(i) if i > 0 => cut_stem_at(stem, i),
                _ => cut_at_title_junk(&clean_title(stem)),
            }
        }
        None => cut_at_title_junk(&clean_title(stem)),
    };
    let title = cut_at_episode_marker(&cut_at_absolute_episode(&title));

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

/// Strip a file extension, and only a file extension.
///
/// **A release name is not a filename.** Cutting at the last dot whatever
/// follows it threw away `S01E91-E100` from `Series.S01E91-E100`, and the
/// parse then saw `Series` with no season and no episode at all. 232 corpus
/// cases lose a non-extension suffix that way; ten of them lose their only
/// season/episode token and six their only year, including
/// `A.I.Artificial.Movie.(2001)`.
///
/// One to four characters, all alphanumeric. Checked against the 25,043-file
/// dogfood library: every extension in it satisfies that, and no basename in
/// it has a suffix that does not — so the guard costs the library nothing.
fn strip_extension(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 && is_extension(&name[i + 1..]) => &name[..i],
        _ => name,
    }
}

fn is_extension(suffix: &str) -> bool {
    (1..=4).contains(&suffix.len()) && suffix.chars().all(|c| c.is_ascii_alphanumeric())
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

/// A four-digit year, preferring one in parentheses over a bare number.
///
/// `Title (YYYY)` is what every renamer writes and what this library uses, and
/// the parentheses are the thing that makes the number a *year* rather than a
/// number that happens to be in the name. Taking the first four digits anywhere
/// reads `Wonder Woman 1984 (2020)` as **1984** and `2001 A Space Odyssey
/// (1968)` as **2001** — and where the number is not at position 0,
/// [`parse_filename`] cuts the title there too, so one rule loses both halves.
///
/// Measured 2026-08-19 across 25,004 items: six files disagree, and no file
/// wants the bare token while a parenthesised year is present. Four of the six
/// bind correctly today only because the *folder* year overrules this parse —
/// which is why this lands before that precedence is touched. Swapping first
/// was measured to turn three of them into failures and to send `2012 (2009)`
/// to a Japanese film whose title ends in `2012`.
///
/// The bare-token scan is unchanged and still runs when there are no
/// parentheses, which is the dotted release form (`Movie.Name.2019.1080p`).
fn find_year(s: &str) -> Option<i32> {
    find_parenthesised_year(s).or_else(|| find_bare_year(s))
}

/// `(YYYY)` anywhere in the name, first one wins.
fn find_parenthesised_year(s: &str) -> Option<i32> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 6 <= bytes.len() {
        if bytes[i] == b'('
            && bytes[i + 5] == b')'
            && bytes[i + 1..i + 5].iter().all(u8::is_ascii_digit)
        {
            let y = std::str::from_utf8(&bytes[i + 1..i + 5])
                .ok()?
                .parse::<i32>()
                .ok()?;
            if (1900..=2100).contains(&y) {
                return Some(y);
            }
        }
        i += 1;
    }
    None
}

fn find_bare_year(s: &str) -> Option<i32> {
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

    /// **A number in the title is not the year.** `find_year` took the first
    /// four digits anywhere, so `Wonder Woman 1984 (2020)` parsed as 1984 —
    /// and because the cut follows the year, the title became `Wonder Woman`.
    /// One rule, both halves wrong.
    ///
    /// Four of the six real files affected bind correctly *only* because the
    /// folder year overrules this parse, which is why this lands before that
    /// precedence is touched.
    #[test]
    fn a_parenthesised_year_outranks_a_number_in_the_title() {
        for (name, title, year) in [
            (
                "Wonder Woman 1984 (2020) Bluray-1080p.mkv",
                "Wonder Woman 1984",
                2020,
            ),
            (
                "Blade Runner 2049 (2017) Bluray-1080p.mkv",
                "Blade Runner 2049",
                2017,
            ),
            ("1917 (2019) Bluray-1080p.mkv", "1917", 2019),
            (
                "2001 A Space Odyssey (1968) Bluray-1080p.mkv",
                "2001 A Space Odyssey",
                1968,
            ),
            ("2012 (2009) Bluray-1080p.mp4", "2012", 2009),
            ("2067 (2020) Bluray-1080p.mkv", "2067", 2020),
            // a four-digit number that is neither the year nor at the start
            ("The 1900 House (1999).mkv", "The 1900 House", 1999),
        ] {
            let p = parse_filename(name);
            assert_eq!(p.title, title, "{name}");
            assert_eq!(p.year, Some(year), "{name}");
        }
    }

    /// The bare-token scan is the fallback, not the rule that was removed. It
    /// still runs whenever there are no parentheses — which is the dotted
    /// release form, and the majority of names that carry a year at all.
    #[test]
    fn a_bare_year_still_parses_when_there_are_no_parentheses() {
        for (name, title, year) in [
            ("Some Film 1999.mkv", "Some Film", 1999),
            ("Movie.Name.2019.1080p.mkv", "Movie Name", 2019),
            ("Some Film (2017).mkv", "Some Film", 2017),
            ("Fight Club (1999) Bluray-1080p.mkv", "Fight Club", 1999),
        ] {
            let p = parse_filename(name);
            assert_eq!(p.title, title, "{name}");
            assert_eq!(p.year, Some(year), "{name}");
        }
    }

    /// The episode branch never parses a year and must stay that way: a year in
    /// an episode title is not the show's year, and `series_library_year` reads
    /// this field.
    #[test]
    fn the_episode_branch_still_parses_no_year() {
        for name in [
            "Show - 1x01 - Title (2019).mkv",
            "Show - S02E03 - Something 1984.mkv",
        ] {
            let p = parse_filename(name);
            assert_eq!(p.year, None, "{name}");
            assert_eq!(p.title, "Show", "{name}");
        }
    }

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

    /// A spaced dash before a number ends the title. This is the anime
    /// absolute-numbering form and it is the single largest title class in the
    /// Sonarr/Radarr corpus: 92 of the 738 applicable cases go from failing to
    /// passing on this rule alone.
    ///
    /// The absolute number itself is not parsed — nothing in `ParsedName`
    /// holds one — so these files still carry no season or episode. The title
    /// is what moves, and the title is what the matcher searches on.
    #[test]
    fn a_spaced_dash_number_ends_the_title() {
        for (name, title) in [
            (
                "[Commie] Anon Anime Show - 11 [65F220B4].mkv",
                "Anon Anime Show",
            ),
            (
                "[HorribleSubs] Anon Anime Show - 145 [720p].mkv",
                "Anon Anime Show",
            ),
            (
                "[Underwater] Anon Anime Show - 12 (720p) [5C7BC4F9]",
                "Anon Anime Show",
            ),
            (
                "Anon_Anime_Show_-_01(DVD)_-_(Anon_Group)[5AF6F1E4].mkv",
                "Anon Anime Show",
            ),
            (
                "[Doki]Anon Show - 07 (1280x720 Hi10P AAC) [80AF7DDE]",
                "Anon Show",
            ),
            // The first candidate wins: what follows the number is an episode
            // title, not more of the show title.
            ("Anon Show - 031 - An Episode Title [Anon].avi", "Anon Show"),
            (
                "[CBM]_Anon_Show_-_11_-_511_Kinderheim_[6C70C4E4].mkv",
                "Anon Show",
            ),
            // A season token in the title is part of it, not junk.
            (
                "[SFW-sage] Anon Show S3 - 12 [720p][D07C91FC]",
                "Anon Show S3",
            ),
            ("[HorribleSubs] Anon Show 2 - 05 [720p].mkv", "Anon Show 2"),
        ] {
            let p = parse_filename(name);
            assert_eq!(p.title, title, "{name}");
        }
    }

    /// The guard that makes the rule safe, and the measurement behind it. A
    /// **glued** dash is part of the title: matching one changes 215 basenames
    /// in the 25,043-file dogfood library, 213 of them bound today, and turns
    /// `Stargate SG-1` into `Stargate SG`. The spaced rule changes two, both
    /// unmatched.
    #[test]
    fn a_glued_dash_number_is_part_of_the_title() {
        assert_eq!(
            parse_filename("Anon SG-1 - 1x03 - An Episode - Bluray-1080p.mp4").title,
            "Anon SG-1"
        );
        assert_eq!(
            parse_filename("Anon Film-24 (2011).mkv").title,
            "Anon Film-24"
        );
        assert_eq!(
            parse_filename("Anon Show-01 - 2x04 - An Episode.mkv").title,
            "Anon Show-01"
        );
    }

    /// The other three guards.
    ///
    /// A four-digit number in the year range is a year. The movie branch has
    /// already cut at any year it found, so this bites on the episode branch,
    /// which never parses one.
    ///
    /// A number glued to a letter is not an episode number (`- 07v2`).
    ///
    /// The head must carry a letter, and when it does not the scan moves on
    /// rather than giving up — cutting `5x09 - 100` to `5x09` would leave a
    /// title that agrees with nothing.
    #[test]
    fn the_absolute_cut_has_three_more_guards() {
        assert_eq!(
            parse_filename("Anon Show - 1999 - S01E02 - An Episode.mkv").title,
            "Anon Show - 1999"
        );
        // `07v2` is skipped, and nothing later is a candidate, so the name is
        // left whole rather than cut at the next dash.
        assert_eq!(
            parse_filename("Anon Show - 07v2 - An Episode.mkv").title,
            "Anon Show - 07v2 - An Episode"
        );
        // Nothing before the number carries a letter at the first candidate, so
        // the scan continues and finds no acceptable cut.
        let p = parse_filename("- 100 - 200.mkv");
        assert!(!p.title.is_empty(), "got {:?}", p.title);
    }

    /// The control: the form 93% of the measured library is written in is
    /// untouched by the absolute cut, and so is every shape that already had a
    /// season and episode token.
    #[test]
    fn the_common_form_is_unchanged_by_the_absolute_cut() {
        let p = parse_filename("Anon Show - 4x11 - An Episode Title - Bluray-1080p.mkv");
        assert_eq!(p.title, "Anon Show");
        assert_eq!(p.season, Some(4));
        assert_eq!(p.episode, Some(11));
        let m = parse_filename("Anon Film (2019) Bluray-1080p.mkv");
        assert_eq!(m.title, "Anon Film");
        assert_eq!(m.year, Some(2019));
    }

    /// A season token with no episode marker is a season, and it ends the
    /// title. This is the season-pack shape and it was the largest
    /// non-title class left in the corpus: 166 cases failed on season, and
    /// this moves 53 of them to a full pass.
    ///
    /// The file carries no episode, so `episode` stays `None` and
    /// `episode_numbers()` stays empty. A pack is TV, so the kind is
    /// `Episode`.
    #[test]
    fn a_season_token_with_no_episode_is_a_season() {
        for (name, title, season) in [
            ("Anon.Show.S02.720p.x264-GROUP", "Anon Show", 2),
            (
                "The.Anon.Show.US.S03.720p.x264-GROUP",
                "The Anon Show US",
                3,
            ),
            ("30 Anon Show S03 WS PDTV XviD GROUP", "30 Anon Show", 3),
            ("Anon Show Season 4 WS PDTV XviD GROUP", "Anon Show", 4),
            ("Anon Show Season4 WS PDTV XviD GROUP", "Anon Show", 4),
            (
                "Anon Show S 01 720p WEB DL DD 5 1 h264 GROUP",
                "Anon Show",
                1,
            ),
            ("Anon.Show.Stagione.3.HDTV.XviD-NOTAG", "Anon Show", 3),
            ("Anon.Show.Saison3.VOSTFR.HDTV.XviD-NOTAG", "Anon Show", 3),
            ("Anon Show (1994) - Temporada 10", "Anon Show (1994)", 10),
            // A season number that looks like a year is still a season when it
            // is glued to the marker.
            ("My.Anon.Show.S2014.720p.HDTV.x264-ME", "My Anon Show", 2014),
        ] {
            let p = parse_filename(name);
            assert_eq!(p.season, Some(season), "{name}");
            assert_eq!(p.episode, None, "{name}");
            assert_eq!(p.kind, MediaKind::Episode, "{name}");
            assert_eq!(p.title, title, "{name}");
            assert!(p.episode_numbers().is_empty(), "{name}");
        }
    }

    /// The three guards, each one bought by a passing corpus case it would
    /// otherwise have broken.
    #[test]
    fn the_bare_season_rule_has_three_guards() {
        // 1. A spaced dash-number means the token is part of the title. This is
        //    the anime absolute form and Sonarr keeps the `S3`.
        let a = parse_filename("[Anon] Anon Anime Show S3 - 12 [720p][D07C91FC]");
        assert_eq!(a.title, "Anon Anime Show S3");
        assert_eq!(a.season, None);

        // 2. A year straight after the number means the `s` is not a marker.
        //    `V.H.S.2` is the film, not season 2.
        let b = parse_filename("V.H.S.2.2013.LIMITED.720p.BluRay.x264-GROUP");
        assert_eq!(b.season, None);
        assert_eq!(b.title, "V H S 2");
        assert_eq!(b.year, Some(2013));

        // 3. `series` is not a season word. It appears 392 times in the corpus
        //    with no number after it, and taking it costs a daily show.
        let c = parse_filename("Anon_Show_2018_06_22_A_Guest_720p_HEVC_x265-GROUP");
        assert_eq!(c.season, None);
        assert_eq!(c.title, "Anon Show");
    }

    /// An apostrophe is not a token boundary. Without that guard `Ocean's 11`
    /// is season 11.
    #[test]
    fn an_apostrophe_does_not_start_a_season_marker() {
        let p = parse_filename("Anon's 11 (2001) Bluray-1080p.mkv");
        assert_eq!(p.season, None);
        assert_eq!(p.title, "Anon's 11");
        assert_eq!(p.year, Some(2001));
    }

    /// A name carrying both a season and an episode still takes the
    /// season/episode branch, which owns it. The bare-season rule runs only
    /// after that scan has declined.
    #[test]
    fn a_season_and_episode_still_beat_the_bare_season_rule() {
        let p = parse_filename("Anon Show - S02E05 - An Episode - Bluray-1080p.mkv");
        assert_eq!(p.season, Some(2));
        assert_eq!(p.episode, Some(5));
        assert_eq!(p.title, "Anon Show");
        let q = parse_filename("Anon.Show.S01E01.720p.mkv");
        assert_eq!(q.season, Some(1));
        assert_eq!(q.episode, Some(1));
    }

    /// A bracket group is atomic: a cut that lands inside one backs out to the
    /// opening bracket. `[BD 1080p FLAC]` is release metadata, so a title
    /// ending `... [BD` has half a bracket on it.
    ///
    /// **The mechanism has three routes and all three go through the same
    /// helper.** Written into the junk cut alone it moved one corpus case;
    /// seventeen of the eighteen come from the episode-token cut and the year
    /// cut instead.
    #[test]
    fn a_cut_inside_a_bracket_backs_out_to_the_bracket() {
        // the junk cut
        assert_eq!(
            parse_filename("[anon] Anon Show - Anon Crown [BD 1080p FLAC] [CD298D48].mkv").title,
            "Anon Show - Anon Crown"
        );
        // the episode-token cut
        assert_eq!(
            parse_filename("Anon Show [1x05] An Episode").title,
            "Anon Show"
        );
        assert_eq!(
            parse_filename("Anon Show [S01E05] An Episode").title,
            "Anon Show"
        );
        assert_eq!(
            parse_filename("Anon Show - [02x01] - An Episode").title,
            "Anon Show"
        );
        assert_eq!(
            parse_filename("The Anon Show (2010) - [S01E01-02-03] - An Episode").title,
            "The Anon Show (2010)"
        );
        // the year cut
        assert_eq!(
            parse_filename("[Anon][Anon Title][2019][234][AVC][GB][1080P]").title,
            "[Anon Title]"
        );
    }

    /// The guard: only a group left **open** at the cut counts. A bracket the
    /// title closes before the cut is part of the title.
    #[test]
    fn a_closed_bracket_before_the_cut_stays_in_the_title() {
        assert_eq!(
            parse_filename("Anon [Bracketed] Film (2011).mkv").title,
            "Anon [Bracketed] Film"
        );
        assert_eq!(
            parse_filename("Anon [Bracketed] Show - 1x02 - An Episode.mkv").title,
            "Anon [Bracketed] Show"
        );
        assert_eq!(
            parse_filename("Anon Show [2022] [S25E13] [PL] [720p].mkv").title,
            "Anon Show [2022]"
        );
    }

    /// A bare episode marker ends the title. These names carry no season, so
    /// the season/episode scan declines and the title used to run on through
    /// the marker and the episode title behind it.
    ///
    /// The number itself is not parsed — it is an absolute episode number and
    /// `ParsedName` has nowhere to put one.
    #[test]
    fn a_bare_episode_marker_ends_the_title() {
        for (name, title) in [
            ("[Anon] Anon Show Ep01 (D2201EC5).mkv", "Anon Show"),
            ("Anon Show EP06 720p x265 GROUP.mp4", "Anon Show"),
            (
                "AnonShow.E1135.Ein.Titel.GERMAN.1080p.WEBRip.x264-Group",
                "AnonShow",
            ),
            ("Anon_Show_e66_time_is_money_part_one", "Anon Show"),
            (
                "Anon.Show.Ep01-12.Complete.English.AC3.DL.1080p.BluRay.x264",
                "Anon Show",
            ),
        ] {
            let p = parse_filename(name);
            assert_eq!(p.title, title, "{name}");
            assert_eq!(p.episode, None, "{name}");
        }
    }

    /// The guards. Two digits minimum, a separator on the left, and no letter
    /// or digit on the right — which is what keeps every codec token that
    /// starts with `E` out of it.
    #[test]
    fn the_episode_marker_does_not_eat_codec_tokens_or_words() {
        assert_eq!(
            parse_filename("Anon Film (2011) HEVC EAC3 E-AC3 EXTENDED.mkv").title,
            "Anon Film"
        );
        // One digit is not a marker: `E3` is as likely a title word.
        assert_eq!(
            parse_filename("Anon E3 Film (2011).mkv").title,
            "Anon E3 Film"
        );
        // Glued to a letter or another digit it is not a marker either.
        assert_eq!(
            parse_filename("Anon Show E06x Film (2011).mkv").title,
            "Anon Show E06x Film"
        );
        // Nothing before it carries a letter, so there is no title to end.
        assert_eq!(
            parse_filename("Ep01 (D2201EC5).mkv").title,
            "Ep01 (D2201EC5)"
        );

        // A year inside the show title is still lost, and this rule does not
        // reach it: the year branch cuts at `2018` long before the marker is
        // looked at, so `Anon Show 2018 EP06` yields `Anon Show`. That is the
        // `Wonder Woman 1984` shape on the TV side; it needs the year branch,
        // not this one.
        assert_eq!(
            parse_filename("Anon Show 2018 EP06 720p x265 GROUP.mp4").title,
            "Anon Show"
        );
    }

    /// A release name is not a filename, so only a real extension is stripped.
    /// Cutting at the last dot whatever followed it threw away `S01E91-E100`
    /// and left the parse looking at `Series` alone.
    #[test]
    fn only_a_real_extension_is_stripped() {
        let a = parse_filename("Anon.Show.S02E15");
        assert_eq!(a.season, Some(2));
        assert_eq!(a.episode, Some(15));
        assert_eq!(a.title, "Anon Show");

        let b = parse_filename("Warehouse.13.S01E01");
        assert_eq!(b.season, Some(1));
        assert_eq!(b.episode, Some(1));

        // A group tag carrying a dot is not an extension either.
        let c = parse_filename("[Anon-Group.Hu] Dr Anon S3 - 21 [1080p]");
        assert_eq!(c.title, "Dr Anon S3");

        // A parenthesised year at the end survives.
        let d = parse_filename("A.Anon.Name.(1998)");
        assert_eq!(d.year, Some(1998));
    }

    /// Real extensions still go, and the guard is the library's own shape: one
    /// to four characters, all alphanumeric.
    #[test]
    fn real_extensions_are_still_stripped() {
        for ext in ["mkv", "mp4", "avi", "m4v", "ts", "webm", "divx", "m2ts"] {
            let p = parse_filename(&format!("Anon Film (2019).{ext}"));
            assert_eq!(p.title, "Anon Film", "{ext}");
            assert_eq!(p.year, Some(2019), "{ext}");
        }
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
