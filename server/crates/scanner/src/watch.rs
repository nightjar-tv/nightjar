//! Debounced filesystem watch that triggers async library rescans.

use crate::{LibraryPool, start_scan_job};
use nightjar_db::Db;
use notify::RecursiveMode;
use notify_debouncer_mini::{DebouncedEventKind, new_debouncer};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

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
    // Recursive FS watches on SMB compete with the index walk for metadata
    // IOPS (dogfood: Movies cold walk >15 min with notify vs ~2 min poll-only).
    // Poll remains the reliable path on network shares (ADR-0013).
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
    loop {
        sync_library_roots(&db, &mut watched)?;
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
    // Recursive SMB watches compete with the cold walk for metadata IOPS.
    // Poll until one index pass finishes (walk cache warm), then arm notify.
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
                        tracing::info!(
                            library_id = id,
                            path = %ev.path.display(),
                            "fs change; starting scan job"
                        );
                        // If a walk is already past this directory, coalescing
                        // into the active job would miss the add. Mark dirty so
                        // a follow-up scan runs when the active job finishes.
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
        maybe_poll(&db, &pool, &watched, &mut last_poll);
    }
}

fn maybe_poll(
    db: &Arc<Db>,
    pool: &Arc<LibraryPool>,
    watched: &HashMap<i64, PathBuf>,
    last_poll: &mut std::time::Instant,
) {
    let poll_every = pool.poll_interval();
    if last_poll.elapsed() < poll_every {
        return;
    }
    for library_id in watched.keys() {
        tracing::info!(
            library_id,
            poll_interval_s = poll_every.as_secs(),
            "poll rescan; starting scan job"
        );
        if let Err(e) = start_scan_job(Arc::clone(db), Arc::clone(pool), *library_id) {
            tracing::warn!(library_id, error = %e, "poll scan failed");
        }
    }
    *last_poll = std::time::Instant::now();
}

fn sync_library_roots(db: &Db, watched: &mut HashMap<i64, PathBuf>) -> Result<(), String> {
    for lib in db.list_libraries()? {
        let path = PathBuf::from(&lib.path);
        if watched.get(&lib.id) != Some(&path) {
            tracing::info!(library_id = lib.id, path = %path.display(), "poll-only library root");
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
    for lib in libs {
        let path = PathBuf::from(&lib.path);
        if watched.get(&lib.id) == Some(&path) {
            continue;
        }
        match debouncer.watcher().watch(&path, RecursiveMode::Recursive) {
            Ok(()) => {
                tracing::info!(library_id = lib.id, path = %path.display(), "watching library");
                watched.insert(lib.id, path);
            }
            Err(e) => {
                tracing::warn!(library_id = lib.id, path = %path.display(), error = %e, "watch path failed");
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
