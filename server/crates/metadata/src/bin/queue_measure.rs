//! Metadata queue drain / first-screen measure (ADR-0026 §7/§8).
//!
//! Default: first-screen gate (Visible proxy terminal + early stop).
//!
//! Env:
//! - `DB`, TMDB credentials, `EXCLUDE_TESTDATA=1`, optional `MEASURE_DB`
//! - `QUEUE_FIRST_SCREEN=0` — full drain (no early stop)
//! - `QUEUE_MAX_GROUPS` — short probe cap (implies not first-screen)
//! - `QUEUE_REQUESTS_PER_SEC`, `QUEUE_MAX_IN_FLIGHT` (if unused while serial)
//!
//! Ceiling / 429 probes: use a personal TMDB key, not the application key.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use nightjar_db::migrate;
use nightjar_metadata::{
    ApiRateLimiter, DEFAULT_MAX_IN_FLIGHT, DEFAULT_REQUESTS_PER_SEC, DrainOptions, Resolver,
    T_FIRST_SCREEN_PASS_SECS, T_FIRST_SCREEN_PREDICTED_SECS, TmdbClient, TmdbCredentials,
    VISIBLE_FIRST_SCREEN_N, drain_pending, snapshot_visible_proxy_filtered,
};
use rusqlite::Connection;
use serde::Serialize;

#[derive(Debug, Serialize)]
struct Report {
    measure_db: String,
    mode: String,
    wall_secs: f64,
    predicted_t_first_screen_secs: f64,
    t_first_screen_secs: Option<f64>,
    pass_bar_secs: f64,
    gate_pass: bool,
    visible_proxy_size: usize,
    movie_groups: usize,
    show_groups: usize,
    proxy_movie_units: usize,
    proxy_show_units: usize,
    unmatched_in_proxy: usize,
    ready_in_proxy: usize,
    ready_missing_poster: usize,
    groups_drained: usize,
    items_ready: usize,
    items_unmatched: usize,
    items_left_pending: usize,
    provider_resolves: usize,
    provider_errors: usize,
    http_requests: u64,
    mean_http_per_group: f64,
    effective_req_per_sec: f64,
    http_429: u64,
    requests_per_sec_budget: u32,
    max_in_flight: usize,
    visible_n: usize,
    stopped_early: bool,
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
    let first_screen = max_groups.is_none()
        && std::env::var("QUEUE_FIRST_SCREEN")
            .map(|v| v != "0")
            .unwrap_or(true);

    let src_db = std::env::var("DB").map(PathBuf::from).unwrap_or_else(|_| {
        dirs_home()
            .map(|h| h.join("nightjar-data/nightjar.db"))
            .expect("HOME")
    });
    let measure_db = std::env::var("MEASURE_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let tag = if first_screen {
                "firstscreen".into()
            } else {
                max_groups
                    .map(|n| format!("g{n}"))
                    .unwrap_or_else(|| "full".into())
            };
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

    let exclude_libs: Vec<&str> = if exclude_testdata {
        vec!["Test Data"]
    } else {
        vec![]
    };
    let proxy = snapshot_visible_proxy_filtered(&conn, VISIBLE_FIRST_SCREEN_N, &exclude_libs)
        .expect("proxy");
    // Scale ADR ~30s (80 units) to this dogfood proxy size.
    let predicted = T_FIRST_SCREEN_PREDICTED_SECS * (proxy.units.len() as f64 / 80.0);
    eprintln!(
        "Visible proxy: {} units ({} movie / {} show), N={VISIBLE_FIRST_SCREEN_N}/library, \
         predicted={predicted:.1}s (ADR baseline 30s @ 80 units) pass_bar={T_FIRST_SCREEN_PASS_SECS}s",
        proxy.units.len(),
        proxy.movie_unit_count(),
        proxy.show_unit_count(),
    );

    if first_screen {
        eprintln!("MODE: first-screen (early-stop when proxy terminal)");
    } else if let Some(n) = max_groups {
        eprintln!("MODE: probe max_groups={n}");
    } else {
        eprintln!("MODE: full drain");
    }

    let t0 = Instant::now();
    let stats = drain_pending(
        &conn,
        &resolver,
        &http_429,
        &http_requests,
        DrainOptions {
            max_groups,
            stop_when_visible_terminal: first_screen,
            exclude_library_names: exclude_libs.iter().map(|s| (*s).to_string()).collect(),
        },
    )
    .expect("drain");
    let wall = t0.elapsed().as_secs_f64();

    let t_fs = stats.t_first_screen_secs;
    let note = if first_screen {
        match t_fs {
            Some(t) if (t - predicted).abs() <= 10.0 => {
                format!("Measured near scaled prediction ({predicted:.1}s) — model holds.")
            }
            Some(t) if (45.0..=T_FIRST_SCREEN_PASS_SECS).contains(&t) => {
                format!(
                    "Inside pass bar but {t:.1}s vs predicted {predicted:.1}s — proxy path may cost more than drain average; investigate before calling it a clean pass."
                )
            }
            Some(t) if t > T_FIRST_SCREEN_PASS_SECS => {
                format!(
                    "FAILED pass bar ({t:.1}s > {T_FIRST_SCREEN_PASS_SECS}s). Do not start fan-out from this alone — report and stop."
                )
            }
            Some(_) => "First-screen terminal reached.".into(),
            None => "Proxy never reached terminal (provider errors left pending?).".into(),
        }
    } else {
        "Not a first-screen run.".into()
    };

    let report = Report {
        measure_db: measure_db.display().to_string(),
        mode: if first_screen {
            "first_screen".into()
        } else if max_groups.is_some() {
            "probe".into()
        } else {
            "full".into()
        },
        wall_secs: wall,
        predicted_t_first_screen_secs: predicted,
        t_first_screen_secs: t_fs,
        pass_bar_secs: T_FIRST_SCREEN_PASS_SECS,
        gate_pass: stats.gate_pass,
        visible_proxy_size: stats.visible_proxy_size,
        movie_groups: stats.movie_groups,
        show_groups: stats.show_groups,
        proxy_movie_units: stats.proxy_movie_units,
        proxy_show_units: stats.proxy_show_units,
        unmatched_in_proxy: stats.unmatched_in_proxy,
        ready_in_proxy: stats.ready_in_proxy,
        ready_missing_poster: stats.ready_missing_poster,
        groups_drained: stats.groups,
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
        effective_req_per_sec: if wall > 0.0 {
            stats.http_requests as f64 / wall
        } else {
            0.0
        },
        http_429: stats.http_429,
        requests_per_sec_budget: rps,
        max_in_flight,
        visible_n: VISIBLE_FIRST_SCREEN_N,
        stopped_early: stats.stopped_early,
        seasons_in_drain: false,
        note,
    };
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}
