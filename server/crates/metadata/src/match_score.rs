//! Search confidence scoring (ADR-0026 §2). Floor is 0.80.
//!
//! Multi-exact collisions use one pin rule: the first discriminator that
//! selects exactly one candidate lifts above the floor; otherwise stay 0.72.

use serde::{Deserialize, Serialize};

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
}

/// Library-side series shape for collision pins (TV).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LibrarySeriesShape {
    /// Premiere year: earliest episode year, else show-folder `(YYYY)`.
    pub year: Option<i32>,
    /// Distinct episode files under the show.
    pub episode_count: Option<u32>,
    /// Distinct season numbers present (excludes null).
    pub season_count: Option<u32>,
}

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

fn title_hit(hit: &SearchHit, query_norm: &str, kind: SearchKind) -> bool {
    let (primary, original) = match kind {
        SearchKind::Movie => (hit.title.as_deref(), hit.original_title.as_deref()),
        SearchKind::Tv => (hit.name.as_deref(), hit.original_name.as_deref()),
    };
    primary.is_some_and(|t| norm_key(t) == query_norm)
        || original.is_some_and(|t| norm_key(t) == query_norm)
}

/// Soft episode-count match: absolute or proportional slack so incomplete
/// libraries (311 vs 327) still pin when only one candidate is close.
fn episode_count_close(library: u32, candidate: u32) -> bool {
    let diff = library.abs_diff(candidate);
    let tol = (candidate as f64 * 0.15).ceil() as u32;
    diff <= tol.max(5)
}

/// First discriminator that selects exactly one of `exact` wins.
/// Order: premiere year → episode count → season count (exact).
fn pin_collision<'a>(
    exact: &[&'a SearchHit],
    shapes: &[CandidateShape],
    library: LibrarySeriesShape,
) -> Option<(&'a SearchHit, &'static str)> {
    debug_assert_eq!(exact.len(), shapes.len());

    let try_pin = |pred: &dyn Fn(usize) -> bool, method: &'static str| {
        let mut hit: Option<&SearchHit> = None;
        for (i, h) in exact.iter().enumerate() {
            if pred(i) {
                if hit.is_some() {
                    return None; // two+
                }
                hit = Some(*h);
            }
        }
        hit.map(|h| (h, method))
    };

    if let Some(ly) = library.year
        && let Some(p) = try_pin(&|i| shapes[i].year == Some(ly), "exact_title_library_year")
    {
        return Some(p);
    }
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
    None
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
        if exact.len() == 1 {
            (exact[0], 0.90, "exact_title")
        } else {
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
            if let Some((hit, method)) = pin_collision(&exact, shapes, library) {
                (hit, 0.90, method)
            } else {
                (exact[0], 0.72, "exact_title_collision_unpinned")
            }
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

/// Whether multi-exact scoring needs `/tv/{id}` detail for count pins.
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
    if library.episode_count.is_none() && library.season_count.is_none() {
        return false;
    }
    let Some(c) = score_search_with_shape(results, title, year, kind, library, None) else {
        return false;
    };
    // Still below floor after year-only pin → fetch detail counts.
    c.confidence < AUTO_MATCH_FLOOR && c.method == "exact_title_collision_unpinned"
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
            },
            Some(&shapes),
        )
        .unwrap();
        assert!((m.confidence - 0.72).abs() < f64::EPSILON);
        assert_eq!(m.method, "exact_title_collision_unpinned");
    }

    #[test]
    fn floor_constant_matches_adr() {
        assert!((AUTO_MATCH_FLOOR - 0.80).abs() < f64::EPSILON);
    }
}
