//! Search confidence scoring (ADR-0026 §2). Floor is 0.80.
//!
//! Multi-exact collisions use one pin rule: the first discriminator that
//! selects exactly one candidate lifts above the floor; otherwise stay 0.72.

use serde::{Deserialize, Serialize};

use crate::model::CanonicalMetadata;

/// Auto-match only at or above this score (ADR-0026).
pub const AUTO_MATCH_FLOOR: f64 = 0.80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchKind {
    Movie,
    Tv,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub id: i64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub original_title: Option<String>,
    #[serde(default)]
    pub original_name: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub first_air_date: Option<String>,
    /// Poster CDN path from the search response (two-tier fast capture; the
    /// same string the detail payload carries, ADR-0027 §2).
    #[serde(default)]
    pub poster_path: Option<String>,
    #[serde(default)]
    pub backdrop_path: Option<String>,
    /// Sparse fast-tier capture fields (ADR-0026 §8.1): overview/plot and
    /// vote rating ride along on search so `matched` rows can be written
    /// without a detail fetch. Empty/absent on older cached payloads.
    #[serde(default)]
    pub overview: Option<String>,
    #[serde(default)]
    pub vote_average: Option<f64>,
    #[serde(default)]
    pub vote_count: Option<i64>,
}

/// Library-side series shape for collision pins (TV).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LibrarySeriesShape {
    /// Premiere year: earliest episode year, else show-folder `(YYYY)`.
    pub year: Option<i32>,
    /// Distinct episode files under the show.
    pub episode_count: Option<u32>,
    /// Distinct season numbers present (excludes null).
    pub season_count: Option<u32>,
    /// The season numbers the **folder** asserts, season 0 already excluded.
    /// Not the same thing as `season_count`: the count is a pin signal that
    /// must equal a candidate's, this is the set a candidate must be able to
    /// hold. Empty means the folder asserts nothing and no coverage evidence
    /// exists.
    pub folder_seasons: Vec<i32>,
    /// ADR-0032 reference episode (usable after-token title only).
    pub ref_season: Option<i32>,
    pub ref_episode: Option<i32>,
    pub ref_episode_title: Option<String>,
}

/// Max multi-exact candidates for the episode-title pin (ADR-0032).
pub const EPISODE_TITLE_TIE_CAP: usize = 5;

/// Per-candidate extras (search year always; counts from `/tv/{id}` detail).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CandidateShape {
    pub year: Option<i32>,
    pub episode_count: Option<u32>,
    pub season_count: Option<u32>,
    /// Season numbers from the `seasons[]` array of the `/tv/{id}` payload the
    /// collision tier already fetches. `None` means not fetched, which is not
    /// evidence about the candidate either way.
    pub season_numbers: Option<Vec<i32>>,
    /// `(episode_number, name)` for the folder's reference season, appended to
    /// the same `/tv/{id}` call via `append_to_response=season/{n}`. `None`
    /// means not fetched — no evidence, never a verdict.
    pub reference_season_episodes: Option<Vec<(i32, String)>>,
}

/// Does this candidate's reference-season episode carry the title the folder's
/// reference file does?
///
/// `Some(true)` confirms, `None` there is nothing to compare. **There is no
/// `Some(false)`**, and that is the point: [`compare_episode_title`] can return
/// `Agree` or `Unknown` on a filename-derived title and nothing else, because
/// `Disagree` needs a corroborating air date and filenames carry none. Title
/// evidence is therefore one-directional **by construction, not by policy** —
/// it is what the measurement supports, where confirmation held on 598 working
/// folders and refutation never once identified a wrong entity. A later author
/// adding a penalty path here would be reversing a measured result, not
/// filling in an oversight.
pub fn candidate_confirms_reference_episode(
    shape: &CandidateShape,
    library: &LibrarySeriesShape,
    show_soft_key: &str,
) -> Option<bool> {
    let want = library.ref_episode_title.as_deref()?;
    let ref_episode = library.ref_episode?;
    let episodes = shape.reference_season_episodes.as_deref()?;
    let name = episodes
        .iter()
        .find(|(n, _)| *n == ref_episode)
        .map(|(_, nm)| nm.as_str())?;
    match compare_episode_title(want, name, show_soft_key, None, None) {
        EpisodeTitleVerdict::Agree => Some(true),
        // Unknown is the only other reachable verdict here, and it is not
        // evidence against the candidate.
        _ => None,
    }
}

/// Episode-title confirmation as **promotion evidence, never a penalty.**
///
/// Same discipline as the season-coverage promotion it sits beside: it only
/// moves a pick between candidates, and an ambiguous or absent signal leaves
/// the pick alone.
///
/// - The chosen candidate being **confirmed** ends it — evidence supports the
///   scorer and nothing moves.
/// - Promotion needs **exactly one** other confirmed candidate. Two confirmed
///   candidates mean the evidence does not discriminate.
/// - Absence of confirmation is **not** evidence against a candidate; it is
///   only the absence of a reason to prefer it.
fn confirmation_beats_pick<'a>(
    chosen: &SearchHit,
    exact: &[&'a SearchHit],
    shapes: Option<&[CandidateShape]>,
    library: &LibrarySeriesShape,
    show_soft_key: &str,
) -> Option<&'a SearchHit> {
    let shapes = shapes?;
    if shapes.len() != exact.len() {
        return None;
    }
    let chosen_i = exact.iter().position(|h| h.id == chosen.id)?;
    if candidate_confirms_reference_episode(&shapes[chosen_i], library, show_soft_key) == Some(true)
    {
        return None;
    }
    let mut winner: Option<&SearchHit> = None;
    for (i, h) in exact.iter().enumerate() {
        if i == chosen_i {
            continue;
        }
        if candidate_confirms_reference_episode(&shapes[i], library, show_soft_key) == Some(true) {
            if winner.is_some() {
                return None;
            }
            winner = Some(h);
        }
    }
    winner
}

/// Can this candidate hold every season the folder asserts?
///
/// `Some(true)` it can, `Some(false)` it demonstrably cannot, `None` there is
/// no evidence — an unfetched season list must never read as either answer.
/// One-directional by construction: at least, never exactly, so a library one
/// season behind a running show still covers.
pub fn candidate_covers_folder_seasons(
    shape: &CandidateShape,
    folder_seasons: &[i32],
) -> Option<bool> {
    if folder_seasons.is_empty() {
        return None;
    }
    let have = shape.season_numbers.as_deref()?;
    Some(folder_seasons.iter().all(|want| have.contains(want)))
}

/// Season coverage as **promotion evidence for the year pin, never a gate.**
///
/// The year selected a candidate that cannot hold the folder — the folder says
/// `(2003)`, the two-episode 2003 miniseries aired 2003, both facts correct and
/// the answer wrong. If exactly one other title-exact candidate demonstrably
/// can hold it, that candidate wins.
///
/// Three rules keep this from becoming the gate that was measured destroying
/// 338 correct bindings to catch 6:
///
/// - It only ever **moves** a pick between candidates. It cannot reject a
///   candidate set, cannot lower a score below the floor, and cannot unmatch a
///   folder — with no better candidate the year's pick stands.
/// - Unknown coverage is **no evidence**. A candidate whose seasons were never
///   fetched neither promotes nor demotes, and the year's pick is only
///   displaced when its own list is present and short.
/// - **Ambiguity keeps the year.** Two candidates that both cover mean the
///   evidence does not discriminate, so nothing moves.
fn coverage_beats_year<'a>(
    chosen: &SearchHit,
    exact: &[&'a SearchHit],
    shapes: Option<&[CandidateShape]>,
    folder_seasons: &[i32],
) -> Option<&'a SearchHit> {
    let shapes = shapes?;
    if shapes.len() != exact.len() {
        return None;
    }
    let chosen_i = exact.iter().position(|h| h.id == chosen.id)?;
    if candidate_covers_folder_seasons(&shapes[chosen_i], folder_seasons) != Some(false) {
        return None;
    }
    let mut winner: Option<&SearchHit> = None;
    for (i, h) in exact.iter().enumerate() {
        if i == chosen_i {
            continue;
        }
        if candidate_covers_folder_seasons(&shapes[i], folder_seasons) == Some(true) {
            if winner.is_some() {
                return None;
            }
            winner = Some(h);
        }
    }
    winner
}

#[derive(Debug, Clone, PartialEq)]
pub struct MatchCandidate {
    pub tmdb_id: i64,
    pub confidence: f64,
    pub method: &'static str,
    pub result_title: Option<String>,
    pub result_year: Option<i32>,
    pub n_results: usize,
}

/// Normalise a title for exact comparison (spike `norm_key` + orthography fold).
pub fn norm_key(s: &str) -> String {
    let s = crate::clean::fold_title_orthography(s);
    let mut s = s.to_ascii_lowercase();
    s = s.trim().to_string();
    for article in ["the ", "a ", "an "] {
        if let Some(rest) = s.strip_prefix(article) {
            s = rest.to_string();
            break;
        }
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c.is_whitespace() {
            out.push(c);
        } else {
            out.push(' ');
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn row_year(hit: &SearchHit, kind: SearchKind) -> Option<i32> {
    let d = match kind {
        SearchKind::Movie => hit.release_date.as_deref(),
        SearchKind::Tv => hit.first_air_date.as_deref(),
    }?;
    let y = d.get(..4)?.parse().ok()?;
    Some(y)
}

fn display_title(hit: &SearchHit, kind: SearchKind) -> Option<String> {
    match kind {
        SearchKind::Movie => hit.title.clone().or_else(|| hit.original_title.clone()),
        SearchKind::Tv => hit.name.clone().or_else(|| hit.original_name.clone()),
    }
}

pub fn title_hit(hit: &SearchHit, query_norm: &str, kind: SearchKind) -> bool {
    let (primary, original) = match kind {
        SearchKind::Movie => (hit.title.as_deref(), hit.original_title.as_deref()),
        SearchKind::Tv => (hit.name.as_deref(), hit.original_name.as_deref()),
    };
    primary.is_some_and(|t| name_matches_query(t, query_norm, kind))
        || original.is_some_and(|t| name_matches_query(t, query_norm, kind))
}

/// Exact fold match, or (TV only) candidate is query plus a longer official name
/// ("The Continental" → "The Continental: From the World of John Wick").
fn name_matches_query(name: &str, query_norm: &str, kind: SearchKind) -> bool {
    let nk = norm_key(name);
    if nk == query_norm {
        return true;
    }
    if kind != SearchKind::Tv || query_norm.is_empty() {
        return false;
    }
    // Prefix: "the continental from the world…" after colon fold.
    if nk.starts_with(query_norm)
        && nk.len() > query_norm.len()
        && nk.as_bytes().get(query_norm.len()) == Some(&b' ')
    {
        return true;
    }
    // Head before ':' if colon survived folding.
    if let Some(head) = nk.split(':').next() {
        let head = head.trim();
        if head == query_norm {
            return true;
        }
    }
    false
}

/// `/find` acceptance gate (strategy note §2, human open question 4): a show
/// returned by an NFO external id must agree with the group's cleaned folder
/// title and `(YYYY)` or the id is discarded and search runs instead — a
/// wrong external id must fail into search, not win. Returns the discard
/// reason, or `None` when the hit passes. Reuses the one TV title-match
/// predicate (`name_matches_query`), including the prefix rule.
pub fn find_hit_reject_reason(
    metadata: &CanonicalMetadata,
    kind: SearchKind,
    query: &str,
    folder_year: Option<i32>,
) -> Option<String> {
    let query_norm = norm_key(query);
    if query_norm.is_empty() {
        return Some("no folder title to cross-check against".into());
    }
    let name_ok = name_matches_query(&metadata.title, &query_norm, kind)
        || metadata
            .original_title
            .as_deref()
            .is_some_and(|t| name_matches_query(t, &query_norm, kind));
    if !name_ok {
        return Some(format!(
            "name '{}' does not match folder title '{query}'",
            metadata.title
        ));
    }
    if let (Some(hit_year), Some(folder_year)) = (metadata.year, folder_year)
        && hit_year != folder_year
    {
        return Some(format!(
            "year {hit_year} does not match folder year {folder_year}"
        ));
    }
    None
}

/// Soft episode-count match: absolute or proportional slack so incomplete
/// libraries (311 vs 327) still pin when only one candidate is close.
fn episode_count_close(library: u32, candidate: u32) -> bool {
    let diff = library.abs_diff(candidate);
    let tol = (candidate as f64 * 0.15).ceil() as u32;
    diff <= tol.max(5)
}

/// Empty TMDB shells (0 seasons / 0 episodes) never auto-pin.
fn is_empty_shell(shape: &CandidateShape) -> bool {
    matches!(shape.season_count, Some(0))
        || (matches!(shape.episode_count, Some(0)) && matches!(shape.season_count, Some(0)))
        || (matches!(shape.episode_count, Some(0)) && shape.season_count.is_none())
}

/// The candidate-set exclusion: a provider entity with zero episodes is not a
/// candidate (ADR-0026, amended) — a folder with files cannot bind to an
/// entity with nothing to bind to. Unknown (`None`) never excludes, the same
/// absence-is-not-evidence discipline as season coverage.
pub(crate) fn has_no_episodes(shape: &CandidateShape) -> bool {
    shape.episode_count == Some(0)
}

/// First discriminator that selects exactly one of `exact` wins.
/// Order: episode count → season count → premiere year.
/// Counts first so folder year cannot pin a miniseries when the library is a
/// multi-season series (Battlestar Galactica 2003 folder vs 2004 series).
fn pin_collision<'a>(
    exact: &[&'a SearchHit],
    shapes: &[CandidateShape],
    library: LibrarySeriesShape,
) -> Option<(&'a SearchHit, &'static str)> {
    debug_assert_eq!(exact.len(), shapes.len());

    let try_pin = |pred: &dyn Fn(usize) -> bool, method: &'static str| {
        let mut hit: Option<&SearchHit> = None;
        for (i, h) in exact.iter().enumerate() {
            if is_empty_shell(&shapes[i]) {
                continue;
            }
            if pred(i) {
                if hit.is_some() {
                    return None; // two+
                }
                hit = Some(*h);
            }
        }
        hit.map(|h| (h, method))
    };

    if let Some(le) = library.episode_count
        && let Some(p) = try_pin(
            &|i| {
                shapes[i]
                    .episode_count
                    .is_some_and(|ce| episode_count_close(le, ce))
            },
            "exact_title_episode_count",
        )
    {
        return Some(p);
    }
    if let Some(ls) = library.season_count
        && let Some(p) = try_pin(
            &|i| shapes[i].season_count == Some(ls),
            "exact_title_season_count",
        )
    {
        return Some(p);
    }
    if let Some(ly) = library.year
        && let Some(p) = try_pin(&|i| shapes[i].year == Some(ly), "exact_title_library_year")
    {
        return Some(p);
    }
    None
}

/// What a comparison of two episode titles is allowed to conclude.
///
/// **A comparator may report agreement, or silence. It may not report
/// disagreement on weak evidence.** That is not a style preference: measured
/// across 696 working folders, title confirmation held on 588 while title
/// refutation never once identified a wrong entity, and a false refutation
/// costs a working binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpisodeTitleVerdict {
    /// The two titles name the same episode.
    Agree,
    /// Cannot tell. Either side carried no identity, or they differ with
    /// nothing to corroborate the difference. Never read this as "different".
    Unknown,
    /// They name different episodes, and a second field agrees that they do.
    Disagree,
}

/// Gap 2: a provider placeholder carries no identity, exactly as a filename
/// placeholder does. The rule existed and was applied to one side only, so a
/// real title on disk was compared against `Episode 1` and reported as a
/// disagreement.
fn provider_title_is_generic(name: &str) -> bool {
    crate::clean::episode_title_rejected(name, "")
}

/// Gap 3: some providers prefix every episode title with the show's own name —
/// `The Grand-ish Tour: A Trip Down Memory Lane` against a filename carrying
/// only `A Trip Down Memory Lane`.
fn strip_show_name_prefix(name: &str, show_soft_key: &str) -> String {
    let show = norm_key(show_soft_key);
    if show.is_empty() {
        return name.to_string();
    }
    for sep in [": ", " - "] {
        if let Some(i) = name.find(sep)
            && norm_key(&name[..i]) == show
        {
            return name[i + sep.len()..].trim().to_string();
        }
    }
    name.to_string()
}

/// Labels that **number** an episode instead of naming it.
///
/// One set in one place, because the shape arrived twice more after gap 4
/// under two different words. Writing each as its own rule is what produces
/// the next one; adding a word here is the whole change instead.
///
/// | | provider | filename | bridged |
/// |---|---|---|---|
/// | gap 4 | `Episode 10: Aftersun` | `Episode 10 - Aftersun` | yes |
/// | gap 7 | `Theatre of Pain` | `Night 3 - Theater of Pain` | yes |
/// | gap 6 | `Part (1)` | `The Peacekeeper Wars (1)` | **no** |
///
/// **Gap 6 is deliberately not bridged**, and it is listed so the next reader
/// does not try. `Part (1)` is a label with nothing after it, so there is no
/// title on the provider side to compare — the only thing the two sides share
/// is the number 1. Bridging it would mean agreeing on a bare part number,
/// which matches any file whose title reduces to one. It is a placeholder
/// case, not a labelling case, and it belongs to whatever handles placeholders.
///
/// On the dogfood library `part` and `night` fire; `episode` shipped with gap
/// 4; `chapter`, `day` and `week` are in the set on the same argument and are
/// exercised only by the unit tests.
const ORDINAL_LABELS: &[&str] = &["episode", "part", "chapter", "night", "day", "week"];

/// Gap 4, generalised to [`ORDINAL_LABELS`]: the label is carried on **both**
/// sides — TMDB writes `Episode 10: Aftersun` and the filename writes
/// `Episode 10 - Aftersun`. Stripping it from one side only turns an exact
/// match into a disagreement, so this is applied to both sides by the caller.
fn strip_ordinal_label(name: &str) -> String {
    for sep in [": ", " - "] {
        if let Some(i) = name.find(sep) {
            let head = norm_key(&name[..i]);
            let mut w = head.split_whitespace();
            if w.next().is_some_and(|l| ORDINAL_LABELS.contains(&l))
                && w.next().is_some_and(is_ordinal_word)
                && w.next().is_none()
            {
                let rest = name[i + sep.len()..].trim();
                // A label with nothing after it is a placeholder, not a title
                // (`Episode 3`, `Part (1)`). Stripping it to the empty string
                // would turn two placeholders on unrelated shows into an exact
                // match, which is the opposite of what identity evidence is
                // for. Leave it whole and let the generic-title rejection in
                // `episode_title_rejected` / `provider_title_is_generic` judge
                // it.
                if !rest.is_empty() {
                    return rest.to_string();
                }
            }
        }
    }
    name.to_string()
}

fn is_ordinal_word(w: &str) -> bool {
    w.chars().all(|c| c.is_ascii_digit())
        || matches!(
            w,
            "one" | "two" | "three" | "four" | "five" | "six" | "seven" | "eight" | "nine" | "ten"
        )
}

/// One measured spelling variant, folded for the comparison only.
///
/// `The Continental` is spelled `Theatre of Pain` by the provider and
/// `Theater of Pain` on disk, and nothing else separates the two titles.
///
/// **Deliberately one pair, and deliberately not in [`norm_key`].**
///
/// Not in `norm_key` because that is the persisted negative-cache key through
/// `cleaner_version` (ADR-0026 §5): folding there invalidates the cache
/// library-wide and is a different change with a different cost. This fold is
/// local to the episode-title comparison and persists nothing.
///
/// One pair because the wider scopes were measured on the dogfood library and
/// both failed. Over 41,876 distinct `norm_key` values:
///
/// | scope | titles rewritten | previously-distinct titles merged |
/// |---|---:|---:|
/// | this pair | 10 | **0** |
/// | eight common pairs | 48 | 4 |
/// | general `-re`/`-er` + `-our`/`-or` | 924 | 4 |
///
/// The merge counts are what rejected the other two, not the rewrite counts:
/// **every merge either produced was between two different shows** —
/// `Shades of Gray` on 655/211288 against `Shades of Grey` on 121/4629,
/// `Honor Thy Father` on 39269 against `Honour Thy Father` on 121040, and so
/// on. Not one was two spellings of a single episode. A wider fold here buys
/// no correct match and hands the confirmation path new ways to confirm a
/// wrong candidate.
fn fold_spelling_variant(norm: &str) -> String {
    if !norm.contains("theatre") {
        return norm.to_string();
    }
    norm.split(' ')
        .map(|w| if w == "theatre" { "theater" } else { w })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Q4: part-number conventions. `The Reptile Room (1)` and
/// `The Reptile Room: Part One` are the same episode, and
/// `Look at the Princess (1) - A Kiss Is But a Kiss` differs from the
/// provider's spelling only in where the part number sits. Punctuation
/// normalisation alone does not bridge either, so the part number is lifted
/// out and the remaining words compared without regard to order.
fn part_number_key(name: &str) -> Option<String> {
    let n = norm_key(name);
    let mut toks: Vec<String> = Vec::new();
    let mut it = n.split_whitespace().peekable();
    while let Some(w) = it.next() {
        if (w == "part" || w == "pt") && it.peek().is_some_and(|x| is_ordinal_word(x)) {
            toks.push(ordinal_value(it.next().unwrap()));
            continue;
        }
        toks.push(w.to_string());
    }
    // Order-insensitivity is granted only to titles that carry a part number,
    // which is where the transposition happens. Without this guard the rule
    // would also merge two genuinely different titles built from the same
    // words in a different order.
    if !toks.iter().any(|t| t.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }
    toks.sort();
    Some(toks.join(" "))
}

fn ordinal_value(w: &str) -> String {
    match w {
        "one" => "1",
        "two" => "2",
        "three" => "3",
        "four" => "4",
        "five" => "5",
        "six" => "6",
        "seven" => "7",
        "eight" => "8",
        "nine" => "9",
        "ten" => "10",
        other => other,
    }
    .to_string()
}

/// Do two air dates disagree? `None` when either is missing — absent data is
/// never a verdict. One day of slack, because 990 of 1,204 measured pairs
/// shared an identical title and differed by exactly one day on date
/// convention alone.
fn air_dates_disagree(a: Option<&str>, b: Option<&str>) -> Option<bool> {
    let (a, b) = (a?, b?);
    let d = |s: &str| -> Option<i64> {
        let y: i64 = s.get(0..4)?.parse().ok()?;
        let m: i64 = s.get(5..7)?.parse().ok()?;
        let dd: i64 = s.get(8..10)?.parse().ok()?;
        Some(y * 372 + m * 31 + dd)
    };
    Some((d(a)? - d(b)?).abs() > 1)
}

/// Compare a filename-derived episode title against a provider episode title.
///
/// The five comparator defects measured on the dogfood library are closed here
/// as five separate rules rather than one normalisation, because a single rule
/// would silently absorb the next case into whichever gap it resembled.
///
/// Gap 5 — a provider using a stylised title set (`BLATANT, NOT SUBTLE`) where
/// the filenames use a plain one (`Pilot`, `Run`) — has **no comparison fix**
/// and is not given one. Two legitimate title sets for the same episodes are
/// not a disagreement about identity, and it falls out as [`Unknown`] through
/// the corroboration rule below rather than through a rule that pretends to
/// recognise aliases.
///
/// [`Unknown`]: EpisodeTitleVerdict::Unknown
pub fn compare_episode_title(
    from_file: &str,
    from_provider: &str,
    show_soft_key: &str,
    file_air_date: Option<&str>,
    provider_air_date: Option<&str>,
) -> EpisodeTitleVerdict {
    let file = crate::clean::strip_trailing_source_token(from_file);
    if crate::clean::episode_title_rejected(&file, show_soft_key)
        || provider_title_is_generic(from_provider)
    {
        return EpisodeTitleVerdict::Unknown;
    }
    let prov = strip_show_name_prefix(from_provider, show_soft_key);
    let file = strip_ordinal_label(&file);
    let prov = strip_ordinal_label(&prov);
    let (file_key, prov_key) = (norm_key(&file), norm_key(&prov));
    if file_key == prov_key {
        return EpisodeTitleVerdict::Agree;
    }
    if fold_spelling_variant(&file_key) == fold_spelling_variant(&prov_key) {
        return EpisodeTitleVerdict::Agree;
    }
    if let (Some(a), Some(b)) = (part_number_key(&file), part_number_key(&prov))
        && a == b
    {
        return EpisodeTitleVerdict::Agree;
    }
    match air_dates_disagree(file_air_date, provider_air_date) {
        Some(true) => EpisodeTitleVerdict::Disagree,
        _ => EpisodeTitleVerdict::Unknown,
    }
}

/// ADR-0032 step 4: unique folded match of local reference title vs candidate
/// episode names (parallel to `exact`). Declines when over cap or no unique hit.
pub fn pin_episode_title<'a>(
    exact: &[&'a SearchHit],
    candidate_episode_names: &[Option<String>],
    local_title: &str,
    show_soft_key: &str,
) -> Option<(&'a SearchHit, &'static str)> {
    if exact.len() > EPISODE_TITLE_TIE_CAP || exact.len() != candidate_episode_names.len() {
        return None;
    }
    if norm_key(local_title).is_empty() {
        return None;
    }
    let mut hit: Option<&SearchHit> = None;
    for (i, h) in exact.iter().enumerate() {
        let Some(name) = candidate_episode_names[i].as_deref() else {
            continue;
        };
        // Only agreement pins. Unknown and Disagree both decline, so a
        // comparator that cannot tell can never select a candidate.
        if compare_episode_title(local_title, name, show_soft_key, None, None)
            == EpisodeTitleVerdict::Agree
        {
            if hit.is_some() {
                return None;
            }
            hit = Some(*h);
        }
    }
    hit.map(|h| (h, "exact_title_episode_title"))
}

/// Score TMDB search results (ADR-0026 table + collision pin).
pub fn score_search(
    results: &[SearchHit],
    title: &str,
    year: Option<i32>,
    kind: SearchKind,
) -> Option<MatchCandidate> {
    score_search_with_shape(
        results,
        title,
        year,
        kind,
        LibrarySeriesShape::default(),
        None,
    )
}

pub fn score_search_with_library_year(
    results: &[SearchHit],
    title: &str,
    year: Option<i32>,
    library_year: Option<i32>,
    kind: SearchKind,
) -> Option<MatchCandidate> {
    score_search_with_shape(
        results,
        title,
        year,
        kind,
        LibrarySeriesShape {
            year: library_year,
            ..Default::default()
        },
        None,
    )
}

pub fn score_search_with_shape(
    results: &[SearchHit],
    title: &str,
    year: Option<i32>,
    kind: SearchKind,
    library: LibrarySeriesShape,
    // Parallel to title-exact hits when provided (detail counts). When None,
    // year pin still works from search first_air_date.
    candidate_shapes: Option<&[CandidateShape]>,
) -> Option<MatchCandidate> {
    if results.is_empty() {
        return None;
    }
    let nk = norm_key(title);
    let exact_all: Vec<&SearchHit> = results.iter().filter(|r| title_hit(r, &nk, kind)).collect();
    let had_title_hits = !exact_all.is_empty();

    // A provider entity with zero episodes is not a candidate (ADR-0026,
    // amended): a folder with files cannot bind to it, so drop such
    // title-hits before any year/pin/coverage/confirmation branch, keeping
    // shapes parallel to the survivors. Only aligned detail shapes carry an
    // episode count; the year-only synthetic shapes have `episode_count:
    // None` and never exclude (unknown is not evidence).
    let (exact, shapes): (Vec<&SearchHit>, Option<Vec<CandidateShape>>) = match candidate_shapes {
        Some(s) if s.len() == exact_all.len() => {
            let mut kept_hits = Vec::with_capacity(exact_all.len());
            let mut kept_shapes = Vec::with_capacity(exact_all.len());
            for (h, sh) in exact_all.iter().zip(s.iter()) {
                if !has_no_episodes(sh) {
                    kept_hits.push(*h);
                    kept_shapes.push((*sh).clone());
                }
            }
            (kept_hits, Some(kept_shapes))
        }
        _ => (exact_all, candidate_shapes.map(|s| s.to_vec())),
    };
    let shapes = shapes.as_deref();
    if exact.is_empty() && had_title_hits {
        // Every title-hit was an empty shell: none is a candidate. Do not
        // fall through to `top1_rank` over the leftover search hits.
        return None;
    }
    let exact_year: Vec<&SearchHit> = exact
        .iter()
        .copied()
        .filter(|r| year.is_some() && row_year(r, kind) == year)
        .collect();

    let (hit, conf, method) = if !exact_year.is_empty() {
        let hit = exact_year[0];
        let conf = if exact_year.len() == 1 { 0.98 } else { 0.80 };
        match coverage_beats_year(hit, &exact, shapes, &library.folder_seasons) {
            Some(better) => (better, 0.90, "exact_title_season_coverage"),
            None => (hit, conf, "exact_title_year"),
        }
    } else if !exact.is_empty() && year.is_some() {
        let y = year.unwrap();
        let hit = exact
            .iter()
            .min_by_key(|r| (row_year(r, kind).unwrap_or(0) - y).unsigned_abs())
            .copied()
            .unwrap();
        (hit, 0.70, "exact_title_year_nearest")
    } else if !exact.is_empty() {
        // Build shapes from search years when caller omitted detail.
        let owned: Vec<CandidateShape> = exact
            .iter()
            .map(|h| CandidateShape {
                year: row_year(h, kind),
                ..Default::default()
            })
            .collect();
        let shapes = match shapes {
            Some(s) if s.len() == exact.len() => s,
            _ => owned.as_slice(),
        };
        if exact.len() == 1 {
            // Sole exact hit that is an empty TMDB shell stays below floor.
            if is_empty_shell(&shapes[0]) {
                (exact[0], 0.72, "exact_title_empty_shell")
            } else {
                (exact[0], 0.90, "exact_title")
            }
        } else if let Some((hit, method)) = pin_collision(&exact, shapes, library.clone()) {
            (hit, 0.90, method)
        } else {
            // Prefer first non-empty candidate for the unpinned method payload,
            // but stay below floor.
            let hit = exact
                .iter()
                .enumerate()
                .find(|(i, _)| !is_empty_shell(&shapes[*i]))
                .map(|(_, h)| *h)
                .unwrap_or(exact[0]);
            (hit, 0.72, "exact_title_collision_unpinned")
        }
    } else {
        let hit = &results[0];
        let mut conf = if results.len() == 1 { 0.55 } else { 0.45 };
        if year.is_some() && row_year(hit, kind) == year {
            conf = 0.65;
        }
        (hit, conf, "top1_rank")
    };

    // Title confirmation applies after the branch has chosen, so it reaches the
    // decided branches — a sole same-year hit scores 0.98 and no tie-break can
    // reach it. It raises a confirmed candidate over an unconfirmed one and
    // never lowers anything.
    let (hit, conf, method) = match confirmation_beats_pick(hit, &exact, shapes, &library, title) {
        Some(better) => (
            better,
            f64::max(conf, 0.90),
            "exact_title_episode_confirmed",
        ),
        None => (hit, conf, method),
    };

    Some(MatchCandidate {
        tmdb_id: hit.id,
        confidence: conf,
        method,
        result_title: display_title(hit, kind),
        result_year: row_year(hit, kind),
        n_results: results.len(),
    })
}

/// Whether multi-exact scoring needs `/tv/{id}` detail for count pins and/or
/// may need the episode-title pin path (ADR-0032).
pub fn needs_collision_detail(
    results: &[SearchHit],
    title: &str,
    year: Option<i32>,
    kind: SearchKind,
    library: LibrarySeriesShape,
) -> bool {
    if kind != SearchKind::Tv {
        return false;
    }
    let has_count = library.episode_count.is_some() || library.season_count.is_some();
    let has_ref = library.ref_episode_title.is_some()
        && library.ref_season.is_some()
        && library.ref_episode.is_some();
    if !has_count && !has_ref {
        return false;
    }
    let nk = norm_key(title);
    let exact_n = results.iter().filter(|r| title_hit(r, &nk, kind)).count();
    // Multi-exact with library shape: always fetch counts so episode/season
    // pins can outrank a misleading folder year (BSG 2003 mini vs 2004 series).
    if exact_n > 1 && has_count {
        return true;
    }
    let Some(c) = score_search_with_shape(results, title, year, kind, library.clone(), None) else {
        return false;
    };
    // Still below floor after year-only pin → fetch detail counts / title tier.
    // Empty-shell sole hit also needs detail (or stays unmatched).
    c.confidence < AUTO_MATCH_FLOOR
        && (c.method == "exact_title_collision_unpinned" || c.method == "exact_title_empty_shell")
}

pub fn meets_auto_match_floor(confidence: f64) -> bool {
    confidence >= AUTO_MATCH_FLOOR
}

#[cfg(test)]
mod tests {
    use super::*;

    fn movie(id: i64, title: &str, year: i32) -> SearchHit {
        SearchHit {
            id,
            title: Some(title.into()),
            name: None,
            original_title: Some(title.into()),
            original_name: None,
            release_date: Some(format!("{year}-01-01")),
            first_air_date: None,
            poster_path: None,
            backdrop_path: None,
            overview: None,
            vote_average: None,
            vote_count: None,
        }
    }

    fn tv(id: i64, name: &str, year: i32) -> SearchHit {
        SearchHit {
            id,
            title: None,
            name: Some(name.into()),
            original_title: None,
            original_name: Some(name.into()),
            release_date: None,
            first_air_date: Some(format!("{year}-01-01")),
            poster_path: None,
            backdrop_path: None,
            overview: None,
            vote_average: None,
            vote_count: None,
        }
    }

    #[test]
    fn unique_title_year_is_0_98() {
        let results = vec![movie(550, "Fight Club", 1999)];
        let m = score_search(&results, "Fight Club", Some(1999), SearchKind::Movie).unwrap();
        assert_eq!(m.tmdb_id, 550);
        assert!((m.confidence - 0.98).abs() < f64::EPSILON);
        assert!(meets_auto_match_floor(m.confidence));
        assert_eq!(m.method, "exact_title_year");
    }

    #[test]
    fn multi_exact_title_is_0_72_below_floor() {
        let results = vec![tv(37854, "One Piece", 1999), tv(111110, "One Piece", 2023)];
        let m = score_search(&results, "One Piece", None, SearchKind::Tv).unwrap();
        assert!((m.confidence - 0.72).abs() < f64::EPSILON);
        assert!(!meets_auto_match_floor(m.confidence));
        assert_eq!(m.method, "exact_title_collision_unpinned");
    }

    #[test]
    fn library_year_pins_unique_multi_exact_above_floor() {
        let results = vec![tv(37854, "One Piece", 1999), tv(111110, "One Piece", 2023)];
        let m =
            score_search_with_library_year(&results, "One Piece", None, Some(1999), SearchKind::Tv)
                .unwrap();
        assert_eq!(m.tmdb_id, 37854);
        assert_eq!(m.method, "exact_title_library_year");
        assert!(meets_auto_match_floor(m.confidence));
    }

    #[test]
    fn library_year_pins_nothing_stays_0_72() {
        let results = vec![tv(1, "Bones", 2005), tv(2, "Bones", 2019)];
        let m = score_search_with_library_year(&results, "Bones", None, Some(1990), SearchKind::Tv)
            .unwrap();
        assert!((m.confidence - 0.72).abs() < f64::EPSILON);
        assert_eq!(m.method, "exact_title_collision_unpinned");
    }

    #[test]
    fn library_year_pins_two_stays_0_72() {
        let results = vec![
            tv(1, "Show", 2001),
            tv(2, "Show", 2001),
            tv(3, "Show", 2010),
        ];
        let m = score_search_with_library_year(&results, "Show", None, Some(2001), SearchKind::Tv)
            .unwrap();
        assert!((m.confidence - 0.72).abs() < f64::EPSILON);
        assert_eq!(m.method, "exact_title_collision_unpinned");
    }

    #[test]
    fn episode_count_pins_supernatural_shape() {
        let results = vec![
            tv(1622, "Supernatural", 2005),
            tv(999, "Supernatural", 2025),
        ];
        let shapes = [
            CandidateShape {
                year: Some(2005),
                episode_count: Some(327),
                season_count: Some(15),
                season_numbers: None,
                reference_season_episodes: None,
            },
            CandidateShape {
                year: Some(2025),
                episode_count: Some(8),
                season_count: Some(1),
                season_numbers: None,
                reference_season_episodes: None,
            },
        ];
        let m = score_search_with_shape(
            &results,
            "Supernatural",
            None,
            SearchKind::Tv,
            LibrarySeriesShape {
                year: None,
                episode_count: Some(311),
                season_count: Some(15),
                ..Default::default()
            },
            Some(&shapes),
        )
        .unwrap();
        assert_eq!(m.tmdb_id, 1622);
        assert_eq!(m.method, "exact_title_episode_count");
        assert!(meets_auto_match_floor(m.confidence));
    }

    #[test]
    fn season_count_pins_when_episode_count_ambiguous() {
        // The Boys: both candidates report 40 episodes; seasons differ.
        let results = vec![tv(76479, "The Boys", 2019), tv(107755, "The Boys", 1997)];
        let shapes = [
            CandidateShape {
                year: Some(2019),
                episode_count: Some(40),
                season_count: Some(5),
                season_numbers: None,
                reference_season_episodes: None,
            },
            CandidateShape {
                year: Some(1997),
                episode_count: Some(40),
                season_count: Some(1),
                season_numbers: None,
                reference_season_episodes: None,
            },
        ];
        let m = score_search_with_shape(
            &results,
            "The Boys",
            None,
            SearchKind::Tv,
            LibrarySeriesShape {
                year: None,
                episode_count: Some(42),
                season_count: Some(5),
                ..Default::default()
            },
            Some(&shapes),
        )
        .unwrap();
        assert_eq!(m.tmdb_id, 76479);
        assert_eq!(m.method, "exact_title_season_count");
    }

    #[test]
    fn episode_count_pins_two_stays_0_72() {
        let results = vec![tv(1, "X", 2000), tv(2, "X", 2010)];
        let shapes = [
            CandidateShape {
                year: Some(2000),
                episode_count: Some(40),
                season_count: Some(1),
                season_numbers: None,
                reference_season_episodes: None,
            },
            CandidateShape {
                year: Some(2010),
                episode_count: Some(40),
                season_count: Some(1),
                season_numbers: None,
                reference_season_episodes: None,
            },
        ];
        let m = score_search_with_shape(
            &results,
            "X",
            None,
            SearchKind::Tv,
            LibrarySeriesShape {
                year: None,
                episode_count: Some(40),
                season_count: Some(1),
                ..Default::default()
            },
            Some(&shapes),
        )
        .unwrap();
        assert!((m.confidence - 0.72).abs() < f64::EPSILON);
        assert_eq!(m.method, "exact_title_collision_unpinned");
    }

    /// Same title, long classic vs short reboot: episode count picks classic
    /// even when both years could confuse a human.
    #[test]
    fn long_run_episode_count_pins_over_short_reboot() {
        let results = vec![tv(10, "Alpha", 2001), tv(20, "Alpha", 2026)];
        let shapes = [
            CandidateShape {
                year: Some(2001),
                episode_count: Some(181),
                season_count: Some(9),
                season_numbers: None,
                reference_season_episodes: None,
            },
            CandidateShape {
                year: Some(2026),
                episode_count: Some(12),
                season_count: Some(2),
                season_numbers: None,
                reference_season_episodes: None,
            },
        ];
        let m = score_search_with_shape(
            &results,
            "Alpha",
            None,
            SearchKind::Tv,
            LibrarySeriesShape {
                year: Some(2001),
                episode_count: Some(181),
                season_count: Some(9),
                ..Default::default()
            },
            Some(&shapes),
        )
        .unwrap();
        assert_eq!(m.tmdb_id, 10);
        assert_eq!(m.method, "exact_title_episode_count");
        assert!(meets_auto_match_floor(m.confidence));
    }

    /// Folder year uniquely matches a miniseries, but library shape is a
    /// multi-season series — counts must outrank year.
    #[test]
    fn folder_year_miniseries_loses_to_library_shape() {
        let results = vec![tv(100, "Bravo", 2004), tv(200, "Bravo", 2003)];
        let shapes = [
            CandidateShape {
                year: Some(2004),
                episode_count: Some(73),
                season_count: Some(4),
                season_numbers: None,
                reference_season_episodes: None,
            },
            CandidateShape {
                year: Some(2003),
                episode_count: Some(2),
                season_count: Some(1),
                season_numbers: None,
                reference_season_episodes: None,
            },
        ];
        let m = score_search_with_shape(
            &results,
            "Bravo",
            None,
            SearchKind::Tv,
            LibrarySeriesShape {
                year: Some(2003), // folder year — mini only
                episode_count: Some(72),
                season_count: Some(4),
                ..Default::default()
            },
            Some(&shapes),
        )
        .unwrap();
        assert_eq!(m.tmdb_id, 100);
        assert!(
            m.method == "exact_title_episode_count" || m.method == "exact_title_season_count",
            "method={}",
            m.method
        );
        assert!(meets_auto_match_floor(m.confidence));
    }

    /// Cleaned folder title is a short prefix of the official TMDB name;
    /// empty shell (0 seasons/eps) must not win.
    #[test]
    fn short_query_matches_long_official_title_over_empty_shell() {
        let mut shell = tv(1, "Charlie", 0);
        shell.first_air_date = None;
        let long = SearchHit {
            id: 2,
            title: None,
            name: Some("Charlie: Extended Official Title".into()),
            original_title: None,
            original_name: Some("Charlie: Extended Official Title".into()),
            release_date: None,
            first_air_date: Some("2023-09-22".into()),
            poster_path: None,
            backdrop_path: None,
            overview: None,
            vote_average: None,
            vote_count: None,
        };
        let results = vec![shell, long];
        let shapes = [
            CandidateShape {
                year: None,
                episode_count: Some(0),
                season_count: Some(0),
                season_numbers: None,
                reference_season_episodes: None,
            },
            CandidateShape {
                year: Some(2023),
                episode_count: Some(3),
                season_count: Some(1),
                season_numbers: None,
                reference_season_episodes: None,
            },
        ];
        let m = score_search_with_shape(
            &results,
            "Charlie",
            None,
            SearchKind::Tv,
            LibrarySeriesShape {
                year: Some(2023),
                episode_count: Some(3),
                season_count: Some(1),
                ..Default::default()
            },
            Some(&shapes),
        )
        .unwrap();
        assert_eq!(m.tmdb_id, 2);
        assert!(meets_auto_match_floor(m.confidence));
    }

    /// A sole candidate that is an empty shell is not a candidate at all:
    /// the scorer returns `None` (unmatched), not a below-floor score.
    #[test]
    fn empty_shell_sole_exact_is_not_a_candidate() {
        let results = vec![tv(1, "Delta", 2000)];
        let shapes = [CandidateShape {
            year: Some(2000),
            episode_count: Some(0),
            season_count: Some(0),
            season_numbers: None,
            reference_season_episodes: None,
        }];
        assert!(
            score_search_with_shape(
                &results,
                "Delta",
                None,
                SearchKind::Tv,
                LibrarySeriesShape::default(),
                Some(&shapes),
            )
            .is_none(),
            "a sole entity with zero episodes is not a candidate"
        );
    }

    /// A shell sharing the title with a real entity is dropped; the real
    /// entity wins (ADR-0026, amended).
    #[test]
    fn empty_shell_loses_to_a_real_entity() {
        let results = vec![tv(1, "Test Show", 2000), tv(2, "Test Show", 2020)];
        let shapes = [
            CandidateShape {
                year: Some(2000),
                episode_count: Some(0),
                season_count: Some(0),
                season_numbers: None,
                reference_season_episodes: None,
            },
            CandidateShape {
                year: Some(2020),
                episode_count: Some(20),
                season_count: Some(2),
                season_numbers: None,
                reference_season_episodes: None,
            },
        ];
        let m = score_search_with_shape(
            &results,
            "Test Show",
            None,
            SearchKind::Tv,
            LibrarySeriesShape::default(),
            Some(&shapes),
        )
        .unwrap();
        assert_eq!(m.tmdb_id, 2);
        assert!(meets_auto_match_floor(m.confidence));
    }

    /// Empty is the rule, not small: a one-episode entity stays a candidate.
    #[test]
    fn one_episode_entity_is_still_a_candidate() {
        let results = vec![tv(1, "Test Show", 2000)];
        let shapes = [CandidateShape {
            year: Some(2000),
            episode_count: Some(1),
            season_count: Some(1),
            season_numbers: None,
            reference_season_episodes: None,
        }];
        let m = score_search_with_shape(
            &results,
            "Test Show",
            None,
            SearchKind::Tv,
            LibrarySeriesShape::default(),
            Some(&shapes),
        )
        .unwrap();
        assert_eq!(m.tmdb_id, 1);
        assert!(meets_auto_match_floor(m.confidence));
    }

    #[test]
    fn episode_title_pins_unique_match() {
        let a = tv(1, "Shameless", 2011);
        let b = tv(2, "Shameless", 2004);
        let exact = vec![&a, &b];
        let names = vec![
            Some("Pilot".into()),
            Some("I Hate You, Stephen Hawking".into()),
        ];
        let (hit, method) =
            pin_episode_title(&exact, &names, "I Hate You, Stephen Hawking", "test show").unwrap();
        assert_eq!(hit.id, 2);
        assert_eq!(method, "exact_title_episode_title");
    }

    #[test]
    fn episode_title_declines_when_both_match_or_over_cap() {
        let a = tv(1, "Top Gear", 1977);
        let b = tv(2, "Top Gear", 2002);
        let exact = vec![&a, &b];
        let names = vec![Some("Episode 1".into()), Some("Episode 1".into())];
        assert!(pin_episode_title(&exact, &names, "Episode 1", "test show").is_none());

        let many: Vec<SearchHit> = (0..6).map(|i| tv(i, "Show", 2000 + i as i32)).collect();
        let refs: Vec<&SearchHit> = many.iter().collect();
        let names: Vec<_> = (0..6).map(|i| Some(format!("Title {i}"))).collect();
        assert!(pin_episode_title(&refs, &names, "Title 1", "test show").is_none());
    }

    fn cmp(file: &str, provider: &str) -> EpisodeTitleVerdict {
        compare_episode_title(file, provider, "test show", None, None)
    }

    /// Gap 1 — the extractor left a trailing source token on the title.
    #[test]
    fn gap1_trailing_source_token() {
        assert_eq!(
            crate::clean::strip_trailing_source_token("Second Chances - SDTV"),
            "Second Chances"
        );
        assert_eq!(
            crate::clean::strip_trailing_source_token("First Flight - DVD"),
            "First Flight"
        );
        // A real title ending in one of those words is not a source token.
        assert_eq!(
            crate::clean::strip_trailing_source_token("The Tangled Web"),
            "The Tangled Web"
        );
        assert_eq!(
            cmp("Second Chances - SDTV", "Second Chances"),
            EpisodeTitleVerdict::Agree
        );
    }

    /// Gap 2 — the generic-title rule was applied to the filename side only,
    /// so a real title on disk was compared against a provider placeholder.
    #[test]
    fn gap2_provider_placeholder_is_silent_not_different() {
        assert_eq!(
            cmp("A Real Title", "Episode 1"),
            EpisodeTitleVerdict::Unknown
        );
        assert_eq!(
            cmp("Another Real Title", "Episode 1"),
            EpisodeTitleVerdict::Unknown
        );
        // Both generic is still silence, never agreement.
        assert_eq!(cmp("Episode 4", "Episode 4"), EpisodeTitleVerdict::Unknown);
    }

    /// Gap 3 — the provider prefixes every episode with the show's own name.
    #[test]
    fn gap3_show_name_prefixed_provider_title() {
        assert_eq!(
            compare_episode_title(
                "A Real Episode Title",
                "Test Show: A Real Episode Title",
                "test show",
                None,
                None,
            ),
            EpisodeTitleVerdict::Agree
        );
        // The prefix only strips when it is the show's name.
        assert_eq!(
            compare_episode_title(
                "A Real Episode Title",
                "Some Other Show: A Real Episode Title",
                "test show",
                None,
                None,
            ),
            EpisodeTitleVerdict::Unknown
        );
    }

    /// Gap 4 — the episode label is on both sides and was stripped from one.
    #[test]
    fn gap4_episode_label_on_both_sides() {
        assert_eq!(
            cmp("Episode 10 - A Real Title", "Episode 10: A Real Title"),
            EpisodeTitleVerdict::Agree
        );
        assert_eq!(
            cmp("Second Real Title", "Episode Two: Second Real Title"),
            EpisodeTitleVerdict::Agree
        );
    }

    /// Rule 1 — every label in [`ORDINAL_LABELS`], stripped from **both**
    /// sides. Titles are invented; the rule is what is under test, not any
    /// particular show.
    #[test]
    fn ordinal_label_is_stripped_for_every_label_and_on_both_sides() {
        for label in ORDINAL_LABELS {
            // provider carries the label, filename does not
            assert_eq!(
                cmp("A Real Title", &format!("{label} 3: A Real Title")),
                EpisodeTitleVerdict::Agree,
                "provider-side `{label}` was not stripped"
            );
            // filename carries the label, provider does not
            assert_eq!(
                cmp(&format!("{label} 3 - A Real Title"), "A Real Title"),
                EpisodeTitleVerdict::Agree,
                "file-side `{label}` was not stripped"
            );
            // both sides carry it, in the two separator styles
            assert_eq!(
                cmp(
                    &format!("{label} 3 - A Real Title"),
                    &format!("{label} 3: A Real Title")
                ),
                EpisodeTitleVerdict::Agree,
                "`{label}` on both sides did not compare equal"
            );
        }
        // Word ordinals too, since gap 4 shipped with them.
        assert_eq!(
            cmp("Chapter Two - A Real Title", "A Real Title"),
            EpisodeTitleVerdict::Agree
        );
    }

    /// Rule 1, the guard — a label with nothing after it is a placeholder, not
    /// a title, and must not be stripped to the empty string. Two placeholders
    /// on unrelated shows would then compare equal and confirm each other.
    #[test]
    fn an_ordinal_label_with_nothing_after_it_is_not_stripped_to_empty() {
        assert_eq!(
            cmp("Chapter 9 - ", "Week 4 - "),
            EpisodeTitleVerdict::Unknown
        );
        // The shipped generic rejection still owns the bare-placeholder case.
        assert_eq!(
            cmp("Episode 3", "A Real Title"),
            EpisodeTitleVerdict::Unknown
        );
        assert_eq!(
            cmp("A Real Title", "Episode 3"),
            EpisodeTitleVerdict::Unknown
        );
    }

    /// Rule 2 — the one measured spelling variant, and only it. The three
    /// negative cases are the wider scopes this deliberately did not ship;
    /// each was measured to merge titles belonging to different shows.
    #[test]
    fn only_the_measured_spelling_variant_folds() {
        assert_eq!(
            cmp("A Theater Piece", "A Theatre Piece"),
            EpisodeTitleVerdict::Agree
        );
        assert_eq!(
            cmp("A Centre Piece", "A Center Piece"),
            EpisodeTitleVerdict::Unknown
        );
        assert_eq!(
            cmp("A Colour Piece", "A Color Piece"),
            EpisodeTitleVerdict::Unknown
        );
        assert_eq!(
            cmp("A Grey Piece", "A Gray Piece"),
            EpisodeTitleVerdict::Unknown
        );
    }

    /// The two rules compose, and the control does not merge. The control is
    /// the point of this test: two genuinely different titles must stay
    /// different after both rules have run.
    #[test]
    fn the_two_rules_compose_and_different_titles_still_differ() {
        assert_eq!(
            cmp("Night 3 - The Theater Piece", "The Theatre Piece"),
            EpisodeTitleVerdict::Agree
        );
        assert_eq!(
            cmp("Night 3 - A Real Title", "Night 4 - A Wholly Other Title"),
            EpisodeTitleVerdict::Unknown
        );
        assert_eq!(
            cmp("A Real Title", "A Wholly Other Title"),
            EpisodeTitleVerdict::Unknown
        );
    }

    /// Gap 5 — two legitimate title sets for the same episodes. There is no
    /// comparison that recognises this, and none is invented: it must come
    /// out silent rather than different.
    #[test]
    fn gap5_stylised_alias_set_is_silent() {
        assert_eq!(
            cmp("Plain Name", "A STYLISED NAME"),
            EpisodeTitleVerdict::Unknown
        );
        assert_eq!(
            cmp("Second Plain Name", "ANOTHER STYLISED ONE"),
            EpisodeTitleVerdict::Unknown
        );
        // With corroborating air dates that also differ, it may disagree.
        assert_eq!(
            compare_episode_title(
                "Pilot",
                "Something Else",
                "test show",
                Some("2026-03-25"),
                Some("2019-01-02")
            ),
            EpisodeTitleVerdict::Disagree
        );
        // One day apart is a date convention, not a different episode.
        assert_eq!(
            compare_episode_title(
                "Pilot",
                "Something Else",
                "test show",
                Some("2026-03-25"),
                Some("2026-03-26")
            ),
            EpisodeTitleVerdict::Unknown
        );
    }

    /// Q4 — part-number conventions, including the transposition that
    /// accounted for most of the 26 format-difference refutations.
    #[test]
    fn q4_part_number_conventions() {
        assert_eq!(
            cmp("Test Episode (1)", "Test Episode: Part One"),
            EpisodeTitleVerdict::Agree
        );
        assert_eq!(
            cmp(
                "Test Episode (1) - A Second Fragment",
                "Test Episode - A Second Fragment (1)"
            ),
            EpisodeTitleVerdict::Agree
        );
        // Different part numbers are different episodes, and stay unknown
        // rather than agreeing.
        assert_ne!(
            cmp("Test Episode (1)", "Test Episode: Part Two"),
            EpisodeTitleVerdict::Agree
        );
    }

    /// The governing constraint, asserted directly: with nothing to
    /// corroborate a difference, the answer is unknown and never different.
    #[test]
    fn absent_corroboration_never_yields_disagreement() {
        assert_eq!(
            cmp("A Real Title", "A Completely Different Title"),
            EpisodeTitleVerdict::Unknown
        );
        assert_eq!(
            compare_episode_title(
                "A Real Title",
                "A Different One",
                "test show",
                Some("2020-01-01"),
                None
            ),
            EpisodeTitleVerdict::Unknown
        );
    }

    #[test]
    fn floor_constant_matches_adr() {
        assert!((AUTO_MATCH_FLOOR - 0.80).abs() < f64::EPSILON);
    }

    /// The class the API-side year filter costs: a folder whose year names a
    /// short same-year entity while the folder itself is a multi-season run.
    /// Narrowing the search on that year returned only the short entity, so no
    /// scoring rule could reach the right one. Unfiltered, it is at least a
    /// candidate. Anonymous titles per the matcher-fixture convention.
    #[test]
    fn unfiltered_search_keeps_the_candidate_a_year_filter_would_remove() {
        let narrowed = [tv(1, "Test Show", 2003)];
        let unfiltered = vec![
            tv(2, "Test Show", 2004),
            tv(1, "Test Show", 2003),
            tv(3, "Test Show", 1978),
        ];

        assert!(
            !narrowed.iter().any(|h| h.id == 2),
            "the year-narrowed set is exactly the problem: the multi-season \
             entity is absent, so nothing downstream can select it"
        );
        assert!(unfiltered.iter().any(|h| h.id == 2));

        // Necessary and not sufficient: unfiltered, the library-year pin still
        // selects the same-year short entity. Beating that is a later change,
        // and this asserts the state as it is rather than as it should end up.
        let shapes = [
            CandidateShape {
                year: Some(2004),
                episode_count: Some(73),
                season_count: Some(4),
                season_numbers: None,
                reference_season_episodes: None,
            },
            CandidateShape {
                year: Some(2003),
                episode_count: Some(2),
                season_count: Some(1),
                season_numbers: None,
                reference_season_episodes: None,
            },
            CandidateShape {
                year: Some(1978),
                episode_count: Some(24),
                season_count: Some(1),
                season_numbers: None,
                reference_season_episodes: None,
            },
        ];
        let c = score_search_with_shape(
            &unfiltered,
            "Test Show",
            None,
            SearchKind::Tv,
            LibrarySeriesShape {
                year: Some(2003),
                ..Default::default()
            },
            Some(&shapes),
        )
        .expect("a candidate");
        assert_eq!(
            c.tmdb_id, 1,
            "library-year pin still selects the 2003 entity"
        );
        assert_eq!(c.method, "exact_title_library_year");
    }

    fn shape_eps(year: i32, seasons: &[i32], eps: &[(i32, &str)]) -> CandidateShape {
        CandidateShape {
            year: Some(year),
            episode_count: None,
            season_count: Some(seasons.len() as u32),
            season_numbers: Some(seasons.to_vec()),
            reference_season_episodes: Some(eps.iter().map(|(n, t)| (*n, t.to_string())).collect()),
        }
    }

    fn lib_with_ref(year: i32, seasons: &[i32], ref_title: &str) -> LibrarySeriesShape {
        LibrarySeriesShape {
            year: Some(year),
            folder_seasons: seasons.to_vec(),
            ref_season: Some(1),
            ref_episode: Some(2),
            ref_episode_title: Some(ref_title.to_string()),
            ..Default::default()
        }
    }

    /// Confirmation promotes: the year picks a candidate whose reference
    /// episode carries a different title, and exactly one other candidate
    /// carries the folder's.
    #[test]
    fn confirmation_promotes_the_candidate_that_carries_the_title() {
        let hits = vec![tv(1, "Test Show", 2004), tv(2, "Test Show", 2003)];
        let shapes = [
            shape_eps(2004, &[1], &[(2, "A Real Episode Title")]),
            shape_eps(2003, &[1], &[(2, "Something Entirely Different")]),
        ];
        let c = score_search_with_shape(
            &hits,
            "Test Show",
            Some(2003),
            SearchKind::Tv,
            lib_with_ref(2003, &[1], "A Real Episode Title"),
            Some(&shapes),
        )
        .expect("a candidate");
        assert_eq!(c.tmdb_id, 1);
        assert_eq!(c.method, "exact_title_episode_confirmed");
        assert!(meets_auto_match_floor(c.confidence));
    }

    /// Confirmation absent: the year pin holds, unchanged and undemoted.
    #[test]
    fn absent_confirmation_leaves_the_year_pin_alone() {
        let hits = vec![tv(1, "Test Show", 2004), tv(2, "Test Show", 2003)];
        // Neither candidate has episode names fetched.
        let shapes = [shape(2004, &[1]), shape(2003, &[1])];
        let c = score_search_with_shape(
            &hits,
            "Test Show",
            Some(2003),
            SearchKind::Tv,
            lib_with_ref(2003, &[1], "A Real Episode Title"),
            Some(&shapes),
        )
        .expect("a candidate");
        assert_eq!(c.tmdb_id, 2);
        assert_eq!(c.method, "exact_title_year");
        assert!((c.confidence - 0.98).abs() < f64::EPSILON);
    }

    /// Confirmation would promote a wrong candidate, and ambiguity prevents
    /// it: two candidates carry the same episode title, so the evidence does
    /// not discriminate and the year's pick stands.
    #[test]
    fn ambiguous_confirmation_cannot_promote() {
        let hits = vec![
            tv(1, "Test Show", 2004),
            tv(2, "Test Show", 2003),
            tv(3, "Test Show", 2010),
        ];
        let shapes = [
            shape_eps(2004, &[1], &[(2, "A Real Episode Title")]),
            shape_eps(2003, &[1], &[(2, "Something Entirely Different")]),
            shape_eps(2010, &[1], &[(2, "A Real Episode Title")]),
        ];
        let c = score_search_with_shape(
            &hits,
            "Test Show",
            Some(2003),
            SearchKind::Tv,
            lib_with_ref(2003, &[1], "A Real Episode Title"),
            Some(&shapes),
        )
        .expect("a candidate");
        assert_eq!(c.tmdb_id, 2);
        assert_eq!(c.method, "exact_title_year");
    }

    /// Confirmation never lowers. A candidate the scorer picked and that the
    /// titles confirm keeps its score exactly; and a candidate nothing
    /// confirms is not demoted for it.
    #[test]
    fn confirmation_raises_and_never_lowers() {
        let hits = vec![tv(1, "Test Show", 2003), tv(2, "Test Show", 2004)];
        // The year's pick is itself confirmed: nothing moves, score intact.
        let shapes = [
            shape_eps(2003, &[1], &[(2, "A Real Episode Title")]),
            shape_eps(2004, &[1], &[(2, "A Real Episode Title")]),
        ];
        let c = score_search_with_shape(
            &hits,
            "Test Show",
            Some(2003),
            SearchKind::Tv,
            lib_with_ref(2003, &[1], "A Real Episode Title"),
            Some(&shapes),
        )
        .expect("a candidate");
        assert_eq!(c.tmdb_id, 1);
        assert_eq!(c.method, "exact_title_year");
        assert!((c.confidence - 0.98).abs() < f64::EPSILON);

        // A title that matches nothing is not a verdict against anyone.
        assert_eq!(
            candidate_confirms_reference_episode(
                &shape_eps(2003, &[1], &[(2, "Something Entirely Different")]),
                &lib_with_ref(2003, &[1], "A Real Episode Title"),
                "test show",
            ),
            None,
            "the comparator has no Disagree to give on a filename title"
        );
    }

    fn shape(year: i32, seasons: &[i32]) -> CandidateShape {
        CandidateShape {
            year: Some(year),
            episode_count: None,
            season_count: Some(seasons.len() as u32),
            season_numbers: Some(seasons.to_vec()),
            reference_season_episodes: None,
        }
    }

    /// The year is correct and decisive, and must stay decisive: the candidate
    /// it picks also covers the folder, so coverage has nothing to say.
    #[test]
    fn coverage_leaves_a_correct_year_pin_alone() {
        let hits = vec![tv(10, "Test Show", 1998), tv(11, "Test Show", 2017)];
        let shapes = [shape(1998, &[1, 2, 3]), shape(2017, &[1, 2, 3])];
        let c = score_search_with_shape(
            &hits,
            "Test Show",
            Some(1998),
            SearchKind::Tv,
            LibrarySeriesShape {
                year: Some(1998),
                folder_seasons: vec![1, 2, 3],
                ..Default::default()
            },
            Some(&shapes),
        )
        .expect("a candidate");
        assert_eq!(c.tmdb_id, 10);
        assert_eq!(c.method, "exact_title_year");
        assert!((c.confidence - 0.98).abs() < f64::EPSILON);
    }

    /// The year is correct about the year and wrong about the scope: a
    /// two-episode same-year entity against a folder asserting four seasons.
    #[test]
    fn coverage_beats_a_year_pin_that_cannot_hold_the_folder() {
        let hits = vec![
            tv(1, "Test Show", 2004),
            tv(2, "Test Show", 2003),
            tv(3, "Test Show", 1978),
        ];
        let shapes = [
            shape(2004, &[1, 2, 3, 4]),
            shape(2003, &[1]),
            shape(1978, &[1]),
        ];
        let lib = LibrarySeriesShape {
            year: Some(2003),
            folder_seasons: vec![1, 2, 3, 4],
            ..Default::default()
        };
        let c = score_search_with_shape(
            &hits,
            "Test Show",
            Some(2003),
            SearchKind::Tv,
            lib.clone(),
            Some(&shapes),
        )
        .expect("a candidate");
        assert_eq!(c.tmdb_id, 1, "the entity that can hold four seasons wins");
        assert_eq!(c.method, "exact_title_season_coverage");
        assert!(
            meets_auto_match_floor(c.confidence),
            "promotion must stay above the floor, not merely change the pick"
        );

        // Same inputs with no shapes: the year pin stands. Without this the
        // test above could pass for the wrong reason.
        let c2 = score_search_with_shape(&hits, "Test Show", Some(2003), SearchKind::Tv, lib, None)
            .expect("a candidate");
        assert_eq!(c2.tmdb_id, 2);
        assert_eq!(c2.method, "exact_title_year");
    }

    /// Absent data is not a verdict. A candidate with no season list must not
    /// be promoted over the year's pick, and a year pick with no season list
    /// must not be displaced — the failure mode is a check that passes because
    /// it cannot fail.
    #[test]
    fn an_unfetched_season_list_is_no_evidence_in_either_direction() {
        let unknown = CandidateShape {
            year: Some(2004),
            episode_count: None,
            season_count: None,
            season_numbers: None,
            reference_season_episodes: None,
        };
        assert_eq!(
            candidate_covers_folder_seasons(&unknown, &[1, 2, 3, 4]),
            None
        );
        assert_eq!(
            candidate_covers_folder_seasons(&shape(2003, &[1]), &[]),
            None,
            "a folder asserting nothing yields no coverage evidence"
        );
        assert_eq!(
            candidate_covers_folder_seasons(&shape(2004, &[1, 2, 3, 4, 5]), &[1, 2, 3]),
            Some(true),
            "at least, never exactly: a candidate ahead of the library still covers"
        );

        let hits = vec![tv(1, "Test Show", 2004), tv(2, "Test Show", 2003)];
        let lib = LibrarySeriesShape {
            year: Some(2003),
            folder_seasons: vec![1, 2, 3, 4],
            ..Default::default()
        };

        // Alternative unknown, year pick short: nothing to promote to.
        let shapes = [unknown.clone(), shape(2003, &[1])];
        let c = score_search_with_shape(
            &hits,
            "Test Show",
            Some(2003),
            SearchKind::Tv,
            lib.clone(),
            Some(&shapes),
        )
        .expect("a candidate");
        assert_eq!(c.tmdb_id, 2, "an unfetched alternative never promotes");
        assert_eq!(c.method, "exact_title_year");

        // Year pick unknown, alternative covers: the year pick is not displaced
        // on evidence it does not have.
        let shapes = [
            shape(2004, &[1, 2, 3, 4]),
            CandidateShape {
                year: Some(2003),
                ..Default::default()
            },
        ];
        let c = score_search_with_shape(
            &hits,
            "Test Show",
            Some(2003),
            SearchKind::Tv,
            lib,
            Some(&shapes),
        )
        .expect("a candidate");
        assert_eq!(c.tmdb_id, 2, "an unfetched year pick is not demoted");
        assert_eq!(c.method, "exact_title_year");
    }

    /// Two candidates that both cover mean the evidence does not discriminate.
    #[test]
    fn ambiguous_coverage_keeps_the_year_pin() {
        let hits = vec![
            tv(1, "Test Show", 2004),
            tv(2, "Test Show", 2003),
            tv(3, "Test Show", 2010),
        ];
        let shapes = [
            shape(2004, &[1, 2, 3, 4]),
            shape(2003, &[1]),
            shape(2010, &[1, 2, 3, 4]),
        ];
        let c = score_search_with_shape(
            &hits,
            "Test Show",
            Some(2003),
            SearchKind::Tv,
            LibrarySeriesShape {
                year: Some(2003),
                folder_seasons: vec![1, 2, 3, 4],
                ..Default::default()
            },
            Some(&shapes),
        )
        .expect("a candidate");
        assert_eq!(c.tmdb_id, 2);
        assert_eq!(c.method, "exact_title_year");
    }

    /// The counterexample case: does the year still earn its place once it is
    /// no longer a provider filter? It does, as a scoring signal — a genuinely
    /// ambiguous same-name set is still pinned by the folder's year.
    #[test]
    fn year_still_discriminates_as_a_scoring_signal() {
        let hits = vec![tv(10, "Test Show", 1998), tv(11, "Test Show", 2017)];
        let shapes = [
            CandidateShape {
                year: Some(1998),
                ..Default::default()
            },
            CandidateShape {
                year: Some(2017),
                ..Default::default()
            },
        ];
        for (library_year, want) in [(1998, 10), (2017, 11)] {
            let c = score_search_with_shape(
                &hits,
                "Test Show",
                None,
                SearchKind::Tv,
                LibrarySeriesShape {
                    year: Some(library_year),
                    ..Default::default()
                },
                Some(&shapes),
            )
            .expect("a candidate");
            assert_eq!(
                c.tmdb_id, want,
                "folder year {library_year} must still pin its entity"
            );
            assert!(meets_auto_match_floor(c.confidence));
        }
    }

    fn show_meta(title: &str, year: Option<i32>) -> CanonicalMetadata {
        CanonicalMetadata {
            kind: crate::model::MetadataKind::Show,
            title: title.into(),
            original_title: None,
            year,
            air_date: None,
            plot: None,
            genres: Vec::new(),
            runtime_minutes: None,
            cast: Vec::new(),
            ratings: Vec::new(),
            ids: crate::model::ProviderIds {
                tmdb: Some(1),
                tmdb_show: Some(1),
                imdb: None,
                tvdb: None,
            },
            artwork: Vec::new(),
            collection: None,
            season: None,
            episode: None,
        }
    }

    #[test]
    fn find_hit_accepts_matching_name_and_year() {
        assert_eq!(
            find_hit_reject_reason(
                &show_meta("Top Gear", Some(2002)),
                SearchKind::Tv,
                "Top Gear",
                Some(2002)
            ),
            None
        );
    }

    #[test]
    fn find_hit_rejects_wrong_name() {
        let reason = find_hit_reject_reason(
            &show_meta("Wrong Show", Some(2002)),
            SearchKind::Tv,
            "Top Gear",
            Some(2002),
        );
        assert!(reason.is_some(), "a different show must fail into search");
        assert!(reason.unwrap().contains("does not match folder title"));
    }

    #[test]
    fn find_hit_rejects_year_disagreement() {
        let reason = find_hit_reject_reason(
            &show_meta("Top Gear", Some(1977)),
            SearchKind::Tv,
            "Top Gear",
            Some(2002),
        );
        assert!(reason.is_some(), "a same-named different year must fail");
        assert!(reason.unwrap().contains("does not match folder year"));
    }

    #[test]
    fn find_hit_yearless_folder_only_checks_name() {
        assert_eq!(
            find_hit_reject_reason(
                &show_meta("Top Gear", Some(2002)),
                SearchKind::Tv,
                "Top Gear",
                None
            ),
            None,
            "no folder year, so there is no year to disagree on"
        );
    }

    #[test]
    fn find_hit_tv_prefix_name_still_passes() {
        // "The Continental" folder vs TMDB's longer official name — the same
        // TV prefix rule the search scorer uses (ADR-0026 §2).
        assert_eq!(
            find_hit_reject_reason(
                &show_meta("The Continental: From the World of John Wick", Some(2023)),
                SearchKind::Tv,
                "The Continental",
                Some(2023)
            ),
            None
        );
    }
}
