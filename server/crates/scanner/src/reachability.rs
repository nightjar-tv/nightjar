//! Library root reachability (ADR-0014).

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
}

/// Timed `path.is_dir()`. Timeout ⇒ unreachable so a hung SMB mount cannot wedge.
pub fn check_root(path: &Path) -> Reachability {
    check_root_with_timeout(path, REACHABILITY_TIMEOUT)
}

pub fn check_root_with_timeout(path: &Path, timeout: Duration) -> Reachability {
    let path = path.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("reachability-check".into())
        .spawn(move || {
            let ok = path.is_dir();
            let _ = tx.send(ok);
        })
        .ok();
    match rx.recv_timeout(timeout) {
        Ok(true) => Reachability::Reachable,
        Ok(false) | Err(_) => Reachability::Unreachable,
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
    use tempfile::tempdir;

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
}
