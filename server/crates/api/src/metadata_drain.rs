//! Background metadata queue drain (ADR-0026 §8).
//!
//! Opens a **second** SQLite connection so long serial TMDB drains do not hold
//! the process-wide `Db` mutex. Scan never waits on this worker.

use nightjar_db::db_path;
use nightjar_metadata::{
    ApiRateLimiter, ArtworkKind, ArtworkStore, CanonicalMetadata, DEFAULT_MAX_IN_FLIGHT,
    DEFAULT_REQUESTS_PER_SEC, DrainOptions, MetadataKind, PosterWarm, Resolver, TmdbClient,
    drain_pending, resolve_credentials_with, sweep_stale_cleaner_versions,
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
    // Best-effort artwork cache for matched-time poster warm (ADR-0027 §5);
    // a failed store only disables warming, never the drain.
    let artwork_store: Option<Arc<ArtworkStore>> = match ArtworkStore::new(data_dir) {
        Ok(store) => Some(Arc::new(store)),
        Err(e) => {
            tracing::warn!(error = %e, "poster warm disabled: artwork store init failed");
            None
        }
    };
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
            DrainOptions {
                poster_warm: artwork_store.as_ref().map(|store| {
                    Box::new(StorePosterWarm {
                        store: Arc::clone(store),
                    }) as Box<dyn PosterWarm>
                }),
                ..Default::default()
            },
        ) {
            Ok(stats) => {
                tracing::info!(
                    groups = stats.groups,
                    items_matched = stats.items_matched,
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

/// Store-backed poster warm hook (ADR-0027 §5). Download runs on a detached
/// thread so it never stalls the drain; failures log at debug and are ignored.
struct StorePosterWarm {
    store: Arc<ArtworkStore>,
}

impl PosterWarm for StorePosterWarm {
    fn on_matched(&self, _item_ids: &[i64], metadata: &CanonicalMetadata) {
        let Some(path) = metadata
            .artwork
            .iter()
            .find(|a| a.kind == ArtworkKind::Poster && a.path.starts_with('/'))
            .map(|a| a.path.clone())
        else {
            return;
        };
        let Some(key) = warm_item_key(metadata) else {
            return;
        };
        let store = Arc::clone(&self.store);
        let closure_key = key.clone();
        match std::thread::Builder::new()
            .name("poster-warm".into())
            .spawn(move || {
                match store.ensure_tmdb_original(&closure_key, ArtworkKind::Poster, &path) {
                    Ok(p) => tracing::debug!(
                        item_key = %closure_key,
                        path = %p.display(),
                        "poster warmed at matched"
                    ),
                    Err(e) => {
                        tracing::debug!(item_key = %closure_key, error = %e, "poster warm skipped")
                    }
                }
            }) {
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(item_key = %key, error = %e, "poster warm thread spawn failed");
            }
        }
    }
}

/// Key under which the freshly matched poster is served: movie watch key, or
/// the provisional non-watch `tmdb:show:{id}` link the search tier wrote for
/// enrich id recovery (ADR-0026 §8.3 / §8.4). Same keys `apply_search_hit`
/// assigns on the matching path.
fn warm_item_key(meta: &CanonicalMetadata) -> Option<String> {
    match meta.kind {
        MetadataKind::Movie => meta.ids.tmdb.map(|id| format!("tmdb:movie:{id}")),
        MetadataKind::Show | MetadataKind::Episode => meta
            .ids
            .tmdb
            .or(meta.ids.tmdb_show)
            .map(|id| format!("tmdb:show:{id}")),
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
