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
    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = new_debouncer(Duration::from_secs(2), move |res| {
        let _ = tx.send(res);
    })
    .map_err(|e| format!("create debouncer: {e}"))?;

    let mut watched: HashMap<i64, PathBuf> = HashMap::new();
    let mut last_poll = std::time::Instant::now();
    loop {
        sync_watches(&db, &mut debouncer, &mut watched)?;

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
        if last_poll.elapsed() >= Duration::from_secs(60) {
            for library_id in watched.keys() {
                tracing::info!(library_id, "poll rescan; starting scan job");
                if let Err(e) = start_scan_job(Arc::clone(&db), Arc::clone(&pool), *library_id) {
                    tracing::warn!(library_id, error = %e, "poll scan failed");
                }
            }
            last_poll = std::time::Instant::now();
        }
    }
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
