//! Metadata queue drain / short probe (ADR-0026 §7/§8).
//!
//! Env:
//! - `DB`, TMDB credentials, `EXCLUDE_TESTDATA=1`, optional `MEASURE_DB`
//! - `QUEUE_REQUESTS_PER_SEC` (default 10)
//! - `QUEUE_MAX_IN_FLIGHT` — unused while drain is serial; do not tune it
//! - `QUEUE_MAX_GROUPS` — **use for probes**. Caps groups so a check finishes
//!   in minutes. Record movie/show split; do not extrapolate full-library
//!   wall from a prefix (show detail is heavier). Omit only for a deliberate
//!   full drain.
//!
//! Ceiling / 429 probes: use a personal TMDB key, not the application key.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use nightjar_db::migrate;
use nightjar_metadata::{
    ApiRateLimiter, DEFAULT_MAX_IN_FLIGHT, DEFAULT_REQUESTS_PER_SEC, Resolver, TmdbClient,
    TmdbCredentials, drain_pending,
};
use rusqlite::Connection;
use serde::Serialize;

#[derive(Debug, Serialize)]
struct Report {
    measure_db: String,
    wall_secs: f64,
    groups: usize,
    movie_groups: usize,
    show_groups: usize,
    max_groups_cap: Option<usize>,
    items_ready: usize,
    items_unmatched: usize,
    items_left_pending: usize,
    provider_resolves: usize,
    provider_errors: usize,
    http_requests: u64,
    mean_http_per_group: f64,
    http_requests_per_1000_items: f64,
    mean_secs_per_request: f64,
    effective_req_per_sec: f64,
    http_429: u64,
    requests_per_sec_budget: u32,
    max_in_flight: usize,
    seasons_in_drain: bool,
    note: String,
}

fn main() {
    let exclude_testdata = std::env::var("EXCLUDE_TESTDATA").ok().as_deref() == Some("1");
    let rps: u32 = std::env::var("QUEUE_REQUESTS_PER_SEC")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_REQUESTS_PER_SEC);
    let max_in_flight: usize = std::env::var("QUEUE_MAX_IN_FLIGHT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MAX_IN_FLIGHT);
    let max_groups: Option<usize> = std::env::var("QUEUE_MAX_GROUPS")
        .ok()
        .and_then(|s| s.parse().ok());

    let src_db = std::env::var("DB").map(PathBuf::from).unwrap_or_else(|_| {
        dirs_home()
            .map(|h| h.join("nightjar-data/nightjar.db"))
            .expect("HOME")
    });
    let measure_db = std::env::var("MEASURE_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let tag = max_groups
                .map(|n| format!("g{n}"))
                .unwrap_or_else(|| "full".into());
            src_db
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join(format!("nightjar-queue-if{max_in_flight}-{tag}.db"))
        });

    let creds = TmdbCredentials::from_env().unwrap_or_else(|| {
        eprintln!("no TMDB credentials");
        std::process::exit(1);
    });
    let limiter = ApiRateLimiter::new(rps, max_in_flight);
    let client = TmdbClient::with_limiter(creds, Arc::clone(&limiter));
    let http_429 = Arc::clone(&client.http_429);
    let http_requests = Arc::clone(&client.http_requests);
    let resolver = Resolver { tmdb: &client };

    if measure_db.exists() {
        std::fs::remove_file(&measure_db).expect("remove old measure db");
    }
    std::fs::copy(&src_db, &measure_db)
        .unwrap_or_else(|e| panic!("copy {} → {}: {e}", src_db.display(), measure_db.display()));

    let conn = Connection::open(&measure_db)
        .unwrap_or_else(|e| panic!("open {}: {e}", measure_db.display()));
    migrate(&conn).expect("migrate");

    if exclude_testdata {
        conn.execute(
            "UPDATE media_items SET metadata_status = 'ready'
             WHERE library_id IN (SELECT id FROM libraries WHERE name = 'Test Data')",
            [],
        )
        .expect("skip testdata");
        conn.execute(
            "UPDATE media_items SET metadata_status = 'pending'
             WHERE library_id NOT IN (SELECT id FROM libraries WHERE name = 'Test Data')",
            [],
        )
        .expect("reset pending");
    } else {
        conn.execute("UPDATE media_items SET metadata_status = 'pending'", [])
            .expect("reset pending");
    }
    conn.execute_batch(
        "DELETE FROM metadata_raw_payloads;
         DELETE FROM metadata_negative_cache;",
    )
    .ok();

    match max_groups {
        Some(n) => eprintln!(
            "PROBE: max_groups={n} rps={rps} max_in_flight={max_in_flight} \
             (serial drain — if unused; not a full-library wall)"
        ),
        None => eprintln!(
            "FULL DRAIN: rps={rps} max_in_flight={max_in_flight} — expect tens of minutes"
        ),
    }

    let t0 = Instant::now();
    let stats =
        drain_pending(&conn, &resolver, &http_429, &http_requests, max_groups).expect("drain");
    let wall = t0.elapsed().as_secs_f64();

    let items_touched =
        (stats.items_ready + stats.items_unmatched + stats.items_left_pending).max(1) as f64;
    let report = Report {
        measure_db: measure_db.display().to_string(),
        wall_secs: wall,
        groups: stats.groups,
        movie_groups: stats.movie_groups,
        show_groups: stats.show_groups,
        max_groups_cap: max_groups,
        items_ready: stats.items_ready,
        items_unmatched: stats.items_unmatched,
        items_left_pending: stats.items_left_pending,
        provider_resolves: stats.provider_resolves,
        provider_errors: stats.provider_errors,
        http_requests: stats.http_requests,
        mean_http_per_group: if stats.groups > 0 {
            stats.http_requests as f64 / stats.groups as f64
        } else {
            0.0
        },
        http_requests_per_1000_items: (stats.http_requests as f64) * 1000.0 / items_touched,
        mean_secs_per_request: if stats.http_requests > 0 {
            wall / stats.http_requests as f64
        } else {
            0.0
        },
        effective_req_per_sec: if wall > 0.0 {
            stats.http_requests as f64 / wall
        } else {
            0.0
        },
        http_429: stats.http_429,
        requests_per_sec_budget: rps,
        max_in_flight,
        seasons_in_drain: false,
        note: "Serial movie+show drain (no seasons). Prefix probes can skew movie-heavy — \
               do not extrapolate wall. max_in_flight unused until group fan-out. \
               429/ceiling probes: personal TMDB key only."
            .into(),
    };
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}
