//! Debounced filesystem watch that triggers async library rescans.

use crate::fs_kind::is_network_fs;
use crate::{LibraryPool, start_scan_job};
use nightjar_db::Db;
use notify::RecursiveMode;
use notify_debouncer_mini::{DebouncedEventKind, new_debouncer};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Steady-state poll interval for roots that must poll (network, or local
/// before notify arms). Fixed on purpose: the earlier
/// `max(60s, 2 × last_index_duration)` never changed the answer for warm walks
/// under 30s, so the floor was the real policy and the formula was decoration.
const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// Watch every library root; on change, start an async rescan (mtime-incremental).
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
    let mut poll_roots: HashMap<i64, PathBuf> = HashMap::new();
    let mut last_poll = std::time::Instant::now();
    let mut last_reach = std::time::Instant::now();
    loop {
        sync_poll_roots(&db, &mut poll_roots)?;
        maybe_reachability(&pool, &mut last_reach);
        std::thread::sleep(Duration::from_secs(5));
        maybe_poll(&db, &pool, &poll_roots, &mut last_poll);
    }
}

fn run_with_notify(db: Arc<Db>, pool: Arc<LibraryPool>) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = new_debouncer(Duration::from_secs(2), move |res| {
        let _ = tx.send(res);
    })
    .map_err(|e| format!("create debouncer: {e}"))?;

    // Network roots always poll (notify can arm and still miss creates).
    // Local roots poll until the first index finishes, then move to notify only.
    let mut poll_roots: HashMap<i64, PathBuf> = HashMap::new();
    let mut notify_roots: HashMap<i64, PathBuf> = HashMap::new();
    let mut last_poll = std::time::Instant::now();
    let mut last_reach = std::time::Instant::now();
    let mut local_notify_armed = false;
    loop {
        if !local_notify_armed && pool.last_index_duration_ms() > 0 {
            local_notify_armed = true;
            tracing::info!("arming recursive FS notify for local library roots");
        }
        sync_watch_sets(
            &db,
            &mut debouncer,
            &mut poll_roots,
            &mut notify_roots,
            local_notify_armed,
        )?;

        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(events)) => {
                for ev in events {
                    if !matches!(ev.kind, DebouncedEventKind::Any) {
                        continue;
                    }
                    if let Some(id) = library_for_path(&notify_roots, &ev.path) {
                        if !pool.is_library_reachable(id) {
                            continue;
                        }
                        tracing::info!(
                            library_id = id,
                            path = %ev.path.display(),
                            "fs change; starting scan job"
                        );
                        match db.active_scan_job(id) {
                            Ok(Some(_)) => pool.mark_scan_dirty(id),
                            Ok(None) => {}
                            Err(e) => tracing::warn!(
                                library_id = id,
                                error = %e,
                                "active scan job check failed"
                            ),
                        }
                        match start_scan_job(Arc::clone(&db), Arc::clone(&pool), id) {
                            Ok(job_id) => {
                                tracing::info!(library_id = id, job_id, "watch scan job accepted")
                            }
                            Err(e) => {
                                tracing::warn!(library_id = id, error = %e, "watch scan failed")
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
        maybe_poll(&db, &pool, &poll_roots, &mut last_poll);
    }
}

fn maybe_reachability(pool: &Arc<LibraryPool>, last: &mut std::time::Instant) {
    if last.elapsed() < crate::reachability::REACHABILITY_INTERVAL {
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
    poll_roots: &HashMap<i64, PathBuf>,
    last_poll: &mut std::time::Instant,
) {
    if last_poll.elapsed() < POLL_INTERVAL {
        return;
    }
    for library_id in poll_roots.keys() {
        if !pool.is_library_reachable(*library_id) {
            continue;
        }
        tracing::info!(
            library_id,
            poll_interval_s = POLL_INTERVAL.as_secs(),
            "poll rescan; starting scan job"
        );
        if let Err(e) = start_scan_job(Arc::clone(db), Arc::clone(pool), *library_id) {
            tracing::warn!(library_id, error = %e, "poll scan failed");
        }
    }
    *last_poll = std::time::Instant::now();
}

fn sync_poll_roots(db: &Db, poll_roots: &mut HashMap<i64, PathBuf>) -> Result<(), String> {
    let libs = db.list_libraries()?;
    let live: std::collections::HashSet<i64> = libs.iter().map(|l| l.id).collect();
    poll_roots.retain(|id, _| live.contains(id));
    for lib in libs {
        let path = PathBuf::from(&lib.path);
        if poll_roots.get(&lib.id) != Some(&path) {
            tracing::info!(library_id = lib.id, path = %path.display(), "poll-only library root");
            poll_roots.insert(lib.id, path);
        }
    }
    Ok(())
}

fn sync_watch_sets(
    db: &Db,
    debouncer: &mut notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>,
    poll_roots: &mut HashMap<i64, PathBuf>,
    notify_roots: &mut HashMap<i64, PathBuf>,
    local_notify_armed: bool,
) -> Result<(), String> {
    let libs = db.list_libraries()?;
    let live: std::collections::HashSet<i64> = libs.iter().map(|l| l.id).collect();
    for id in notify_roots
        .keys()
        .copied()
        .filter(|id| !live.contains(id))
        .collect::<Vec<_>>()
    {
        if let Some(path) = notify_roots.remove(&id) {
            let _ = debouncer.watcher().unwatch(&path);
        }
    }
    poll_roots.retain(|id, _| live.contains(id));

    for lib in libs {
        let path = PathBuf::from(&lib.path);
        let network = is_network_fs(&path);
        // Network: poll forever (notify untrustworthy). Local: poll until armed,
        // then notify only.
        let use_poll = network || !local_notify_armed;
        if use_poll {
            if let Some(old) = notify_roots.remove(&lib.id) {
                let _ = debouncer.watcher().unwatch(&old);
            }
            if poll_roots.get(&lib.id) != Some(&path) {
                tracing::info!(
                    library_id = lib.id,
                    path = %path.display(),
                    network,
                    "poll library root"
                );
                poll_roots.insert(lib.id, path);
            }
            continue;
        }

        poll_roots.remove(&lib.id);
        if notify_roots.get(&lib.id) == Some(&path) {
            continue;
        }
        if let Some(old) = notify_roots.remove(&lib.id) {
            let _ = debouncer.watcher().unwatch(&old);
        }
        match debouncer.watcher().watch(&path, RecursiveMode::Recursive) {
            Ok(()) => {
                tracing::info!(library_id = lib.id, path = %path.display(), "watching library");
                notify_roots.insert(lib.id, path);
            }
            Err(e) => {
                // Watch failed: keep polling this root so adds are not lost.
                tracing::warn!(
                    library_id = lib.id,
                    path = %path.display(),
                    error = %e,
                    "watch path failed; falling back to poll"
                );
                poll_roots.insert(lib.id, path);
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
    fn poll_interval_is_fixed_sixty_seconds() {
        assert_eq!(POLL_INTERVAL, Duration::from_secs(60));
    }
}
