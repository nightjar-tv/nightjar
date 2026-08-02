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

/// Score TMDB search results against a cleaned filename title/year (ADR-0026 table).
pub fn score_search(
    results: &[SearchHit],
    title: &str,
    year: Option<i32>,
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
        let hit = exact[0];
        let conf = if exact.len() == 1 { 0.90 } else { 0.72 };
        (hit, conf, "exact_title")
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
        let results = vec![
            SearchHit {
                id: 37854,
                title: None,
                name: Some("One Piece".into()),
                original_title: None,
                original_name: Some("One Piece".into()),
                release_date: None,
                first_air_date: Some("1999-10-20".into()),
            },
            SearchHit {
                id: 111110,
                title: None,
                name: Some("One Piece".into()),
                original_title: None,
                original_name: Some("One Piece".into()),
                release_date: None,
                first_air_date: Some("2023-08-31".into()),
            },
        ];
        let m = score_search(&results, "One Piece", None, SearchKind::Tv).unwrap();
        assert!((m.confidence - 0.72).abs() < f64::EPSILON);
        assert!(!meets_auto_match_floor(m.confidence));
        assert_eq!(m.method, "exact_title");
    }

    #[test]
    fn floor_constant_matches_adr() {
        assert!((AUTO_MATCH_FLOOR - 0.80).abs() < f64::EPSILON);
    }
}
