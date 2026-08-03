//! Fast-tier (search-hit) vs full-metadata (drain) timing measure.
//!
//! Answers the two-tier poster question: how long until the UI *could* paint
//! a poster grid (fast tier = one TMDB search per group through the real API
//! limiter, scored with the production matcher, capturing poster/backdrop
//! paths from the search response) vs how long until the same subset has
//! complete canonical metadata (slow tier = production `drain_pending`,
//! search + detail + season bind + store).
//!
//! Both phases use the same limiter shape as production
//! (`DEFAULT_REQUESTS_PER_SEC` / `DEFAULT_MAX_IN_FLIGHT`) and the same group
//! selection (band, then newest-first) so the numbers are like-for-like.
//!
//! Env:
//! - `DB` — source library DB (default `~/nightjar-data/nightjar.db`)
//! - `GRID_GROUPS` — subset size in groups (default 60)
//! - `EXCLUDE_TESTDATA=1` — skip Test Data / DV / DV2 libraries
//! - `MEASURE_DB` — writable copy for the slow tier (default derived)
//! - `GRID_FAST_ONLY=1` — skip the slow tier; `GRID_SLOW_ONLY=1` — skip the fast tier

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use nightjar_db::migrate;
use nightjar_metadata::{
    AUTO_MATCH_FLOOR, ApiRateLimiter, DEFAULT_MAX_IN_FLIGHT, DEFAULT_REQUESTS_PER_SEC,
    DrainOptions, LibrarySeriesShape, QueueBand, Resolver, SearchKind, TmdbClient,
    VISIBLE_FIRST_SCREEN_N, VisibleProxy, clean_movie_title, clean_show_title, drain_pending,
    measure_exclude_libraries_sql_in, measure_exclude_library_names, meets_auto_match_floor,
    pick_reference_episode, queue_band_for_item, resolve_credentials, score_search_with_shape,
    series_library_year, snapshot_visible_proxy_filtered, year_from_path,
};
use rusqlite::Connection;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Movie,
    Tv,
}

/// One resolve group, mirroring `pending_query_groups` selection (band, then
/// newest-first) so the fast and slow phases walk the same subset.
#[derive(Debug, Clone)]
struct Group {
    kind: Kind,
    title: String,
    year: Option<i32>,
    library_year: Option<i32>,
    episode_count: Option<u32>,
    season_count: Option<u32>,
    ref_season: Option<i32>,
    ref_episode: Option<i32>,
    ref_episode_title: Option<String>,
    band: QueueBand,
    max_id: i64,
    item_ids: Vec<i64>,
}

struct Row {
    id: i64,
    kind: String,
    title: String,
    year: Option<i32>,
    path: String,
    season: Option<i32>,
    episode: Option<i32>,
    library_id: i64,
}

fn build_groups(
    conn: &Connection,
    exclude_ids: &HashSet<i64>,
    visible: &VisibleProxy,
    cap: usize,
) -> Vec<Group> {
    let mut stmt = conn
        .prepare(
            "SELECT id, kind, title, year, path, season, episode, library_id
             FROM media_items WHERE metadata_status = 'pending' ORDER BY id DESC",
        )
        .expect("prepare pending rows");
    let mut items: Vec<Row> = Vec::new();
    for r in stmt
        .query_map([], |r| {
            Ok(Row {
                id: r.get(0)?,
                kind: r.get(1)?,
                title: r.get(2)?,
                year: r.get(3)?,
                path: r.get(4)?,
                season: r.get(5)?,
                episode: r.get(6)?,
                library_id: r.get(7)?,
            })
        })
        .expect("query pending rows")
    {
        let row = r.expect("pending row");
        if exclude_ids.contains(&row.library_id) {
            continue;
        }
        items.push(row);
    }

    let mut ep_by_show: HashMap<String, Vec<&Row>> = HashMap::new();
    for it in &items {
        if it.kind == "episode" {
            let (ct, _) = clean_show_title(&it.title);
            ep_by_show.entry(ct).or_default().push(it);
        }
    }

    let mut groups: HashMap<String, Group> = HashMap::new();
    for it in &items {
        let band = queue_band_for_item(it.id, visible);
        match it.kind.as_str() {
            "movie" => {
                let folder_year = year_from_path(&it.path);
                let (ct, cy) = clean_movie_title(&it.title, folder_year.or(it.year));
                let key = format!("movie|{ct}|{cy:?}");
                let g = groups.entry(key).or_insert_with(|| Group {
                    kind: Kind::Movie,
                    title: ct,
                    year: cy,
                    library_year: None,
                    episode_count: None,
                    season_count: None,
                    ref_season: None,
                    ref_episode: None,
                    ref_episode_title: None,
                    band,
                    max_id: it.id,
                    item_ids: Vec::new(),
                });
                g.item_ids.push(it.id);
                g.max_id = g.max_id.max(it.id);
            }
            "episode" => {
                let (ct, _) = clean_show_title(&it.title);
                let siblings = ep_by_show.get(&ct).map(|v| v.as_slice()).unwrap_or(&[]);
                let years = siblings.iter().map(|s| s.year);
                let path0 = siblings
                    .first()
                    .map(|s| s.path.as_str())
                    .unwrap_or(it.path.as_str());
                let library_year = series_library_year(years, path0);
                let seasons: HashSet<i32> = siblings.iter().filter_map(|s| s.season).collect();
                let season_count = (!seasons.is_empty()).then_some(seasons.len() as u32);
                let episode_count = Some(siblings.len() as u32);
                let ep_triples: Vec<(i32, i32, &str)> = siblings
                    .iter()
                    .filter_map(|s| {
                        Some((
                            s.season?,
                            s.episode?,
                            std::path::Path::new(&s.path)
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or(s.path.as_str()),
                        ))
                    })
                    .collect();
                let ref_ep = pick_reference_episode(&ep_triples, &ct);
                let key = format!("tv|{ct}");
                let g = groups.entry(key).or_insert_with(|| Group {
                    kind: Kind::Tv,
                    title: ct,
                    year: None,
                    library_year,
                    episode_count,
                    season_count,
                    ref_season: ref_ep.as_ref().map(|p| p.0),
                    ref_episode: ref_ep.as_ref().map(|p| p.1),
                    ref_episode_title: ref_ep.map(|p| p.2),
                    band,
                    max_id: it.id,
                    item_ids: Vec::new(),
                });
                g.item_ids.push(it.id);
                g.max_id = g.max_id.max(it.id);
            }
            _ => {}
        }
    }

    let mut out: Vec<Group> = groups.into_values().collect();
    out.sort_by(|a, b| {
        (a.band as i32)
            .cmp(&(b.band as i32))
            .then_with(|| b.max_id.cmp(&a.max_id))
    });
    out.truncate(cap);
    out
}

#[derive(Debug, Serialize)]
struct FastReport {
    groups: usize,
    wall_secs: f64,
    requests: u64,
    effective_req_per_sec: f64,
    matched: usize,
    below_floor: usize,
    miss: usize,
    errors: usize,
    groups_with_poster: usize,
    floor: f64,
    sample_poster_paths: Vec<String>,
}

/// Fast tier: one search per group through the limiter, scored with the
/// production matcher; the poster path is read from the search response.
fn phase_a_fast(client: &TmdbClient, groups: &[Group]) -> FastReport {
    let t0 = Instant::now();
    let req0 = client.http_requests.load(Ordering::Relaxed);
    let mut matched = 0usize;
    let mut below = 0usize;
    let mut miss = 0usize;
    let mut errors = 0usize;
    let mut with_poster = 0usize;
    let mut sample: Vec<String> = Vec::new();
    let mut done = 0usize;

    for g in groups {
        done += 1;
        if done.is_multiple_of(25) || done == groups.len() {
            eprintln!("  fast tier searched {done}/{} …", groups.len());
        }
        let kind = match g.kind {
            Kind::Movie => SearchKind::Movie,
            Kind::Tv => SearchKind::Tv,
        };
        let library = LibrarySeriesShape {
            year: g.library_year,
            episode_count: g.episode_count,
            season_count: g.season_count,
            ref_season: g.ref_season,
            ref_episode: g.ref_episode,
            ref_episode_title: g.ref_episode_title.clone(),
        };
        let hits = match client.search(kind, &g.title, g.year) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("  search error {:?} {}: {e}", g.kind, g.title);
                errors += 1;
                continue;
            }
        };
        let cand = score_search_with_shape(&hits, &g.title, g.year, kind, library, None);
        match &cand {
            Some(c) if meets_auto_match_floor(c.confidence) => matched += 1,
            Some(_) => below += 1,
            None => miss += 1,
        }
        let poster = cand
            .as_ref()
            .and_then(|c| hits.iter().find(|h| h.id == c.tmdb_id))
            .and_then(|h| h.poster_path.clone())
            .or_else(|| hits.first().and_then(|h| h.poster_path.clone()));
        if let Some(p) = poster {
            with_poster += 1;
            if sample.len() < 8 {
                sample.push(p);
            }
        }
    }

    let wall = t0.elapsed().as_secs_f64();
    let requests = client.http_requests.load(Ordering::Relaxed) - req0;
    FastReport {
        groups: groups.len(),
        wall_secs: wall,
        requests,
        effective_req_per_sec: if wall > 0.0 {
            requests as f64 / wall
        } else {
            0.0
        },
        matched,
        below_floor: below,
        miss,
        errors,
        groups_with_poster: with_poster,
        floor: AUTO_MATCH_FLOOR,
        sample_poster_paths: sample,
    }
}

#[derive(Debug, Serialize)]
struct SlowReport {
    groups: usize,
    movie_groups: usize,
    show_groups: usize,
    wall_secs: f64,
    requests: u64,
    effective_req_per_sec: f64,
    items_ready: usize,
    items_unmatched: usize,
    items_left_pending: usize,
    provider_resolves: usize,
    provider_errors: usize,
    http_429: u64,
    seasons_fetched: usize,
    episodes_projected: usize,
    files_linked: usize,
    seasons_skipped: usize,
    ready_episodes_unlinked: i64,
}

/// Slow tier: production `drain_pending` over the same capped group set on a
/// copy of the source DB (search + detail + season bind + store).
fn phase_b_slow(
    client: &TmdbClient,
    src_db: &PathBuf,
    measure_db: &PathBuf,
    exclude_names: &[String],
    cap: usize,
) -> Result<SlowReport, String> {
    if measure_db.exists() {
        std::fs::remove_file(measure_db).map_err(|e| format!("remove old measure db: {e}"))?;
    }
    std::fs::copy(src_db, measure_db)
        .map_err(|e| format!("copy {} -> {}: {e}", src_db.display(), measure_db.display()))?;

    let conn =
        Connection::open(measure_db).map_err(|e| format!("open {}: {e}", measure_db.display()))?;
    migrate(&conn).map_err(|e| format!("migrate: {e}"))?;

    if !exclude_names.is_empty() {
        let in_list = measure_exclude_libraries_sql_in(exclude_names);
        conn.execute(
            &format!(
                "UPDATE media_items SET metadata_status = 'ready'
                 WHERE library_id IN (SELECT id FROM libraries WHERE name IN ({in_list}))"
            ),
            [],
        )
        .map_err(|e| format!("exclude libs: {e}"))?;
        conn.execute(
            &format!(
                "UPDATE media_items SET metadata_status = 'pending'
                 WHERE library_id NOT IN (SELECT id FROM libraries WHERE name IN ({in_list}))"
            ),
            [],
        )
        .map_err(|e| format!("reset pending: {e}"))?;
    } else {
        conn.execute("UPDATE media_items SET metadata_status = 'pending'", [])
            .map_err(|e| format!("reset pending: {e}"))?;
    }
    // Clean store so the slow tier is first-run, not residual from a prior measure.
    conn.execute_batch(
        "DELETE FROM metadata_raw_payloads;
         DELETE FROM metadata_negative_cache;
         DELETE FROM metadata_canonical;
         DELETE FROM media_item_links;",
    )
    .map_err(|e| format!("clean store: {e}"))?;

    let resolver = Resolver { tmdb: client };
    let t0 = Instant::now();
    let stats = drain_pending(
        &conn,
        &resolver,
        &client.http_429,
        &client.http_requests,
        DrainOptions {
            max_groups: Some(cap),
            stop_when_visible_terminal: false,
            exclude_library_names: exclude_names.to_vec(),
        },
    )
    .map_err(|e| format!("drain: {e}"))?;
    let wall = t0.elapsed().as_secs_f64();

    let ready_episodes_unlinked: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM media_items m
             WHERE m.kind = 'episode' AND m.metadata_status = 'ready'
               AND NOT EXISTS (
                 SELECT 1 FROM media_item_links l WHERE l.media_item_id = m.id
               )",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    Ok(SlowReport {
        groups: stats.groups,
        movie_groups: stats.movie_groups,
        show_groups: stats.show_groups,
        wall_secs: wall,
        requests: stats.http_requests,
        effective_req_per_sec: if wall > 0.0 {
            stats.http_requests as f64 / wall
        } else {
            0.0
        },
        items_ready: stats.items_ready,
        items_unmatched: stats.items_unmatched,
        items_left_pending: stats.items_left_pending,
        provider_resolves: stats.provider_resolves,
        provider_errors: stats.provider_errors,
        http_429: stats.http_429,
        seasons_fetched: stats.seasons_fetched,
        episodes_projected: stats.episodes_projected,
        files_linked: stats.files_linked,
        seasons_skipped: stats.seasons_skipped,
        ready_episodes_unlinked,
    })
}

#[derive(Debug, Serialize)]
struct Report {
    src_db: String,
    measure_db: String,
    groups_cap: usize,
    exclude_testdata: bool,
    fast: Option<FastReport>,
    slow: Option<SlowReport>,
    ratio_slow_over_fast_wall: Option<f64>,
    note: String,
}

fn main() {
    let exclude_testdata = std::env::var("EXCLUDE_TESTDATA").ok().as_deref() == Some("1");
    let fast_only = std::env::var("GRID_FAST_ONLY").ok().as_deref() == Some("1");
    let slow_only = std::env::var("GRID_SLOW_ONLY").ok().as_deref() == Some("1");
    let cap: usize = std::env::var("GRID_GROUPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
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
                .join(format!("nightjar-grid-g{cap}.db"))
        });

    let creds = resolve_credentials().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    let limiter = Arc::new(ApiRateLimiter::new(
        DEFAULT_REQUESTS_PER_SEC,
        DEFAULT_MAX_IN_FLIGHT,
    ));
    let client = TmdbClient::with_limiter(creds, Arc::clone(&limiter));

    let conn = Connection::open_with_flags(&src_db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap_or_else(|e| panic!("open {}: {e}", src_db.display()));

    let exclude_names: Vec<String> = if exclude_testdata {
        measure_exclude_library_names()
    } else {
        vec![]
    };
    let exclude_ids: HashSet<i64> = if exclude_names.is_empty() {
        HashSet::new()
    } else {
        let in_list = measure_exclude_libraries_sql_in(&exclude_names);
        let mut stmt = conn
            .prepare(&format!(
                "SELECT id FROM libraries WHERE name IN ({in_list})"
            ))
            .unwrap();
        stmt.query_map([], |r| r.get::<_, i64>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };

    let exclude_refs: Vec<&str> = exclude_names.iter().map(String::as_str).collect();
    let visible = snapshot_visible_proxy_filtered(&conn, VISIBLE_FIRST_SCREEN_N, &exclude_refs)
        .unwrap_or_default();
    let groups = build_groups(&conn, &exclude_ids, &visible, cap);
    eprintln!(
        "groups={} ({} movie / {} tv) cap={cap} exclude_testdata={exclude_testdata} limiter={DEFAULT_REQUESTS_PER_SEC} rps / {DEFAULT_MAX_IN_FLIGHT} in-flight",
        groups.len(),
        groups.iter().filter(|g| g.kind == Kind::Movie).count(),
        groups.iter().filter(|g| g.kind == Kind::Tv).count(),
    );

    let fast = if slow_only {
        None
    } else {
        eprintln!("PHASE A: fast tier (search-only, poster capture)");
        Some(phase_a_fast(&client, &groups))
    };

    let slow = if fast_only {
        None
    } else {
        eprintln!("PHASE B: slow tier (full drain on copy)");
        Some(
            phase_b_slow(&client, &src_db, &measure_db, &exclude_names, cap).unwrap_or_else(|e| {
                eprintln!("phase B failed: {e}");
                std::process::exit(1);
            }),
        )
    };

    let ratio = match (&fast, &slow) {
        (Some(f), Some(s)) if f.wall_secs > 0.0 => Some(s.wall_secs / f.wall_secs),
        _ => None,
    };

    let report = Report {
        src_db: src_db.display().to_string(),
        measure_db: measure_db.display().to_string(),
        groups_cap: cap,
        exclude_testdata,
        fast,
        slow,
        ratio_slow_over_fast_wall: ratio,
        note: "Fast tier: one TMDB search per group through the production limiter, scored with the matcher; poster path read from the search response (two-tier capture). Slow tier: production drain_pending (search + detail + season bind + store). Same group selection (band, newest-first), same limiter shape.".into(),
    };
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}
