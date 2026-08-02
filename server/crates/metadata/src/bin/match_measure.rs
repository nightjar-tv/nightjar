//! Search-only match-rate measure across dogfood `media_items` (ADR-0026 floor).
//!
//! Env: `DB`, TMDB credentials. Optional `EXCLUDE_TESTDATA=1` drops the
//! `Test Data` library from movie counts.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use nightjar_metadata::{
    AUTO_MATCH_FLOOR, LibrarySeriesShape, SearchKind, TmdbClient, TmdbCredentials,
    clean_movie_title, clean_show_title, meets_auto_match_floor, series_library_year,
    year_from_path,
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
}

#[derive(Debug, Clone)]
struct EpisodeRow {
    id: i64,
    title: String,
    year: Option<i32>,
    path: String,
    season: Option<i32>,
}

#[derive(Debug, Serialize)]
struct BucketStats {
    items: usize,
    matched: usize,
    below_threshold: usize,
    no_results: usize,
    errors: usize,
    match_rate: f64,
    below_threshold_fraction: f64,
}

#[derive(Debug, Serialize)]
struct Report {
    floor: f64,
    exclude_testdata: bool,
    total_items: usize,
    unique_queries: usize,
    movies: BucketStats,
    episodes: BucketStats,
    fragile_watch_state_fraction: f64,
    combined: BucketStats,
    elapsed_secs: f64,
    note: String,
}

fn main() {
    let exclude_testdata = std::env::var("EXCLUDE_TESTDATA").ok().as_deref() == Some("1");
    let db = std::env::var("DB").map(PathBuf::from).unwrap_or_else(|_| {
        dirs_home()
            .map(|h| h.join("nightjar-data/nightjar.db"))
            .expect("HOME")
    });
    let creds = TmdbCredentials::from_env().unwrap_or_else(|| {
        eprintln!("no TMDB credentials");
        std::process::exit(1);
    });
    let client = TmdbClient::new(creds);

    let con = Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap_or_else(|e| panic!("open {}: {e}", db.display()));

    let mut testdata_libs = HashSet::new();
    if exclude_testdata {
        let mut stmt = con
            .prepare("SELECT id FROM libraries WHERE name = 'Test Data'")
            .unwrap();
        for id in stmt.query_map([], |r| r.get::<_, i64>(0)).unwrap() {
            testdata_libs.insert(id.unwrap());
        }
    }

    let mut movies: Vec<(i64, String, Option<i32>, String)> = Vec::new();
    let mut episodes: Vec<EpisodeRow> = Vec::new();

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
            let (id, title, year, path, lib) = row.unwrap();
            if testdata_libs.contains(&lib) {
                continue;
            }
            movies.push((id, title, year, path));
        }
    }
    {
        let mut stmt = con
            .prepare("SELECT id, title, year, path, season FROM media_items WHERE kind = 'episode'")
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok(EpisodeRow {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    year: r.get(2)?,
                    path: r.get(3)?,
                    season: r.get(4)?,
                })
            })
            .unwrap();
        for row in rows {
            episodes.push(row.unwrap());
        }
    }

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
        let library_year = series_library_year(years, &path0);
        let episode_count = Some(rows.len() as u32);
        let seasons: HashSet<i32> = rows.iter().filter_map(|r| r.season).collect();
        let season_count = (!seasons.is_empty()).then_some(seasons.len() as u32);
        let key = QueryKey {
            kind: QueryKind::Tv,
            title: ct,
            year: None,
            library_year,
            episode_count,
            season_count,
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
            year: key.library_year,
            episode_count: key.episode_count,
            season_count: key.season_count,
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

    let movies_n = movies.len();
    let episodes_n = episode_total;
    let total = movies_n + episodes_n;
    let c_matched = m_matched + e_matched;
    let c_below = m_below + e_below;
    let c_miss = m_miss + e_miss;
    let c_err = m_err + e_err;

    let bucket =
        |items: usize, matched: usize, below: usize, miss: usize, errors: usize| BucketStats {
            items,
            matched,
            below_threshold: below,
            no_results: miss,
            errors,
            match_rate: if items == 0 {
                0.0
            } else {
                matched as f64 / items as f64
            },
            below_threshold_fraction: if items == 0 {
                0.0
            } else {
                below as f64 / items as f64
            },
        };

    let report = Report {
        floor: AUTO_MATCH_FLOOR,
        exclude_testdata,
        total_items: total,
        unique_queries: unique,
        movies: bucket(movies_n, m_matched, m_below, m_miss, m_err),
        episodes: bucket(episodes_n, e_matched, e_below, e_miss, e_err),
        fragile_watch_state_fraction: if total == 0 {
            0.0
        } else {
            c_below as f64 / total as f64
        },
        combined: bucket(total, c_matched, c_below, c_miss, c_err),
        elapsed_secs: started.elapsed().as_secs_f64(),
        note: "Collision pin: year → episode_count → season_count (unique). Cleaner folds and/&, apostrophes, colons, diacritics."
            .into(),
    };

    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}
