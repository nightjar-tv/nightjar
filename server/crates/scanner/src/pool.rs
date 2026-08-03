use crate::probe;
use crate::reachability::{self, Availability, Reachability, message_looks_unavailable};
use crate::walk::WalkCache;
use nightjar_db::{Db, ProbeUpdate};
use nightjar_transcode::{ExtractOutcome, SidecarInput, SubsStore, extract_item_subtitles};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkKind {
    Probe,
    Extract,
    /// Keyframe map build (ADR-0023). Shares the background queue with Extract.
    Map,
}

#[derive(Clone)]
pub struct WorkItem {
    pub kind: WorkKind,
    pub item_id: i64,
    pub library_id: i64,
    pub path: PathBuf,
    pub scan_job_id: Option<i64>,
    /// First-play bump: may run while an index walk holds SMB (ADR-0013 §11).
    pub priority: bool,
    batch: Option<Arc<ProbeBatchState>>,
}

impl WorkItem {
    pub fn probe(item_id: i64, library_id: i64, path: PathBuf, scan_job_id: Option<i64>) -> Self {
        Self {
            kind: WorkKind::Probe,
            item_id,
            library_id,
            path,
            scan_job_id,
            priority: false,
            batch: None,
        }
    }

    pub fn extract(item_id: i64, library_id: i64, path: PathBuf) -> Self {
        Self {
            kind: WorkKind::Extract,
            item_id,
            library_id,
            path,
            scan_job_id: None,
            priority: false,
            batch: None,
        }
    }

    pub fn map(item_id: i64, library_id: i64, path: PathBuf) -> Self {
        Self {
            kind: WorkKind::Map,
            item_id,
            library_id,
            path,
            scan_job_id: None,
            priority: false,
            batch: None,
        }
    }
}

struct Queue {
    probes: VecDeque<WorkItem>,
    /// Subtitle extract + keyframe map (ADR-0023 shares this bound with extract).
    background: VecDeque<WorkItem>,
}

struct ProbeBatchState {
    remaining: Mutex<usize>,
    ready: Condvar,
}

pub struct ProbeBatch {
    state: Arc<ProbeBatchState>,
}

impl ProbeBatch {
    pub fn wait(self) {
        let mut remaining = self
            .state
            .remaining
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while *remaining != 0 {
            remaining = self
                .state
                .ready
                .wait(remaining)
                .unwrap_or_else(|e| e.into_inner());
        }
    }
}

pub struct LibraryPool {
    db: Arc<Db>,
    subs: Arc<SubsStore>,
    queue: Mutex<Queue>,
    available: Condvar,
    walk_caches: Mutex<HashMap<i64, WalkCache>>,
    last_index_ms: AtomicU64,
    /// Manual POST .../scan while active: one follow-up after the job ends.
    scan_dirty: Mutex<HashSet<i64>>,
    /// Path-hint upsert while a scan is active: skip delete_missing on that
    /// job (row may be outside the walk keep-set). Does not schedule follow-up.
    dirty_add: Mutex<HashSet<i64>>,
    /// After repoint with deferred_remove > 0: poll skips full scans until
    /// expiry or a successful ordinary scan (ADR-0030 Gate 3).
    repoint_holdoff_until: Mutex<HashMap<i64, Instant>>,
    /// Process-wide exclusive index/walk epoch (ADR-0015). Holding this mutex
    /// is the only way to run a tree walk or index upsert pass.
    index_epoch: Mutex<()>,
    index_active: AtomicUsize,
    /// Item ids whose extract worker is running (not merely queued).
    extracting: Mutex<HashSet<i64>>,
    /// Item ids whose map worker is running (not merely queued).
    mapping: Mutex<HashSet<i64>>,
    pub availability: Arc<Availability>,
}

/// RAII permit for one process-wide index/walk epoch (ADR-0015).
///
/// Drop releases the epoch so the next library can walk. While held,
/// background extract/map wait unless play-priority (ADR-0013).
pub struct IndexEpochGuard<'a> {
    pool: &'a LibraryPool,
    _lock: std::sync::MutexGuard<'a, ()>,
}

impl Drop for IndexEpochGuard<'_> {
    fn drop(&mut self) {
        let prev = self.pool.index_active.fetch_sub(1, Ordering::SeqCst);
        if prev == 1 {
            self.pool.available.notify_all();
        }
    }
}

impl LibraryPool {
    pub fn spawn(db: Arc<Db>, subs: Arc<SubsStore>) -> Arc<Self> {
        let availability = Availability::new();
        let pool = Arc::new(Self {
            db,
            subs,
            queue: Mutex::new(Queue {
                probes: VecDeque::new(),
                background: VecDeque::new(),
            }),
            available: Condvar::new(),
            walk_caches: Mutex::new(HashMap::new()),
            last_index_ms: AtomicU64::new(0),
            scan_dirty: Mutex::new(HashSet::new()),
            dirty_add: Mutex::new(HashSet::new()),
            repoint_holdoff_until: Mutex::new(HashMap::new()),
            index_epoch: Mutex::new(()),
            index_active: AtomicUsize::new(0),
            extracting: Mutex::new(HashSet::new()),
            mapping: Mutex::new(HashSet::new()),
            availability,
        });
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(2, 16);
        for i in 0..workers {
            let worker = Arc::clone(&pool);
            std::thread::Builder::new()
                .name(format!("library-worker-{i}"))
                .spawn(move || worker.run_worker())
                .expect("spawn library worker");
        }
        // Seed pause set from DB.
        if let Ok(libs) = pool.db.list_libraries() {
            for lib in libs {
                pool.availability.pause.set_paused(lib.id, !lib.reachable);
            }
        }
        pool
    }

    pub fn transition_count(&self) -> u64 {
        self.availability.transitions.load(Ordering::Relaxed)
    }

    pub fn record_index_duration_ms(&self, ms: u64) {
        self.last_index_ms.fetch_max(ms, Ordering::Relaxed);
    }

    pub fn last_index_duration_ms(&self) -> u64 {
        self.last_index_ms.load(Ordering::Relaxed)
    }

    pub fn with_walk_cache<R>(&self, library_id: i64, f: impl FnOnce(&mut WalkCache) -> R) -> R {
        let mut caches = self.walk_caches.lock().unwrap_or_else(|e| e.into_inner());
        let cache = caches.entry(library_id).or_default();
        f(cache)
    }

    /// Replace the per-library walk cache (e.g. after repoint to new absolute roots).
    pub fn replace_walk_cache(&self, library_id: i64, cache: WalkCache) {
        self.walk_caches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(library_id, cache);
    }

    pub fn clear_walk_cache(&self, library_id: i64) {
        self.walk_caches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&library_id);
    }

    /// Manual scan coalesce: one follow-up after the active job finishes.
    pub fn mark_scan_dirty(&self, library_id: i64) {
        self.scan_dirty
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(library_id);
    }

    pub fn is_scan_dirty(&self, library_id: i64) -> bool {
        self.scan_dirty
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&library_id)
    }

    pub fn take_scan_dirty(&self, library_id: i64) -> bool {
        self.scan_dirty
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&library_id)
    }

    /// Hint upsert while a walk is active (ADR-0015 B′). Skips delete_missing
    /// on that job only; does not schedule a follow-up walk.
    pub fn mark_dirty_add(&self, library_id: i64) {
        self.dirty_add
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(library_id);
    }

    pub fn is_dirty_add(&self, library_id: i64) -> bool {
        self.dirty_add
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&library_id)
    }

    pub fn take_dirty_add(&self, library_id: i64) -> bool {
        self.dirty_add
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&library_id)
    }

    /// Arm poll holdoff after repoint deferred deletes (default 1 h in product).
    pub fn set_repoint_delete_holdoff(&self, library_id: i64, duration: Duration) {
        let until = Instant::now() + duration;
        self.repoint_holdoff_until
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(library_id, until);
    }

    /// True while poll should not start a full walk for this library.
    pub fn repoint_delete_holdoff_active(&self, library_id: i64) -> bool {
        let mut map = self
            .repoint_holdoff_until
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match map.get(&library_id).copied() {
            Some(until) if Instant::now() < until => true,
            Some(_) => {
                map.remove(&library_id);
                false
            }
            None => false,
        }
    }

    pub fn clear_repoint_delete_holdoff(&self, library_id: i64) {
        self.repoint_holdoff_until
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&library_id);
    }

    /// Block until no other library is indexing, then hold the epoch.
    /// At most one walk/index upsert runs process-wide (ADR-0015).
    /// Logs when waiting longer than 5s (ops: "indexing" while queued on epoch).
    pub fn enter_index_epoch(&self, library_id: i64) -> IndexEpochGuard<'_> {
        const WAIT_LOG: Duration = Duration::from_secs(5);
        let started = Instant::now();
        let lock = self.index_epoch.lock().unwrap_or_else(|e| e.into_inner());
        let waited = started.elapsed();
        if waited >= WAIT_LOG {
            tracing::info!(
                library_id,
                waited_ms = waited.as_millis() as u64,
                "index epoch wait (another library was walking)"
            );
        }
        self.index_active.fetch_add(1, Ordering::SeqCst);
        IndexEpochGuard {
            pool: self,
            _lock: lock,
        }
    }

    pub fn index_epoch_held(&self) -> bool {
        self.index_active.load(Ordering::SeqCst) != 0
    }

    pub fn is_library_reachable(&self, library_id: i64) -> bool {
        !self.availability.pause.is_paused(library_id)
    }

    /// Apply a reachability transition. One log line per change (ADR-0014).
    pub fn set_library_reachability(
        &self,
        library_id: i64,
        path: &str,
        reachable: bool,
    ) -> Result<(), String> {
        let was_paused = self.availability.pause.is_paused(library_id);
        let now_paused = !reachable;
        if was_paused == now_paused {
            let _ = self.db.set_library_reachable(library_id, reachable);
            return Ok(());
        }
        self.db.set_library_reachable(library_id, reachable)?;
        self.availability.pause.set_paused(library_id, now_paused);
        self.availability
            .transitions
            .fetch_add(1, Ordering::Relaxed);
        if reachable {
            tracing::info!(library_id, path, "library reachable");
            let (probes, extracts, maps) = self.db.requeue_unavailable_for_library(library_id)?;
            if probes > 0 || extracts > 0 || maps > 0 {
                tracing::info!(
                    library_id,
                    probes,
                    extracts,
                    maps,
                    "re-queued availability failures"
                );
            }
            self.purge_queue_for_library(library_id);
            self.drain_pending_probes()?;
            self.drain_pending_extracts()?;
            self.drain_pending_maps()?;
        } else {
            tracing::info!(library_id, path, "library unreachable");
            self.purge_queue_for_library(library_id);
        }
        self.available.notify_all();
        Ok(())
    }

    fn purge_queue_for_library(&self, library_id: i64) {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        let mut kept_probes = VecDeque::new();
        while let Some(item) = queue.probes.pop_front() {
            if item.library_id == library_id {
                if let Some(batch) = item.batch {
                    let mut remaining = batch.remaining.lock().unwrap_or_else(|e| e.into_inner());
                    *remaining = remaining.saturating_sub(1);
                    if *remaining == 0 {
                        batch.ready.notify_all();
                    }
                }
            } else {
                kept_probes.push_back(item);
            }
        }
        queue.probes = kept_probes;
        let mut kept_background = VecDeque::new();
        while let Some(item) = queue.background.pop_front() {
            if item.library_id != library_id {
                kept_background.push_back(item);
            }
        }
        queue.background = kept_background;
    }

    pub fn enqueue(&self, item: WorkItem) {
        if self.availability.pause.is_paused(item.library_id) {
            return;
        }
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        match item.kind {
            WorkKind::Probe => queue.probes.push_back(item),
            WorkKind::Extract | WorkKind::Map => {
                Self::enqueue_background_unique(&mut queue, item);
            }
        }
        self.available.notify_one();
    }

    fn enqueue_background_unique(queue: &mut Queue, item: WorkItem) {
        // Same-pass enqueue + drain can otherwise queue the same item twice.
        if queue
            .background
            .iter()
            .any(|w| w.item_id == item.item_id && w.kind == item.kind)
        {
            return;
        }
        queue.background.push_back(item);
    }

    /// Enqueue a map rebuild unless one is already pending or in flight (ADR-0023 §8).
    pub fn enqueue_map_rebuild(&self, item_id: i64, library_id: i64, path: PathBuf) {
        if self.availability.pause.is_paused(library_id) {
            return;
        }
        if self
            .mapping
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&item_id)
        {
            return;
        }
        let _ = self.db.mark_map_pending(item_id);
        self.enqueue(WorkItem::map(item_id, library_id, path));
    }

    /// Move an item's extract to the front of the queue (first-play path).
    /// No-op when already extracting or already at the front. Does not
    /// interrupt an in-flight demux. Priority extracts may run while an
    /// index walk is active so the title being watched is not stuck behind
    /// a multi-day backfill (ADR-0013 §11).
    pub fn prioritize_extract(&self, item_id: i64, library_id: i64, path: PathBuf) {
        if self.availability.pause.is_paused(library_id) {
            return;
        }
        if self
            .extracting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&item_id)
        {
            return;
        }
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pos) = queue
            .background
            .iter()
            .position(|w| w.item_id == item_id && w.kind == WorkKind::Extract)
        {
            if pos == 0 {
                if let Some(front) = queue.background.front_mut() {
                    front.priority = true;
                }
                self.available.notify_one();
                return;
            }
            if let Some(mut item) = queue.background.remove(pos) {
                item.priority = true;
                queue.background.push_front(item);
            }
        } else {
            let mut item = WorkItem::extract(item_id, library_id, path);
            item.priority = true;
            queue.background.push_front(item);
        }
        self.available.notify_one();
    }

    /// Priority map rebuild for session fallback (ADR-0023 §8).
    pub fn prioritize_map_rebuild(&self, item_id: i64, library_id: i64, path: PathBuf) {
        if self.availability.pause.is_paused(library_id) {
            return;
        }
        if self
            .mapping
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&item_id)
        {
            return;
        }
        let _ = self.db.mark_map_pending(item_id);
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pos) = queue
            .background
            .iter()
            .position(|w| w.item_id == item_id && w.kind == WorkKind::Map)
        {
            if let Some(mut item) = queue.background.remove(pos) {
                item.priority = true;
                queue.background.push_front(item);
            }
        } else {
            let mut item = WorkItem::map(item_id, library_id, path);
            item.priority = true;
            queue.background.push_front(item);
        }
        self.available.notify_one();
    }

    pub fn enqueue_probe_batch(&self, items: Vec<WorkItem>) -> ProbeBatch {
        let state = Arc::new(ProbeBatchState {
            remaining: Mutex::new(items.len()),
            ready: Condvar::new(),
        });
        if items.is_empty() {
            return ProbeBatch { state };
        }
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        for mut item in items {
            if self.availability.pause.is_paused(item.library_id) {
                let mut remaining = state.remaining.lock().unwrap_or_else(|e| e.into_inner());
                *remaining = remaining.saturating_sub(1);
                continue;
            }
            item.kind = WorkKind::Probe;
            item.batch = Some(Arc::clone(&state));
            queue.probes.push_back(item);
        }
        self.available.notify_all();
        ProbeBatch { state }
    }

    pub fn drain_pending_probes(&self) -> Result<usize, String> {
        let items = self.db.list_indexed_unprobed()?;
        let n = items.len();
        for (item_id, path, library_id) in items {
            let abs = match self.abs_media_path(library_id, &path) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(item_id, library_id, error = %e, "resolve path for pending probe");
                    continue;
                }
            };
            self.enqueue(WorkItem::probe(item_id, library_id, abs, None));
        }
        Ok(n)
    }

    pub fn drain_pending_extracts(&self) -> Result<(), String> {
        for (item_id, path, _, _, library_id) in self.db.list_pending_subtitle_items()? {
            let abs = match self.abs_media_path(library_id, &path) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(item_id, library_id, error = %e, "resolve path for pending extract");
                    continue;
                }
            };
            self.enqueue(WorkItem::extract(item_id, library_id, abs));
        }
        Ok(())
    }

    pub fn drain_pending_maps(&self) -> Result<(), String> {
        for (item_id, path, library_id) in self.db.list_pending_map_items()? {
            let abs = match self.abs_media_path(library_id, &path) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(item_id, library_id, error = %e, "resolve path for pending map");
                    continue;
                }
            };
            self.enqueue(WorkItem::map(item_id, library_id, abs));
        }
        Ok(())
    }

    /// ADR-0030: join library root to stored relpath (absolute leftovers pass through).
    fn abs_media_path(&self, library_id: i64, stored: &str) -> Result<PathBuf, String> {
        let lib = self
            .db
            .get_library(library_id)?
            .ok_or_else(|| format!("library {library_id} not found"))?;
        Ok(nightjar_db::resolve_media_path(&lib.path, stored))
    }

    pub fn remove_item_subtitles(&self, item_id: i64) -> Result<(), String> {
        self.subs.remove_item(item_id)
    }

    pub fn cleanup_orphan_subtitles(&self) -> Result<usize, String> {
        self.subs.cleanup_orphans(&self.db.list_all_item_ids()?)
    }

    /// One reachability tick for all libraries (non-overlapping).
    pub fn tick_reachability(&self) -> Result<(), String> {
        if !self.availability.tick_gate.try_begin() {
            return Ok(());
        }
        let result = (|| {
            for lib in self.db.list_libraries()? {
                let state = reachability::check_root(std::path::Path::new(&lib.path));
                let reachable = matches!(state, Reachability::Reachable);
                self.set_library_reachability(lib.id, &lib.path, reachable)?;
            }
            Ok(())
        })();
        self.availability.tick_gate.end();
        result
    }

    fn run_worker(&self) {
        loop {
            let item = {
                let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if let Some(item) = queue.probes.pop_front() {
                        if self.availability.pause.is_paused(item.library_id) {
                            if let Some(batch) = &item.batch {
                                let mut remaining =
                                    batch.remaining.lock().unwrap_or_else(|e| e.into_inner());
                                *remaining = remaining.saturating_sub(1);
                                if *remaining == 0 {
                                    batch.ready.notify_all();
                                }
                            }
                            continue;
                        }
                        break item;
                    }
                    let index_busy = self.index_active.load(Ordering::SeqCst) != 0;
                    let should_pop = queue
                        .background
                        .front()
                        .is_some_and(|front| !index_busy || front.priority);
                    let next_bg = should_pop.then(|| queue.background.pop_front()).flatten();
                    if let Some(item) = next_bg {
                        if self.availability.pause.is_paused(item.library_id) {
                            continue;
                        }
                        break item;
                    }
                    queue = self
                        .available
                        .wait_timeout(queue, Duration::from_secs(1))
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
            };
            match item.kind {
                WorkKind::Probe => self.probe(item),
                WorkKind::Extract => self.extract(item),
                WorkKind::Map => self.map_item(item),
            }
        }
    }

    fn finish_batch(item: &WorkItem) {
        if let Some(batch) = &item.batch {
            let mut remaining = batch.remaining.lock().unwrap_or_else(|e| e.into_inner());
            *remaining -= 1;
            if *remaining == 0 {
                batch.ready.notify_all();
            }
        }
    }

    fn probe(&self, item: WorkItem) {
        if self.availability.pause.is_paused(item.library_id) {
            Self::finish_batch(&item);
            return;
        }
        // Prefer DB-stored path (relpath) so WorkItem can carry either form (ADR-0030).
        let stored = self
            .db
            .get_item(item.item_id)
            .ok()
            .flatten()
            .map(|r| r.path)
            .unwrap_or_else(|| item.path.to_string_lossy().into_owned());
        let abs = match self.abs_media_path(item.library_id, &stored) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(item_id = item.item_id, error = %e, "resolve path for probe");
                let _ = self.db.apply_probe_update(&ProbeUpdate {
                    item_id: item.item_id,
                    duration_ms: None,
                    container: None,
                    video_codec: None,
                    audio_codec: None,
                    audio_channels: None,
                    width: None,
                    height: None,
                    video_bitrate_bps: None,
                    hdr: None,
                    probe_status: "unavailable".into(),
                    scan_error: Some(e),
                });
                if let Some(job_id) = item.scan_job_id {
                    let _ = self.db.bump_scan_job_probe(job_id, false);
                }
                Self::finish_batch(&item);
                return;
            }
        };
        let update = match probe::ffprobe(&abs) {
            Ok(p) => ProbeUpdate {
                item_id: item.item_id,
                duration_ms: p.duration_ms,
                container: p.container,
                video_codec: p.video_codec,
                audio_codec: p.audio_codec,
                audio_channels: p.audio_channels,
                width: p.width,
                height: p.height,
                video_bitrate_bps: p.video_bitrate_bps,
                hdr: p.hdr,
                probe_status: "probed".into(),
                scan_error: None,
            },
            Err(e) => {
                let unavailable = self.availability.pause.is_paused(item.library_id)
                    || message_looks_unavailable(&e)
                    || {
                        // Re-check root: mid-pass unmount.
                        self.db
                            .get_library(item.library_id)
                            .ok()
                            .flatten()
                            .map(|lib| {
                                matches!(
                                    reachability::check_root(std::path::Path::new(&lib.path)),
                                    Reachability::Unreachable
                                )
                            })
                            .unwrap_or(false)
                    };
                if unavailable {
                    tracing::warn!(
                        path = %abs.display(),
                        error = %e,
                        "ffprobe unavailable"
                    );
                    ProbeUpdate {
                        item_id: item.item_id,
                        duration_ms: None,
                        container: None,
                        video_codec: None,
                        audio_codec: None,
                        audio_channels: None,
                        width: None,
                        height: None,
                        video_bitrate_bps: None,
                        hdr: None,
                        probe_status: "unavailable".into(),
                        scan_error: Some(e),
                    }
                } else {
                    tracing::warn!(path = %abs.display(), error = %e, "ffprobe failed");
                    ProbeUpdate {
                        item_id: item.item_id,
                        duration_ms: None,
                        container: None,
                        video_codec: None,
                        audio_codec: None,
                        audio_channels: None,
                        width: None,
                        height: None,
                        video_bitrate_bps: None,
                        hdr: None,
                        probe_status: "error".into(),
                        scan_error: Some(e),
                    }
                }
            }
        };
        let failed = update.probe_status == "error";
        if let Err(e) = self.db.apply_probe_update(&update) {
            tracing::warn!(item_id = item.item_id, error = %e, "probe update failed");
        }
        if let Some(job_id) = item.scan_job_id
            && let Err(e) = self.db.bump_scan_job_probe(job_id, failed)
        {
            tracing::warn!(job_id, error = %e, "probe counter bump failed");
        }
        Self::finish_batch(&item);
    }

    fn extract(&self, item: WorkItem) {
        if self.availability.pause.is_paused(item.library_id) {
            return;
        }
        let item_id = item.item_id;
        {
            let mut extracting = self.extracting.lock().unwrap_or_else(|e| e.into_inner());
            extracting.insert(item_id);
        }
        let finish = || {
            self.extracting
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&item_id);
        };
        let row = match self.db.get_item(item_id) {
            Ok(Some(row)) => row,
            Ok(None) => {
                finish();
                return;
            }
            Err(e) => {
                tracing::warn!(item_id, error = %e, "load subtitle item failed");
                finish();
                return;
            }
        };
        // Permanent failure: no path to success until the source row is
        // re-upserted (mtime/size change resets status to pending).
        if row.subtitle_status == "error" {
            finish();
            return;
        }
        let lib_root = match self.db.get_library(item.library_id) {
            Ok(Some(lib)) => lib.path,
            Ok(None) => {
                tracing::warn!(
                    item_id = item.item_id,
                    library_id = item.library_id,
                    "library missing for subtitle extract"
                );
                finish();
                return;
            }
            Err(e) => {
                tracing::warn!(item_id = item.item_id, error = %e, "load library for extract failed");
                finish();
                return;
            }
        };
        let media_path = nightjar_db::resolve_media_path(&lib_root, &row.path);
        let sidecars = match self.db.list_item_sidecars(item.item_id) {
            Ok(rows) => rows
                .into_iter()
                .map(|s| SidecarInput {
                    track_id: s.track_id,
                    path: nightjar_db::resolve_media_path(&lib_root, &s.path),
                    format: s.format,
                })
                .collect::<Vec<_>>(),
            Err(e) => {
                tracing::warn!(item_id = item.item_id, error = %e, "load subtitle sidecars failed");
                finish();
                return;
            }
        };
        match extract_item_subtitles(&self.subs, item.item_id, &media_path, &sidecars) {
            Ok(ExtractOutcome::Ready) => {
                if let Err(e) = self.db.set_subtitle_status(
                    item.item_id,
                    "ready",
                    Some(row.mtime_ms),
                    Some(row.size_bytes),
                ) {
                    tracing::warn!(item_id = item.item_id, error = %e, "set subtitle ready failed");
                }
            }
            Ok(ExtractOutcome::None) => {
                if let Err(e) = self.db.set_subtitle_status(
                    item.item_id,
                    "none",
                    Some(row.mtime_ms),
                    Some(row.size_bytes),
                ) {
                    tracing::warn!(item_id = item.item_id, error = %e, "set subtitle none failed");
                }
            }
            Err(e) if e.starts_with("subtitle extract refused:") => {
                tracing::warn!(item_id = item.item_id, error = %e, "subtitle extract deferred");
            }
            Err(e) => {
                let unavailable = self.availability.pause.is_paused(item.library_id)
                    || message_looks_unavailable(&e)
                    || self
                        .db
                        .get_library(item.library_id)
                        .ok()
                        .flatten()
                        .map(|lib| {
                            matches!(
                                reachability::check_root(std::path::Path::new(&lib.path)),
                                Reachability::Unreachable
                            )
                        })
                        .unwrap_or(false);
                if unavailable {
                    tracing::warn!(
                        item_id = item.item_id,
                        error = %e,
                        "subtitle extract unavailable"
                    );
                    if let Err(status_err) =
                        self.db
                            .set_subtitle_status(item.item_id, "unavailable", None, None)
                    {
                        tracing::warn!(item_id = item.item_id, error = %status_err, "set subtitle unavailable failed");
                    }
                } else {
                    tracing::warn!(item_id = item.item_id, error = %e, "subtitle extract failed");
                    if let Err(status_err) =
                        self.db
                            .set_subtitle_status(item.item_id, "error", None, None)
                    {
                        tracing::warn!(item_id = item.item_id, error = %status_err, "set subtitle error failed");
                    }
                }
            }
        }
        finish();
    }

    fn map_item(&self, item: WorkItem) {
        if self.availability.pause.is_paused(item.library_id) {
            return;
        }
        let item_id = item.item_id;
        {
            let mut mapping = self.mapping.lock().unwrap_or_else(|e| e.into_inner());
            mapping.insert(item_id);
        }
        let finish = || {
            self.mapping
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&item_id);
        };
        let row = match self.db.get_item(item_id) {
            Ok(Some(row)) => row,
            Ok(None) => {
                finish();
                return;
            }
            Err(e) => {
                tracing::warn!(item_id, error = %e, "load map item failed");
                finish();
                return;
            }
        };
        if row.map_status == "error" {
            finish();
            return;
        }
        let media_path = match self.abs_media_path(item.library_id, &row.path) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(item_id, error = %e, "resolve path for map");
                let _ = self.db.set_map_status(item_id, "unavailable");
                finish();
                return;
            }
        };
        let content_id = match row.content_id.clone() {
            Some(id) => id,
            None => match nightjar_db::content_id_for_path(&media_path) {
                Ok(id) => {
                    if let Err(e) = self.db.set_content_id(item_id, &id) {
                        tracing::warn!(item_id, error = %e, "store content_id failed");
                        finish();
                        return;
                    }
                    id
                }
                Err(e) => {
                    tracing::warn!(item_id, error = %e, "content_id for map failed");
                    let _ = self.db.set_map_status(item_id, "error");
                    finish();
                    return;
                }
            },
        };
        match crate::keymap::build_keyframe_map(&media_path, row.duration_ms) {
            Ok(built) => {
                let entries: Vec<(i64, i64)> = built
                    .entries
                    .iter()
                    .map(|e| (e.pts_ms, e.byte_offset))
                    .collect();
                if let Err(e) = self.db.replace_keyframe_map(
                    item_id,
                    &content_id,
                    built.container_kind,
                    &entries,
                    built.usable_extent_ms,
                ) {
                    tracing::warn!(item_id, error = %e, "store keyframe map failed");
                    let _ = self.db.set_map_status(item_id, "error");
                } else {
                    tracing::info!(
                        item_id,
                        entries = entries.len(),
                        source = built.source,
                        kind = built.container_kind,
                        "keyframe map ready"
                    );
                }
            }
            Err(e) => {
                let unavailable = self.availability.pause.is_paused(item.library_id)
                    || message_looks_unavailable(&e)
                    || self
                        .db
                        .get_library(item.library_id)
                        .ok()
                        .flatten()
                        .map(|lib| {
                            matches!(
                                reachability::check_root(std::path::Path::new(&lib.path)),
                                Reachability::Unreachable
                            )
                        })
                        .unwrap_or(false);
                if unavailable {
                    tracing::warn!(item_id, error = %e, "keyframe map unavailable");
                    let _ = self.db.set_map_status(item_id, "unavailable");
                } else {
                    tracing::warn!(item_id, error = %e, "keyframe map failed");
                    let _ = self.db.set_map_status(item_id, "error");
                }
            }
        }
        finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enqueue_background_unique_skips_duplicate_kind_item() {
        let mut queue = Queue {
            probes: VecDeque::new(),
            background: VecDeque::new(),
        };
        let first = WorkItem::extract(42, 1, PathBuf::from("/tmp/a.mkv"));
        let second = WorkItem::extract(42, 1, PathBuf::from("/tmp/a.mkv"));
        let map = WorkItem::map(42, 1, PathBuf::from("/tmp/a.mkv"));

        LibraryPool::enqueue_background_unique(&mut queue, first);
        LibraryPool::enqueue_background_unique(&mut queue, second);
        LibraryPool::enqueue_background_unique(&mut queue, map);

        assert_eq!(queue.background.len(), 2);
        assert_eq!(queue.background[0].kind, WorkKind::Extract);
        assert_eq!(queue.background[1].kind, WorkKind::Map);
    }

    #[test]
    fn prioritize_extract_moves_existing_to_front() {
        let mut queue = Queue {
            probes: VecDeque::new(),
            background: VecDeque::new(),
        };
        LibraryPool::enqueue_background_unique(
            &mut queue,
            WorkItem::extract(1, 1, PathBuf::from("/a")),
        );
        LibraryPool::enqueue_background_unique(
            &mut queue,
            WorkItem::extract(2, 1, PathBuf::from("/b")),
        );
        let pos = queue
            .background
            .iter()
            .position(|w| w.item_id == 2)
            .unwrap();
        let mut item = queue.background.remove(pos).unwrap();
        item.priority = true;
        queue.background.push_front(item);
        assert_eq!(queue.background[0].item_id, 2);
        assert!(queue.background[0].priority);
        assert_eq!(queue.background[1].item_id, 1);
    }

    #[test]
    fn enter_index_epoch_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let subs = Arc::new(SubsStore::new(dir.path().join("subs")).unwrap());
        let pool = LibraryPool::spawn(db, subs);

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = Arc::clone(&pool);
        std::thread::spawn(move || {
            let _epoch = holder.enter_index_epoch(1);
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        ready_rx.recv().unwrap();
        assert!(pool.index_epoch_held());

        let waiter = Arc::clone(&pool);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let _epoch = waiter.enter_index_epoch(2);
            done_tx.send(started.elapsed()).unwrap();
        });
        std::thread::sleep(Duration::from_millis(80));
        assert!(
            done_rx.try_recv().is_err(),
            "second enter_index_epoch must block while first holds"
        );
        release_tx.send(()).unwrap();
        let waited = done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("waiter should acquire after release");
        assert!(
            waited >= Duration::from_millis(50),
            "waiter elapsed {waited:?} too short to have blocked"
        );
    }
}
