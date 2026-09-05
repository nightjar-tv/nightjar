//! Library discovery triggers: notify + fixed poll backstop (ADR-0015).
//!
//! Full walks enter through [`crate::request_scan`] (poll, manual, create).
//! Notify media paths use [`crate::hint_ingest`] alone for creates; poll
//! remains the delete/heal bound. Notify never disables poll.

use crate::reachability::REACHABILITY_INTERVAL;
use crate::{HintIngestOutcome, LibraryPool, ScanTrigger, hint_ingest, is_media, request_scan};
use nightjar_db::Db;
use notify::RecursiveMode;
use notify_debouncer_mini::{DebouncedEventKind, new_debouncer};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Fixed poll interval (ADR-0015). Safety net when notify is mute; notify still
/// accelerates local creates. Default 300 s after multi-library walk pile-up
/// on shared mounts; override with `NIGHTJAR_POLL_INTERVAL_SECS`.
const DEFAULT_POLL_INTERVAL_SECS: u64 = 300;

/// Poll backstop interval (ADR-0015 decision 6): env override, clamp 5..=3600.
fn poll_interval() -> Duration {
    let secs = std::env::var("NIGHTJAR_POLL_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_POLL_INTERVAL_SECS)
        .clamp(5, 3600);
    Duration::from_secs(secs)
}

/// Steady-state wait of the watch loops: the poll-only sleep and the notify
/// receive window are the same tick. Five seconds is the cadence these loops
/// have always used; no record states where it came from, so it stays a
/// constant rather than a value someone re-derives.
fn tick() -> Duration {
    #[cfg(test)]
    {
        let override_ms = TEST_TICK_MS.load(Ordering::Relaxed);
        if override_ms > 0 {
            return Duration::from_millis(override_ms);
        }
    }
    Duration::from_secs(5)
}

/// Test-only pacing for the watch loops ([`tick`], [`TEST_MAX_TICKS`], and the
/// tests at the foot of this file). Production builds carry none of these: the
/// loops run at the fixed five-second cadence and end only when the process
/// does.
#[cfg(test)]
static TEST_TICK_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// When nonzero, the watch loops stop (returning `Ok`) after this many ticks,
/// so a test can observe a bounded number of iterations of a loop that in
/// production never ends.
#[cfg(test)]
static TEST_MAX_TICKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// True once an armed [`TEST_MAX_TICKS`] budget is spent. Never true when the
/// budget is unarmed (zero), which is every non-test run.
#[cfg(test)]
fn test_tick_budget_exhausted() -> bool {
    let mut remaining = TEST_MAX_TICKS.load(Ordering::SeqCst);
    loop {
        if remaining == 0 {
            return false;
        }
        match TEST_MAX_TICKS.compare_exchange_weak(
            remaining,
            remaining - 1,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => return remaining == 1,
            Err(observed) => remaining = observed,
        }
    }
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
        // A `list_libraries` failure used to end the watcher here: the `?`
        // propagated out of the loop and the thread stopped notifying, polling,
        // healing deletes and ticking reachability for the life of the process
        // — one unlucky SQLITE_BUSY read among the scans, metadata drain and
        // API writes this loop runs beside. That class of error is transient
        // (db/src/store.rs retries exactly it), and this sync re-runs every
        // tick anyway: log, keep the previous map, retry next tick. Nothing in
        // this loop is worth ending the watcher over.
        // A `list_libraries` failure used to end the watcher here: the `?`
        // propagated out of the loop and the thread stopped notifying, polling,
        // healing deletes and ticking reachability for the life of the process
        // — one unlucky SQLITE_BUSY read among the scans, metadata drain and
        // API writes this loop runs beside. That class of error is transient
        // (db/src/store.rs retries exactly it), and this sync re-runs every
        // tick anyway: log, keep the previous map, retry next tick. Nothing in
        // this loop is worth ending the watcher over.
        if let Err(e) = sync_library_roots(&db, &mut watched) {
            tracing::warn!(error = %e, "library root sync failed; retrying next tick");
        }
        maybe_reachability(&pool, &mut last_reach);
        std::thread::sleep(tick());
        maybe_poll(&db, &pool, &watched, &mut last_poll);
        #[cfg(test)]
        if test_tick_budget_exhausted() {
            return Ok(());
        }
    }
}

fn run_with_notify(db: Arc<Db>, pool: Arc<LibraryPool>) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = match new_debouncer(Duration::from_secs(2), move |res| {
        let _ = tx.send(res);
    }) {
        Ok(d) => d,
        Err(e) => {
            // Notify is only an accelerator; the fixed poll is the discovery
            // and delete-heal guarantee (ADR-0015). A debouncer that will not
            // start must not take the poll backstop and reachability ticks
            // down with it: fall back to the poll-only cadence instead of
            // ending the watcher thread.
            tracing::error!(error = %e, "create fs debouncer failed; running poll-only");
            return run_poll_only(db, pool);
        }
    };

    let mut watched: HashMap<i64, PathBuf> = HashMap::new();
    let mut last_poll = std::time::Instant::now();
    let mut last_reach = std::time::Instant::now();
    // Defer recursive watches until the first index finishes so they do not
    // compete with cold SMB metadata IOPS (ADR-0013). Poll still runs.
    let mut notify_armed = false;
    loop {
        if notify_armed {
            // Same transient-DB-error rule as run_poll_only: a failed watch
            // sync is logged and retried next tick, never allowed to end the
            // thread that also owns the poll backstop.
            if let Err(e) = sync_watches(&db, &mut debouncer, &mut watched) {
                tracing::warn!(error = %e, "watch sync failed; retrying next tick");
            }
        } else {
            if let Err(e) = sync_library_roots(&db, &mut watched) {
                tracing::warn!(error = %e, "library root sync failed; retrying next tick");
            } else if pool.last_index_duration_ms() > 0 {
                watched.clear();
                if let Err(e) = sync_watches(&db, &mut debouncer, &mut watched) {
                    tracing::warn!(error = %e, "watch sync failed; retrying next tick");
                } else {
                    notify_armed = true;
                    tracing::info!("armed recursive FS notify after first index pass");
                }
            }
        }

        match rx.recv_timeout(tick()) {
            Ok(Ok(events)) => {
                for ev in events {
                    if !matches!(ev.kind, DebouncedEventKind::Any) {
                        continue;
                    }
                    if let Some(id) = library_for_path(&watched, &ev.path) {
                        if !pool.is_library_reachable(id) {
                            continue;
                        }
                        // Show/season dirs and sidecars fire often on SMB; not media creates.
                        // Ignored is expected — not "old files failed to index."
                        if ev.path.is_dir() {
                            tracing::debug!(
                                library_id = id,
                                path = %ev.path.display(),
                                "fs change; ignored directory (hint is media files only)"
                            );
                            continue;
                        }
                        if !is_media(&ev.path) {
                            tracing::debug!(
                                library_id = id,
                                path = %ev.path.display(),
                                "fs change; ignored non-media (sidecars/poll heal deletes)"
                            );
                            continue;
                        }
                        // Creates: hint only. No request_scan (poll heals deletes).
                        match hint_ingest(db.as_ref(), pool.as_ref(), id, &ev.path) {
                            Ok(HintIngestOutcome::Upserted { item_id }) => {
                                tracing::info!(
                                    library_id = id,
                                    item_id,
                                    path = %ev.path.display(),
                                    "fs change; hint ingest upserted"
                                );
                            }
                            Ok(HintIngestOutcome::Unchanged { item_id }) => {
                                // SMB often re-notifies existing files; not a new create.
                                tracing::debug!(
                                    library_id = id,
                                    item_id,
                                    path = %ev.path.display(),
                                    "fs change; hint ingest unchanged"
                                );
                            }
                            Ok(HintIngestOutcome::Ignored) => {
                                tracing::debug!(
                                    library_id = id,
                                    path = %ev.path.display(),
                                    "fs change; hint ingest ignored"
                                );
                            }
                            Ok(HintIngestOutcome::Collision) => {
                                tracing::warn!(
                                    library_id = id,
                                    path = %ev.path.display(),
                                    "fs change; hint ingest fold collision"
                                );
                            }
                            Err(e) => tracing::warn!(
                                library_id = id,
                                path = %ev.path.display(),
                                error = %e,
                                "hint ingest failed"
                            ),
                        }
                    }
                }
            }
            Ok(Err(e)) => tracing::warn!(error = %e, "watch error"),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // What still ends the notify half, deliberately: a disconnected
                // channel means the debouncer's worker thread is gone and
                // nothing in this process can restart it, so staying here would
                // spin — recv_timeout returns Disconnected instantly, with no
                // five-second wait — and notify would never deliver again. What
                // it must NOT end is the watcher: poll is the discovery
                // guarantee (ADR-0015), so degrade to the poll-only cadence.
                // Resetting the poll/reach timers can only delay the next tick,
                // never fire one early.
                tracing::error!(
                    "watch channel disconnected; notify worker gone; running poll-only"
                );
                return run_poll_only(db, pool);
            }
        }
        maybe_reachability(&pool, &mut last_reach);
        maybe_poll(&db, &pool, &watched, &mut last_poll);
        #[cfg(test)]
        if test_tick_budget_exhausted() {
            return Ok(());
        }
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
        if let Err(e) = request_scan(
            Arc::clone(db),
            Arc::clone(pool),
            *library_id,
            ScanTrigger::Poll,
        ) {
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
    use std::sync::Mutex;

    /// Serialises the watch tests: they share the process-global env
    /// (`NIGHTJAR_POLL_INTERVAL_SECS`) and the [`TEST_MAX_TICKS`] /
    /// [`TEST_TICK_MS`] pacing statics, and `cargo test` runs them on threads.
    static WATCH_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn test_pool(db: &Arc<Db>, dir: &std::path::Path) -> Arc<LibraryPool> {
        let subs = Arc::new(nightjar_transcode::SubsStore::new(dir.join("subs")).unwrap());
        LibraryPool::spawn(Arc::clone(db), subs)
    }

    /// Poison the Db connection mutex so every later call fails with
    /// "database lock poisoned" — the error [`Db::lock`] returns after a panic
    /// elsewhere while the connection was held. This is a real failure mode of
    /// [`Db::list_libraries`], and it is deterministic, which no amount of
    /// SQLite busy traffic is in a unit test.
    fn poison_db_lock(db: &Db) {
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<(), String> = db.with_conn(|_| panic!("poison the db lock"));
        }));
        std::panic::set_hook(previous_hook);
        assert!(
            panicked.is_err(),
            "the with_conn closure must panic to poison the connection mutex"
        );
        assert!(
            db.list_libraries().is_err(),
            "list_libraries must fail once the lock is poisoned"
        );
    }

    /// A failing `list_libraries` must not end the poll loop. Before the fix
    /// the `?` at the top of the iteration propagated out of the loop and the
    /// watcher thread ended; this runs the real loop against a Db whose every
    /// read fails and asserts it still completes its (test-bounded) run.
    #[test]
    fn a_failing_list_libraries_does_not_end_the_poll_loop() {
        let _guard = WATCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        poison_db_lock(&db);

        TEST_MAX_TICKS.store(3, Ordering::SeqCst);
        TEST_TICK_MS.store(2, Ordering::SeqCst);
        let result = run_poll_only(Arc::clone(&db), pool);
        TEST_MAX_TICKS.store(0, Ordering::SeqCst);
        TEST_TICK_MS.store(0, Ordering::SeqCst);

        assert_eq!(result, Ok(()), "the loop must survive every sync failing");
        assert!(
            db.list_libraries().is_err(),
            "test premise: the db stayed poisoned for the whole run"
        );
    }

    /// The notify loop's pre-arm sync has the same `?` defect shape as the
    /// poll loop's: one failing `list_libraries` used to end the thread that
    /// also owns the poll backstop. Same premise, same bounded run.
    #[test]
    fn a_failing_list_libraries_does_not_end_the_notify_loop() {
        let _guard = WATCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        poison_db_lock(&db);

        TEST_MAX_TICKS.store(3, Ordering::SeqCst);
        TEST_TICK_MS.store(2, Ordering::SeqCst);
        let result = run_with_notify(Arc::clone(&db), pool);
        TEST_MAX_TICKS.store(0, Ordering::SeqCst);
        TEST_TICK_MS.store(0, Ordering::SeqCst);

        assert_eq!(result, Ok(()), "the loop must survive every sync failing");
        assert!(
            db.list_libraries().is_err(),
            "test premise: the db stayed poisoned for the whole run"
        );
    }

    /// The poll interval is read from the env by the real function. The old
    /// test asserted the constant against itself and re-implemented the clamp
    /// inline, so a broken env parse, a wrong clamp bound or a `.unwrap_or`
    /// regression all passed. The literals here are the ADR-0015 decision 6
    /// values (default 300, clamp 5..=3600), not re-reads of the constant.
    #[test]
    fn poll_interval_reads_the_env_and_clamps() {
        let _guard = WATCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        // Restore the caller's value on the way out, whatever happens between.
        struct EnvGuard(Option<std::ffi::OsString>);
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match &self.0 {
                    Some(v) => unsafe {
                        std::env::set_var("NIGHTJAR_POLL_INTERVAL_SECS", v);
                    },
                    None => unsafe {
                        std::env::remove_var("NIGHTJAR_POLL_INTERVAL_SECS");
                    },
                }
            }
        }
        let _env = EnvGuard(std::env::var_os("NIGHTJAR_POLL_INTERVAL_SECS"));
        // SAFETY: env is per-process and set_var/remove_var are unsafe in
        // edition 2024. WATCH_TEST_LOCK serialises this test against the only
        // other readers of NIGHTJAR_POLL_INTERVAL_SECS in this binary (the
        // watch-loop tests, which call poll_interval from maybe_poll), and the
        // EnvGuard restores the original value before the guard drops.
        unsafe { std::env::remove_var("NIGHTJAR_POLL_INTERVAL_SECS") };
        assert_eq!(
            poll_interval(),
            Duration::from_secs(300),
            "an unset env must fall back to the ADR-0015 default of 300 s"
        );
        unsafe { std::env::set_var("NIGHTJAR_POLL_INTERVAL_SECS", "not-a-number") };
        assert_eq!(
            poll_interval(),
            Duration::from_secs(300),
            "an unparsable env must fall back to the default"
        );
        unsafe { std::env::set_var("NIGHTJAR_POLL_INTERVAL_SECS", "120") };
        assert_eq!(
            poll_interval(),
            Duration::from_secs(120),
            "a parseable env must be honoured"
        );
        unsafe { std::env::set_var("NIGHTJAR_POLL_INTERVAL_SECS", "99999") };
        assert_eq!(
            poll_interval(),
            Duration::from_secs(3600),
            "the clamp upper bound is 3600 s (ADR-0015)"
        );
        unsafe { std::env::set_var("NIGHTJAR_POLL_INTERVAL_SECS", "1") };
        assert_eq!(
            poll_interval(),
            Duration::from_secs(5),
            "the clamp lower bound is 5 s (ADR-0015)"
        );
    }
}
