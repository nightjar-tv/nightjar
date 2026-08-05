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
    /// ADR-0032 reference episode (usable after-token title only).
    pub ref_season: Option<i32>,
    pub ref_episode: Option<i32>,
    pub ref_episode_title: Option<String>,
}

/// Max multi-exact candidates for the episode-title pin (ADR-0032).
pub const EPISODE_TITLE_TIE_CAP: usize = 5;

/// Per-candidate extras (search year always; counts from `/tv/{id}` detail).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CandidateShape {
    pub year: Option<i32>,
    pub episode_count: Option<u32>,
    pub season_count: Option<u32>,
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

/// ADR-0032 step 4: unique folded match of local reference title vs candidate
/// episode names (parallel to `exact`). Declines when over cap or no unique hit.
pub fn pin_episode_title<'a>(
    exact: &[&'a SearchHit],
    candidate_episode_names: &[Option<String>],
    local_title: &str,
) -> Option<(&'a SearchHit, &'static str)> {
    if exact.len() > EPISODE_TITLE_TIE_CAP || exact.len() != candidate_episode_names.len() {
        return None;
    }
    let want = norm_key(local_title);
    if want.is_empty() {
        return None;
    }
    let mut hit: Option<&SearchHit> = None;
    for (i, h) in exact.iter().enumerate() {
        let Some(name) = candidate_episode_names[i].as_deref() else {
            continue;
        };
        if norm_key(name) == want {
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
    let exact_year: Vec<&SearchHit> = results
        .iter()
        .filter(|r| title_hit(r, &nk, kind) && year.is_some() && row_year(r, kind) == year)
        .collect();
    let exact: Vec<&SearchHit> = results.iter().filter(|r| title_hit(r, &nk, kind)).collect();

    let (hit, conf, method) = if !exact_year.is_empty() {
        let hit = exact_year[0];
        let conf = if exact_year.len() == 1 { 0.98 } else { 0.80 };
        (hit, conf, "exact_title_year")
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
                episode_count: None,
                season_count: None,
            })
            .collect();
        let shapes = match candidate_shapes {
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
        } else if let Some((hit, method)) = pin_collision(&exact, shapes, library) {
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
            },
            CandidateShape {
                year: Some(2025),
                episode_count: Some(8),
                season_count: Some(1),
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
            },
            CandidateShape {
                year: Some(1997),
                episode_count: Some(40),
                season_count: Some(1),
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
            },
            CandidateShape {
                year: Some(2010),
                episode_count: Some(40),
                season_count: Some(1),
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
            },
            CandidateShape {
                year: Some(2026),
                episode_count: Some(12),
                season_count: Some(2),
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
            },
            CandidateShape {
                year: Some(2003),
                episode_count: Some(2),
                season_count: Some(1),
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
            },
            CandidateShape {
                year: Some(2023),
                episode_count: Some(3),
                season_count: Some(1),
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

    #[test]
    fn empty_shell_sole_exact_stays_below_floor() {
        let results = vec![tv(1, "Delta", 2000)];
        let shapes = [CandidateShape {
            year: Some(2000),
            episode_count: Some(0),
            season_count: Some(0),
        }];
        let m = score_search_with_shape(
            &results,
            "Delta",
            None,
            SearchKind::Tv,
            LibrarySeriesShape::default(),
            Some(&shapes),
        )
        .unwrap();
        assert_eq!(m.method, "exact_title_empty_shell");
        assert!(!meets_auto_match_floor(m.confidence));
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
            pin_episode_title(&exact, &names, "I Hate You, Stephen Hawking").unwrap();
        assert_eq!(hit.id, 2);
        assert_eq!(method, "exact_title_episode_title");
    }

    #[test]
    fn episode_title_declines_when_both_match_or_over_cap() {
        let a = tv(1, "Top Gear", 1977);
        let b = tv(2, "Top Gear", 2002);
        let exact = vec![&a, &b];
        let names = vec![Some("Episode 1".into()), Some("Episode 1".into())];
        assert!(pin_episode_title(&exact, &names, "Episode 1").is_none());

        let many: Vec<SearchHit> = (0..6).map(|i| tv(i, "Show", 2000 + i as i32)).collect();
        let refs: Vec<&SearchHit> = many.iter().collect();
        let names: Vec<_> = (0..6).map(|i| Some(format!("Title {i}"))).collect();
        assert!(pin_episode_title(&refs, &names, "Title 1").is_none());
    }

    #[test]
    fn floor_constant_matches_adr() {
        assert!((AUTO_MATCH_FLOOR - 0.80).abs() < f64::EPSILON);
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
