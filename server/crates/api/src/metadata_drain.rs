//! Background metadata queue drain (ADR-0026 §8).
//!
//! Opens a **second** SQLite connection so long serial TMDB drains do not hold
//! the process-wide `Db` mutex. Scan never waits on this worker.

use nightjar_db::db_path;
use nightjar_metadata::{
    ApiRateLimiter, DEFAULT_MAX_IN_FLIGHT, DEFAULT_REQUESTS_PER_SEC, DrainOptions, Resolver,
    TmdbClient, drain_pending, resolve_credentials_with, sweep_stale_cleaner_versions,
};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Idle poll when nothing is pending (or credentials missing).
const IDLE_SLEEP: Duration = Duration::from_secs(30);
/// Brief pause after a non-empty drain before selecting more pending work.
const WORK_PAUSE: Duration = Duration::from_secs(1);

/// Spawn the metadata drain thread. No-op-safe: missing TMDB key logs and retries.
pub fn spawn_metadata_drain(data_dir: PathBuf) {
    std::thread::Builder::new()
        .name("metadata-drain".into())
        .spawn(move || {
            run_loop(&data_dir);
        })
        .expect("spawn metadata-drain");
}

fn run_loop(data_dir: &Path) {
    let path = db_path(data_dir);
    let mut warned_no_key = false;
    loop {
        let env_key = std::env::var("NIGHTJAR_TMDB_API_KEY").ok();
        let creds = match resolve_credentials_with(
            Some(data_dir),
            env_key.as_deref(),
            nightjar_metadata::embedded_application_key(),
        ) {
            Ok(c) => {
                warned_no_key = false;
                c
            }
            Err(e) => {
                if !warned_no_key {
                    tracing::warn!(
                        error = %e,
                        "metadata drain idle: no TMDB credentials (ADR-0031)"
                    );
                    warned_no_key = true;
                }
                std::thread::sleep(IDLE_SLEEP);
                continue;
            }
        };

        let conn = match open_drain_conn(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "metadata drain open failed");
                std::thread::sleep(IDLE_SLEEP);
                continue;
            }
        };

        let remaining: i64 = match conn.query_row(
            "SELECT COUNT(*) FROM media_items WHERE metadata_status IN ('pending', 'matched')",
            [],
            |r| r.get(0),
        ) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(error = %e, "metadata drain pending count failed");
                std::thread::sleep(IDLE_SLEEP);
                continue;
            }
        };
        if remaining == 0 {
            std::thread::sleep(IDLE_SLEEP);
            continue;
        }

        let limiter = ApiRateLimiter::new(DEFAULT_REQUESTS_PER_SEC, DEFAULT_MAX_IN_FLIGHT);
        let client = TmdbClient::with_limiter(creds, Arc::clone(&limiter));
        let http_429 = Arc::clone(&client.http_429);
        let http_requests = Arc::clone(&client.http_requests);
        let resolver = Resolver { tmdb: &client };

        if let Ok(n) = sweep_stale_cleaner_versions(&conn)
            && n > 0
        {
            tracing::info!(n, "swept stale negative-cache cleaner versions");
        }

        tracing::info!(remaining, "metadata drain starting");
        match drain_pending(
            &conn,
            &resolver,
            &http_429,
            &http_requests,
            DrainOptions::default(),
        ) {
            Ok(stats) => {
                tracing::info!(
                    groups = stats.groups,
                    items_ready = stats.items_ready,
                    items_unmatched = stats.items_unmatched,
                    items_left_pending = stats.items_left_pending,
                    seasons_fetched = stats.seasons_fetched,
                    files_linked = stats.files_linked,
                    bind_errors = stats.bind_errors,
                    http_requests = stats.http_requests,
                    http_429 = stats.http_429,
                    "metadata drain pass complete"
                );
                if stats.groups == 0 {
                    std::thread::sleep(IDLE_SLEEP);
                } else {
                    std::thread::sleep(WORK_PAUSE);
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "metadata drain failed");
                std::thread::sleep(IDLE_SLEEP);
            }
        }
    }
}

fn open_drain_conn(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA foreign_keys=ON;
         PRAGMA busy_timeout=30000;",
    )
    .map_err(|e| format!("pragma: {e}"))?;
    Ok(conn)
}
