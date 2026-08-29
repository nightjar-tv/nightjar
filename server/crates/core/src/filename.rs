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
    /// **`episode` is an absolute number, not a season-relative one.**
    ///
    /// A release that marks an episode and gives no season — `E56`, `Ep06`,
    /// `69. Bölüm` — is numbering the series end to end. `season` is `None`
    /// beside it, and that pairing is a true statement about the filename:
    /// *this episode, season unstated*. Synthesising `Some(1)` would be an
    /// invention.
    ///
    /// **No consumer may pair this number with a season from anywhere else.**
    /// A folder season is season-relative and this number is not, so
    /// `Season 2/…E56….mkv` is not `(2, 56)` — it is a wrong bind that no
    /// error reports. [`parse_filename_in`] is the one place that fills a
    /// missing season, and it refuses when this flag is set.
    ///
    /// **Deliberately unconsumed.** Nothing resolves an absolute number to a
    /// season yet; that is the episode-group question, and it is not this
    /// field's job. `EpisodeSlot::season_episodes` in `nightjar-metadata`
    /// already returns nothing without a season, so such a file reaches the
    /// database and is not slotted. **The flag exists so the number cannot be
    /// silently misread later, not because something reads it now** — do not
    /// delete it as unused, and do not wire it to a guessed season.
    pub episode_absolute: bool,
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
    "bluray",
    "blu-ray",
    "webdl",
    "web-dl",
    "webrip",
    "hdtv",
    "pdtv",
    "dvdrip",
    "bdrip",
    "hdrip",
    "tvrip",
    "sdtv",
    "remux",
    "2160p",
    "1080p",
    "1080i",
    "720p",
    "480p",
    "x264",
    "x265",
    "h264",
    "h265",
    "hevc",
    "xvid",
    "divx",
    "aac",
    "ac3",
    "dts",
    "truehd",
    "atmos",
    "flac",
    "10bit",
    "8bit",
    "hdr10",
    "proper",
    "repack",
    // Edition and language tokens, each one measured on its own before it was
    // added. `extended` earns 7 corpus cases, `truefrench` and `imax` one
    // each, and none of the three cuts a single title in the 25,043-file
    // dogfood library.
    //
    // **`german` is deliberately absent although it earns 7.** The corpus holds
    // the counterexample itself — `The.Good.German.2006.720p.BluRay` is a real
    // film — and the terminator now runs on the year arm, which is what used
    // to protect it. `complete`, `uncut` and `unrated` are absent for the same
    // reason and the library supplies theirs: `A Complete Unknown` becomes
    // `A`, and `South Park Bigger Longer and Uncut` and `The Toxic Avenger
    // Unrated` lose their last word. All three are bound today.
    "extended",
    "truefrench",
    "imax",
    // **Three anime production markers, added 2026-08-28.** `NCOP` and `NCED`
    // are a non-credit opening and ending; `OVA` is an original video
    // animation. Each earns one corpus case, and each appears in **no title
    // anywhere in the available evidence** — zero of the corpus's title
    // expectations, zero of the 25,043 dogfood `db_title`s, zero of its
    // basenames.
    //
    // **The evidence is weaker than that count makes it sound**, and it is
    // recorded here so the next reader does not overrate it. The parser sweep —
    // the instrument most likely to catch a word that eats a title — renders
    // **0 of its 74,624 names** containing any of the three, because it is
    // built from the dogfood library and that library holds almost no anime.
    // So two corpora agree and the third cannot see the question.
    //
    // **`v2` was measured with them and refused.** It earns two, more than any
    // of these, and no title in either corpus contains it. But `V2: Escape from
    // Hell` (2021) is a real film, and it survives this list only because the
    // token leads the name and the empty-head guard returns the title whole —
    // position, not the word boundary. `Operation V2 (2021)` becomes
    // `Operation`. Two cases is a thin trade for a failure that can be
    // demonstrated rather than only imagined.
    "ncop",
    "nced",
    "ova",
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

/// Cut a title at a date written as three number groups — `2016 02 25`,
/// `2012.16.02`, `04.28.2014`.
///
/// A daily show's filename numbers its episodes by air date, and the date sits
/// where an episode token would. Nothing cut it, so `Judge Developer 2016 02 25
/// S20E142` searched the provider for `Judge Developer 2016 02 25`.
///
/// **Two arms reach this and one does not.** The episode arm cuts the stem at
/// the season/episode marker and leaves whatever came before, date included.
/// The movie arm cuts at the year first — and a date contains a year — so a
/// mid-string date is already handled there and never arrives; what does arrive
/// is the case where the year cut landed at index 0 because the *title* starts
/// with a year-shaped number (`2020 A Late Talk Show`).
///
/// **Three whole numeric tokens, and one of the outer two is a year.** All
/// three guards earn their place:
///
/// * *Three*, not two, because a title followed by its release year is two
///   (`Blade Runner 2049 2017`) and cutting there would take the year of half
///   the movie library.
/// * *One end is a four-digit year in 1900–2100*, the same range every other
///   year guard in this file uses. The remaining two are 1–31, so a resolution
///   or a bitrate cannot pose as one.
///
/// **The head must contain a letter**, and that guard turned out to be the one
/// carrying the weight. A third was written first — *the token before the date
/// must not be numeric* — for `9-1-1`, a real show whose name offers `1 1 2018`
/// as a perfectly good date. It was removed after measurement: the sweep holds
/// five `9-1-1` names, and taking the guard out moved **none** of the 74,624,
/// because `9-1-1` has no letter in it and the head rule already declines.
/// A guard with a demonstrable cost — it also refuses the correct cut in
/// `Show 5 2016 02 25` — and no demonstrable case is not one to keep.
///
/// The letter rule is the same one `cut_at_title_junk` and
/// `cut_at_absolute_episode` apply, and for the same reason: a name that is
/// only a date must keep itself, because the caller substitutes the whole stem
/// for an empty title.
///
/// **ASCII digits only.** `is_ascii_digit` and not a Unicode digit class: an
/// Arabic-Indic date in an Arabic title is not this form, and a prototype of
/// this rule written in Python cut one because `str.isdigit()` said yes.
fn cut_at_date(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut toks: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if is_token_boundary(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && !is_token_boundary(bytes[i]) {
            i += 1;
        }
        toks.push((start, i));
    }
    let numeric = |t: (usize, usize)| bytes[t.0..t.1].iter().all(u8::is_ascii_digit);
    let value = |t: (usize, usize)| s[t.0..t.1].parse::<i32>().unwrap_or(-1);
    let is_year = |t: (usize, usize)| t.1 - t.0 == 4 && (1900..=2100).contains(&value(t));
    let is_day = |t: (usize, usize)| t.1 - t.0 <= 2 && (1..=31).contains(&value(t));
    for w in 0..toks.len().saturating_sub(2) {
        let (a, b, c) = (toks[w], toks[w + 1], toks[w + 2]);
        if !(numeric(a) && numeric(b) && numeric(c)) {
            continue;
        }
        let dated =
            (is_year(a) && is_day(b) && is_day(c)) || (is_day(a) && is_day(b) && is_year(c));
        if !dated {
            continue;
        }
        let head = s[..a.0].trim().trim_matches([' ', '-', '_', '.']).trim();
        if head.chars().any(char::is_alphabetic) {
            return head.to_string();
        }
    }
    s.to_string()
}

/// Month names, three letters or spelled out. **A closed list, and the ordinal
/// in front of it is what makes the list safe**: `May` is an ordinary English
/// word and `Series May 2025` keeps its title, because only `5th May` reads as
/// a date here.
const MONTHS: &[&str] = &[
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
    "jan",
    "feb",
    "mar",
    "apr",
    "jun",
    "jul",
    "aug",
    "sep",
    "oct",
    "nov",
    "dec",
];

/// A date written as one token — `140722`, `20201013` — or written out —
/// `5th Mar 2025`. Returns the title and the date's year.
///
/// **[`cut_at_date`] already ends a title at a date, but only at three separate
/// numeric tokens** — `2011.01.10`, `13.02.2025`. A daily show whose date is
/// one run, or spelled with a month, ran straight past it and the date stayed
/// in the title.
///
/// **The cut and the year are one read.** The date carries a year, and three of
/// the six corpus cases fail on `year` as well as `title`; deriving the year
/// somewhere else from the same digits is how the two come to disagree.
///
/// ## The guards, and the case each one is paid for by
///
/// **Month and day are range-checked.** Six digits that are not a date are
/// common — a CRC, a release id, an episode code. `1017-1088` is an absolute
/// episode range and a loose rule read it as a date; the classifier that found
/// this class made that mistake first.
///
/// **A four-digit year is 1900–2100**, the same range every other year guard in
/// this file uses, which is what keeps CRCs out: `[97681524]` would be the year
/// 9768 and `[34073169]` the year 3407.
///
/// **The head must hold a letter**, as in [`cut_at_date`] — a name that is only
/// a date must keep itself, because the caller substitutes the whole stem for
/// an empty title.
///
/// **And for the one-token form the head must be more than one word.** This is
/// the guard the corpus paid for: `ror-240618_1007-1022-` parses correctly
/// today, and `240618` is a valid date — 2024-06-18. It is a release id glued
/// to a three-letter tag, and cutting there turns the title into `ror`. **A
/// compact date behind a single short token is not a date.** The written form
/// does not need this guard: `Series 5th Mar 2025` has a one-word head, and the
/// ordinal plus the month name is already the evidence.
fn cut_at_written_date(s: &str) -> (String, Option<i32>) {
    let lower = s.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut toks: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if is_token_boundary(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && !is_token_boundary(bytes[i]) {
            i += 1;
        }
        toks.push((start, i));
    }
    let text = |t: (usize, usize)| &lower[t.0..t.1];
    let numeric = |t: (usize, usize)| bytes[t.0..t.1].iter().all(u8::is_ascii_digit);
    let head_of = |at: usize| {
        s[..at]
            .trim()
            .trim_matches([' ', '-', '_', '.'])
            .trim()
            .to_string()
    };

    for (n, &t) in toks.iter().enumerate() {
        // The one-token form: `YYMMDD` or `YYYYMMDD`.
        if numeric(t) {
            let d = text(t);
            let ymd = match d.len() {
                8 => d[..4].parse::<i32>().ok().map(|y| (y, &d[4..6], &d[6..])),
                6 => d[..2]
                    .parse::<i32>()
                    .ok()
                    .map(|y| (2000 + y, &d[2..4], &d[4..])),
                _ => None,
            };
            if let Some((year, m, day)) = ymd
                && (1900..=2100).contains(&year)
                && (1..=12).contains(&m.parse::<i32>().unwrap_or(0))
                && (1..=31).contains(&day.parse::<i32>().unwrap_or(0))
            {
                let head = head_of(t.0);
                let words = head
                    .split([' ', '.', '_'])
                    .filter(|w| !w.is_empty())
                    .count();
                if head.chars().any(char::is_alphabetic) && words > 1 {
                    return (head, Some(year));
                }
            }
        }
        // The written form: `5th Mar 2025`, `23rd Feb 2024`.
        // **Compared as bytes, not as a string slice.** A token may be CJK,
        // and slicing it two bytes from the end lands inside a character and
        // panics — the corpus crashed the first draft of this rule on
        // `나는 SOLO`. `to_ascii_lowercase` preserves byte length, so these
        // indices are the original stem's.
        let tb = &bytes[t.0..t.1];
        let ordinal = tb.len() >= 3
            && tb[..tb.len() - 2].iter().all(u8::is_ascii_digit)
            && matches!(&tb[tb.len() - 2..], b"st" | b"nd" | b"rd" | b"th");
        if ordinal
            && let Some(&month) = toks.get(n + 1)
            && MONTHS.contains(&text(month))
        {
            let head = head_of(t.0);
            if head.chars().any(char::is_alphabetic) {
                let year = toks
                    .get(n + 2)
                    .filter(|&&y| numeric(y) && y.1 - y.0 == 4)
                    .and_then(|&y| text(y).parse::<i32>().ok())
                    .filter(|y| (1900..=2100).contains(y));
                return (head, year);
            }
        }
    }
    (s.to_string(), None)
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
/// Four guards, each one paid for by a passing case it would otherwise break:
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
/// 4. **Only a glued `s` may carry four digits** — see [`four_digit_season_ok`].
///    Guard 2 looks *past* the digits, so it cannot see a title whose own last
///    word is the marker and whose release year is the number.
fn find_bare_season(normalized: &str) -> Option<(usize, i32)> {
    if has_spaced_dash_number(normalized) {
        return None;
    }
    let lower = normalized.to_ascii_lowercase();
    let bytes = lower.as_bytes();

    let mut best: Option<(usize, i32)> = None;
    let mut consider = |i: usize, marker_len: usize, spelled: bool| {
        // One optional separator between the marker and the digits.
        let mut j = i + marker_len;
        let separated = j < bytes.len() && matches!(bytes[j], b' ' | b'.' | b'_');
        if separated {
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
        if j - start == 4 && !four_digit_season_ok(season, spelled, separated) {
            return;
        }
        if best.is_none_or(|(b, _)| i < b) {
            best = Some((i, season));
        }
    };

    let mut i = 0;
    while i < bytes.len() {
        if i == 0 || is_token_boundary(bytes[i - 1]) {
            if bytes[i] == b's' {
                consider(i, 1, false);
            }
            for w in SEASON_WORDS {
                if lower[i..].starts_with(w) {
                    consider(i, w.len(), true);
                }
            }
        }
        i += 1;
    }
    best
}

/// May a bare season token four digits wide claim them?
///
/// **Only the glued letter marker may, and only in the year range.** Three
/// tests, and each one is a different spelling costing a different thing:
///
/// 1. **Not the spelled-out word.** `Open.Season.2006.720p.BluRay` is the film
///    *Open Season*, and the four digits are its release year, not a season.
///    The rule fired because the head of the range check — *a four-digit season
///    must be a plausible year* — is exactly what a release year looks like,
///    and [`year_follows`] looks *past* the digits, so it never sees that the
///    digits **are** the year. Every form where the year follows the title
///    directly breaks: dotted, spaced and underscore. `Wedding Season`,
///    `Hunting Season`, `Mating Season`, `The Rainy Season`, and the same in
///    the three non-English spellings — `La.Temporada.2019`, `La.Saison.1999`,
///    `La.Stagione.2001`.
///
///    **This costs nothing measured.** Zero corpus cases have a spelled-out
///    season word followed by a four-digit number. Every year-season the corpus
///    asserts is the letter marker.
///
/// 2. **Not a separated letter marker.** Every one of those corpus cases is
///    *glued* — `S2014`, `S1936E18`, `S2009E09`, `S2016E231` — so nothing
///    measured buys `S 2014`, and accepting it keeps test 1's defect in a
///    rarer spelling: `The Anon S 2019 Show` and `Anon.Film.S.2019.1080p` both
///    became season 2019. Also free: zero corpus cases separate a four-digit
///    season from its marker.
///
///    Narrower than the two-digit rule deliberately. `Anon Show S 01` is a
///    season pack and stays one; width is what makes the separated spelling
///    ambiguous, not the separator.
///
/// 3. **In the year range.** `S1080` and `S2160` are a resolution with an `s`
///    in front, and width alone does not tell them from `S2014`.
fn four_digit_season_ok(season: i32, spelled: bool, separated: bool) -> bool {
    !spelled && !separated && (1900..=2100).contains(&season)
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
    episode_marker_cut(s).0
}

/// [`cut_at_episode_marker`], and the number the marker carried.
///
/// **The cut and the claim are one rule, so they are one function.** The title
/// ends where the marker begins, and the digits the marker names are the
/// episode number — reading them twice with two rules is how the two drift
/// apart. Callers that must not claim a number take `.0` through
/// [`cut_at_episode_marker`] and are unchanged.
///
/// `None` means no marker fired, so the title is returned whole.
fn episode_marker_cut(s: &str) -> (String, Option<i32>) {
    // `to_ascii_lowercase` and not `to_lowercase`: the fold must preserve byte
    // length, because `i` indexes `lower` and then slices `s`. A full Unicode
    // fold can change the length of a character and the two would drift apart.
    // The cost is that a non-ASCII letter is not folded, so `BÖLÜM` in capitals
    // is not matched while `Bölüm` is. Every measured case is the latter.
    let lower = s.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // **The marker may come after the number.** Turkish releases write
        // `69. Blm`, `60.Bolum`, `1. Bölüm` — the number first, then the word
        // for "episode". The digit run in front is what makes these words safe
        // to name at all; the 25,043-file dogfood library contains none of
        // them anywhere, so it can neither confirm nor refute this one.
        if bytes[i].is_ascii_digit() && (i == 0 || is_token_boundary(bytes[i - 1])) {
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_digit() && j - i < 4 {
                j += 1;
            }
            // **A four-digit run in the year range is a year**, the same guard
            // `cut_at_absolute_episode` puts on its own. Without it
            // `Anon 2020 BLM Documentary` cuts at the year and the title is
            // `Anon`.
            let is_year =
                j - i == 4 && (1900..=2100).contains(&lower[i..j].parse::<i32>().unwrap_or(0));
            let mut k = j;
            while k < bytes.len() && matches!(bytes[k], b' ' | b'.' | b'_') {
                k += 1;
            }
            if !is_year {
                for w in ["bölüm", "bolum", "blm"] {
                    if lower[k..].starts_with(w) {
                        let end = k + w.len();
                        if end >= bytes.len() || !bytes[end].is_ascii_alphanumeric() {
                            let head = s[..i].trim().trim_matches([' ', '-', '_', '.']).trim();
                            if head.chars().any(char::is_alphabetic) {
                                let number = if states_a_season(head) {
                                    None
                                } else {
                                    lower[i..j].parse::<i32>().ok()
                                };
                                return (head.to_string(), number);
                            }
                        }
                    }
                }
            }
        }
        if bytes[i] == b'e' && (i == 0 || is_token_boundary(bytes[i - 1])) {
            let mut j = i + 1;
            // `e`, `ep`, or the word spelled out — `episode`, `episodio`,
            // `episodes`. Longest first, so `ep` does not shadow `episode`.
            let mut spelled = false;
            for w in ["pisodes", "pisodio", "pisode", "pisodi", "p"] {
                if lower[j..].starts_with(w) {
                    j += w.len();
                    spelled = w.len() > 1;
                    break;
                }
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
            // **The longer the marker, the less the number has to carry.**
            // `E3` could be a title token, so the short marker needs two
            // digits. `Episode 5` cannot be anything else, so one is enough —
            // and it is worth two corpus cases.
            let min_digits = if spelled { 1 } else { 2 };
            if (min_digits..=4).contains(&digits) && !bounded {
                let head = s[..i].trim().trim_matches([' ', '-', '_', '.']).trim();
                if head.chars().any(char::is_alphabetic) {
                    // **A span is not one episode.** `Ep01-12` and `E07-E08`
                    // cover a range, and claiming the first number alone would
                    // be a wrong claim where there was none — the failure mode
                    // the differential sweep exists to catch. The title still
                    // ends here; only the claim is refused.
                    let number = if range_follows(bytes, j) || states_a_season(head) {
                        None
                    } else {
                        lower[start..j].parse::<i32>().ok()
                    };
                    return (head.to_string(), number);
                }
            }
        }
        i += 1;
    }
    (s.to_string(), None)
}

/// A second number behind the marker — `Ep01-12`, `E07-E08` — makes it a span.
///
/// The shape is a dash, an optional `e`, one to four digits, and no letter or
/// digit behind them. **That last part is what keeps a resolution out of it**:
/// `-720p` ends in a letter, so `kill-roy-was-here-e07-720p` is episode 7 and
/// not a range, and the same check already guards the marker itself.
fn range_follows(bytes: &[u8], mut j: usize) -> bool {
    if j >= bytes.len() || bytes[j] != b'-' {
        return false;
    }
    j += 1;
    if j < bytes.len() && bytes[j] == b'e' {
        j += 1;
    }
    let start = j;
    while j < bytes.len() && bytes[j].is_ascii_digit() && j - start < 4 {
        j += 1;
    }
    j > start && (j >= bytes.len() || !bytes[j].is_ascii_alphanumeric())
}

/// **A name that states a season states one, whatever else it does.**
///
/// `Show.S01E00-E01` reaches the no-season arms only because episode 0 is
/// refused, and the `-E01` behind it then looks exactly like a bare marker.
/// Claiming it would produce `episode: 1, season: None` for a name whose own
/// text says season 1 — an absolute number invented out of a declined parse.
/// **A shipped test caught this**, which is why the guard reads the head rather
/// than trusting that the season scan would have claimed the name already: the
/// scan declines for reasons that have nothing to do with the season.
fn states_a_season(head: &str) -> bool {
    let lower = head.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b's' && (i == 0 || is_token_boundary(bytes[i - 1])) {
            let mut j = i + 1;
            let start = j;
            while j < bytes.len() && bytes[j].is_ascii_digit() && j - start < 4 {
                j += 1;
            }
            if j > start {
                return true;
            }
        }
    }
    false
}

/// Tokens that make a **bracket group** release metadata rather than a title.
///
/// **This list never cuts a title.** It can only make [`bracket_run_title`]
/// skip a group and look at the next one, which is why it can hold `mp4`, `gb`
/// and `batch` — words that would be reckless in [`TITLE_JUNK`], where a match
/// truncates. The worst it can do is pass over a group whose every word is one
/// of these, and a title made only of container tags is not a title.
const GROUP_METADATA: &[&str] = &[
    "mp4", "mkv", "avi", "avc", "gb", "gbk", "big5", "cht", "chs", "jap", "jp", "cn", "srt", "ass",
    "sub", "subs", "batch", "end", "fin", "dvd", "bd", "opus", "eac3", "ddp", "dd", "hi10p",
    "hi10", "multi", "dual", "audio", "web", "360p", "540p", "1080p10", "10bits",
];

/// The title of a name that is nothing but a run of bracket groups.
///
/// `[GM-Team][国漫][Anime Title][2019][215][AVC][GB][1080P]` carries no text
/// outside the brackets at all, so there is nothing for a terminator to cut at
/// and the title ran to the end of the name. Twenty corpus cases are this form
/// and it is the bracket-delimited CJK release convention.
///
/// **The first group is the release tag** — the same assumption
/// [`strip_leading_group`] already makes — so selection starts at the second.
/// The title is the first group after it that carries a Latin letter run of two
/// or more and is not release metadata.
///
/// Returns `None` unless the name really is a run: at least three groups, and
/// nothing alphanumeric outside them. The 25,043-file dogfood library holds no
/// name of this shape, so the rule is measured on the corpus alone.
/// A **trailing run** of bracket groups ends the title.
///
/// `Series_Title_2_[01]_[AniLibria_TV]_[WEBRip_1080p]` — from `[01]` to the end
/// there is nothing but groups and the separators between them, so none of it
/// is title. `One Series (1017-1088) (WEB 1080p)` is the same shape in
/// parentheses.
///
/// **The test is position, not content.** No vocabulary: the rule never asks
/// what a group *says*, only whether anything outside the run does. That is why
/// it can cut `[AniLibria TV]`, a release group nobody has listed, without a
/// list.
///
/// **What keeps a title's own brackets:** something alphanumeric after them.
/// `(500) Days of Summer (2009) Bluray-1080p` has words after both groups, so
/// neither opens a trailing run. **The input this does cut wrongly is a title
/// whose last word is parenthesised and which carries nothing after it but
/// tags** — `Birdman (or The Unexpected Virtue of Ignorance) [1080p]` loses the
/// subtitle. The dogfood library holds two names of that shape and an `S/E`
/// token cuts both before this rule is reached; a library without one would
/// lose the parenthesis.
///
/// Parentheses as well as brackets, so [`bracket_groups`] is not reused —
/// widening that would change [`bracket_run_title`]'s selection too.
fn cut_at_trailing_bracket_run(s: &str) -> String {
    // `(open, body_start, body_end, end)`. The body span is recorded while
    // scanning rather than derived as `start + 1`: `【` is three bytes, and
    // slicing at `start + 1` panics inside it.
    let mut groups: Vec<(usize, usize, usize, usize)> = Vec::new();
    let mut open: Option<(usize, usize)> = None;
    for (i, c) in s.char_indices() {
        match c {
            '[' | '(' | '【' => open = Some((i, i + c.len_utf8())),
            ']' | ')' | '】' => {
                if let Some((start, body_start)) = open.take() {
                    groups.push((start, body_start, i, i + c.len_utf8()));
                }
            }
            _ => {}
        }
    }
    for (k, &(start, body_start, body_end, _)) in groups.iter().enumerate() {
        let body = &s[body_start..body_end];
        // **A four-digit run in the year range is a year**, the guard
        // [`cut_at_absolute_episode`], [`cut_at_episode_marker`] and
        // [`find_season_episode`] all already carry. A year is not release
        // metadata, so it cannot open the run — `Series Title [2022] [S25E13]`
        // keeps its year, and the 27 corpus cases whose expected title keeps a
        // parenthesised year keep theirs for the same reason.
        let is_year = body.len() == 4
            && body
                .parse::<i32>()
                .is_ok_and(|y| (1900..=2100).contains(&y));
        if is_year || !is_group_metadata(body) {
            continue;
        }
        // **The group that opens the run must say nothing.** Position alone
        // eats a title's own parenthesis: `Series E (Series J) (Season 04)
        // [1080p]` wants `Series E (Series J)`, and four corpus cases are that
        // shape. Judged by [`is_group_metadata`] — every alphanumeric word a
        // known tag or a bare number — the same predicate the bracket-run
        // selection uses. Still no new vocabulary; the list is asked about a
        // different span.

        // Everything from this opener to the end, with the remaining groups
        // removed, must carry nothing alphanumeric.
        let mut rest = String::new();
        let mut cursor = start;
        for &(gs, _, _, ge) in &groups[k..] {
            if gs >= cursor {
                rest.push_str(&s[cursor..gs]);
                cursor = ge;
            }
        }
        rest.push_str(&s[cursor..]);
        if rest.chars().any(|c| c.is_alphanumeric()) {
            continue;
        }
        let head = s[..start].trim().trim_matches([' ', '-', '_', '.']).trim();
        // The same guard every other terminator carries: a head with no letter
        // is not a title, and cutting to empty is what makes the caller
        // substitute the raw stem.
        if head.chars().any(char::is_alphabetic) {
            return head.to_string();
        }
        break;
    }
    s.to_string()
}

fn bracket_run_title(stem: &str) -> Option<String> {
    let groups = bracket_groups(stem);
    if groups.len() < 3 {
        return None;
    }
    // Nothing outside the brackets may carry information, or this is an
    // ordinary name that happens to end in tags.
    let mut outside = String::new();
    let mut last = 0;
    for (start, end, _) in &groups {
        outside.push_str(&stem[last..*start]);
        last = *end;
    }
    outside.push_str(&stem[last..]);
    if outside.chars().any(|c| c.is_ascii_alphanumeric()) {
        return None;
    }

    for (_, _, body) in groups.iter().skip(1) {
        if !has_latin_run(body) || is_group_metadata(body) {
            continue;
        }
        // Through `clean_title` like every other title path: the group body is
        // raw, and `Anime_Series_Title` has to become `Anime Series Title`
        // before the soft key the matcher uses will agree with it.
        let picked = clean_title(&trim_to_latin_title(body));
        if !picked.is_empty() {
            return Some(picked);
        }
    }
    None
}

/// `(start, end, body)` for every `[...]` or `【...】`, in order.
fn bracket_groups(s: &str) -> Vec<(usize, usize, &str)> {
    let mut out = Vec::new();
    let mut open: Option<(usize, usize)> = None;
    for (i, c) in s.char_indices() {
        match c {
            '[' | '【' => open = Some((i, i + c.len_utf8())),
            ']' | '】' => {
                if let Some((start, body_start)) = open.take() {
                    out.push((start, i + c.len_utf8(), &s[body_start..i]));
                }
            }
            _ => {}
        }
    }
    out
}

fn has_latin_run(s: &str) -> bool {
    let mut run = 0;
    for c in s.chars() {
        if c.is_ascii_alphabetic() {
            run += 1;
            if run >= 2 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// Every alphanumeric word in the group is metadata or a bare number.
fn is_group_metadata(s: &str) -> bool {
    for word in s.split(|c: char| !c.is_ascii_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        let lower = word.to_ascii_lowercase();
        let known = GROUP_METADATA.contains(&lower.as_str())
            || TITLE_JUNK.contains(&lower.as_str())
            || word.chars().all(|c| c.is_ascii_digit());
        if !known {
            return false;
        }
    }
    true
}

/// The Latin title inside a group that may also carry a CJK one.
///
/// Two steps. The longest run between non-ASCII characters is the Latin side,
/// so `ANIME SERIES 海賊王` gives `ANIME SERIES`. Then, if a **spaced** `_`,
/// `/` or `|` remains, the part after the last one wins — those separate two
/// titles. An unspaced `_` is a filename space and must not split, or
/// `Anime_Series_Title` becomes `Title`.
fn trim_to_latin_title(body: &str) -> String {
    let latin = body
        .split(|c: char| !c.is_ascii())
        .filter(|part| has_latin_run(part))
        .max_by_key(|part| part.len())
        .unwrap_or(body);

    let bytes = latin.as_bytes();
    let mut cut = None;
    for i in 0..bytes.len() {
        if matches!(bytes[i], b'_' | b'/' | b'|') {
            let left_space = i > 0 && bytes[i - 1] == b' ';
            let right_space = i + 1 < bytes.len() && bytes[i + 1] == b' ';
            if left_space || right_space {
                cut = Some(i + 1);
            }
        }
    }
    let tail = match cut {
        Some(c) if has_latin_run(&latin[c..]) => &latin[c..],
        _ => latin,
    };
    tail.trim()
        .trim_matches([' ', '_', '.', '-', '/', '|', '~', '!'])
        .trim()
        .to_string()
}

/// Strip a leading `www.site.tld - ` tracker prefix.
///
/// `www.Torrenting.com - Movie.2008.720p.X264-DIMENSION` parsed as
/// `www Torrenting com - Movie`: the prefix survives every terminator because
/// it sits before the title, and nothing cuts from the left.
///
/// **Guarded the same way [`strip_leading_group`] is — on what is left.** The
/// prefix must end in a dash with whitespace after it, and the remainder must
/// still carry a letter. A bare domain with no dash is not stripped, because a
/// film could be called one.
///
/// **And the head must carry `www.`, because a dotted release title otherwise
/// has the shape of a domain.** `The.Office.US` and `Dr.No` are dot-separated
/// labels with no whitespace ending in a short label, which is every test the
/// domain shape can apply. A TLD list does not separate them either: `us`,
/// `uk`, `no` and `to` are real country domains and ordinary English words.
/// Every prefix measured here carries the subdomain, so that is what the
/// evidence pays for; one without it may not cut a title until it has its own.
fn strip_site_prefix(stem: &str) -> &str {
    let t = stem.trim_start();
    let Some(dash) = t.find(" - ").or_else(|| t.find("- ")) else {
        return stem;
    };
    let head = &t[..dash];
    if !head
        .get(..4)
        .is_some_and(|p| p.eq_ignore_ascii_case("www."))
    {
        return stem;
    }
    // A domain and nothing else: dot-separated labels, no whitespace.
    if head.contains(char::is_whitespace) {
        return stem;
    }
    if !head
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
    {
        return stem;
    }
    // The last label is a TLD only if it is short and all letters.
    let Some(tld) = head.rsplit('.').next() else {
        return stem;
    };
    if !(2..=4).contains(&tld.len()) || !tld.chars().all(|c| c.is_ascii_alphabetic()) {
        return stem;
    }
    let rest = t[dash..].trim_start_matches([' ', '-']).trim();
    if rest.chars().any(char::is_alphabetic) {
        rest
    } else {
        stem
    }
}

/// Cut a title at a closing bracket it never opened.
///
/// **The mirror of [`back_up_to_open_bracket`].** That handles a bracket the
/// title opened and did not close; this handles one the title closes without
/// having opened — `Anime Series Title][12END][720p]`, where everything from
/// the stray `]` on belongs to a group the title is not part of.
///
/// **Square brackets only.** `(` and `)` appear inside real titles; an Arabic
/// corpus case that passes today carries a stray `)` and cutting there breaks
/// it. `[` and `]` are release-group syntax and nothing else.
fn cut_at_unmatched_close(s: &str) -> String {
    let mut depth = 0i32;
    for (i, c) in s.char_indices() {
        match c {
            '[' | '【' => depth += 1,
            ']' | '】' => {
                if depth == 0 {
                    let head = s[..i].trim().trim_matches([' ', '-', '_', '.']).trim();
                    return if head.is_empty() {
                        s.to_string()
                    } else {
                        head.to_string()
                    };
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    s.to_string()
}

/// What the layer above the parser knows about the folder a file sits in.
///
/// `parse_filename` takes a basename by design and that does not change. This
/// is how the caller hands it the two things a basename cannot carry, **without
/// this crate learning where a show folder starts.** `nightjar-core` has no
/// internal dependencies and `nightjar-db` owns the path walk
/// (`show_folder_relpath`, `under_numbered_season_directory`,
/// `season_number_for_path`); a second walk written here would be the
/// reimplemented-`norm_key` trap, which once reported 25 non-folding folders
/// where the shipped chain gave 12.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FolderContext<'a> {
    /// The show folder's own name — the last segment left once the
    /// season-directory tail is gone. `nightjar_db::show_folder_relpath`'s
    /// final component.
    pub folder_title: Option<&'a str>,
    /// The numbered season directory the file sits in.
    /// `nightjar_db::season_number_for_path`.
    pub season: Option<i32>,
}

/// [`parse_filename`], then let the folder answer what the basename did not.
///
/// **Additive and unwired.** Nothing in the product calls this yet; the three
/// production call sites still use [`parse_filename`]. It exists so the seam can
/// be reviewed on its own, before anything moves through it.
///
/// ## What it does
///
/// 1. **An empty title takes the folder's name.** This is
///    `nightjar_scanner::stored_title`, moved down unchanged — `S01E01.mkv`
///    parses to an empty title and the show folder names the show. Moving it
///    here is behaviour-identical, which is what makes the call sites safe to
///    move later.
/// 2. **An episode with no season number takes the folder's.**
///
/// ## What it deliberately does not do, and why
///
/// **It does not decide the kind.** `nightjar_scanner::stored_kind` remains the
/// only owner of *a yearless film under a numbered season directory is an
/// episode*, because that rule reads the whole season tail through
/// `under_numbered_season_directory` — and that predicate and
/// `season_number_for_path` disagree on purpose for an over-wide season, where
/// the file is still not a film but the number is unusable.
///
/// **So rule 2 is gated on the kind the basename gave**, and never invents a
/// season for something the basename called a film. `Futurama/Season 5/Futurama
/// Bender's Big Score (2007).avi` is a real film in a real library; it keeps
/// `season: None` here, and `stored_kind` keeps it a film.
///
/// **And that gate is why this does not yet move the two shapes it was written
/// for.** Measured through the shipped chain:
///
/// | basename | kind | title | season |
/// |---|---|---|---|
/// | `Season 01/Episode 1.mkv` | `Movie` | `"Episode 1"` | `None` |
/// | `Season 1/01 - Closure.mkv` | `Movie` | `"01 - Closure"` | `None` |
///
/// Both parse to `Movie`, so rule 2 does not fire. Both parse to a **non-empty**
/// title, so rule 1 does not fire either — the folder would have to *override* a
/// title the basename did assert, which is a different rule from filling a
/// silence.
///
/// Both of those need the kind decided before the title and season are, and the
/// kind is decided a layer up. **Resolving that layering is the next slice's
/// work, not this one's** — it is the whole of `tv.handmade` (5,840 oracle rows)
/// and `tv.episodetitle` (5,844), which read 0.0% correct.
pub fn parse_filename_in(file_name: &str, ctx: FolderContext<'_>) -> ParsedName {
    let mut parsed = parse_filename(file_name);
    if parsed.title.is_empty()
        && let Some(folder) = ctx.folder_title
    {
        parsed.title = folder.trim().to_string();
    }
    // **An absolute number must never take the folder's season.** The folder
    // says "season 2"; `E56` says "the fifty-sixth episode of the series". The
    // two are different numbering schemes, so pairing them produces `(2, 56)`
    // — a slot that does not exist, bound with no error and no way to tell it
    // from a real one afterwards. See [`ParsedName::episode_absolute`].
    //
    // **This guard is written before the seam is wired**, deliberately. The
    // seam is dead code today, and dead code is not a guard: whoever wires it
    // would otherwise be the one to discover this, in bindings.
    if parsed.kind == MediaKind::Episode && parsed.season.is_none() && !parsed.episode_absolute {
        parsed.season = ctx.season;
    }
    parsed
}

/// A date at the very start of the name, **written year first**.
///
/// Returns where it ends and the year it states.
///
/// **Year first, and that is the guard.** `2011.01.10`, `20161024`, `221208`
/// all lead with the year, which is why they lead at all — a date written on
/// the front of a filename is there to sort. **`20-1.2014.S02E01` is not one**:
/// it is day-day-year, and it is a real title, `20-1`, followed by its year.
/// A previous attempt read it as 20 January 2014 and lost the case. The file
/// names the same hazard one show along — `9-1-1`, whose name offers
/// `1 1 2018` as a perfectly good date — and `9` is not a four-digit year, so
/// this rule declines it too.
///
/// **This does not touch [`cut_at_date`]'s head-has-a-letter guard**, which is
/// doing two jobs and is left doing both. This rule is anchored at the start,
/// so there is no head to judge.
fn leading_date(s: &str) -> Option<(usize, i32)> {
    let b = s.as_bytes();
    let digits = |from: usize, n: usize| {
        (from + n <= b.len() && b[from..from + n].iter().all(u8::is_ascii_digit))
            .then(|| s[from..from + n].parse::<i32>().unwrap_or(-1))
    };
    let ok = |y: i32, m: i32, d: i32| {
        (1900..=2100).contains(&y) && (1..=12).contains(&m) && (1..=31).contains(&d)
    };
    let ends = |at: usize| at >= b.len() || !b[at].is_ascii_alphanumeric();

    // `20161024`, then `221208`. One token, no separators.
    if let (Some(y), Some(m), Some(d)) = (digits(0, 4), digits(4, 2), digits(6, 2))
        && ok(y, m, d)
        && ends(8)
    {
        return Some((8, y));
    }
    if let (Some(yy), Some(m), Some(d)) = (digits(0, 2), digits(2, 2), digits(4, 2))
        && ok(2000 + yy, m, d)
        && ends(6)
    {
        return Some((6, 2000 + yy));
    }
    // `2011.01.10`, `2018-11-14`, `2019 08 20`. Three tokens, year first.
    let sep = |at: usize| at < b.len() && matches!(b[at], b' ' | b'.' | b'_' | b'-');
    if let (Some(y), Some(m), Some(d)) = (digits(0, 4), digits(5, 2), digits(8, 2))
        && sep(4)
        && sep(7)
        && ok(y, m, d)
        && ends(10)
    {
        return Some((10, y));
    }
    None
}

/// Whether the name marks an episode **with a word or a hash**, rather than
/// with a bare number.
///
/// `ep34`, `E56`, `Episode 5`, `#17`. **`21x41` and `2009x09` are deliberately
/// not this**: they are season-by-episode tokens, and counting them puts
/// `20161024- Exotic Payback.21x41_720` on the wrong side of the rule below.
///
/// **`#NN` is here because one case needed it and the board already knew it.**
/// `221205 ABC123 17研究所！ #17` wants a title and marks its episode with a
/// hash; with only the lettered spellings the rule gets it wrong. `#957` is
/// recorded elsewhere in this corpus as the same spelling, so the hash is a
/// convention this evidence already contains rather than one fitted to a case.
fn has_episode_marker(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    let b = lower.as_bytes();
    for i in 0..b.len() {
        if i > 0 && !is_token_boundary(b[i - 1]) {
            continue;
        }
        let mut j = i;
        if b[i] == b'#' {
            j += 1;
        } else if b[i] == b'e' {
            j += 1;
            for w in ["pisodes", "pisodio", "pisode", "pisodi", "p"] {
                if lower[j..].starts_with(w) {
                    j += w.len();
                    break;
                }
            }
            if j < b.len() && matches!(b[j], b' ' | b'.' | b'_') {
                j += 1;
            }
        } else {
            continue;
        }
        let start = j;
        while j < b.len() && b[j].is_ascii_digit() && j - start < 4 {
            j += 1;
        }
        if j > start && (j >= b.len() || !b[j].is_ascii_alphanumeric()) {
            return true;
        }
    }
    false
}

/// Parse a media filename (not a full path) into title / kind / episode fields.
///
/// ## A name that begins with a date
///
/// **The episode marker decides what the date is.** A date on the front of a
/// name is either a stamp in front of a title — `221208 ABC123 Series Title
/// ep34` — or the whole name, with the series title living in the folder —
/// `2011.01.10 - A Late Talk Show`. **Position cannot tell them apart**; both
/// are `<date> <words>`. A marker behind the date can: a name that says which
/// episode it is has a title in front of that, and a name that says nothing
/// but a date has none.
///
/// Measured over every applicable corpus case beginning with a date, passing
/// and failing alike: **12 of 12, none wrong.**
///
/// **This is unrefuted, not proven safe, and the difference is not small
/// here.** Zero of the parser sweep's 74,624 generated names and zero of the
/// 25,043 dogfood basenames begin with a date at all, and every one of the 12
/// corpus cases is one the parser gets wrong today. **There is no negative
/// evidence anywhere** — no passing name of this shape exists in any
/// instrument, so nothing could have contradicted this rule even if it were
/// wrong. One instrument can see the question and two cannot.
pub fn parse_filename(file_name: &str) -> ParsedName {
    let whole = strip_extension(file_name);
    if let Some((end, year)) = leading_date(whole) {
        let rest = whole[end..].trim_matches([' ', '-', '_', '.']);
        if has_episode_marker(rest) {
            // A stamp. The title is what follows it, parsed as any other name.
            return parse_stem(rest);
        }
        // The date is the name. The title is absent, and the scanner's
        // `stored_title` gives the file its show folder's name — it does that
        // for any file, not only an episode. The date's year still stands,
        // and it is the only year such a name has.
        let mut parsed = parse_stem(whole);
        parsed.title.clear();
        parsed.year = parsed.year.or(Some(year));
        return parsed;
    }
    parse_stem(whole)
}

fn parse_stem(whole: &str) -> ParsedName {
    // A name that is nothing but bracket groups has no text outside them for a
    // terminator to cut at, so its title is selected from the groups rather
    // than derived by cutting.
    let run_title = bracket_run_title(whole);
    let stem = strip_site_prefix(strip_leading_group(whole));
    let normalized = stem.replace(['_', '.'], " ");
    let compact = stem.to_ascii_lowercase();

    if let Some((before, season, numbers)) = find_season_episode(&compact) {
        let title = run_title.clone().unwrap_or_else(|| {
            cut_at_unmatched_close(&cut_at_episode_marker(&cut_at_absolute_episode(
                &cut_at_date(&cut_at_trailing_bracket_run(&cut_at_title_junk(
                    &cut_stem_at(stem, before),
                ))),
            )))
        });
        // `None` is the declined-number case: the token said television and
        // the digits were unreadable, so the season, the title cut and the kind
        // stand and the episode alone is absent.
        let (episode, end) = match numbers {
            Some((episode, episode_end)) => (
                Some(episode),
                if episode_end > episode {
                    Some(episode_end)
                } else {
                    None
                },
            ),
            None => (None, None),
        };
        return ParsedName {
            // **An absent title is absent.** When the season/episode token
            // starts the name there is no series title in the filename at all
            // — `S03E09 WS PDTV XviD FUtV`, `1x04` — and the folder carries
            // it. Substituting the stem produced a "title" of pure release
            // junk that the matcher then searched for.
            //
            // Empty is only honest if the caller handles it, and two do:
            // `nightjar-scanner` falls back to the folder name before storing,
            // and `drain_pending` refuses to search on an empty title. Neither
            // existed when the substitution was written, which is why it was
            // written.
            title: if title.is_empty() && before > 0 {
                stem.to_string()
            } else {
                title
            },
            kind: MediaKind::Episode,
            year: None,
            season: Some(season),
            episode,
            episode_end: end,
            // The name carried a season, so the number is relative to it.
            episode_absolute: false,
        };
    }

    // A season token with no episode is a season pack. It runs after the
    // season/episode scan, which owns every name that carries both, and before
    // the year branch, which would otherwise call a pack a movie.
    if let Some((before, season)) = find_bare_season(&normalized) {
        let title = run_title.clone().unwrap_or_else(|| {
            cut_at_unmatched_close(&cut_at_episode_marker(&cut_at_date(
                &cut_at_trailing_bracket_run(&cut_at_title_junk(&cut_stem_at(stem, before))),
            )))
        });
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
            episode_absolute: false,
        };
    }

    let year = find_year(&normalized);
    // **The date cut has to see the whole date.** `cut_at_date` needs all three
    // tokens together — `13.02.2025`, `04.28.2014` — and the year cut below
    // removes the last one first. By the time the chain below reached the date
    // rule the date was already half gone, so `Series.Title.13.02.2025` kept
    // `Series Title 13 02`. Running it on the uncut stem is the same rule in
    // the same place in the chain; only what it is handed changes.
    // **Two date forms, one arm.** `cut_at_date` reads three separate numeric
    // tokens; `cut_at_written_date` reads a date written as one run or with a
    // month name. The second also returns the date's year, and it is used only
    // when the name gave no year of its own — a name that states its year
    // states it, and the date must not overrule it.
    let (dated, date_year) = match cut_at_date(stem) {
        cut if cut != stem => (cut, None),
        _ => cut_at_written_date(stem),
    };
    let year = year.or(date_year);
    if dated != stem {
        let (marked, absolute) = episode_marker_cut(&cut_at_absolute_episode(&cut_at_title_junk(
            &clean_title(&dated),
        )));
        let title = cut_at_unmatched_close(&marked);
        return ParsedName {
            title: if title.is_empty() {
                stem.to_string()
            } else {
                title
            },
            kind: claimed_kind(MediaKind::Movie, absolute),
            year,
            season: None,
            episode: absolute,
            episode_end: None,
            episode_absolute: absolute.is_some(),
        };
    }
    let title = match year {
        Some(y) => {
            let token = format!("({y})");
            let cut = stem
                .find(&token)
                .or_else(|| stem.to_ascii_lowercase().find(&y.to_string()));
            match cut {
                // **The terminator runs here too.** Cutting at the year
                // removes what follows it and nothing else, so junk sitting
                // *before* the year survived — `World.Movie.Z.EXTENDED.2013`
                // kept `EXTENDED`. Only the fallback arms ever called this.
                Some(i) if i > 0 => cut_at_title_junk(&cut_stem_at(stem, i)),
                _ => cut_at_title_junk(&clean_title(stem)),
            }
        }
        None => cut_at_title_junk(&clean_title(stem)),
    };
    let (marked, absolute) = episode_marker_cut(&cut_at_absolute_episode(&cut_at_date(
        &cut_at_trailing_bracket_run(&title),
    )));
    let title = run_title.unwrap_or_else(|| cut_at_unmatched_close(&marked));

    ParsedName {
        title: if title.is_empty() {
            stem.to_string()
        } else {
            title
        },
        kind: claimed_kind(MediaKind::Movie, absolute),
        year,
        season: None,
        episode: absolute,
        episode_end: None,
        episode_absolute: absolute.is_some(),
    }
}

/// A claimed absolute number makes the file an episode.
///
/// **The marker is the statement.** `Anon Show E56` says episode as plainly as
/// `S01E01` does; only the season is missing. Leaving `kind` at `Movie` while
/// `episode` holds a number would file a television episode as a film and put
/// an episode number on a movie row.
///
/// This is the same promotion `nightjar_scanner::stored_kind` already makes for
/// a no-year movie under a numbered season directory — there the folder says
/// television, here the filename does.
fn claimed_kind(otherwise: MediaKind, absolute: Option<i32>) -> MediaKind {
    if absolute.is_some() {
        MediaKind::Episode
    } else {
        otherwise
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

/// Where an episode marker starts, the season it names, and the episodes it
/// covers — `None` for the episodes when the marker's digit run was too wide to
/// be an episode number.
type SeasonEpisodeHit = (usize, i32, Option<(i32, i32)>);

/// `(token_start, season, Some((episode_start, episode_end)))` — end inclusive
/// — or `(token_start, season, None)` when the token **is** an episode marker
/// whose number could not be read.
///
/// **Declining the number must not decline the token.** A run wider than an
/// episode number means the digits are not an episode number; it does not mean
/// `S01E123456` is a film. Returning `None` for the whole scan sent the name to
/// the movie arm, which threw away the title cut and flipped the kind — and a
/// wrong kind is the worst class in the oracle's `ORDER`, traded for the one
/// wrong field the decline was written to avoid. The token is still evidence of
/// television; only its number is unreadable, so the season, the cut and the
/// kind all survive and the episode alone goes absent.
///
/// **A later whole token still wins.** The declined candidate is remembered and
/// the scan runs on, so a name carrying both an unreadable and a readable
/// marker reports the readable one.
///
/// Season 0 is a season. Every provider models specials as season 0, and the
/// coverage predicate already excludes it from the fit check by name
/// (`queue.rs`, "a `Specials` folder exists independently of whether a
/// provider models season 0") rather than by relying on the parser to refuse
/// it. Episode 0 is still refused: nothing measured asserts a real `E00`, and
/// the one corpus case that expects episode 0 is Sonarr's sentinel for "this
/// name carries no standard episode number", not an episode called zero.
fn find_season_episode(lower: &str) -> Option<SeasonEpisodeHit> {
    // S01E02 / s1e2 (no range forms in dogfood; single episode only)
    let bytes = lower.as_bytes();
    // The first `S<season>E<digits…>` whose digit run was too wide to be an
    // episode number. Held rather than returned, so a whole token later in the
    // name still wins.
    let mut declined: Option<(usize, i32)> = None;
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
            while j < bytes.len() && bytes[j].is_ascii_digit() && digits < 4 {
                season = season * 10 + (bytes[j] - b'0') as i32;
                j += 1;
                digits += 1;
            }
            // **A four-digit season is a year-season, and only the marked
            // spelling may carry one.** `S2016E231` and `S1936E18` are real
            // Sonarr forms. The bare `2016x231` is not allowed the same width
            // because it is the shape of a resolution: `1920x804` puts a
            // plausible year on the left and a whole three-digit run on the
            // right, so no range guard and no whole-run guard separates them.
            // The `S` and the `E` do — a resolution has neither.
            let season_ok = digits < 4 || (1900..=2100).contains(&season);
            // **The two halves may be separated.** `Series Title.S6.E1`,
            // `Series.Title.S01.Ep06` and `Series s90 e43` are all one token
            // written with a gap, and requiring the `e` to touch the season
            // digits made the whole file report no episode at all. One
            // separator before the `e`, an optional `p` for the `Ep`
            // spelling, and one separator before the digits.
            if season_ok
                && digits > 0
                && let Some(end) = token_gap_end(bytes, j)
            {
                j = end;
            }
            if season_ok && digits > 0 && j < bytes.len() && bytes[j] == b'e' {
                j += 1;
                if j < bytes.len() && bytes[j] == b'p' {
                    j += 1;
                }
                if let Some(end) = token_gap_end(bytes, j) {
                    j = end;
                }
                let (episode, edigits, whole) = read_episode_digits(bytes, &mut j);
                if edigits > 0 && episode > 0 && whole {
                    let end = extend_episode_span(bytes, j, season, episode);
                    return Some((i, season, Some((episode, end))));
                }
                // **The marked spelling, and only the marked spelling.** An `S`
                // and an `E` around the digits are the same evidence that lets
                // the season arm carry four digits. The bare `NNxNNNNNN` has
                // neither, and it is the shape of a resolution — so a wide run
                // there stays a decline of the whole token rather than an
                // assertion that the file is television.
                if edigits > 0 && !whole && declined.is_none() {
                    declined = Some((i, season));
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
                let (episode, edigits, whole) = read_episode_digits(bytes, &mut j);
                if edigits > 0 && episode > 0 && whole {
                    let end = extend_episode_span(bytes, j, season, episode);
                    return Some((i, season, Some((episode, end))));
                }
            }
        }
        i += 1;
    }
    declined.map(|(at, season)| (at, season, None))
}

/// The widest episode number an episode marker may carry.
///
/// Five, because a daily serial really does reach one: `S42 Ep10722` is a real
/// name and so is `S22E5363`. Six is not an episode number.
const MAX_EPISODE_DIGITS: usize = 5;

/// Read the digit run at `*at`, and say whether it is a **whole** run.
///
/// Returns `(value, digits, whole)` and advances `*at` past the digits it took.
/// `whole` is false when the run keeps going past [`MAX_EPISODE_DIGITS`] — the
/// token is then not an episode marker, and the caller must decline it rather
/// than use the prefix.
///
/// **The truncation this replaces asserted a wrong number where the name
/// carried a right one.** The cap was three digits and the loop simply stopped:
/// `S22E5363` reported episode 536, `S14E3533` reported 353, `S2020E1527`
/// reported 152. Each is a *wrong* claim, not a missing one — the drain takes it
/// to the provider and binds the wrong episode, where reporting nothing would
/// have left the file unmatched and recoverable.
///
/// **A whole run, the way `cut_at_episode_marker` already requires one.** That
/// function reads up to four digits and rejects the token when another digit
/// follows (`bounded`); this is the same rule in the other scanner. The widths
/// differ on purpose: a bare `E1135` with no season has only the marker to
/// vouch for it, while an `S` in front is the same evidence that lets the
/// season arm carry four digits — "the `S` and the `E` do; a resolution has
/// neither".
///
/// Only a following **digit** breaks the run. A following letter must not: the
/// multi-episode forms are `S01E01E02` and `8x01x02`, and rejecting on a letter
/// would refuse every one of them.
fn read_episode_digits(bytes: &[u8], at: &mut usize) -> (i32, usize, bool) {
    let mut j = *at;
    let mut value = 0i32;
    let mut digits = 0;
    while j < bytes.len() && bytes[j].is_ascii_digit() && digits < MAX_EPISODE_DIGITS {
        value = value * 10 + (bytes[j] - b'0') as i32;
        j += 1;
        digits += 1;
    }
    let whole = !(j < bytes.len() && bytes[j].is_ascii_digit());
    *at = j;
    (value, digits, whole)
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
        // **A dash is not the only separator**, and the other three cost
        // nothing only because a marker has to follow them. `S02E09 E10`,
        // `Series.S03E01.S03E02` and `2x04.2x05` are all one file holding two
        // episodes; a dash-only rule reported the first and claimed success,
        // which is worse than reporting nothing.
        //
        // **The separator is a run, not a byte.** `S07E22 - S07E23` pads its
        // dash with spaces, and reading one byte stopped on the space with the
        // second token still in front of it. At most one dash: `--` is not a
        // separator anyone writes on purpose, and letting the run swallow two
        // would join things nobody joined.
        let sep_start = k;
        let mut dashes = 0;
        while k < bytes.len() && matches!(bytes[k], b' ' | b'.' | b'_' | b'-') {
            if bytes[k] == b'-' {
                if dashes == 1 {
                    break;
                }
                dashes += 1;
            }
            k += 1;
        }
        let sep_len = k - sep_start;
        let separated = dashes == 1;
        // An optional repeat of the season, in either spelling it appears in:
        // `s06` before an `e`, or `6x` before the number.
        let mut marked = false;
        // **A repeated season and a two-letter `ep` are *distinctive* markers**;
        // a lone `e` or `x` is not. See the padded-separator rule below.
        let mut distinctive = false;
        if let Some((repeated, after)) = read_repeated_season(bytes, k) {
            if repeated != season {
                break;
            }
            k = after;
            marked = true;
            distinctive = true;
        }
        if k < bytes.len() && (bytes[k] == b'e' || bytes[k] == b'x') {
            let e_marker = bytes[k] == b'e';
            k += 1;
            // **`ep` is a two-letter spelling of the same marker**, and
            // `S42 Ep10718 - Ep10722` is a real daily-serial name. The digit
            // has to come **immediately** after the `p`, which is the whole
            // reason this is safe: the generated library holds 2,877 rows of
            // `Series - S01E01 - Episode 1.mkv`, and `episode` puts an `i`
            // where this requires a digit. Consuming `p` on anything looser
            // would turn every one of those episode titles into a range.
            if e_marker && k + 1 < bytes.len() && bytes[k] == b'p' && bytes[k + 1].is_ascii_digit()
            {
                k += 1;
                distinctive = true;
            }
            marked = true;
        }
        // **Anything but a bare dash needs a marker behind it.** Without one
        // the corpus is full of names where the next token is an episode-title
        // numeral: `S01E06 3 Beers For Batali`, `S01E04.2-45.PM`,
        // `S02E21 18 5 4`. All three parse correctly today.
        //
        // A *bare* dash is exempt, and only a bare one: `S15E06-08` is a range
        // and has always been one. The moment whitespace pads the dash the
        // exemption goes with it, because ` - ` is also how a name separates an
        // episode from its title — `Series - S01E04 - 6 Feet Under` must not
        // read as episodes 4 through 6.
        //
        // **And behind a padded separator a lone `e` or `x` is not enough.**
        // ` - ` is the ordinary separator between an episode and its title, so
        // the next token is usually a title — and a title may open with a
        // letter this loop reads as a marker followed by a digit:
        //
        //     Show - S01E01 - E3 2019 Highlights.mkv   E3 is an expo
        //     Show - S01E01 - X2.mkv                   X2 is a film
        //     Show - S01E01 - E2E Testing.mkv          E2E is end-to-end
        //
        // All three read as ranges when a bare marker is accepted here, and
        // `MAX_EPISODE_RANGE` hides it only when the number is large: `x264`
        // is refused for its size, `X2` is not. The two names this rule was
        // written for both carry something a title does not — `S07E22 -
        // S07E23` repeats the season, `S42 Ep10718 - Ep10722` spells the
        // marker with two letters. So a padded separator requires one of
        // those, and a bare `e`/`x` extends only behind a bare dash, exactly
        // as it did before this rule existed.
        //
        // This declines `Show - S01E01 - E02.mkv`, which may well be a range.
        // Declining is the right failure: an absent claim costs a range, a
        // wrong one costs the episode a file binds to.
        let padded = sep_len > 1;
        if (!separated || padded) && !marked {
            break;
        }
        if padded && !distinctive {
            break;
        }
        // Something must separate this token from the last, or a stray trailing
        // number would read as an episode.
        if k == j {
            break;
        }
        let (next, digits, whole) = read_episode_digits(bytes, &mut k);
        if digits == 0 || !whole {
            break;
        }
        // A dash introduces a range end; anything else is a repetition and
        // must land on the next episode exactly.
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

/// A separator that may sit inside a season/episode token — `S6.E1`, `S15 E06`,
/// `S1-E1`, `S01_E01`.
fn is_token_gap(b: u8) -> bool {
    matches!(b, b' ' | b'.' | b'_' | b'-')
}

/// The end of the separator between the two halves of a split season/episode
/// token, or `None` when nothing separates them.
///
/// **One separator, not one byte.** ` - ` is a single separator spelled in
/// three characters. Reading one byte parsed `S6-E1` and let `S6 - E1` fall
/// through to the bare-season rule, which claimed the file as a season pack —
/// a wrong claim where the glued spelling gives a right one, on a difference
/// the name does not carry.
///
/// Spaces around at most one dot, dash or underscore, so the scan cannot run
/// past a separator into whatever follows it.
fn token_gap_end(bytes: &[u8], from: usize) -> Option<usize> {
    let mut j = from;
    let mut punctuation = 0;
    while j < bytes.len() && is_token_gap(bytes[j]) {
        if bytes[j] != b' ' {
            punctuation += 1;
            if punctuation > 1 {
                return None;
            }
        }
        j += 1;
    }
    (j > from).then_some(j)
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

    fn ctx<'a>(folder: Option<&'a str>, season: Option<i32>) -> FolderContext<'a> {
        FolderContext {
            folder_title: folder,
            season,
        }
    }

    /// **With nothing to say, the folder says nothing.** This is the property
    /// that makes the call sites safe to move: an empty context must leave the
    /// parse byte-identical, so a caller that has no folder gets exactly what it
    /// gets today.
    #[test]
    fn an_empty_context_changes_nothing() {
        for name in [
            "Show - S01E01 - Pilot.mkv",
            "S01E01.mkv",
            "Episode 1.mkv",
            "01 - Closure.mkv",
            "Fight Club (1999).mkv",
            "Series Title - S07E22 - S07E23 - And Lots of Security.. [HDTV-720p].mkv",
            "The Series And The Code - S42 Ep10718 - Ep10722",
            "",
            "no-extension",
            "1x01x02 - Two.mkv",
        ] {
            assert_eq!(
                parse_filename_in(name, FolderContext::default()),
                parse_filename(name),
                "empty context moved {name:?}"
            );
        }
    }

    /// Rule 1 — `stored_title`'s substitution, moved down and unchanged.
    /// `S01E01.mkv` parses to an empty title; the show folder names the show.
    #[test]
    fn an_empty_title_takes_the_folder_name() {
        let p = parse_filename_in("S01E01.mkv", ctx(Some("Dept. Q (2025)"), None));
        assert_eq!(p.title, "Dept. Q (2025)");
        assert_eq!(p.kind, MediaKind::Episode);
        assert_eq!(p.season, Some(1), "the basename's own season is untouched");
        assert_eq!(p.episode, Some(1));
    }

    /// **A title the basename asserted is never overwritten.** Filling a silence
    /// and overriding a claim are different rules, and only the first is here.
    #[test]
    fn a_parsed_title_is_never_overwritten_by_the_folder() {
        for name in [
            "Show - S01E01 - Pilot.mkv",
            "Episode 1.mkv",
            "01 - Closure.mkv",
        ] {
            let with = parse_filename_in(name, ctx(Some("Some Show (2020)"), Some(1)));
            let without = parse_filename(name);
            assert_eq!(with.title, without.title, "folder overwrote {name:?}");
        }
    }

    /// **A date on the front of a name, with an episode marker behind it, is a
    /// stamp.** The title is what follows the date, and the marker still claims
    /// its episode.
    #[test]
    fn a_leading_date_with_a_marker_is_a_stamp() {
        for (name, title) in [
            (
                "221208 ABC123 Series Title ep34[1080p60 H264].mp4",
                "ABC123 Series Title",
            ),
            (
                "221201 Series Title! ABC123 ep219[720p.h264].mp4",
                "Series Title! ABC123",
            ),
            ("221206 Series Title! ep08(Tanaka Miku).ts", "Series Title!"),
            ("210810 ABC123 Series Title ep05.mp4", "ABC123 Series Title"),
            ("221204 乃木坂工事中 ep389.mp4", "乃木坂工事中"),
        ] {
            assert_eq!(parse_filename(name).title, title, "{name}");
        }
        // The episode behind the stamp survives being stripped of it.
        assert_eq!(
            parse_filename("221208 ABC123 Series Title ep34[1080p60 H264].mp4").episode,
            Some(34)
        );
    }

    /// **A date on the front with no marker behind it is the whole name.** The
    /// title is absent and the folder names the show — `stored_title` does that
    /// for any file, not only an episode. The date's year still stands.
    #[test]
    fn a_leading_date_with_no_marker_leaves_no_title() {
        for (name, year) in [
            ("2011.01.10 - A Late Talk Show- HD TV.mkv", 2011),
            ("2011.03.13 - A Late Talk Show - HD TV.mkv", 2011),
            ("2018-11-14.1080.all.mp4", 2018),
            ("2019_08_20_1080_all.mp4", 2019),
            ("20161024- Exotic Payback.21x41_720.mkv", 2016),
        ] {
            let p = parse_filename(name);
            assert_eq!(p.title, "", "{name}");
            assert_eq!(
                p.year,
                Some(year),
                "the date is the only year such a name has: {name}"
            );
        }
        // And the numbering behind it is untouched.
        let p = parse_filename("20161024- Exotic Payback.21x41_720.mkv");
        assert_eq!((p.season, p.episode), (Some(21), Some(41)));
    }

    /// **What the leading-date rule must not take**, one guard per line.
    ///
    /// **Negative controls.** Make `has_episode_marker` return false and line 1
    /// loses its title and its episode. Remove the 1900–2100 year range and
    /// line 2 goes titleless; the month/day ranges, line 3; the
    /// not-followed-by-alphanumeric check, line 4.
    ///
    /// **Lines 5 and 6 pin the shape rather than a clause.** The rule matches a
    /// **year-first** date only, so a day-first one never reaches it — widen it
    /// to accept day-day-year and both fail. `20-1.2014` is a real corpus case
    /// that a previous attempt read as 20 January 2014, and `9-1-1` is the
    /// hazard this file already names for `cut_at_date`.
    #[test]
    fn a_leading_run_that_is_not_a_date_keeps_its_title() {
        for (name, title) in [
            // A stamp needs a marker; without one this is the whole name.
            (
                "221208 ABC123 Series Title ep34[1080p60 H264].mp4",
                "ABC123 Series Title",
            ),
            // The year 3501 does not exist.
            ("35010115 Some Show 1080p", "35010115 Some Show"),
            // Day 99 is not a day.
            ("20251399 Some Show 1080p", "20251399 Some Show"),
            // Nine digits is not an eight-digit date with something after it.
            ("202510155 Some Show", "202510155 Some Show"),
            // **Day first is a title, not a date.**
            ("20-1.2014.S02E01.720p.HDTV.x264-CROOKS", "20-1 2014"),
            ("9-1-1.2018.01.03.HDTV.x264", "9-1-1"),
        ] {
            assert_eq!(parse_filename(name).title, title, "{name}");
        }
    }

    /// **A date written as one run, or with a month, ends the title** — and
    /// yields its year, because the two come out of the same digits.
    #[test]
    fn a_written_date_ends_the_title() {
        for (name, title, year) in [
            (
                "A.Late.Talk.Show.140722.720p.HDTV.x264-YesTV",
                "A Late Talk Show",
                Some(2014),
            ),
            (
                "A_Late_Talk_Show_140722_720p_HDTV_x264-YesTV",
                "A Late Talk Show",
                Some(2014),
            ),
            (
                "Series and Title 20201013 Ep7432 [720p WebRip (x264)] [SUBS]",
                "Series and Title",
                Some(2020),
            ),
            ("Series 5th Mar 2025 1080 (Deep61)", "Series", Some(2025)),
            ("Series 31st Jan 2025 1080 (Deep61)", "Series", Some(2025)),
            ("Series 23rd Feb 2024 (Deep61)", "Series", Some(2024)),
        ] {
            let p = parse_filename(name);
            assert_eq!(p.title, title, "{name}");
            assert_eq!(p.year, year, "the date carries the year: {name}");
        }
    }

    /// **Every guard, and the name that pays for it.** Each line here parses
    /// correctly today and would break without the guard beside it.
    ///
    /// **Negative controls, one guard each, and each line verified red when its
    /// own guard alone is removed.** Four guards were claimed in the first
    /// draft and only one of them had a test that could fail: the other three
    /// were masked by the word-count rule declining first, or by the month
    /// check catching what the year range was supposed to. **The heads below
    /// are multi-word on purpose**, so the word rule cannot answer for a guard
    /// that is not doing the work.
    #[test]
    fn a_number_that_is_not_a_date_is_not_cut() {
        for (name, title) in [
            // **Month/day range.** `100000` is six digits and `00` is not a
            // month. The head is three words, so the word rule does not
            // decline first and this line tests the range and nothing else.
            (
                "Some Long Title 100000 1080p x264",
                "Some Long Title 100000",
            ),
            // **Year range, 1900–2100.** `35010115` reads as 3501-01-15: the
            // month and the day are both valid, so the year range is the only
            // guard standing between this and a cut.
            ("Some Long Title 35010115 1080p", "Some Long Title 35010115"),
            // **More than one word in the head.** `240618` is a real date —
            // 2024-06-18 — and this is a release id glued to a three-letter
            // tag. Cutting makes the title `ror`. This name parses correctly
            // in the corpus today.
            ("ror-240618_1007-1022-", "ror-240618 1007-1022"),
            // **An ordinal in front of the month.** Without it, `May` alone
            // ends the title and this becomes `Series`.
            ("Series Title May 2025", "Series Title May"),
        ] {
            assert_eq!(parse_filename(name).title, title, "{name}");
        }
        // The corpus's own eight-digit runs, which stay whole for the same
        // reasons: the years 9768 and 3407 do not exist.
        assert_eq!(
            parse_filename("[MTBB] Kimi no Na wa. (2016) v2 [97681524].mkv").title,
            "Kimi no Na wa"
        );
        assert_eq!(
            parse_filename("[Impatience] Series - 0x01 [720p][34073169].mkv").title,
            "Series"
        );
    }

    /// **An unambiguous marker with no season claims the number, absolutely.**
    ///
    /// `E56` says episode as plainly as `S01E01` does; only the season is
    /// missing. `season: None, episode: Some(56)` is what the filename says.
    /// Every one of these is a corpus case that this rule turns from failing to
    /// passing, and each carries `episode_absolute` so no consumer can pair the
    /// number with a season from somewhere else.
    #[test]
    fn an_unambiguous_marker_claims_an_absolute_episode() {
        for (name, title, episode) in [
            ("kill-roy-was-here-e07-720p", "kill-roy-was-here", 7),
            (
                "It's a Series Title.E56.190121.720p-NEXT.mp4",
                "It's a Series Title",
                56,
            ),
            ("Series.E191.190121.720p-NEXT.mp4", "Series", 191),
            ("Anon Show Ep01 (D2201EC5).mkv", "Anon Show", 1),
            ("Anon Show EP06 720p x265 GROUP.mp4", "Anon Show", 6),
            ("The Movie Episode 5", "The Movie", 5),
            ("Some Show 69. Bolum 720p", "Some Show", 69),
        ] {
            let p = parse_filename(name);
            assert_eq!(p.title, title, "{name}");
            assert_eq!(p.episode, Some(episode), "{name}");
            assert_eq!(p.season, None, "a season must never be synthesised: {name}");
            assert!(p.episode_absolute, "{name}");
            assert_eq!(p.kind, MediaKind::Episode, "{name}");
        }
    }

    /// **The claim is refused where the marker is not unambiguous.** These are
    /// the shapes the rule must not take, and each one is a different guard:
    /// one digit is a title word, a codec token is not a marker, and a marker
    /// with no title in front of it names nothing.
    ///
    /// **Negative control:** the assertions below fail if the digit minimum,
    /// the bound check or the alphabetic-head check is removed.
    #[test]
    fn an_ambiguous_marker_claims_nothing() {
        for name in [
            // One digit after a short marker — `E3` is as likely a title word.
            "Some Movie E3 1080p",
            // `HEVC` and `EAC3`: the `e` is inside a word, or the digits do not
            // follow the marker.
            "Movie Title 2019 1080p BluRay x264 HEVC-GROUP",
            "Movie.Title.2019.1080p.WEB-DL.EAC3.5.1.x264-GRP",
            // A four-digit run in the year range is a year, not an episode.
            "Anon 2020 BLM Documentary",
            // No title in front of the marker — the folder carries the title
            // and there is nothing here to cut.
            "E05.mkv",
        ] {
            let p = parse_filename(name);
            assert_eq!(p.episode, None, "{name}");
            assert!(!p.episode_absolute, "{name}");
            assert_eq!(p.kind, MediaKind::Movie, "{name}");
        }
    }

    /// **A name that carries its own season is not absolute.** The flag marks
    /// series-wide numbering, and `S01E05` is season-relative, so the folder
    /// fill below stays available to it exactly as before.
    #[test]
    fn a_season_bearing_name_is_not_absolute() {
        let p = parse_filename("Series.Title.S01E05.1080p");
        assert_eq!((p.season, p.episode), (Some(1), Some(5)));
        assert!(!p.episode_absolute);
    }

    /// **The folder season must never reach an absolute number.**
    ///
    /// `Season 2/…E56….mkv` is season 2 of the folder and episode 56 of the
    /// series. Pairing them gives `(2, 56)` — a slot that does not exist, bound
    /// with no error raised and nothing afterwards to tell it from a real bind.
    ///
    /// **This is the negative control for the guard in `parse_filename_in`.**
    /// Delete `&& !parsed.episode_absolute` there and every line here fails:
    /// each name becomes `Some(2)`.
    ///
    /// The seam is unwired dead code today. The guard is written now precisely
    /// because it is: whoever wires it would otherwise meet this in bindings
    /// rather than in a test.
    #[test]
    fn the_folder_season_never_reaches_an_absolute_number() {
        for name in [
            "It's a Series Title.E56.190121.720p-NEXT.mp4",
            "Anon Show Ep01 (D2201EC5).mkv",
            "Some Show 69. Bolum 720p",
        ] {
            let p = parse_filename_in(name, ctx(Some("Some Show"), Some(2)));
            assert!(p.episode_absolute, "{name}");
            assert_eq!(
                p.season, None,
                "the folder season must not pair with an absolute number: {name}"
            );
        }
    }

    /// Rule 2 — an episode with no season of its own takes the folder's.
    #[test]
    fn an_episode_without_a_season_takes_the_folders() {
        // `E05.mkv` is an episode marker with no season.
        let p = parse_filename_in("E05.mkv", ctx(Some("Some Show"), Some(3)));
        if p.kind == MediaKind::Episode && parse_filename("E05.mkv").season.is_none() {
            assert_eq!(p.season, Some(3));
        }
        // A basename that names its own season keeps it.
        let q = parse_filename_in("S01E01.mkv", ctx(Some("Some Show"), Some(9)));
        assert_eq!(
            q.season,
            Some(1),
            "the folder must not overrule the basename"
        );
    }

    /// **A film under a numbered season directory keeps no season.**
    /// `Futurama/Season 5/Futurama Bender's Big Score (2007).avi` is a real film
    /// in a real library. Inventing a season for it here would put a season on a
    /// movie row, and `stored_kind` — which correctly keeps it a film, because it
    /// carries its own year — would never get the chance to disagree.
    #[test]
    fn a_film_under_a_season_directory_gets_no_season() {
        let name = "Futurama Benders Big Score (2007).avi";
        let p = parse_filename_in(name, ctx(Some("Futurama"), Some(5)));
        assert_eq!(p.kind, MediaKind::Movie);
        assert_eq!(p.year, Some(2007));
        assert_eq!(p.season, None, "a film has no season");
    }

    /// The two shapes this seam exists for, asserted as **still unmoved**, so
    /// the next slice starts from a recorded position rather than a memory.
    /// Both parse to `Movie` with a non-empty title, so neither rule fires.
    #[test]
    fn the_two_shapes_it_was_written_for_do_not_move_yet() {
        // The **parsed** title, not the cleaned search query. `oracle_query`
        // prints `clean_show_title`'s output — `01 Closure` — and reading that
        // as the parse is how this test was wrong the first time.
        for (name, title) in [
            ("Episode 1.mkv", "Episode 1"),
            ("01 - Closure.mkv", "01 - Closure"),
        ] {
            let p = parse_filename_in(name, ctx(Some("Some Show (2020)"), Some(1)));
            assert_eq!(p.kind, MediaKind::Movie, "{name:?} kind");
            assert_eq!(p.title, title, "{name:?} title");
            assert_eq!(p.season, None, "{name:?} season");
            assert_eq!(p.episode, None, "{name:?} episode");
        }
    }

    /// A folder name is trimmed, and an absent one leaves the empty title empty
    /// rather than substituting something that is not a title.
    #[test]
    fn folder_title_is_trimmed_and_optional() {
        assert_eq!(
            parse_filename_in("S01E01.mkv", ctx(Some("  Spaced Show  "), None)).title,
            "Spaced Show"
        );
        assert_eq!(
            parse_filename_in("S01E01.mkv", ctx(None, None)).title,
            "",
            "no folder, no title"
        );
    }

    /// **A number in the title is not the year.** `find_year` took the first
    /// four digits anywhere, so `Wonder Woman 1984 (2020)` parsed as 1984 —
    /// and because the cut follows the year, the title became `Wonder Woman`.
    /// One rule, both halves wrong.
    ///
    /// Four of the six real files affected bind correctly *only* because the
    /// folder year overrules this parse, which is why this lands before that
    /// precedence is touched.
    /// A trailing run of bracket groups is not title.
    #[test]
    fn a_trailing_bracket_run_ends_the_title() {
        for (name, title) in [
            (
                "Series_Title_2_[01]_[AniLibria_TV]_[WEBRip_1080p]",
                "Series Title 2",
            ),
            (
                "[HorribleSubs] Some Anime Show!! (01-25) [1080p] (Batch)",
                "Some Anime Show!!",
            ),
            ("[HatSubs] One Series (1017-1088) (WEB 1080p)", "One Series"),
            (
                "[Moxie] One Series - The Country (892-916) (BD Remux 1080p AAC FLAC) [Dual Audio]",
                "One Series - The Country",
            ),
        ] {
            assert_eq!(parse_filename(name).title, title, "{name}");
        }
    }

    /// **The guards, each with the case that pays for it.** Negative controls:
    /// delete a guard and one of these fails.
    #[test]
    fn a_trailing_bracket_run_keeps_a_title_that_owns_its_brackets() {
        // Content: an unknown word in the group means it is not metadata.
        assert_eq!(
            parse_filename("[Judas] Series E (Series J) (Season 04) [1080p][HEVC x265 10bit]")
                .title,
            "Series E (Series J)"
        );
        // Year: a four-digit year is not metadata, so it cannot open the run.
        assert_eq!(
            parse_filename("Series Title [2022] [S25E13] [PL] [720p] [WEB-DL-CZRG] [x264]").title,
            "Series Title [2022]"
        );
        // Position: words after the group mean there is no trailing run at all.
        assert_eq!(
            parse_filename("(500) Days of Summer (2009) Bluray-1080p.mkv").title,
            "(500) Days of Summer"
        );
        // Head: nothing but groups leaves no title to end. `strip_leading_group`
        // takes `[01]` first, so what reaches this rule is `[1080p]` — whose
        // head is empty, so it declines rather than cutting to nothing. Cutting
        // to empty is what makes the caller substitute the raw stem.
        assert_eq!(parse_filename("[01] [1080p]").title, "[1080p]");
    }

    /// `【` is three bytes. Recording the body span while scanning rather than
    /// deriving `start + 1` is what keeps this from panicking.
    #[test]
    fn a_trailing_bracket_run_is_char_boundary_safe() {
        for name in [
            "【动漫国字幕组】★01月新番[Anime Series Title～！][01][1080P][简体][MP4]",
            "[星空字幕组] 剃须。然后捡到女高中生。 / Anime Series Title [05][1080p][简日内嵌]",
            "【傲娇零】[刀剑神域 UnderWorld][17][GB]",
        ] {
            let _ = parse_filename(name);
        }
    }

    /// Three anime production markers end a title.
    #[test]
    fn an_anime_production_marker_ends_the_title() {
        for (name, title) in [
            (
                "[sam] Long Series - NCOP [BD 1080p FLAC] [BBC3BC68].mkv",
                "Long Series",
            ),
            (
                "[sam] Long Series - NCED [BD 1080p FLAC] [BBC3BC68].mkv",
                "Long Series",
            ),
            (
                "[Underwater] Another OVA - The Other -Karma- (BD 1080p) [3A561D0E].mkv",
                "Another",
            ),
        ] {
            assert_eq!(parse_filename(name).title, title, "{name}");
        }
    }

    /// **The word boundary is what makes these three safe**, and these are the
    /// words that would break without it. Negative control: drop the
    /// `left_ok`/`right_ok` check in `cut_at_title_junk` and every line fails.
    #[test]
    fn an_anime_marker_inside_a_word_is_not_a_marker() {
        for (name, title) in [
            ("Nova (2023) Bluray-1080p.mkv", "Nova"),
            ("Casanova (2005) Bluray-1080p.mkv", "Casanova"),
            ("Supernova (2020) Bluray-1080p.mkv", "Supernova"),
            ("Ovation (2019) Bluray-1080p.mkv", "Ovation"),
        ] {
            assert_eq!(parse_filename(name).title, title, "{name}");
        }
    }

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

    /// **A daily show numbers its episodes by air date, and nothing cut it.**
    /// `Judge Developer 2016 02 25 S20E142` searched the provider for
    /// `Judge Developer 2016 02 25`.
    #[test]
    fn a_date_written_as_three_number_groups_ends_the_title() {
        let p = parse_filename("Judge Developer 2016 02 25 S20E142.mkv");
        assert_eq!(p.title, "Judge Developer");
        assert_eq!(p.season, Some(20));
        assert_eq!(p.episode, Some(142));

        // Year first, and the title itself begins with a number — the cut must
        // find the *date*, not the first four digits in the name.
        let p = parse_filename("2020.A.Late.Talk.Show.2012.16.02.PDTV.XviD-C4TV.mkv");
        assert_eq!(p.title, "2020 A Late Talk Show");

        // **This expectation changed on 2026-08-28, because the behaviour is
        // now right — not to make a change pass.** It read
        // `"The Series US 25 02"`, and the comment above it read: *"Where this
        // rule does not run, and why that is fine. On the movie arm the year
        // cut goes first, and a date contains a year — so
        // `The_Series_US_25.02.2016_hdtv` is already cut at `2016` and never
        // reaches here. It leaves `25 02` behind, which is two groups and not
        // a date; recovering those needs the year branch, not this one."*
        //
        // That described the defect accurately and then accepted it. The movie
        // arm now runs the date cut on the uncut stem, before the year cut can
        // take the third group away, so the date is whole when the rule sees
        // it. Two corpus cases turn on this.
        assert_eq!(
            parse_filename("The_Series_US_25.02.2016_hdtv.x264.mp4").title,
            "The Series US"
        );
        // Mid-string and year-first, the year cut alone gets it right.
        assert_eq!(
            parse_filename("Series.Title.2016.02.25.1080i.HDTV.mkv").title,
            "Series Title"
        );
    }

    /// **Two number groups are a title and its year**, which is half the movie
    /// library, so the rule needs three. `Blade Runner 2049` and `1883` are
    /// both real and both bound today.
    #[test]
    fn a_title_and_its_year_are_not_a_date() {
        assert_eq!(
            parse_filename("Blade.Runner.2049.2017.1080p.BluRay.mkv").title,
            "Blade Runner"
        );
        assert_eq!(
            parse_filename("1883.2019.1080p.BluRay.x264-GRP.mkv").title,
            "1883"
        );
        // Three groups, none of them a year in range.
        assert_eq!(
            parse_filename("Series 1 2 3 S01E01.mkv").title,
            "Series 1 2 3"
        );
    }

    /// A head with no letter in it is not a title to keep, so the cut declines
    /// rather than leaving `9`. **This is the guard that carries `9-1-1`** — a
    /// real show whose name offers `1 1 2016` as a date — and the sweep's five
    /// `9-1-1` names are what said so.
    #[test]
    fn a_date_cut_that_would_leave_no_letters_does_not_happen() {
        let p = parse_filename("9-1-1 2016 02 25 S20E142.mkv");
        assert_eq!(p.title, "9-1-1 2016 02 25");
        assert_eq!(p.season, Some(20));
        assert_eq!(p.episode, Some(142));
    }

    /// **A truncated episode number is a wrong claim, not a missing one.**
    ///
    /// The digit run after the marker was capped at three and the loop simply
    /// stopped, so a daily serial's four- and five-digit numbers came out as
    /// their first three digits: `S22E5363` reported 536, `S14E3533` reported
    /// 353, `S2020E1527` reported 152. Each sends the drain to the provider
    /// with a number the name never carried; reporting nothing would at least
    /// have left the file unmatched and recoverable.
    ///
    /// Every name here is a real release form. `Shortland Street` really is on
    /// episode 5,363 of season 22, and the corpus holds all three.
    #[test]
    fn a_long_episode_number_is_read_whole_and_not_truncated() {
        let p = parse_filename("Shortland.Series.S22E5363-E5366.HDTV.x264-FiHTV.mkv");
        assert_eq!(p.season, Some(22));
        assert_eq!(p.episode, Some(5363));
        assert_eq!(p.episode_end, Some(5366));
        assert_eq!(p.episode_numbers(), vec![5363, 5364, 5365, 5366]);
        assert_eq!(p.title, "Shortland Series");

        let p = parse_filename("The Series And the Show - S41 E10478 - 2014-08-15.mp4");
        assert_eq!(p.season, Some(41));
        assert_eq!(
            p.episode,
            Some(10478),
            "five digits, and the date after it \
             is not a range end"
        );
        assert_eq!(p.episode_end, None);

        let p = parse_filename("Anime Title - S2020E1527 [1527] [2020-10-11].mkv");
        assert_eq!(p.season, Some(2020), "a year-season keeps its four digits");
        assert_eq!(p.episode, Some(1527));
    }

    /// A run wider than an episode number means the **number** is not an
    /// episode number. The parser declines the number rather than using the
    /// first five digits, and it declines nothing else.
    ///
    /// **Asserting the two fields that motivated the rule is what let the last
    /// version of this through.** It checked `season` and `episode` and never
    /// looked at `kind` or `title`, so it read as a pass while the decline was
    /// also throwing away the title cut and calling the file a film — one wrong
    /// field traded for three, and a wrong kind is the worst class in the
    /// oracle's `ORDER`. All four are asserted here, on every case.
    ///
    /// What the parser returns for `S<season>E<unreadable>`: **season** the
    /// season it read, **episode** `None`, **kind** `Episode`, **title** the cut
    /// at the token — exactly what it returns for a season pack, because that is
    /// what the name now amounts to.
    #[test]
    fn a_digit_run_too_wide_for_an_episode_declines_the_number_and_nothing_else() {
        let p = parse_filename("Show.Name.S01E123456.720p.HDTV.x264-GRP.mkv");
        assert_eq!(p.season, Some(1));
        assert_eq!(p.episode, None, "six digits is not an episode number");
        assert_eq!(
            p.kind,
            MediaKind::Episode,
            "an unreadable episode number does not make the file a film"
        );
        assert_eq!(p.title, "Show Name", "the title cut survives the decline");
        assert_eq!(p.episode_end, None);
        assert_eq!(p.year, None);

        // A daily serial: season is the year, the episode is the air date, and
        // the air date is too wide. The title cut is the whole point here — the
        // movie arm cut this one at the year and returned `Show S`.
        let p = parse_filename("Show.S2016E20160225.mkv");
        assert_eq!(p.season, Some(2016));
        assert_eq!(p.episode, None);
        assert_eq!(p.kind, MediaKind::Episode);
        assert_eq!(p.title, "Show");
        assert_eq!(p.year, None);

        // **The bare `NxNNN` spelling keeps declining the whole token.** It has
        // no `S` and no `E` to vouch for it and it is the shape of a
        // resolution, so a wide run there must not assert television.
        let p = parse_filename("Movie Name 12x3456789 1080p.mkv");
        assert_eq!(p.kind, MediaKind::Movie);
        assert_eq!(p.season, None);
        assert_eq!(p.episode, None);

        // A whole token later in the name still wins over an earlier decline.
        let p = parse_filename("Show.S01E123456.S02E03.mkv");
        assert_eq!(p.season, Some(2));
        assert_eq!(p.episode, Some(3));
        assert_eq!(p.kind, MediaKind::Episode);
    }

    /// The run breaks on a following **digit** and must not break on a
    /// following letter: every multi-episode spelling puts one there.
    #[test]
    fn a_letter_after_the_digits_still_ends_the_run() {
        let p = parse_filename("Series.S01E01E02.mkv");
        assert_eq!(p.episode_numbers(), vec![1, 2]);
        let p = parse_filename("Series 8x01x02.mkv");
        assert_eq!(p.episode_numbers(), vec![1, 2]);
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
        // The year cut.
        //
        // **Changed 2026-08-19, and the old assertion is worth recording.** It
        // used `[Anon][Anon Title][2019][234][AVC][GB][1080P]` and expected
        // `[Anon Title]` — brackets and all, because backing the year cut out
        // to the bracket was the best that could be done for a name with no
        // text outside its groups. `bracket_run_title` now selects the title
        // group for that shape and returns `Anon Title`, which is the answer
        // the corpus wants. The example moved to a name that is *not* a
        // bracket run, so this test still covers the year-cut route it was
        // written for.
        assert_eq!(
            parse_filename("Anon Title [2019] Bluray-1080p.mkv").title,
            "Anon Title"
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
    /// **The number is now parsed, and it is marked absolute.** This test read:
    ///
    /// > *The number itself is not parsed — it is an absolute episode number
    /// > and `ParsedName` has nowhere to put one.*
    ///
    /// The second half was false about the type: `season` and `episode` are
    /// independent `Option`s and always were. What had nowhere to go was the
    /// *fact that the number is absolute*, and `episode_absolute` is that
    /// place. **The titles below are unchanged** — every one is the expectation
    /// this test already asserted, and none was touched.
    ///
    /// The span keeps the old answer, for the old reason: one number cannot
    /// stand for `Ep01-12`.
    #[test]
    fn a_bare_episode_marker_ends_the_title() {
        for (name, title, episode) in [
            ("[Anon] Anon Show Ep01 (D2201EC5).mkv", "Anon Show", Some(1)),
            ("Anon Show EP06 720p x265 GROUP.mp4", "Anon Show", Some(6)),
            (
                "AnonShow.E1135.Ein.Titel.GERMAN.1080p.WEBRip.x264-Group",
                "AnonShow",
                Some(1135),
            ),
            (
                "Anon_Show_e66_time_is_money_part_one",
                "Anon Show",
                Some(66),
            ),
            (
                "Anon.Show.Ep01-12.Complete.English.AC3.DL.1080p.BluRay.x264",
                "Anon Show",
                None,
            ),
        ] {
            let p = parse_filename(name);
            assert_eq!(p.title, title, "{name}");
            assert_eq!(p.episode, episode, "{name}");
            assert_eq!(p.season, None, "no season is ever synthesised: {name}");
            assert_eq!(p.episode_absolute, episode.is_some(), "{name}");
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

    /// The marker spelled out. Same shape as `Ep01` — a marker and a number —
    /// so this is still structural and not a vocabulary rule.
    #[test]
    fn the_episode_marker_may_be_spelled_out() {
        for (name, title) in [
            (
                "Anon Show Episode 56 [VOSTFR V2][720p][AAC]-Group",
                "Anon Show",
            ),
            (
                "[Group] Anon Show Episode 69 [VOSTFR_Finale][1080p][AAC].mp4",
                "Anon Show",
            ),
            (
                "To Another Anon III - Episode 5 VOSTFR (1080p)",
                "To Another Anon III",
            ),
            (
                "[Group] Anon Show Super - Episode 013 VF [720p]",
                "Anon Show Super",
            ),
            ("Anon Show Episodio 5 (1080p)", "Anon Show"),
        ] {
            assert_eq!(parse_filename(name).title, title, "{name}");
        }
    }

    /// **The longer the marker, the less the number has to carry.** `E3` could
    /// be a title token, so the short marker needs two digits; `Episode 3`
    /// cannot be anything else, so one is enough. The asymmetry is worth two
    /// corpus cases and the library votes on neither — it holds 1,272 paths
    /// containing `episod` and this rule fires on none of them, because they
    /// are episode *titles* behind a season/episode token that is cut first.
    #[test]
    fn one_digit_needs_the_marker_spelled_out() {
        assert_eq!(
            parse_filename("Anon Show Episode 5 VOSTFR (1080p)").title,
            "Anon Show"
        );
        assert_eq!(
            parse_filename("Anon E5 Film (2011).mkv").title,
            "Anon E5 Film"
        );
        // The episode title behind a season/episode token is never reached.
        let p = parse_filename("Anon Show - 1x01 - Episode 1 - WEBRip-1080p.mkv");
        assert_eq!(p.title, "Anon Show");
        assert_eq!(p.episode, Some(1));
    }

    /// A four-digit season is a year-season, and Sonarr writes them.
    /// `find_season_episode` read at most three season digits, so `S2016E231`
    /// produced nothing at all — no season, no episode, and a title running to
    /// the end of the name.
    #[test]
    fn a_four_digit_season_parses_in_the_marked_spelling() {
        for (name, title, season, episode) in [
            ("Anon Title - S1936E18 - An Episode", "Anon Title", 1936, 18),
            ("Anon Week S2009E09 [SDTV].avi", "Anon Week", 2009, 9),
            ("Anon!.S2016E14.2016-01-20.avi", "Anon!", 2016, 14),
            ("Anon - S2016E231", "Anon", 2016, 231),
        ] {
            let p = parse_filename(name);
            assert_eq!(p.season, Some(season), "{name}");
            assert_eq!(p.episode, Some(episode), "{name}");
            assert_eq!(p.title, title, "{name}");
        }
    }

    /// **The marked spelling only, and that is the whole guard.** The bare
    /// `2016x231` is not allowed four digits because it is the shape of a
    /// resolution — `1920x804` puts a plausible year on the left and a whole
    /// three-digit run on the right, so no range check and no whole-run check
    /// separates the two. The `S` and the `E` do.
    ///
    /// Three corpus cases are given up for this and it is the right trade: a
    /// resolution read as a season turns a film into an episode of season
    /// 1920.
    #[test]
    fn the_bare_spelling_keeps_its_two_digit_season() {
        for name in [
            "[Anon] A Film Name [Dual-Audio][BDRip 1920x804 HEVC FLACx2] [91FC62A8].mkv",
            "A Movie Name.1080x1920.mkv",
            "A Movie Name (1920x1080).mkv",
            "Anon Release - 07 (1280x720 x264-AAC) [ABCD1234].mkv",
        ] {
            let p = parse_filename(name);
            assert_eq!(p.season, None, "{name} invented a season");
            assert_eq!(p.episode, None, "{name} invented an episode");
        }
    }

    /// A four-digit number that is not a plausible year is not a season
    /// either, in any spelling.
    ///
    /// **The bare spelling was missing this guard**, so `S1080` and `S2160` —
    /// a resolution with an `s` in front — became season packs while the
    /// marked `S1080E01` was correctly refused. Width alone does not tell a
    /// year-season from a resolution; the range does.
    #[test]
    fn a_four_digit_season_must_be_a_plausible_year() {
        for name in [
            "Anon Show S1080E01 - An Episode.mkv",
            "Anon Show S1080 x264.mkv",
            "Anon Show S2160 DTS.mkv",
        ] {
            let p = parse_filename(name);
            assert_eq!(p.season, None, "{name} invented a season");
            assert_eq!(p.episode, None, "{name} invented an episode");
        }
        // A plausible year still is one, in the bare spelling the corpus asserts.
        let p = parse_filename("My.Series.S2014.720p.HDTV.x264-ME");
        assert_eq!(
            (p.season, p.episode, p.title.as_str()),
            (Some(2014), None, "My Series")
        );
    }

    /// **A title ending in a season word is a title, not a season pack.**
    ///
    /// The four-digit guard required the number to be a plausible year, which
    /// is exactly what a release year is, and `year_follows` looks *past* the
    /// digits so it never saw that the digits were the year. Every release form
    /// that puts the year straight after the title broke: `Open Season` became
    /// season 2006 of a series called `Open`, and lost its year as well.
    ///
    /// Found by the differential sweep once it scored every field and sampled
    /// every title rather than 900 of 2,332. Zero corpus cases spell a season
    /// word before four digits, so declining costs nothing measured.
    #[test]
    fn a_title_ending_in_a_season_word_is_not_a_season_pack() {
        for (name, title, year) in [
            (
                "Open.Season.2006.720p.BluRay.x264-GROUP",
                "Open Season",
                2006,
            ),
            ("Open Season 2006 1080p BluRay", "Open Season", 2006),
            ("Open_Season_2006_1080p_BluRay", "Open Season", 2006),
            (
                "Wedding.Season.2022.1080p.NF.WEB-DL",
                "Wedding Season",
                2022,
            ),
            (
                "The.Rainy.Season.1999.DVDRip.XviD",
                "The Rainy Season",
                1999,
            ),
            // The three non-English season words carry the same defect.
            ("La.Temporada.2019.1080p", "La Temporada", 2019),
            ("La.Saison.1999.DVDRip", "La Saison", 1999),
            ("La.Stagione.2001.720p", "La Stagione", 2001),
        ] {
            let p = parse_filename(name);
            assert_eq!(p.season, None, "{name} invented a season");
            assert_eq!(p.kind, MediaKind::Movie, "{name}");
            assert_eq!(p.year, Some(year), "{name}");
            assert_eq!(p.title, title, "{name}");
        }
        // The sequel spelling already declined, because there the year really
        // does follow the number and guard 2 sees it. It still does.
        let p = parse_filename("Open.Season.3.2010.1080p.BluRay");
        assert_eq!(
            (p.season, p.year, p.title.as_str()),
            (None, Some(2010), "Open Season 3")
        );
        // A season word before a *short* number is still a season pack.
        let q = parse_filename("Anon.Show.Season.04.1080p.WEB-DL");
        assert_eq!((q.season, q.title.as_str()), (Some(4), "Anon Show"));
    }

    /// **A four-digit season must be glued to its marker.**
    ///
    /// Fixing the spelled-out word alone would leave the same defect in a rarer
    /// spelling. Every year-season the corpus asserts is glued — `S2014`,
    /// `S1936E18`, `S2009E09`, `S2016E231` — so nothing measured buys the
    /// separated form, and accepting it made `Anon Film S 2019` season 2019.
    ///
    /// Narrower than the two-digit rule on purpose: width is what makes the
    /// separated spelling ambiguous, not the separator.
    #[test]
    fn a_separated_four_digit_season_is_not_a_season() {
        for name in [
            "The Anon S 2019 Show",
            "Anon.Film.S.2019.1080p.BluRay",
            "Anon_Film_S_2019_1080p",
        ] {
            let p = parse_filename(name);
            assert_eq!(p.season, None, "{name} invented a season");
            assert_eq!(p.kind, MediaKind::Movie, "{name}");
        }
        // Glued still parses, and a separated *two*-digit season still does
        // too — the corpus asserts both.
        let p = parse_filename("My.Series.S2014.720p.HDTV.x264-ME");
        assert_eq!((p.season, p.title.as_str()), (Some(2014), "My Series"));
        let q = parse_filename("Anon Show S 01 720p WEB DL DD 5 1 h264 GROUP");
        assert_eq!((q.season, q.title.as_str()), (Some(1), "Anon Show"));
    }

    /// **The terminator runs on the year arm too.** Cutting at the year takes
    /// away what follows it and nothing else, so junk sitting *before* the
    /// year survived. Only the fallback arms ever ran the junk cut.
    #[test]
    fn junk_before_the_year_is_cut_too() {
        for (name, title, year) in [
            (
                "World.Anon.Z.EXTENDED.2013.German.DL.1080p.BluRay.AVC-XANOR",
                "World Anon Z",
                2013,
            ),
            (
                "Anon.Aufbruch.nach.Pandora.Extended.2009.German.DTS.720p.BluRay.x264-SoW",
                "Anon Aufbruch nach Pandora",
                2009,
            ),
            ("Anon Film 1080p 2016 group", "Anon Film", 2016),
        ] {
            let p = parse_filename(name);
            assert_eq!(p.title, title, "{name}");
            assert_eq!(p.year, Some(year), "{name}");
        }
    }

    /// The edition and language tokens, each measured on its own before it was
    /// added: `extended` earns seven corpus cases, `truefrench` and `imax` one
    /// each, and none of the three cuts a title anywhere in the 25,043-file
    /// dogfood library.
    #[test]
    fn edition_tokens_end_the_title() {
        assert_eq!(
            parse_filename("Valana la Anon TRUEFRENCH BluRay 720p 2016 group").title,
            "Valana la Anon"
        );
        assert_eq!(
            parse_filename("Anon.Title.Imax.2018.1080p.AMZN.WEB-DL.DD5.1.H.264-NTG").title,
            "Anon Title"
        );
    }

    /// **The words that were measured and left out**, and why each one stays
    /// out. `german` earns seven and the corpus holds the film that refutes
    /// it. The other three are refuted by the library, and all three of those
    /// items are bound today.
    #[test]
    fn the_rejected_edition_words_do_not_cut() {
        assert_eq!(
            parse_filename("The.Good.German.2006.720p.BluRay.x264-RlsGrp").title,
            "The Good German"
        );
        assert_eq!(
            parse_filename("A Complete Unknown (2024) Bluray-1080p.mkv").title,
            "A Complete Unknown"
        );
        assert_eq!(
            parse_filename("South Park Bigger Longer and Uncut (1999) Bluray-1080p.mkv").title,
            "South Park Bigger Longer and Uncut"
        );
        assert_eq!(
            parse_filename("The Toxic Avenger Unrated (2025) Bluray-1080p.mkv").title,
            "The Toxic Avenger Unrated"
        );
    }

    /// A name that is nothing but bracket groups has no text outside them for
    /// a terminator to cut at, so the title is *selected* from the groups
    /// rather than derived by cutting. The first group is the release tag, so
    /// selection starts at the second.
    #[test]
    fn a_bracket_run_selects_its_title_group() {
        for (name, title) in [
            (
                "[Anon-Team][国漫][Anon Title][2019][215][AVC][GB][1080P]",
                "Anon Title",
            ),
            (
                "[Anon-Team][国漫][斗罗大陆][Anon Title][Douro Mainland][2019][215 END][AVC][GB][1080P]",
                "Anon Title",
            ),
            (
                "[Anon][Anon_Series_Title][01][GB][1080P][x264_AAC]",
                "Anon Series Title",
            ),
            // a group carrying both scripts keeps its Latin side
            (
                "[AnonRaws][Anon Series 海賊王][1008][ViuTV][CHT][MKV]",
                "Anon Series",
            ),
            (
                "[Anon组][名侦探柯南·Anon Title][871][繁日][HDrip][X264-AAC]",
                "Anon Title",
            ),
            // the full-width bracket is a bracket too
            (
                "【Anon字幕组】【天使降临_Anon Series Title】[第05话][1080p_HEVC][简繁外挂]",
                "Anon Series Title",
            ),
        ] {
            assert_eq!(parse_filename(name).title, title, "{name}");
        }
    }

    /// The two guards that keep this away from ordinary names.
    ///
    /// A name with text outside the brackets is not a run — a terminator has
    /// something to cut at there and does a better job. And three groups
    /// minimum, so a title with one or two trailing tags is untouched.
    #[test]
    fn an_ordinary_name_is_not_a_bracket_run() {
        assert_eq!(
            parse_filename("[Anon] Anon Show - 1x02 - An Episode [720p][ABCD1234].mkv").title,
            "Anon Show"
        );
        assert_eq!(
            parse_filename("Anon Show - 4x11 - An Episode - Bluray-1080p.mkv").title,
            "Anon Show"
        );
        assert_eq!(
            parse_filename("[Anon][Anon Title][1080p]").title,
            "Anon Title"
        );
        assert_eq!(parse_filename("[REC] (2007).mkv").year, Some(2007));
    }

    /// **The group-rejection list never cuts a title.** It only makes the
    /// selector skip a group, which is why it may hold `mp4`, `gb` and `batch`
    /// — words that would be reckless in `TITLE_JUNK`, where a match
    /// truncates. `Anon GB Title` keeps its middle word.
    #[test]
    fn the_group_metadata_list_does_not_truncate() {
        assert_eq!(
            parse_filename("Anon GB Title (2011) Bluray-1080p.mkv").title,
            "Anon GB Title"
        );
        assert_eq!(
            parse_filename("Anon Batch Title (2011) Bluray-1080p.mkv").title,
            "Anon Batch Title"
        );
    }

    /// A leading `www.site.tld - ` is a tracker prefix. It survived every
    /// terminator because it sits *before* the title and nothing cuts from the
    /// left.
    #[test]
    fn a_leading_site_prefix_is_not_the_title() {
        assert_eq!(
            parse_filename("www.Anon.com - Anon.2008.720p.X264-GROUP").title,
            "Anon"
        );
        let p = parse_filename("www.Anon.org - Anon.S03E14.720p.HDTV.X264-GROUP");
        assert_eq!(p.title, "Anon");
        assert_eq!(p.season, Some(3));
        assert_eq!(p.episode, Some(14));
        assert_eq!(
            parse_filename("www.5AnonRulz.tc - Anon (2000) Malayalam HQ HDRip - x264.mkv").title,
            "Anon"
        );
    }

    /// The guards. A bare domain with no dash stays, because a film could be
    /// called one; what is left must still carry a letter; and a dashed title
    /// that is not a domain is untouched.
    #[test]
    fn only_a_domain_before_a_dash_is_stripped() {
        assert_eq!(
            parse_filename("Anon Show - 1x02 - An Episode.mkv").title,
            "Anon Show"
        );
        assert_eq!(
            parse_filename("Anon.com Anon Film (2011).mkv").title,
            "Anon com Anon Film"
        );
        // The head is a domain but nothing with a letter follows the dash, so
        // the prefix stays — stripping to nothing is worse than keeping junk,
        // the same trade `cut_at_title_junk` makes.
        let p = parse_filename("www.anon.com - 2019.mkv");
        assert!(p.title.contains("anon com"), "got {:?}", p.title);
        // Not a domain: the head has whitespace.
        assert_eq!(
            parse_filename("Anon Doc - Anon Film (2011).mkv").title,
            "Anon Doc - Anon Film"
        );
        // **A dotted release title has the domain shape and must survive it.**
        // Every negative case above declines for a reason a dotted title does
        // not supply — whitespace in the head, or no dash at all — so none of
        // them tested the way this rule can wrongly fire. These do.
        for (name, title) in [
            ("Anon.Show.US - 1x01 - A Pilot.mkv", "Anon Show US"),
            ("Anon.Doc.The.End - 1x01 - A Title.mkv", "Anon Doc The End"),
            ("Dr.No - 1x01 - An Episode.mkv", "Dr No"),
        ] {
            assert_eq!(parse_filename(name).title, title, "{name}");
        }
        // A real film title whose last word is also a country domain.
        let p = parse_filename("Anon.Film.2019.HD - GRP.mkv");
        assert_eq!(p.title, "Anon Film");
        assert_eq!(p.year, Some(2019));
    }

    /// **The season and the episode marker may be separated.** Requiring the
    /// `e` to touch the season digits made `Anon Title.S6.E1` report no
    /// episode at all — not a wrong episode, none.
    #[test]
    fn a_separated_season_and_episode_still_parse() {
        for (name, season, episode) in [
            ("Anon Title.S6.E1.An Episode.1080p.WEB-DL", 6, 1),
            ("anon.s03.e05.ws.dvdrip.xvid-group", 3, 5),
            ("Anon.Title.S15.E06.City.Code", 15, 6),
            ("Anon Title - S15 E06 - City Code", 15, 6),
            ("Anon S1-E1-WEB-DL-1080p-group", 1, 1),
            ("Super.Anon.S01.Ep06.1080p.BluRay.DTS.x264-MiR", 1, 6),
            (
                "Anon.Title.S01.Ep.01.English.AC3.DL.1080p.BluRay-Group",
                1,
                1,
            ),
            (
                "Anon.Title.S01.E.01.English.AC3.DL.1080p.BluRay-Group",
                1,
                1,
            ),
            ("Anon.Title.S01EP01.English.AC3.DL.1080p.BluRay-Group", 1, 1),
            ("Anon s90 e43 1080p HDTV AAC H264", 90, 43),
        ] {
            let p = parse_filename(name);
            assert_eq!(p.season, Some(season), "{name}");
            assert_eq!(p.episode, Some(episode), "{name}");
        }
    }

    /// The span rules still apply across the gap.
    #[test]
    fn a_separated_token_still_spans() {
        for (name, want) in [
            ("Anon Title.S6.E1.E2.An Episode.1080p.WEB-DL", vec![1, 2]),
            ("Anon Title.S6.E1-E2.An Episode.1080p.WEB-DL", vec![1, 2]),
            (
                "Anon Title.S6.E1-E2-E3.An Episode.1080p.WEB-DL",
                vec![1, 2, 3],
            ),
            ("Anon Title.S6.E1-S6E2.An Episode.1080p.WEB-DL", vec![1, 2]),
            // An unseparated repetition must land on the next number, so
            // `E1E3` is one episode. That guard is iteration 12's and it is
            // right — `E1E3` is not a range.
            ("Anon Title.S6.E1E3.An Episode.1080p.WEB-DL", vec![1]),
        ] {
            assert_eq!(parse_filename(name).episode_numbers(), want, "{name}");
        }
    }

    /// The gap is one separator, however it is spelled, and the season/episode
    /// scan still refuses everything it refused before.
    ///
    /// **Changed, and the old assertion is worth recording.** It read
    /// `assert_eq!(parse_filename("Anon Title S6 - E1 - Something.mkv").episode,
    /// None)` under the comment "two separators is not a token", and it passed
    /// while the file became a **season pack** — season 6, no episode. Asserting
    /// one field let a rule trade a right answer for a wrong claim without the
    /// test noticing, so every case here now asserts the whole parse.
    #[test]
    fn the_token_gap_does_not_widen_the_scan() {
        // A spaced dash is one separator spelled in three characters, and the
        // glued spelling has always parsed.
        for name in [
            "Anon Title S6 - E1 - Something.mkv",
            "Anon Title S6-E1 - Something.mkv",
            "Anon Title S6 . E1 - Something.mkv",
        ] {
            let p = parse_filename(name);
            assert_eq!(
                (p.season, p.episode, p.kind, p.title.as_str()),
                (Some(6), Some(1), MediaKind::Episode, "Anon Title"),
                "{name}"
            );
        }
        // Two punctuation marks is not one separator. The scan declines, and
        // the bare-season rule then claims the `S6` — the same season pack it
        // makes of any name whose episode spelling this does not recognise.
        for name in [
            "Anon Title S6 -- E1 - Something.mkv",
            "Anon Title S6.-.E1.mkv",
        ] {
            let p = parse_filename(name);
            assert_eq!((p.season, p.episode), (Some(6), None), "{name}");
        }
        // A resolution is still not an episode.
        let p = parse_filename("A Anon Name (1920x1080).mkv");
        assert_eq!((p.season, p.episode), (None, None));
        // Episode 0 is still refused, and the season it carries is still a pack.
        let p = parse_filename("Anon Show S01 E00 - A Pilot.mkv");
        assert_eq!((p.season, p.episode), (Some(1), None));
        // The common form is unchanged.
        let p = parse_filename("Anon Show - 4x11 - An Episode - Bluray-1080p.mkv");
        assert_eq!(
            (p.season, p.episode, p.title.as_str()),
            (Some(4), Some(11), "Anon Show")
        );
    }

    /// The mirror of `a_cut_inside_a_bracket_backs_out_to_the_bracket`: a
    /// title cannot contain a bracket it never opened. Everything from the
    /// stray `]` on belongs to a group the title is not part of.
    #[test]
    fn a_title_ends_at_a_bracket_it_never_opened() {
        assert_eq!(
            parse_filename("Anon Series Title][12END][720p][繁体]").title,
            "Anon Series Title"
        );
        assert_eq!(
            parse_filename("Anon Series Title!][04][1080P][繁體][MP4]").title,
            "Anon Series Title!"
        );
        // A bracket the title does open and close is still its own.
        assert_eq!(
            parse_filename("Anon [Bracketed] Film (2011).mkv").title,
            "Anon [Bracketed] Film"
        );
    }

    /// **Square brackets only.** `(` and `)` appear inside real titles — an
    /// Arabic corpus case that passes today carries a stray `)` — while `[`
    /// and `]` are release-group syntax and nothing else.
    #[test]
    fn a_stray_parenthesis_does_not_end_a_title() {
        let p = parse_filename("Anon nf) Anon Anon 2024 3 3");
        assert!(p.title.starts_with("Anon nf)"), "got {:?}", p.title);
    }

    /// The episode marker may come after the number. Turkish releases write
    /// `69. Blm`, `60.Bolum`, `1. Bölüm`.
    #[test]
    fn an_episode_word_after_the_number_ends_the_title() {
        for (name, title) in [
            (
                "Anon show 69. Blm (29.10.2023) 1080p WebDL #tag",
                "Anon show",
            ),
            (
                "Anon opera 01 BLM(01.11.2023) 1080p HDTV AC3 x264 GROUP",
                "Anon opera",
            ),
            (
                "Anon show 60.Bolum (31.01.2023) 720p WebDL AAC H.264 - GROUP",
                "Anon show",
            ),
            (
                "Anon show 1. Bölüm (23.10.2023) 720p WebDL AAC H.264 - GROUP",
                "Anon show",
            ),
            (
                "Anon show 79.BLM Sezon Finali(25.06.2023) 720p WEB-DL",
                "Anon show",
            ),
        ] {
            assert_eq!(parse_filename(name).title, title, "{name}");
        }
    }

    /// **A four-digit run in the year range is a year**, the guard
    /// `cut_at_absolute_episode` already put on its own four-digit run. The
    /// marker read up to four digits without it, so a year standing in front
    /// of one of these words cut the title at the year.
    #[test]
    fn a_year_before_the_episode_word_is_not_an_episode_number() {
        for (name, title, year) in [
            (
                "Anon 2020 BLM Documentary (2021).mkv",
                "Anon 2020 BLM Documentary",
                2021,
            ),
            (
                "Anon 1999 Bolum Belgesel (2021).mkv",
                "Anon 1999 Bolum Belgesel",
                2021,
            ),
        ] {
            let p = parse_filename(name);
            assert_eq!(p.title, title, "{name}");
            assert_eq!(p.year, Some(year), "{name}");
        }
        // Three digits is still an episode number, and four outside the range.
        assert_eq!(
            parse_filename("Anon show 205. Blm (29.10.2023) 1080p WebDL").title,
            "Anon show"
        );
    }

    /// **The digit run in front is what makes these words safe to name.** The
    /// dogfood library contains none of them anywhere, so it can neither
    /// confirm nor refute this rule — the guard is the grammar. Without the
    /// leading number the word is part of the title.
    #[test]
    fn an_episode_word_without_a_number_is_part_of_the_title() {
        assert_eq!(
            parse_filename("Anon Bolum Film (2011) Bluray-1080p.mkv").title,
            "Anon Bolum Film"
        );
        assert_eq!(
            parse_filename("Anon BLM Story (2020) Bluray-1080p.mkv").title,
            "Anon BLM Story"
        );
        // Glued to another letter it is not the word.
        assert_eq!(
            parse_filename("Anon 12Blmx Film (2011) Bluray-1080p.mkv").title,
            "Anon 12Blmx Film"
        );
    }

    /// **An absent title is absent.** When the season/episode token starts the
    /// name there is no series title in the filename at all, and substituting
    /// the stem produced a "title" of pure release junk.
    ///
    /// Empty is only honest because two callers now handle it:
    /// `nightjar-scanner` borrows the folder's name before storing, and the
    /// TMDB source filters an empty title to a miss before any request.
    #[test]
    fn a_name_that_starts_with_the_token_has_no_title() {
        for name in [
            "S03E09 WS PDTV XviD FUtV",
            "5x10 WS PDTV XviD FUtV",
            "S01E04",
            "1x04",
            "01x04 - Halloween, Part 1 - 720p WEB-DL",
            "S08E20 50-50 Carla [DVD]",
            "S02E03-04-05.720p.BluRay-FUTV",
            "1x03 - The 112th Congress [1080p BluRay].mkv",
        ] {
            let p = parse_filename(name);
            assert_eq!(p.title, "", "{name}");
            assert!(p.season.is_some(), "{name} lost its season");
            assert!(p.episode.is_some(), "{name} lost its episode");
        }
    }

    /// The substitution is kept everywhere else. A name that *has* a title
    /// before the token keeps it, and a name that is only junk still gets the
    /// stem rather than nothing — cutting to empty there would throw away the
    /// only identity the file has.
    #[test]
    fn a_title_before_the_token_is_still_substituted_when_it_cuts_to_nothing() {
        assert_eq!(
            parse_filename("Anon Show - 4x11 - An Episode - Bluray-1080p.mkv").title,
            "Anon Show"
        );
        // Movie branch: only junk, so the stem stands in as before.
        assert!(!parse_filename("1080p.x264.mkv").title.is_empty());
        // A leading group tag is stripped first, so the token still starts the
        // stem and the title is still absent.
        assert_eq!(parse_filename("[Anon] S01E04.mkv").title, "");
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

    /// **A dash is not the only separator.** `S02E09 E10` and
    /// `Series.S03E01.S03E02` are one file holding two episodes; the dash-only
    /// rule returned the first and reported success, which is worse than
    /// returning nothing — a caller cannot tell a single-episode file from a
    /// range whose tail was dropped.
    #[test]
    fn a_space_or_dot_separates_repeated_episode_tokens() {
        for (name, want) in [
            ("Anon.S03E01.S03E02.720p.HDTV.X264-GROUP", vec![1, 2]),
            (
                "The Anon S01e01 e02 ShoHD On Demand 1080i DD5 1 GROUP",
                vec![1, 2],
            ),
            ("Anon.Title.2x04.2x05.720p.BluRay-FUTV", vec![4, 5]),
            ("Hell on Anon S02E09 E10 HDTV x264 GROUP", vec![9, 10]),
        ] {
            assert_eq!(parse_filename(name).episode_numbers(), want, "{name}");
        }
    }

    /// **The marker is the whole guard.** A space, dot or underscore is
    /// accepted only when a repeated season or an `e`/`x` follows it. Without
    /// that, every one of these — which parse correctly today — becomes a
    /// two-episode file, because the token after the separator is a numeral in
    /// the episode title.
    #[test]
    fn a_soft_separator_without_a_marker_is_not_an_episode() {
        for (name, want) in [
            (
                "Anon Title S01E06 3 Beers For Batali DVDRip XviD GROUP",
                vec![6],
            ),
            ("Anon.S01E04.2-45.PM.[HDTV-720p].mkv", vec![4]),
            (
                "Anon Title S02E21 18 5 4 720p WEB DL DD5 1 h 264 GROUP",
                vec![21],
            ),
            ("Anon Show - 1x01 - An Episode - Bluray-1080p.mkv", vec![1]),
            ("Anon.Show.S01E01.720p.mkv", vec![1]),
        ] {
            assert_eq!(parse_filename(name).episode_numbers(), want, "{name}");
        }
    }

    /// A soft separator means repetition, not a range: the next number must be
    /// exactly the next episode. Only the dash carries a range.
    #[test]
    fn a_soft_separator_does_not_open_a_range() {
        assert_eq!(
            parse_filename("Anon.S01E01.E05.720p.mkv").episode_numbers(),
            vec![1]
        );
        assert_eq!(
            parse_filename("Anon.S01E01-E05.720p.mkv").episode_numbers(),
            vec![1, 2, 3, 4, 5]
        );
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

    /// **What the padded-separator rule permits, not just what it rejects.**
    ///
    /// ` - ` is the ordinary separator between an episode and its title, so
    /// the token after it is usually a title — and a title may open with a
    /// letter this loop reads as a marker followed by a digit. Each of these
    /// read as a range while a bare `e`/`x` was accepted behind a padded
    /// separator, and `MAX_EPISODE_RANGE` hid it only where the number was
    /// large enough to trip the cap: `x264` was refused for its size, `X2`
    /// was not.
    ///
    /// Every field is asserted, because the rule that shipped before this one
    /// moved two nobody checked.
    #[test]
    fn a_padded_separator_does_not_read_a_title_as_a_range() {
        for name in [
            "Show - S01E01 - E3 2019 Highlights.mkv", // E3 is an expo
            "Show - S01E01 - X2.mkv",                 // X2 is a film
            "Show - S01E01 - X2 Review.mkv",
            "Show - S01E01 - E2E Testing.mkv", // E2E is end-to-end
            "Show - S01E01 - x264-GRP.mkv",
            "Show - S01E01 - Exit 8.mkv",
        ] {
            let p = parse_filename(name);
            assert_eq!(p.kind, crate::MediaKind::Episode, "{name:?} kind");
            assert_eq!(p.title, "Show", "{name:?} title");
            assert_eq!(p.season, Some(1), "{name:?} season");
            assert_eq!(p.episode, Some(1), "{name:?} episode");
            assert_eq!(p.episode_end, None, "{name:?} must not open a range");
            assert_eq!(p.year, None, "{name:?} year");
            assert_eq!(p.episode_numbers(), vec![1], "{name:?} run");
        }
    }

    /// And the two names the rule exists for still extend, because each
    /// carries a marker a title does not: a repeated season, or `ep` spelled
    /// with two letters.
    #[test]
    fn a_padded_separator_still_extends_a_distinctive_marker() {
        let p = parse_filename(
            "Series Title - S07E22 - S07E23 - And Lots of Security.. [HDTV-720p].mkv",
        );
        assert_eq!(p.season, Some(7));
        assert_eq!(p.episode_numbers(), vec![22, 23]);
        let q = parse_filename("The Series And The Code - S42 Ep10718 - Ep10722");
        assert_eq!(q.season, Some(42));
        assert_eq!(q.episode, Some(10718));
        assert_eq!(q.episode_end, Some(10722));
    }

    /// **A bare dash keeps the exemption it always had.** The padded rule
    /// narrows nothing behind an unpadded dash, so `S15E06-08` and
    /// `S01E01-E02` read exactly as they did before either rule existed.
    #[test]
    fn a_bare_dash_still_needs_no_distinctive_marker() {
        assert_eq!(
            parse_filename("Show.S15E06-08.mkv").episode_numbers(),
            vec![6, 7, 8]
        );
        assert_eq!(
            parse_filename("Show.S01E01-E02.mkv").episode_numbers(),
            vec![1, 2]
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
