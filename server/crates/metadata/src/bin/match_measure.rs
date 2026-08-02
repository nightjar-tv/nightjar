//! Search-only match-rate measure across dogfood `media_items` (ADR-0026 floor).
//!
//! Runs TMDB `/search/{movie,tv}` + confidence scoring. Does **not** fetch
//! detail (measure is the floor gate, not payload size). Episode rows share one
//! search per unique cleaned show query; rates are expanded to item counts.
//!
//! Usage:
//!   cargo run -p nightjar-metadata --release --bin metadata-match-measure
//!
//! Env: `DB` (default `~/nightjar-data/nightjar.db`), TMDB credentials as in
//! [`nightjar_metadata::TmdbCredentials`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use nightjar_metadata::{
    AUTO_MATCH_FLOOR, SearchKind, TmdbClient, TmdbCredentials, clean_movie_title, clean_show_title,
    meets_auto_match_floor, series_library_year, year_from_path,
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
    /// Query/filename year (movies; rare on show titles).
    year: Option<i32>,
    /// Series premiere year from library (TV only).
    library_year: Option<i32>,
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
    total_items: usize,
    unique_queries: usize,
    movies: BucketStats,
    episodes: BucketStats,
    /// Episode items matched via the library-year pin (scorer change).
    episode_library_year_pin_items: usize,
    /// Below-threshold items / total — fragile path-key watch state (ADR-0025/0026).
    fragile_watch_state_fraction: f64,
    combined: BucketStats,
    elapsed_secs: f64,
    note: String,
}

fn main() {
    let db = std::env::var("DB").map(PathBuf::from).unwrap_or_else(|_| {
        dirs_home()
            .map(|h| h.join("nightjar-data/nightjar.db"))
            .expect("HOME")
    });
    let creds = TmdbCredentials::from_env().unwrap_or_else(|| {
        eprintln!("no TMDB credentials (NIGHTJAR_TMDB_API_KEY / ~/.config/nightjar/tmdb_secret)");
        std::process::exit(1);
    });
    let client = TmdbClient::new(creds);

    let con = Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap_or_else(|e| panic!("open {}: {e}", db.display()));

    let mut movies: Vec<(i64, String, Option<i32>, String)> = Vec::new();
    let mut episodes: Vec<(i64, String, Option<i32>, String)> = Vec::new();

    {
        let mut stmt = con
            .prepare("SELECT id, title, year, path FROM media_items WHERE kind = 'movie'")
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<i32>>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .unwrap();
        for row in rows {
            movies.push(row.unwrap());
        }
    }
    {
        let mut stmt = con
            .prepare("SELECT id, title, year, path FROM media_items WHERE kind = 'episode'")
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<i32>>(2)?,
                    r.get::<_, String>(3)?,
                ))
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
        };
        movie_groups.entry(key).or_default().push(*id);
    }

    // Group episodes by cleaned show title; library year from min(episode.year)
    // else show-folder (YYYY).
    let mut ep_raw: HashMap<String, Vec<(i64, Option<i32>, String)>> = HashMap::new();
    for (id, title, year, path) in &episodes {
        let (ct, _cy) = clean_show_title(title);
        ep_raw
            .entry(ct)
            .or_default()
            .push((*id, *year, path.clone()));
    }

    let mut ep_groups: HashMap<QueryKey, Vec<i64>> = HashMap::new();
    for (ct, rows) in ep_raw {
        let years = rows.iter().map(|(_, y, _)| *y);
        let path0 = &rows[0].2;
        let library_year = series_library_year(years, path0);
        let key = QueryKey {
            kind: QueryKind::Tv,
            title: ct,
            year: None, // don't pass folder year as API search filter
            library_year,
        };
        ep_groups
            .entry(key)
            .or_default()
            .extend(rows.into_iter().map(|(id, _, _)| id));
    }

    let unique = movie_groups.len() + ep_groups.len();
    eprintln!(
        "items movies={} episodes={} unique_queries={} floor={}",
        movies.len(),
        episodes.len(),
        unique,
        AUTO_MATCH_FLOOR
    );

    let started = Instant::now();
    let mut done = 0usize;
    let mut library_pin_items = 0usize;

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
        match client.match_search_with_library_year(kind, &key.title, key.year, key.library_year) {
            Ok(Some(c)) if meets_auto_match_floor(c.confidence) => {
                if c.method == "exact_title_library_year" {
                    library_pin_items += n;
                }
                *matched += n;
            }
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
    let episodes_n = episodes.len();
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
        total_items: total,
        unique_queries: unique,
        movies: bucket(movies_n, m_matched, m_below, m_miss, m_err),
        episodes: bucket(episodes_n, e_matched, e_below, e_miss, e_err),
        episode_library_year_pin_items: library_pin_items,
        fragile_watch_state_fraction: if total == 0 {
            0.0
        } else {
            c_below as f64 / total as f64
        },
        combined: bucket(total, c_matched, c_below, c_miss, c_err),
        elapsed_secs: started.elapsed().as_secs_f64(),
        note: "Search+confidence only. TV multi-exact pinned by library_year (earliest episode year else show-folder YYYY). Region tags untouched."
            .into(),
    };

    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}
