//! Search confidence scoring (ADR-0026 §2). Floor is 0.80.

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

#[derive(Debug, Clone, PartialEq)]
pub struct MatchCandidate {
    pub tmdb_id: i64,
    pub confidence: f64,
    pub method: &'static str,
    pub result_title: Option<String>,
    pub result_year: Option<i32>,
    pub n_results: usize,
}

/// Normalise a title for exact comparison (spike `norm_key`).
pub fn norm_key(s: &str) -> String {
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

/// Score TMDB search results (ADR-0026 table + library-year pin for TV multi-exact).
///
/// `year` is the query/filename year when present. `library_year` is the series
/// premiere year known from the library (earliest episode year, else show-folder
/// `(YYYY)`). On multi exact-title hits, exactly one candidate whose
/// `first_air_date` year equals `library_year` lifts above the floor; zero or
/// two+ pins stay at 0.72 (One Piece guard).
pub fn score_search(
    results: &[SearchHit],
    title: &str,
    year: Option<i32>,
    kind: SearchKind,
) -> Option<MatchCandidate> {
    score_search_with_library_year(results, title, year, None, kind)
}

pub fn score_search_with_library_year(
    results: &[SearchHit],
    title: &str,
    year: Option<i32>,
    library_year: Option<i32>,
    kind: SearchKind,
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
        } else if let Some(ly) = library_year {
            let pinned: Vec<&SearchHit> = exact
                .iter()
                .copied()
                .filter(|r| row_year(r, kind) == Some(ly))
                .collect();
            if pinned.len() == 1 {
                // Multi-exact disambiguated by library premiere year.
                (pinned[0], 0.90, "exact_title_library_year")
            } else {
                // Pins nothing or ≥2 — stay unmatched (floor guard).
                (exact[0], 0.72, "exact_title")
            }
        } else {
            (exact[0], 0.72, "exact_title")
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
        assert_eq!(m.method, "exact_title");
    }

    #[test]
    fn library_year_pins_unique_multi_exact_above_floor() {
        let results = vec![tv(37854, "One Piece", 1999), tv(111110, "One Piece", 2023)];
        let m =
            score_search_with_library_year(&results, "One Piece", None, Some(1999), SearchKind::Tv)
                .unwrap();
        assert_eq!(m.tmdb_id, 37854);
        assert!((m.confidence - 0.90).abs() < f64::EPSILON);
        assert!(meets_auto_match_floor(m.confidence));
        assert_eq!(m.method, "exact_title_library_year");
    }

    #[test]
    fn library_year_wrong_side_still_pins_that_year_only() {
        // If library year were 2023 (mis-tagged), pin live-action — caller's
        // year source must be trustworthy; scorer only counts the pin.
        let results = vec![tv(37854, "One Piece", 1999), tv(111110, "One Piece", 2023)];
        let m =
            score_search_with_library_year(&results, "One Piece", None, Some(2023), SearchKind::Tv)
                .unwrap();
        assert_eq!(m.tmdb_id, 111110);
        assert_eq!(m.method, "exact_title_library_year");
    }

    #[test]
    fn library_year_pins_nothing_stays_0_72() {
        let results = vec![tv(1, "Bones", 2005), tv(2, "Bones", 2019)];
        let m = score_search_with_library_year(&results, "Bones", None, Some(1990), SearchKind::Tv)
            .unwrap();
        assert!((m.confidence - 0.72).abs() < f64::EPSILON);
        assert_eq!(m.method, "exact_title");
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
        assert!(!meets_auto_match_floor(m.confidence));
    }

    #[test]
    fn floor_constant_matches_adr() {
        assert!((AUTO_MATCH_FLOOR - 0.80).abs() < f64::EPSILON);
    }
}
