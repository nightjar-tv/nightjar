//! Full-library metadata queue drain with the API rate limiter (ADR-0026 §7/§8).
//!
//! Copies `DB` → `MEASURE_DB`, migrates, resets `metadata_status` to pending,
//! drains the queue (recently-added then everything else), reports wall time
//! and HTTP 429 count.
//!
//! Env: `DB`, TMDB credentials, `EXCLUDE_TESTDATA=1`, optional `MEASURE_DB`.

use std::path::PathBuf;
use std::time::Instant;

use nightjar_db::migrate;
use nightjar_metadata::{
    DEFAULT_MAX_IN_FLIGHT, DEFAULT_REQUESTS_PER_SEC, Resolver, TmdbClient, TmdbCredentials,
    drain_pending,
};
use rusqlite::Connection;
use serde::Serialize;

#[derive(Debug, Serialize)]
struct Report {
    measure_db: String,
    wall_secs: f64,
    baseline_search_pass_secs: f64,
    groups: usize,
    items_ready: usize,
    items_unmatched: usize,
    provider_resolves: usize,
    http_429: u64,
    requests_per_sec_budget: u32,
    max_in_flight: usize,
    note: String,
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
                .join("nightjar-queue-measure.db")
        });

    let creds = TmdbCredentials::from_env().unwrap_or_else(|| {
        eprintln!("no TMDB credentials");
        std::process::exit(1);
    });
    let client = TmdbClient::new(creds);
    let http_429 = std::sync::Arc::clone(&client.http_429);
    let resolver = Resolver { tmdb: &client };

    if measure_db.exists() {
        std::fs::remove_file(&measure_db).expect("remove old measure db");
    }
    std::fs::copy(&src_db, &measure_db)
        .unwrap_or_else(|e| panic!("copy {} → {}: {e}", src_db.display(), measure_db.display()));

    let conn = Connection::open(&measure_db)
        .unwrap_or_else(|e| panic!("open {}: {e}", measure_db.display()));
    migrate(&conn).expect("migrate");
    let schema_v: i64 = conn
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
            r.get(0)
        })
        .expect("schema version");
    let has_status: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('media_items') WHERE name = 'metadata_status'",
            [],
            |r| r.get(0),
        )
        .expect("status col");
    if has_status != 1 {
        panic!("metadata_status missing after migrate (v={schema_v})");
    }
    eprintln!("measure db schema_migrations={schema_v}; metadata_status ok");

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

    let pending: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM media_items WHERE metadata_status = 'pending'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    eprintln!(
        "draining {pending} pending items at {DEFAULT_REQUESTS_PER_SEC} req/s \
         (max_in_flight={DEFAULT_MAX_IN_FLIGHT})…"
    );

    let t0 = Instant::now();
    let stats = drain_pending(&conn, &resolver, &http_429).expect("drain");
    let wall = t0.elapsed().as_secs_f64();

    let report = Report {
        measure_db: measure_db.display().to_string(),
        wall_secs: wall,
        baseline_search_pass_secs: 12.5 * 60.0,
        groups: stats.groups,
        items_ready: stats.items_ready,
        items_unmatched: stats.items_unmatched,
        provider_resolves: stats.provider_resolves,
        http_429: stats.http_429,
        requests_per_sec_budget: DEFAULT_REQUESTS_PER_SEC,
        max_in_flight: DEFAULT_MAX_IN_FLIGHT,
        note: "Queue is SELECT over metadata_status (no jobs table). Wall includes search+detail under API limiter; ~12.5 min baseline was unique-query search pass."
            .into(),
    };
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}
