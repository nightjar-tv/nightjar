//! Full-library measure: raw payload store size + negative-cache by reason.
//!
//! Copies `DB` to `MEASURE_DB` (default sibling `nightjar-store-measure.db`),
//! migrates, resolves unique movie/show queries with store wiring, fetches
//! season details for matched shows, then prints JSON stats.
//!
//! Env: `DB`, TMDB credentials, `EXCLUDE_TESTDATA=1`, optional `MEASURE_DB`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use nightjar_db::migrate;
use nightjar_metadata::{
    MetadataKind, ResolveInput, ResolveOutcome, Resolver, TmdbClient, TmdbCredentials,
    clean_movie_title, clean_show_title, counts_by_reason, payload_store_stats,
    persist_hit_with_canonical, series_library_year, year_from_path,
};
use rusqlite::Connection;
use serde::Serialize;

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct MovieQuery {
    title: String,
    year: Option<i32>,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct ShowQuery {
    title: String,
    library_year: Option<i32>,
    episode_count: Option<u32>,
    season_count: Option<u32>,
}

#[derive(Debug, Serialize)]
struct Report {
    /// Direct read: `SUM(LENGTH(payload))` from `metadata_raw_payloads`.
    /// Not the SQLite file size (that includes the library copy).
    payload_column_bytes: i64,
    payload_column_mib: f64,
    payload_row_count: i64,
    measure_db_file_bytes: u64,
    measure_db: String,
    payload_by_kind: HashMap<String, KindStats>,
    negative_cache_total: i64,
    negative_cache_by_reason: HashMap<String, i64>,
    movies_resolved: usize,
    movies_negative: usize,
    shows_resolved: usize,
    shows_negative: usize,
    seasons_stored: usize,
    season_errors: usize,
    elapsed_secs: f64,
    adr_0026_projected_median_mib: f64,
    note: String,
}

#[derive(Debug, Clone)]
struct EpRow {
    year: Option<i32>,
    path: String,
    season: Option<i32>,
}

#[derive(Debug, Serialize)]
struct KindStats {
    rows: i64,
    bytes: i64,
}

fn main() {
    let exclude_testdata = std::env::var("EXCLUDE_TESTDATA").ok().as_deref() == Some("1");
    let src_db = std::env::var("DB").map(PathBuf::from).unwrap_or_else(|_| {
        dirs_home()
            .map(|h| h.join("nightjar-data/nightjar.db"))
            .expect("HOME")
    });
    let measure_db = std::env::var("MEASURE_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            src_db
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("nightjar-store-measure.db")
        });

    let creds = TmdbCredentials::from_env().unwrap_or_else(|| {
        eprintln!("no TMDB credentials");
        std::process::exit(1);
    });
    let client = TmdbClient::new(creds);
    let resolver = Resolver { tmdb: &client };

    if measure_db.exists() {
        std::fs::remove_file(&measure_db).expect("remove old measure db");
    }
    std::fs::copy(&src_db, &measure_db)
        .unwrap_or_else(|e| panic!("copy {} → {}: {e}", src_db.display(), measure_db.display()));

    let conn = Connection::open(&measure_db)
        .unwrap_or_else(|e| panic!("open {}: {e}", measure_db.display()));
    migrate(&conn).expect("migrate measure db");
    let schema_v: i64 = conn
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
            r.get(0)
        })
        .expect("schema version");
    let has_payloads: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='metadata_raw_payloads'",
            [],
            |r| r.get(0),
        )
        .expect("payload table check");
    if has_payloads != 1 {
        panic!(
            "metadata_raw_payloads missing after migrate (schema_migrations={schema_v}); \
             rebuild nightjar-db and re-run"
        );
    }
    eprintln!("measure db schema_migrations={schema_v}; payload table ok");
    conn.execute_batch(
        "DELETE FROM metadata_raw_payloads;
         DELETE FROM metadata_negative_cache;",
    )
    .expect("clear prior payload/cache rows");

    let mut testdata_libs = HashSet::new();
    if exclude_testdata {
        let mut stmt = conn
            .prepare("SELECT id FROM libraries WHERE name = 'Test Data'")
            .unwrap();
        for id in stmt.query_map([], |r| r.get::<_, i64>(0)).unwrap() {
            testdata_libs.insert(id.unwrap());
        }
    }

    let mut movies: Vec<(String, Option<i32>, String)> = Vec::new();
    let mut episodes: Vec<(String, Option<i32>, String, Option<i32>)> = Vec::new();
    {
        let mut stmt = conn
            .prepare("SELECT title, year, path, library_id FROM media_items WHERE kind = 'movie'")
            .unwrap();
        for row in stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<i32>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .unwrap()
        {
            let (title, year, path, lib) = row.unwrap();
            if testdata_libs.contains(&lib) {
                continue;
            }
            movies.push((title, year, path));
        }
    }
    {
        let mut stmt = conn
            .prepare("SELECT title, year, path, season FROM media_items WHERE kind = 'episode'")
            .unwrap();
        for row in stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<i32>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<i32>>(3)?,
                ))
            })
            .unwrap()
        {
            episodes.push(row.unwrap());
        }
    }

    let mut movie_queries: HashSet<MovieQuery> = HashSet::new();
    for (title, year, path) in &movies {
        let folder_year = year_from_path(path);
        let (ct, cy) = clean_movie_title(title, folder_year.or(*year));
        movie_queries.insert(MovieQuery {
            title: ct,
            year: cy,
        });
    }

    let mut ep_raw: HashMap<String, Vec<EpRow>> = HashMap::new();
    for (title, year, path, season) in &episodes {
        let (ct, _) = clean_show_title(title);
        ep_raw.entry(ct).or_default().push(EpRow {
            year: *year,
            path: path.clone(),
            season: *season,
        });
    }

    let mut show_queries: HashMap<ShowQuery, HashSet<i32>> = HashMap::new();
    for (ct, rows) in ep_raw {
        let years = rows.iter().map(|r| r.year);
        let path0 = rows[0].path.clone();
        let library_year = series_library_year(years, &path0);
        let seasons: HashSet<i32> = rows.iter().filter_map(|r| r.season).collect();
        let q = ShowQuery {
            title: ct,
            library_year,
            episode_count: Some(rows.len() as u32),
            season_count: (!seasons.is_empty()).then_some(seasons.len() as u32),
        };
        show_queries.entry(q).or_default().extend(seasons);
    }

    let t0 = Instant::now();
    let mut movies_resolved = 0usize;
    let mut movies_negative = 0usize;
    let mut shows_resolved = 0usize;
    let mut shows_negative = 0usize;
    let mut seasons_stored = 0usize;
    let mut season_errors = 0usize;

    let unique = movie_queries.len() + show_queries.len();
    eprintln!(
        "resolving {} unique movies, {} unique shows ({} total)…",
        movie_queries.len(),
        show_queries.len(),
        unique
    );

    let mut done = 0usize;
    for q in &movie_queries {
        done += 1;
        if done.is_multiple_of(50) || done == unique {
            eprintln!("  resolved {done}/{unique} …");
        }
        let input = ResolveInput {
            title: Some(q.title.clone()),
            year: q.year,
            kind: Some(MetadataKind::Movie),
            ..Default::default()
        };
        match resolver.resolve_with_store(&input, &conn) {
            Ok(ResolveOutcome::Resolved { .. }) => movies_resolved += 1,
            Ok(ResolveOutcome::Unresolved { .. }) => movies_negative += 1,
            Err(e) => {
                eprintln!("movie provider error (not cached): {} — {e}", q.title);
                movies_negative += 1;
            }
        }
    }

    for (q, seasons) in &show_queries {
        done += 1;
        if done.is_multiple_of(50) || done == unique {
            eprintln!("  resolved {done}/{unique} …");
        }
        let input = ResolveInput {
            title: Some(q.title.clone()),
            year: None,
            library_year: q.library_year,
            library_episode_count: q.episode_count,
            library_season_count: q.season_count,
            kind: Some(MetadataKind::Episode),
            ..Default::default()
        };
        match resolver.resolve_with_store(&input, &conn) {
            Ok(ResolveOutcome::Resolved { metadata, .. }) => {
                shows_resolved += 1;
                let show_id = metadata.ids.tmdb_show.or(metadata.ids.tmdb);
                if let Some(id) = show_id {
                    for &sn in seasons {
                        match client.season_detail(id, sn) {
                            Ok(raw) => {
                                match persist_hit_with_canonical(&conn, "tmdb", &raw, |_| Ok(())) {
                                    Ok(()) => seasons_stored += 1,
                                    Err(e) => {
                                        eprintln!("season store {id}/{sn}: {e}");
                                        season_errors += 1;
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!("season {id}/{sn}: {e}");
                                season_errors += 1;
                            }
                        }
                    }
                }
            }
            Ok(ResolveOutcome::Unresolved { .. }) => shows_negative += 1,
            Err(e) => {
                eprintln!("show provider error (not cached): {} — {e}", q.title);
                shows_negative += 1;
            }
        }
    }

    let stats = payload_store_stats(&conn).expect("payload stats");
    let (neg_total, neg_counts) = counts_by_reason(&conn).expect("neg counts");

    let mut by_kind = HashMap::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT entity_kind, COUNT(*), COALESCE(SUM(LENGTH(payload)), 0)
                 FROM metadata_raw_payloads GROUP BY entity_kind",
            )
            .unwrap();
        for row in stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .unwrap()
        {
            let (k, rows, bytes) = row.unwrap();
            by_kind.insert(k, KindStats { rows, bytes });
        }
    }

    let mut neg_map = HashMap::new();
    neg_map.insert("no_results".into(), neg_counts.no_results);
    neg_map.insert("below_threshold".into(), neg_counts.below_threshold);
    neg_map.insert("api_error".into(), neg_counts.api_error);

    let file_bytes = conn
        .query_row(
            "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0) as u64;

    let report = Report {
        payload_column_bytes: stats.payload_bytes,
        payload_column_mib: stats.payload_bytes as f64 / (1024.0 * 1024.0),
        payload_row_count: stats.row_count,
        measure_db_file_bytes: file_bytes,
        measure_db: measure_db.display().to_string(),
        payload_by_kind: by_kind,
        negative_cache_total: neg_total,
        negative_cache_by_reason: neg_map,
        movies_resolved,
        movies_negative,
        shows_resolved,
        shows_negative,
        seasons_stored,
        season_errors,
        elapsed_secs: t0.elapsed().as_secs_f64(),
        adr_0026_projected_median_mib: 420.0,
        note: "Claim is payload_column_bytes (SUM LENGTH(payload)), not measure_db_file_bytes (page_count*page_size of whole DB including library). api_error not cached. Uncompressed UTF-8 JSON."
            .into(),
    };

    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}
