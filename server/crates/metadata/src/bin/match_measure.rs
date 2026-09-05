//! Search-only match-rate measure across dogfood `media_items` (ADR-0026 floor).
//!
//! Env: `DB`, TMDB credentials. Optional `EXCLUDE_TESTDATA=1` applies
//! `MEASURE_EXCLUDE_LIBRARY_NAMES` (dogfood default Test Data,DV,DV2).
//!
//! An empty population is not a perfect score (Rule 4.15). When exclusions
//! leave zero items the run exits non-zero instead of printing a report whose
//! fractions all read as the best possible result. A fraction over an empty
//! population serialises as `null`, never as `0.0`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use nightjar_metadata::{
    AUTO_MATCH_FLOOR, LibrarySeriesShape, SearchKind, TmdbClient, clean_movie_title,
    clean_show_title, meets_auto_match_floor, pick_reference_episode, resolve_credentials,
    series_library_year, year_from_path,
};
use rusqlite::Connection;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum QueryKind {
    Movie,
    Tv,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct QueryKey {
    kind: QueryKind,
    title: String,
    year: Option<i32>,
    library_year: Option<i32>,
    episode_count: Option<u32>,
    season_count: Option<u32>,
    ref_season: Option<i32>,
    ref_episode: Option<i32>,
    ref_episode_title: Option<String>,
}

#[derive(Debug, Clone)]
struct EpisodeRow {
    id: i64,
    title: String,
    year: Option<i32>,
    path: String,
    season: Option<i32>,
    episode: Option<i32>,
    library_id: i64,
}

#[derive(Debug, Serialize)]
struct BucketStats {
    items: usize,
    matched: usize,
    below_threshold: usize,
    no_results: usize,
    errors: usize,
    /// `null` when the bucket is empty; a fake `0.0` would read as "perfect".
    match_rate: Option<f64>,
    /// `null` when the bucket is empty; a fake `0.0` would read as "perfect".
    below_threshold_fraction: Option<f64>,
}

#[derive(Debug, Serialize)]
struct ExclusionStat {
    name: String,
    items_removed: usize,
}

#[derive(Debug, Serialize)]
struct Report {
    floor: f64,
    exclude_testdata: bool,
    /// Resolved exclusion names actually applied, each with the number of
    /// media items it removed. Names that matched no library read 0.
    exclusions: Vec<ExclusionStat>,
    total_items: usize,
    unique_queries: usize,
    movies: BucketStats,
    episodes: BucketStats,
    /// `null` when nothing was measured; a fake `0.0` would read as "perfect".
    fragile_watch_state_fraction: Option<f64>,
    combined: BucketStats,
    elapsed_secs: f64,
    note: String,
}

/// Per-kind item counts before they are turned into report fractions.
#[derive(Debug, Clone, Copy)]
struct BucketCounts {
    items: usize,
    matched: usize,
    below_threshold: usize,
    no_results: usize,
    errors: usize,
}

impl BucketCounts {
    fn to_stats(self) -> BucketStats {
        BucketStats {
            items: self.items,
            matched: self.matched,
            below_threshold: self.below_threshold,
            no_results: self.no_results,
            errors: self.errors,
            match_rate: ratio(self.matched, self.items),
            below_threshold_fraction: ratio(self.below_threshold, self.items),
        }
    }
}

/// `part / whole`, or `null` when the population is empty. An empty population
/// has no observed fraction; reporting `0.0` would claim a measurement.
fn ratio(part: usize, whole: usize) -> Option<f64> {
    if whole == 0 {
        None
    } else {
        Some(part as f64 / whole as f64)
    }
}

fn exclusion_stats(applied: &[String], removed: &HashMap<String, usize>) -> Vec<ExclusionStat> {
    applied
        .iter()
        .map(|name| ExclusionStat {
            name: name.clone(),
            items_removed: removed.get(name).copied().unwrap_or(0),
        })
        .collect()
}

fn make_report(
    exclude_testdata: bool,
    exclusions: Vec<ExclusionStat>,
    unique_queries: usize,
    movies: BucketCounts,
    episodes: BucketCounts,
    elapsed_secs: f64,
) -> Report {
    let total_items = movies.items + episodes.items;
    let combined = BucketCounts {
        items: total_items,
        matched: movies.matched + episodes.matched,
        below_threshold: movies.below_threshold + episodes.below_threshold,
        no_results: movies.no_results + episodes.no_results,
        errors: movies.errors + episodes.errors,
    };
    Report {
        floor: AUTO_MATCH_FLOOR,
        exclude_testdata,
        exclusions,
        total_items,
        unique_queries,
        movies: movies.to_stats(),
        episodes: episodes.to_stats(),
        fragile_watch_state_fraction: ratio(combined.below_threshold, total_items),
        combined: combined.to_stats(),
        elapsed_secs,
        note: "Collision pin: year → episode_count → season_count (unique). Cleaner folds and/&, apostrophes, colons, diacritics."
            .into(),
    }
}

/// Everything the run learned from the database before any provider call.
struct Population {
    movies: Vec<(i64, String, Option<i32>, String)>,
    episodes: Vec<EpisodeRow>,
    /// Media items seen before exclusion (excluded + kept).
    before_exclusion: usize,
    /// Resolved exclusion names actually applied, in resolution order.
    exclusion_names: Vec<String>,
    /// Media items removed per excluded library name.
    exclusion_removed: HashMap<String, usize>,
    /// Every library name present in the database, sorted.
    libraries_in_db: Vec<String>,
}

fn scan_population(con: &Connection, exclude_testdata: bool) -> Population {
    let exclusion_names: Vec<String> = if exclude_testdata {
        nightjar_metadata::measure_exclude_library_names()
    } else {
        Vec::new()
    };
    if !exclusion_names.is_empty() {
        eprintln!("EXCLUDE_TESTDATA=1: skipping {exclusion_names:?}");
    }

    let excluded_by_id: HashMap<i64, String> = if exclusion_names.is_empty() {
        HashMap::new()
    } else {
        let in_list = nightjar_metadata::measure_exclude_libraries_sql_in(&exclusion_names);
        let mut stmt = con
            .prepare(&format!(
                "SELECT id, name FROM libraries WHERE name IN ({in_list})"
            ))
            .unwrap();
        stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };

    let mut libraries_in_db = Vec::new();
    {
        let mut stmt = con
            .prepare("SELECT name FROM libraries ORDER BY name")
            .unwrap();
        for row in stmt.query_map([], |r| r.get::<_, String>(0)).unwrap() {
            libraries_in_db.push(row.unwrap());
        }
    }

    let mut exclusion_removed: HashMap<String, usize> = HashMap::new();
    let mut movies = Vec::new();
    let mut episodes = Vec::new();
    let mut movies_seen = 0usize;
    let mut episodes_seen = 0usize;

    {
        let mut stmt = con
            .prepare(
                "SELECT id, title, year, path, library_id FROM media_items WHERE kind = 'movie'",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<i32>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })
            .unwrap();
        for row in rows {
            movies_seen += 1;
            let (id, title, year, path, lib) = row.unwrap();
            if let Some(name) = excluded_by_id.get(&lib) {
                *exclusion_removed.entry(name.clone()).or_default() += 1;
            } else {
                movies.push((id, title, year, path));
            }
        }
    }
    {
        let mut stmt = con
            .prepare(
                "SELECT id, title, year, path, season, episode, library_id
                 FROM media_items WHERE kind = 'episode'",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok(EpisodeRow {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    year: r.get(2)?,
                    path: r.get(3)?,
                    season: r.get(4)?,
                    episode: r.get(5)?,
                    library_id: r.get(6)?,
                })
            })
            .unwrap();
        for row in rows {
            let row = row.unwrap();
            episodes_seen += 1;
            if let Some(name) = excluded_by_id.get(&row.library_id) {
                *exclusion_removed.entry(name.clone()).or_default() += 1;
            } else {
                episodes.push(row);
            }
        }
    }

    Population {
        movies,
        episodes,
        before_exclusion: movies_seen + episodes_seen,
        exclusion_names,
        exclusion_removed,
        libraries_in_db,
    }
}

fn empty_population_message(pop: &Population) -> String {
    let removed_lines: Vec<String> = if pop.exclusion_names.is_empty() {
        vec!["  (none applied)".to_string()]
    } else {
        pop.exclusion_names
            .iter()
            .map(|name| {
                format!(
                    "  {name}: {} item(s) removed",
                    pop.exclusion_removed.get(name).copied().unwrap_or(0)
                )
            })
            .collect()
    };
    format!(
        "match_measure: measured nothing — 0 media items remain after exclusions; a report of nothing is not a report\n  items before exclusions: {}\n  exclusions applied: {:?}\n  removed per applied exclusion:\n{}\n  libraries in db: {:?}",
        pop.before_exclusion,
        pop.exclusion_names,
        removed_lines.join("\n"),
        pop.libraries_in_db,
    )
}

fn main() {
    let exclude_testdata = std::env::var("EXCLUDE_TESTDATA").ok().as_deref() == Some("1");
    let db = std::env::var("DB").map(PathBuf::from).unwrap_or_else(|_| {
        dirs_home()
            .map(|h| h.join("nightjar-data/nightjar.db"))
            .expect("HOME")
    });

    let con = Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap_or_else(|e| panic!("open {}: {e}", db.display()));

    let population = scan_population(&con, exclude_testdata);
    if population.movies.is_empty() && population.episodes.is_empty() {
        eprintln!("{}", empty_population_message(&population));
        std::process::exit(1);
    }

    let creds = resolve_credentials().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    let client = TmdbClient::new(creds);

    let exclusions = exclusion_stats(&population.exclusion_names, &population.exclusion_removed);
    let movies = population.movies;
    let episodes = population.episodes;

    let mut movie_groups: HashMap<QueryKey, Vec<i64>> = HashMap::new();
    for (id, title, year, path) in &movies {
        let folder_year = year_from_path(path);
        let (ct, cy) = clean_movie_title(title, folder_year.or(*year));
        let key = QueryKey {
            kind: QueryKind::Movie,
            title: ct,
            year: cy,
            library_year: None,
            episode_count: None,
            season_count: None,
            ref_season: None,
            ref_episode: None,
            ref_episode_title: None,
        };
        movie_groups.entry(key).or_default().push(*id);
    }

    let mut ep_raw: HashMap<String, Vec<EpisodeRow>> = HashMap::new();
    for row in episodes {
        let (ct, _) = clean_show_title(&row.title);
        ep_raw.entry(ct).or_default().push(row);
    }

    let mut ep_groups: HashMap<QueryKey, Vec<i64>> = HashMap::new();
    let mut episode_total = 0usize;
    for (ct, rows) in ep_raw {
        episode_total += rows.len();
        let years = rows.iter().map(|r| r.year);
        let path0 = rows[0].path.clone();
        let library_year = series_library_year(years, &path0, "");
        let episode_count = Some(rows.len() as u32);
        let seasons: HashSet<i32> = rows.iter().filter_map(|r| r.season).collect();
        let season_count = (!seasons.is_empty()).then_some(seasons.len() as u32);
        let ep_triples: Vec<(i32, i32, &str)> = rows
            .iter()
            .filter_map(|r| {
                Some((
                    r.season?,
                    r.episode?,
                    std::path::Path::new(&r.path)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(r.path.as_str()),
                ))
            })
            .collect();
        let ref_ep = pick_reference_episode(&ep_triples, &ct);
        let key = QueryKey {
            kind: QueryKind::Tv,
            title: ct,
            year: None,
            library_year,
            episode_count,
            season_count,
            ref_season: ref_ep.as_ref().map(|p| p.0),
            ref_episode: ref_ep.as_ref().map(|p| p.1),
            ref_episode_title: ref_ep.map(|p| p.2),
        };
        ep_groups
            .entry(key)
            .or_default()
            .extend(rows.into_iter().map(|r| r.id));
    }

    let unique = movie_groups.len() + ep_groups.len();
    eprintln!(
        "items movies={} episodes={} unique_queries={} floor={} exclude_testdata={}",
        movies.len(),
        episode_total,
        unique,
        AUTO_MATCH_FLOOR,
        exclude_testdata
    );

    let started = Instant::now();
    let mut done = 0usize;

    let mut score_group = |key: &QueryKey,
                           ids: &[i64],
                           matched: &mut usize,
                           below: &mut usize,
                           miss: &mut usize,
                           errors: &mut usize| {
        done += 1;
        if done.is_multiple_of(50) || done == unique {
            eprintln!("  searched {done}/{unique} …");
        }
        let kind = match key.kind {
            QueryKind::Movie => SearchKind::Movie,
            QueryKind::Tv => SearchKind::Tv,
        };
        let n = ids.len();
        let library = LibrarySeriesShape {
            folder_seasons: Vec::new(),
            year: key.library_year,
            episode_count: key.episode_count,
            season_count: key.season_count,
            ref_season: key.ref_season,
            ref_episode: key.ref_episode,
            ref_episode_title: key.ref_episode_title.clone(),
            // Measurement bin: the folder's per-season shape is not plumbed
            // here. Empty is "no evidence", never "no seasons".
            folder_season_counts: Vec::new(),
            folder_episode_titles: Vec::new(),
        };
        match client.match_search_with_series_shape(kind, &key.title, key.year, library) {
            Ok(Some(c)) if meets_auto_match_floor(c.confidence) => *matched += n,
            Ok(Some(_)) => *below += n,
            Ok(None) => *miss += n,
            Err(e) => {
                eprintln!("  error {:?} {}: {e}", key.kind, key.title);
                *errors += n;
            }
        }
    };

    let mut m_matched = 0usize;
    let mut m_below = 0usize;
    let mut m_miss = 0usize;
    let mut m_err = 0usize;
    for (key, ids) in &movie_groups {
        score_group(
            key,
            ids,
            &mut m_matched,
            &mut m_below,
            &mut m_miss,
            &mut m_err,
        );
    }

    let mut e_matched = 0usize;
    let mut e_below = 0usize;
    let mut e_miss = 0usize;
    let mut e_err = 0usize;
    for (key, ids) in &ep_groups {
        score_group(
            key,
            ids,
            &mut e_matched,
            &mut e_below,
            &mut e_miss,
            &mut e_err,
        );
    }

    let movie_counts = BucketCounts {
        items: movies.len(),
        matched: m_matched,
        below_threshold: m_below,
        no_results: m_miss,
        errors: m_err,
    };
    let episode_counts = BucketCounts {
        items: episode_total,
        matched: e_matched,
        below_threshold: e_below,
        no_results: e_miss,
        errors: e_err,
    };

    let report = make_report(
        exclude_testdata,
        exclusions,
        unique,
        movie_counts,
        episode_counts,
        started.elapsed().as_secs_f64(),
    );

    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(items: usize, matched: usize, below: usize) -> BucketCounts {
        BucketCounts {
            items,
            matched,
            below_threshold: below,
            no_results: 0,
            errors: 0,
        }
    }

    #[test]
    fn empty_population_fractions_are_null_not_zero() {
        let report = make_report(false, Vec::new(), 0, counts(0, 0, 0), counts(0, 0, 0), 0.0);
        assert_eq!(report.total_items, 0);
        assert_eq!(report.movies.match_rate, None);
        assert_eq!(report.movies.below_threshold_fraction, None);
        assert_eq!(report.episodes.match_rate, None);
        assert_eq!(report.episodes.below_threshold_fraction, None);
        assert_eq!(report.combined.match_rate, None);
        assert_eq!(report.combined.below_threshold_fraction, None);
        assert_eq!(report.fragile_watch_state_fraction, None);

        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"match_rate\":null"), "json: {json}");
        assert!(
            json.contains("\"below_threshold_fraction\":null"),
            "json: {json}"
        );
        assert!(
            json.contains("\"fragile_watch_state_fraction\":null"),
            "json: {json}"
        );
        assert!(!json.contains("\"match_rate\":0.0"), "json: {json}");
        assert!(
            !json.contains("\"below_threshold_fraction\":0.0"),
            "json: {json}"
        );
        assert!(
            !json.contains("\"fragile_watch_state_fraction\":0.0"),
            "json: {json}"
        );
    }

    #[test]
    fn one_below_threshold_item_still_reports_real_fraction() {
        let report = make_report(false, Vec::new(), 1, counts(1, 0, 1), counts(0, 0, 0), 0.0);
        assert_eq!(report.total_items, 1);
        assert_eq!(report.movies.below_threshold_fraction, Some(1.0));
        assert_eq!(report.movies.match_rate, Some(0.0));
        assert_eq!(report.fragile_watch_state_fraction, Some(1.0));
        assert_eq!(report.combined.below_threshold_fraction, Some(1.0));

        let json = serde_json::to_string(&report).unwrap();
        assert!(
            json.contains("\"below_threshold_fraction\":1.0"),
            "json: {json}"
        );
        assert!(
            json.contains("\"fragile_watch_state_fraction\":1.0"),
            "json: {json}"
        );
    }

    #[test]
    fn exclusion_stats_carry_applied_names_in_order_with_removed_counts() {
        let applied = vec!["Test Data".to_string(), "DV".to_string()];
        let removed = HashMap::from([("DV".to_string(), 12)]);
        let stats = exclusion_stats(&applied, &removed);
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].name, "Test Data");
        assert_eq!(stats[0].items_removed, 0);
        assert_eq!(stats[1].name, "DV");
        assert_eq!(stats[1].items_removed, 12);
    }

    #[test]
    fn report_serialises_exclusion_removal_counts() {
        let report = make_report(
            true,
            vec![ExclusionStat {
                name: "DV".into(),
                items_removed: 3,
            }],
            1,
            counts(1, 0, 0),
            counts(0, 0, 0),
            0.0,
        );
        let json = serde_json::to_string(&report).unwrap();
        assert!(
            json.contains("\"exclusions\":[{\"name\":\"DV\",\"items_removed\":3}]"),
            "json: {json}"
        );
    }
}
