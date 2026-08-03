//! Library discovery triggers: notify + fixed poll backstop (ADR-0015).
//!
//! Full walks still enter through [`crate::request_scan`]. Notify may also
//! run path-hinted ingest for a concrete media path, then request_scan so
//! deletes and missed paths heal on the next full keep-set. Notify never
//! disables poll.

use crate::reachability::REACHABILITY_INTERVAL;
use crate::{LibraryPool, hint_ingest, request_scan};
use nightjar_db::Db;
use notify::RecursiveMode;
use notify_debouncer_mini::{DebouncedEventKind, new_debouncer};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Fixed poll interval (ADR-0015). Safety net when notify is mute; notify still
/// accelerates local creates. Default 300 s after multi-library walk pile-up
/// on shared mounts; override with `NIGHTJAR_POLL_INTERVAL_SECS`.
const DEFAULT_POLL_INTERVAL_SECS: u64 = 300;

fn poll_interval() -> Duration {
    let secs = std::env::var("NIGHTJAR_POLL_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_POLL_INTERVAL_SECS)
        .clamp(5, 3600);
    Duration::from_secs(secs)
}

/// Watch every library root; on change, request a scan. Poll remains the
/// verification backstop whether or not notify delivers events.
pub fn spawn_library_watcher(db: Arc<Db>, pool: Arc<LibraryPool>) {
    std::thread::Builder::new()
        .name("nightjar-watch".into())
        .spawn(move || {
            if let Err(e) = run(db, pool) {
                tracing::error!(error = %e, "library watcher stopped");
            }
        })
        .expect("spawn library watcher");
}

fn run(db: Arc<Db>, pool: Arc<LibraryPool>) -> Result<(), String> {
    let poll_only = std::env::var("NIGHTJAR_POLL_ONLY")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if poll_only {
        tracing::info!("library watcher poll-only; FS notify disabled");
        return run_poll_only(db, pool);
    }
    run_with_notify(db, pool)
}

fn run_poll_only(db: Arc<Db>, pool: Arc<LibraryPool>) -> Result<(), String> {
    let mut watched: HashMap<i64, PathBuf> = HashMap::new();
    let mut last_poll = std::time::Instant::now();
    let mut last_reach = std::time::Instant::now();
    loop {
        sync_library_roots(&db, &mut watched)?;
        maybe_reachability(&pool, &mut last_reach);
        std::thread::sleep(Duration::from_secs(5));
        maybe_poll(&db, &pool, &watched, &mut last_poll);
    }
}

fn run_with_notify(db: Arc<Db>, pool: Arc<LibraryPool>) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = new_debouncer(Duration::from_secs(2), move |res| {
        let _ = tx.send(res);
    })
    .map_err(|e| format!("create debouncer: {e}"))?;

    let mut watched: HashMap<i64, PathBuf> = HashMap::new();
    let mut last_poll = std::time::Instant::now();
    let mut last_reach = std::time::Instant::now();
    // Defer recursive watches until the first index finishes so they do not
    // compete with cold SMB metadata IOPS (ADR-0013). Poll still runs.
    let mut notify_armed = false;
    loop {
        if notify_armed {
            sync_watches(&db, &mut debouncer, &mut watched)?;
        } else {
            sync_library_roots(&db, &mut watched)?;
            if pool.last_index_duration_ms() > 0 {
                watched.clear();
                sync_watches(&db, &mut debouncer, &mut watched)?;
                notify_armed = true;
                tracing::info!("armed recursive FS notify after first index pass");
            }
        }

        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(events)) => {
                for ev in events {
                    if !matches!(ev.kind, DebouncedEventKind::Any) {
                        continue;
                    }
                    if let Some(id) = library_for_path(&watched, &ev.path) {
                        if !pool.is_library_reachable(id) {
                            continue;
                        }
                        match hint_ingest(db.as_ref(), pool.as_ref(), id, &ev.path) {
                            Ok(outcome) => tracing::debug!(
                                library_id = id,
                                path = %ev.path.display(),
                                ?outcome,
                                "hint ingest"
                            ),
                            Err(e) => tracing::warn!(
                                library_id = id,
                                path = %ev.path.display(),
                                error = %e,
                                "hint ingest failed"
                            ),
                        }
                        tracing::info!(
                            library_id = id,
                            path = %ev.path.display(),
                            "fs change; requesting scan"
                        );
                        match request_scan(Arc::clone(&db), Arc::clone(&pool), id) {
                            Ok(job_id) => {
                                tracing::info!(library_id = id, job_id, "scan accepted")
                            }
                            Err(e) => {
                                tracing::warn!(library_id = id, error = %e, "scan request failed")
                            }
                        }
                    }
                }
            }
            Ok(Err(e)) => tracing::warn!(error = %e, "watch error"),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("watch channel disconnected".into());
            }
        }
        maybe_reachability(&pool, &mut last_reach);
        maybe_poll(&db, &pool, &watched, &mut last_poll);
    }
}

fn maybe_reachability(pool: &Arc<LibraryPool>, last: &mut std::time::Instant) {
    if last.elapsed() < REACHABILITY_INTERVAL {
        return;
    }
    if let Err(e) = pool.tick_reachability() {
        tracing::warn!(error = %e, "reachability tick failed");
    }
    *last = std::time::Instant::now();
}

fn maybe_poll(
    db: &Arc<Db>,
    pool: &Arc<LibraryPool>,
    watched: &HashMap<i64, PathBuf>,
    last_poll: &mut std::time::Instant,
) {
    let poll_every = poll_interval();
    if last_poll.elapsed() < poll_every {
        return;
    }
    for library_id in watched.keys() {
        if !pool.is_library_reachable(*library_id) {
            continue;
        }
        tracing::info!(
            library_id,
            poll_interval_s = poll_every.as_secs(),
            "poll; requesting scan"
        );
        if let Err(e) = request_scan(Arc::clone(db), Arc::clone(pool), *library_id) {
            tracing::warn!(library_id, error = %e, "poll scan failed");
        }
    }
    *last_poll = std::time::Instant::now();
}

fn sync_library_roots(db: &Db, watched: &mut HashMap<i64, PathBuf>) -> Result<(), String> {
    let libs = db.list_libraries()?;
    let live: std::collections::HashSet<i64> = libs.iter().map(|l| l.id).collect();
    watched.retain(|id, _| live.contains(id));
    for lib in libs {
        let path = PathBuf::from(&lib.path);
        if watched.get(&lib.id) != Some(&path) {
            tracing::info!(library_id = lib.id, path = %path.display(), "library root for poll");
            watched.insert(lib.id, path);
        }
    }
    Ok(())
}

fn sync_watches(
    db: &Db,
    debouncer: &mut notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>,
    watched: &mut HashMap<i64, PathBuf>,
) -> Result<(), String> {
    let libs = db.list_libraries()?;
    let live: std::collections::HashSet<i64> = libs.iter().map(|l| l.id).collect();
    for id in watched
        .keys()
        .copied()
        .filter(|id| !live.contains(id))
        .collect::<Vec<_>>()
    {
        if let Some(path) = watched.remove(&id) {
            let _ = debouncer.watcher().unwatch(&path);
        }
    }
    for lib in libs {
        let path = PathBuf::from(&lib.path);
        if watched.get(&lib.id) == Some(&path) {
            continue;
        }
        if let Some(old) = watched.remove(&lib.id) {
            let _ = debouncer.watcher().unwatch(&old);
        }
        match debouncer.watcher().watch(&path, RecursiveMode::Recursive) {
            Ok(()) => {
                tracing::info!(library_id = lib.id, path = %path.display(), "watching library");
                watched.insert(lib.id, path);
            }
            Err(e) => {
                // Still poll this root; notify is only an accelerator.
                tracing::warn!(
                    library_id = lib.id,
                    path = %path.display(),
                    error = %e,
                    "watch path failed; poll continues"
                );
                watched.insert(lib.id, path);
            }
        }
    }
    Ok(())
}

fn library_for_path(watched: &HashMap<i64, PathBuf>, path: &Path) -> Option<i64> {
    watched
        .iter()
        .find(|(_, root)| path.starts_with(root))
        .map(|(id, _)| *id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_interval_default_is_three_hundred() {
        // Avoid depending on ambient env in unit tests: clamp logic only.
        assert_eq!(DEFAULT_POLL_INTERVAL_SECS, 300);
        let secs = 300u64.clamp(5, 3600);
        assert_eq!(Duration::from_secs(secs), Duration::from_secs(300));
    }
}
