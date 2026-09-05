//! Library root reachability (ADR-0014).
//!
//! Each root path gets one dedicated worker thread that runs the blocking
//! `is_dir` probe for every tick. A probe can block in the kernel forever on a
//! hung SMB/NFS mount; a dedicated worker per root bounds that leak to one
//! stranded thread per hung root instead of one new stranded thread per tick.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// How long a single root `is_dir` may block before we treat the mount as hung.
pub const REACHABILITY_TIMEOUT: Duration = Duration::from_secs(5);

/// Interval between reachability ticks.
pub const REACHABILITY_INTERVAL: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reachability {
    Reachable,
    /// Root missing, not a directory, or timed out.
    Unreachable,
    /// The check itself could not run (its thread could not be spawned or
    /// died). This is a statement about the instrument, not about the root
    /// (Rule 4.15): callers must not pause, unpause, or refuse work on it.
    CheckFailed,
}

/// One probe request handed to a root's dedicated worker thread.
struct ProbeRequest {
    reply: Sender<Reachability>,
}

/// A probe run inside the worker: tests inject a probe that hangs or fails.
type ProbeFn = fn(&Path) -> bool;

/// Spawns the worker thread for a root. Injectable so a test can make the
/// spawn itself fail.
type SpawnFn = fn(PathBuf, ProbeFn) -> std::io::Result<Sender<ProbeRequest>>;

/// One live worker per root path, so a blocked probe never grows a new
/// stranded thread per tick.
fn workers() -> &'static Mutex<HashMap<PathBuf, Sender<ProbeRequest>>> {
    static WORKERS: OnceLock<Mutex<HashMap<PathBuf, Sender<ProbeRequest>>>> = OnceLock::new();
    WORKERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Spawn the single worker that serves every probe for `path`. The worker runs
/// one probe at a time; callers whose probe does not answer within their own
/// timeout give up and report `Unreachable`, and the request is answered when
/// the mount answers (or is never answered, leaving one stranded thread).
fn spawn_worker(path: PathBuf, probe: ProbeFn) -> std::io::Result<Sender<ProbeRequest>> {
    let (tx, rx): (Sender<ProbeRequest>, _) = mpsc::channel();
    std::thread::Builder::new()
        .name("reachability-check".into())
        .spawn(move || {
            while let Ok(request) = rx.recv() {
                let outcome = if probe(&path) {
                    Reachability::Reachable
                } else {
                    Reachability::Unreachable
                };
                let _ = request.reply.send(outcome);
            }
        })?;
    Ok(tx)
}

/// Timed `path.is_dir()`. Timeout ⇒ unreachable so a hung SMB mount cannot wedge.
pub fn check_root(path: &Path) -> Reachability {
    check_root_with_timeout(path, REACHABILITY_TIMEOUT)
}

pub fn check_root_with_timeout(path: &Path, timeout: Duration) -> Reachability {
    check_with(path, timeout, Path::is_dir, spawn_worker)
}

fn check_with(path: &Path, timeout: Duration, probe: ProbeFn, spawn: SpawnFn) -> Reachability {
    let key = path.to_path_buf();
    let worker = {
        let mut registry = workers().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(worker) = registry.get(&key) {
            worker.clone()
        } else {
            match spawn(key.clone(), probe) {
                Ok(worker) => {
                    registry.insert(key.clone(), worker.clone());
                    worker
                }
                Err(e) => {
                    tracing::warn!(
                        path = %key.display(),
                        error = %e,
                        "reachability check thread spawn failed; not a finding"
                    );
                    return Reachability::CheckFailed;
                }
            }
        }
    };

    let (reply, reply_rx) = mpsc::channel();
    if worker.send(ProbeRequest { reply }).is_err() {
        let mut registry = workers().lock().unwrap_or_else(|e| e.into_inner());
        registry.remove(&key);
        tracing::error!(
            path = %key.display(),
            "reachability check worker died; not a finding"
        );
        return Reachability::CheckFailed;
    }
    match reply_rx.recv_timeout(timeout) {
        Ok(outcome) => outcome,
        // The worker is still blocked in its probe: the mount has not answered
        // within the budget, which means unreachable (ADR-0014 §8). The worker
        // stays registered so the next tick queues behind it rather than
        // stranding another thread.
        Err(RecvTimeoutError::Timeout) => Reachability::Unreachable,
        Err(RecvTimeoutError::Disconnected) => {
            let mut registry = workers().lock().unwrap_or_else(|e| e.into_inner());
            registry.remove(&key);
            tracing::error!(
                path = %key.display(),
                "reachability check worker died mid-probe; not a finding"
            );
            Reachability::CheckFailed
        }
    }
}

/// True when an error string indicates mount/IO absence rather than corrupt media.
pub fn message_looks_unavailable(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("no such file")
        || lower.contains("not a directory")
        || lower.contains("input/output error")
        || lower.contains("host is down")
        || lower.contains("network is unreachable")
        || lower.contains("connection timed out")
        || lower.contains("connection reset")
        || lower.contains("broken pipe")
        || lower.contains("stale file handle")
        || lower.contains("estale")
        || lower.contains("enotconn")
        || lower.starts_with("unavailable:")
}

/// Non-overlapping tick gate: skip if a previous tick is still running.
pub struct TickGate {
    busy: AtomicBool,
}

impl TickGate {
    pub fn new() -> Self {
        Self {
            busy: AtomicBool::new(false),
        }
    }

    /// Returns true if this caller acquired the tick (must call `end`).
    pub fn try_begin(&self) -> bool {
        self.busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn end(&self) {
        self.busy.store(false, Ordering::SeqCst);
    }
}

impl Default for TickGate {
    fn default() -> Self {
        Self::new()
    }
}

/// In-memory pause set for libraries whose roots are unreachable.
#[derive(Default)]
pub struct PauseSet {
    inner: Mutex<std::collections::HashSet<i64>>,
}

impl PauseSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_paused(&self, library_id: i64) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&library_id)
    }

    pub fn set_paused(&self, library_id: i64, paused: bool) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if paused {
            g.insert(library_id);
        } else {
            g.remove(&library_id);
        }
    }
}

/// Shared handle used by the pool and the watcher tick.
pub struct Availability {
    pub pause: PauseSet,
    pub tick_gate: TickGate,
    /// Test/support counter of availability transitions.
    pub transitions: std::sync::atomic::AtomicU64,
}

impl Availability {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            pause: PauseSet::new(),
            tick_gate: TickGate::new(),
            transitions: std::sync::atomic::AtomicU64::new(0),
        })
    }
}

/// Whether an index pass may call `delete_missing` (ADR-0014 §2).
pub fn allow_delete_missing(
    root_reachable_before: bool,
    root_reachable_after: bool,
    listing_errors: u32,
    files_empty: bool,
    existing_item_count: i64,
) -> bool {
    if !root_reachable_before || !root_reachable_after {
        return false;
    }
    if listing_errors > 0 {
        return false;
    }
    // Empty walk with prior rows: treat as reachability doubt (stale/half-dead mount).
    if files_empty && existing_item_count > 0 {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;
    use tempfile::tempdir;

    static HUNG_PROBE_ENTRIES: AtomicUsize = AtomicUsize::new(0);
    static HUNG_GATE: OnceLock<std::sync::Barrier> = OnceLock::new();

    /// Probe that never returns, like `is_dir` wedged in a hung SMB mount.
    fn hung_probe(_: &Path) -> bool {
        HUNG_PROBE_ENTRIES.fetch_add(1, Ordering::SeqCst);
        // Barrier of two: only this worker ever arrives, so it blocks forever.
        let gate = HUNG_GATE.get_or_init(|| std::sync::Barrier::new(2));
        let _ = gate.wait();
        false
    }

    /// A path no other test uses, so workers registered here are not shared.
    fn probe_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("nightjar-reach-{name}-{}", std::process::id()))
    }

    #[test]
    fn reachable_dir() {
        let d = tempdir().unwrap();
        assert_eq!(check_root(d.path()), Reachability::Reachable);
    }

    #[test]
    fn missing_path_unreachable() {
        let p = PathBuf::from("/no/such/nightjar/library/root");
        assert_eq!(check_root(&p), Reachability::Unreachable);
    }

    #[test]
    fn empty_walk_with_items_blocks_delete() {
        assert!(!allow_delete_missing(true, true, 0, true, 10));
        assert!(allow_delete_missing(true, true, 0, true, 0));
        assert!(allow_delete_missing(true, true, 0, false, 10));
        assert!(!allow_delete_missing(true, false, 0, false, 10));
        assert!(!allow_delete_missing(true, true, 1, false, 10));
    }

    #[test]
    fn tick_gate_non_overlapping() {
        let g = TickGate::new();
        assert!(g.try_begin());
        assert!(!g.try_begin());
        g.end();
        assert!(g.try_begin());
        g.end();
    }

    #[test]
    fn file_is_not_a_library_root() {
        let d = tempdir().unwrap();
        let f = d.path().join("x");
        fs::write(&f, b"x").unwrap();
        assert_eq!(check_root(&f), Reachability::Unreachable);
    }

    #[test]
    fn repeated_timeouts_reuse_one_worker_thread() {
        HUNG_PROBE_ENTRIES.store(0, Ordering::SeqCst);
        let path = probe_path("hung");
        let short = Duration::from_millis(50);

        assert_eq!(
            check_with(&path, short, hung_probe, spawn_worker),
            Reachability::Unreachable
        );
        // The first tick spawns one worker; wait until it is blocked inside the
        // hung probe so the entry-count assertions below are not racing it.
        for _ in 0..1000 {
            if HUNG_PROBE_ENTRIES.load(Ordering::SeqCst) == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(HUNG_PROBE_ENTRIES.load(Ordering::SeqCst), 1);

        // Every later tick queues behind the one blocked worker: the probe is
        // never entered again, so no second stranded thread is created.
        for _ in 0..4 {
            assert_eq!(
                check_with(&path, short, hung_probe, spawn_worker),
                Reachability::Unreachable
            );
        }
        assert_eq!(HUNG_PROBE_ENTRIES.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn spawn_failure_is_not_reported_as_unreachable() {
        let path = probe_path("spawn-failure");
        let result = check_with(&path, Duration::from_millis(50), Path::is_dir, |_, _| {
            Err(std::io::Error::new(
                std::io::ErrorKind::ResourceBusy,
                "test: thread spawn failed",
            ))
        });
        assert_eq!(result, Reachability::CheckFailed);
        assert_ne!(result, Reachability::Unreachable);
    }
}
