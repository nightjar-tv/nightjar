use crate::probe;
use crate::reachability::{self, Availability, Reachability, message_looks_unavailable};
use crate::walk::WalkCache;
use nightjar_db::{Db, ProbeUpdate};
use nightjar_transcode::{ExtractOutcome, SidecarInput, SubsStore, extract_item_subtitles};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkKind {
    Probe,
    Extract,
}

#[derive(Clone)]
pub struct WorkItem {
    pub kind: WorkKind,
    pub item_id: i64,
    pub library_id: i64,
    pub path: PathBuf,
    pub scan_job_id: Option<i64>,
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
            batch: None,
        }
    }
}

struct Queue {
    probes: VecDeque<WorkItem>,
    extracts: VecDeque<WorkItem>,
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
    scan_dirty: Mutex<HashSet<i64>>,
    index_active: AtomicUsize,
    pub availability: Arc<Availability>,
}

impl LibraryPool {
    pub fn spawn(db: Arc<Db>, subs: Arc<SubsStore>) -> Arc<Self> {
        let availability = Availability::new();
        let pool = Arc::new(Self {
            db,
            subs,
            queue: Mutex::new(Queue {
                probes: VecDeque::new(),
                extracts: VecDeque::new(),
            }),
            available: Condvar::new(),
            walk_caches: Mutex::new(HashMap::new()),
            last_index_ms: AtomicU64::new(0),
            scan_dirty: Mutex::new(HashSet::new()),
            index_active: AtomicUsize::new(0),
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

    pub fn mark_scan_dirty(&self, library_id: i64) {
        self.scan_dirty
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(library_id);
    }

    pub fn take_scan_dirty(&self, library_id: i64) -> bool {
        self.scan_dirty
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&library_id)
    }

    pub fn begin_index(&self) {
        self.index_active.fetch_add(1, Ordering::SeqCst);
    }

    pub fn end_index(&self) {
        let prev = self.index_active.fetch_sub(1, Ordering::SeqCst);
        if prev == 1 {
            self.available.notify_all();
        }
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
            let (probes, extracts) = self.db.requeue_unavailable_for_library(library_id)?;
            if probes > 0 || extracts > 0 {
                tracing::info!(
                    library_id,
                    probes,
                    extracts,
                    "re-queued availability failures"
                );
            }
            self.purge_queue_for_library(library_id);
            self.drain_pending_probes()?;
            self.drain_pending_extracts()?;
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
        let mut kept_extracts = VecDeque::new();
        while let Some(item) = queue.extracts.pop_front() {
            if item.library_id != library_id {
                kept_extracts.push_back(item);
            }
        }
        queue.extracts = kept_extracts;
    }

    pub fn enqueue(&self, item: WorkItem) {
        if self.availability.pause.is_paused(item.library_id) {
            return;
        }
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        match item.kind {
            WorkKind::Probe => queue.probes.push_back(item),
            WorkKind::Extract => queue.extracts.push_back(item),
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
            self.enqueue(WorkItem::probe(
                item_id,
                library_id,
                PathBuf::from(path),
                None,
            ));
        }
        Ok(n)
    }

    pub fn drain_pending_extracts(&self) -> Result<(), String> {
        for (item_id, path, _, _, library_id) in self.db.list_pending_subtitle_items()? {
            self.enqueue(WorkItem::extract(item_id, library_id, PathBuf::from(path)));
        }
        Ok(())
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
                    if self.index_active.load(Ordering::SeqCst) == 0
                        && let Some(item) = queue.extracts.pop_front()
                    {
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
        let update = match probe::ffprobe(&item.path) {
            Ok(p) => ProbeUpdate {
                item_id: item.item_id,
                duration_ms: p.duration_ms,
                container: p.container,
                video_codec: p.video_codec,
                audio_codec: p.audio_codec,
                audio_channels: p.audio_channels,
                width: p.width,
                height: p.height,
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
                        path = %item.path.display(),
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
                        probe_status: "unavailable".into(),
                        scan_error: Some(e),
                    }
                } else {
                    tracing::warn!(path = %item.path.display(), error = %e, "ffprobe failed");
                    ProbeUpdate {
                        item_id: item.item_id,
                        duration_ms: None,
                        container: None,
                        video_codec: None,
                        audio_codec: None,
                        audio_channels: None,
                        width: None,
                        height: None,
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
        let row = match self.db.get_item(item.item_id) {
            Ok(Some(row)) => row,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(item_id = item.item_id, error = %e, "load subtitle item failed");
                return;
            }
        };
        let sidecars = match self.db.list_item_sidecars(item.item_id) {
            Ok(rows) => rows
                .into_iter()
                .map(|s| SidecarInput {
                    track_id: s.track_id,
                    path: PathBuf::from(s.path),
                    format: s.format,
                })
                .collect::<Vec<_>>(),
            Err(e) => {
                tracing::warn!(item_id = item.item_id, error = %e, "load subtitle sidecars failed");
                return;
            }
        };
        match extract_item_subtitles(&self.subs, item.item_id, &item.path, &sidecars) {
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
    }
}
