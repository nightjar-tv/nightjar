//! Library scanner: index pass, then bounded ffprobe pool (ADR-0004).

mod keymap;
mod pool;
mod probe;
mod reachability;
mod walk;
mod watch;

pub use keymap::{KeyframeEntry, KeyframeMapBuild, build_keyframe_map};
pub use pool::LibraryPool;
pub use probe::{ProbeResult, ffprobe};
pub use reachability::{Reachability, allow_delete_missing, check_root};
pub use walk::{
    WalkCache, WalkOutcome, is_media, walk_concurrency, walk_media_files_cached,
    walk_media_files_cached_with_concurrency,
};
pub use watch::spawn_library_watcher;

use nightjar_core::parse_filename;
use nightjar_db::{Db, ItemPathRow, UpsertItem, fold_path, resolve_media_path, to_relpath};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// ADR-0030 §3: refuse repoint if matched/current < this fraction.
pub const REPOINT_RETAIN_FRACTION: f64 = 0.90;

/// After a repoint with deferred_remove > 0, poll skips full walks for this
/// long so the operator can review before delete_missing runs (ADR-0030).
pub const REPOINT_DELETE_HOLDOFF: Duration = Duration::from_secs(3600);

const INDEX_BATCH: usize = 200;

/// Who asked for a full-library walk (ADR-0015). Notify creates use
/// [`hint_ingest`] alone and do not go through this entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanTrigger {
    /// Periodic poll. While a scan is already active, no-op on the dirty bit
    /// so long walks can still run `delete_missing`.
    Poll,
    /// `POST .../scan` (and tests that mean manual discovery). Coalesces to one
    /// follow-up if a job is already active.
    Manual,
    /// Library create. Same coalesce as Manual if somehow concurrent.
    Create,
    /// Internal follow-up after a manual dirty bit.
    FollowUp,
}

/// Request a full-library scan (ADR-0015). Entry for poll, manual scan, library
/// create, and internal follow-up — not for notify creates ([`hint_ingest`]).
///
/// Returns the active or newly accepted job id.
pub fn request_scan(
    db: Arc<Db>,
    pool: Arc<LibraryPool>,
    library_id: i64,
    trigger: ScanTrigger,
) -> Result<i64, String> {
    let lib = db
        .get_library(library_id)?
        .ok_or_else(|| format!("library {library_id} not found"))?;
    if matches!(check_root(Path::new(&lib.path)), Reachability::Unreachable) {
        let _ = pool.set_library_reachability(library_id, &lib.path, false);
        return Err(format!("library path is not reachable: {}", lib.path));
    }
    if let Some(existing) = db.active_scan_job(library_id)? {
        match trigger {
            // Running walk is this poll; do not suppress delete_missing.
            ScanTrigger::Poll | ScanTrigger::FollowUp => {}
            ScanTrigger::Manual | ScanTrigger::Create => {
                pool.mark_scan_dirty(library_id);
            }
        }
        return Ok(existing);
    }
    // Poll must not apply deferred_remove until holdoff ends or manual scan.
    if matches!(trigger, ScanTrigger::Poll) && pool.repoint_delete_holdoff_active(library_id) {
        tracing::info!(
            library_id,
            "poll skipped; repoint deferred_remove holdoff active"
        );
        return Ok(0);
    }
    let job_id = db.create_scan_job(library_id)?;
    std::thread::Builder::new()
        .name(format!("scan-job-{job_id}"))
        .spawn(move || {
            let scan_ok = match run_scan_job(&db, &pool, job_id, library_id) {
                Ok(()) => true,
                Err(e) => {
                    tracing::error!(job_id, library_id, error = %e, "scan job failed");
                    let _ = db.fail_scan_job(job_id, &e);
                    false
                }
            };
            // Drop any leftover hint dirt so it cannot suppress a later job.
            let _ = pool.take_dirty_add(library_id);
            if scan_ok {
                // Ordinary scan is the clear for deferred_remove holdoff.
                pool.clear_repoint_delete_holdoff(library_id);
            }
            if pool.take_scan_dirty(library_id) {
                tracing::info!(
                    library_id,
                    "library dirty after scan; starting follow-up job"
                );
                if let Err(e) = request_scan(
                    Arc::clone(&db),
                    Arc::clone(&pool),
                    library_id,
                    ScanTrigger::FollowUp,
                ) {
                    tracing::warn!(library_id, error = %e, "follow-up scan failed");
                }
            }
        })
        .map_err(|e| format!("spawn scan job {job_id}: {e}"))?;
    Ok(job_id)
}

/// Alias for manual / test discovery starts.
pub fn start_scan_job(db: Arc<Db>, pool: Arc<LibraryPool>, library_id: i64) -> Result<i64, String> {
    request_scan(db, pool, library_id, ScanTrigger::Manual)
}

/// Path-hinted notify ingest (ADR-0015). Upserts one media file immediately so a
/// new episode can appear without waiting for the full walk. Never calls
/// `delete_missing` — poll (or manual scan) remains the heal/delete path.
///
/// Skips non-media, missing, non-file, and zero-size paths (copy-in-progress /
/// debounce miss). Does not take the index epoch: concurrent with an in-flight
/// walk is intentional. Does **not** call [`request_scan`]; callers must not
/// force a full walk after a successful hint.
pub fn hint_ingest(
    db: &Db,
    pool: &LibraryPool,
    library_id: i64,
    path: &Path,
) -> Result<HintIngestOutcome, String> {
    if !is_media(path) {
        return Ok(HintIngestOutcome::Ignored);
    }
    let meta = match std::fs::metadata(path) {
        Ok(m) if m.is_file() => m,
        Ok(_) => return Ok(HintIngestOutcome::Ignored),
        Err(_) => return Ok(HintIngestOutcome::Ignored),
    };
    let size_bytes = meta.len() as i64;
    if size_bytes <= 0 {
        return Ok(HintIngestOutcome::Ignored);
    }
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let lib = db
        .get_library(library_id)?
        .ok_or_else(|| format!("library {library_id} not found"))?;
    if !pool.is_library_reachable(library_id) {
        return Ok(HintIngestOutcome::Ignored);
    }
    let library_root = std::fs::canonicalize(&lib.path)
        .map(|p| nightjar_db::normalize_library_root(&p.to_string_lossy()))
        .unwrap_or_else(|_| nightjar_db::normalize_library_root(&lib.path));
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let Some(rel) = to_relpath(&library_root, &resolved) else {
        return Ok(HintIngestOutcome::Ignored);
    };

    let folded = fold_path(&rel);
    let matches: Vec<ItemPathRow> = db
        .list_item_paths(library_id)?
        .into_iter()
        .filter(|r| fold_path(&r.path) == folded)
        .collect();

    if matches.len() > 1 {
        tracing::warn!(
            library_id,
            path = %rel,
            count = matches.len(),
            "hint ingest: fold-equal path collision; refusing upsert"
        );
        return Ok(HintIngestOutcome::Collision);
    }

    if let Some(row) = matches.first()
        && row.mtime_ms == mtime_ms
    {
        if row.probe_status == "indexed" {
            let abs = resolve_media_path(&library_root, &row.path);
            pool.enqueue(pool::WorkItem::probe(row.id, library_id, abs, None));
        }
        return Ok(HintIngestOutcome::Unchanged { item_id: row.id });
    }

    let store_path = matches
        .first()
        .map(|r| r.path.clone())
        .unwrap_or_else(|| rel.clone());
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| rel.clone());
    let parsed = parse_filename(&file_name);
    let content_id = match nightjar_db::content_id_for_path(path) {
        Ok(id) => Some(id),
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "content_id read failed; map rebuild will retry"
            );
            None
        }
    };
    let item = UpsertItem {
        path: store_path.clone(),
        mtime_ms,
        size_bytes,
        title: parsed.title,
        kind: parsed.kind.as_str().to_string(),
        year: parsed.year,
        season: parsed.season,
        episode: parsed.episode,
        content_id,
    };
    let ids = db.upsert_items_indexed(library_id, &[item])?;
    let item_id = ids
        .into_iter()
        .next()
        .ok_or_else(|| "hint upsert returned no id".to_string())?;
    let abs = resolve_media_path(&library_root, &store_path);
    pool.enqueue(pool::WorkItem::probe(
        item_id,
        library_id,
        abs.clone(),
        None,
    ));
    let mut sidecar_dirs = nightjar_transcode::SidecarDirCache::default();
    match associate_sidecars(db, item_id, &library_root, &abs, &mut sidecar_dirs) {
        Ok(true) => {
            db.mark_items_subtitle_pending(&[item_id])?;
            pool.enqueue(pool::WorkItem::extract(item_id, library_id, abs.clone()));
        }
        Ok(false) => {}
        Err(e) => tracing::warn!(
            item_id,
            path = %abs.display(),
            error = %e,
            "hint sidecar association failed"
        ),
    }
    pool.enqueue_map_rebuild(item_id, library_id, abs);
    // If a full walk is in flight, mark dirty_add so that job skips
    // delete_missing (would otherwise drop this row). Poll heals deletes later;
    // do not schedule a follow-up full walk for the hint alone.
    if db.active_scan_job(library_id)?.is_some() {
        pool.mark_dirty_add(library_id);
    }
    tracing::info!(
        library_id,
        item_id,
        path = %rel,
        "hint ingest upserted media file"
    );
    Ok(HintIngestOutcome::Upserted { item_id })
}

/// Result of [`hint_ingest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintIngestOutcome {
    Ignored,
    Unchanged { item_id: i64 },
    Upserted { item_id: i64 },
    Collision,
}

/// Async library repoint (ADR-0030 §3). Returns a job id immediately; dry-run
/// walk + commit run on a worker thread.
pub fn request_repoint(
    db: Arc<Db>,
    pool: Arc<LibraryPool>,
    library_id: i64,
    candidate_path: &str,
) -> Result<i64, String> {
    let _ = db
        .get_library(library_id)?
        .ok_or_else(|| format!("library {library_id} not found"))?;
    if let Some(existing) = db.active_scan_job(library_id)? {
        return Err(format!(
            "library {library_id} already has active job {existing}"
        ));
    }
    let job_id = db.create_repoint_job(library_id, candidate_path)?;
    let candidate = candidate_path.to_string();
    std::thread::Builder::new()
        .name(format!("repoint-job-{job_id}"))
        .spawn(move || {
            if let Err(e) = run_repoint_job(&db, &pool, job_id, library_id, &candidate) {
                tracing::error!(job_id, library_id, error = %e, "repoint job failed");
                let _ = db.fail_scan_job(job_id, &e);
            }
        })
        .map_err(|e| format!("spawn repoint job {job_id}: {e}"))?;
    Ok(job_id)
}

fn run_repoint_job(
    db: &Arc<Db>,
    pool: &Arc<LibraryPool>,
    job_id: i64,
    library_id: i64,
    candidate_path: &str,
) -> Result<(), String> {
    db.set_scan_job_state(job_id, "indexing")?;
    let probe_queue = {
        // One epoch for dry-run walk + commit index so another library cannot
        // interleave a cold walk on the same share (ADR-0015).
        let _epoch = pool.enter_index_epoch();
        let candidate = std::fs::canonicalize(candidate_path)
            .map(|p| nightjar_db::normalize_library_root(&p.to_string_lossy()))
            .unwrap_or_else(|_| nightjar_db::normalize_library_root(candidate_path));
        let root = Path::new(&candidate);
        if !matches!(check_root(root), Reachability::Reachable) {
            return Err(format!("repoint path is not reachable: {candidate}"));
        }
        let current = db.count_items(library_id)?;
        let existing = db.list_item_paths(library_id)?;
        let existing_folds: HashSet<String> = existing
            .iter()
            .filter(|r| !nightjar_db::is_absolute_stored(&r.path))
            .map(|r| fold_path(&r.path))
            .collect();

        // Single cold walk: retain math + commit index reuse the same file list
        // (ADR-0030). Seed WalkCache under the new absolute root for the next poll.
        let mut dry_cache = walk::WalkCache::new();
        let outcome = walk::walk_media_files_cached(root, Some(&mut dry_cache))?;
        let mut walked_folds = HashSet::new();
        for file in &outcome.files {
            if let Some(rel) = to_relpath(&candidate, &file.path) {
                walked_folds.insert(fold_path(&rel));
            }
        }
        let matched = existing_folds
            .iter()
            .filter(|f| walked_folds.contains(*f))
            .count() as i64;
        let would_remove = current - matched;

        if current >= 1 && matched == 0 {
            return Err(format!(
                "repoint_empty_match: current={current} walked={} matched=0 would_remove={would_remove}",
                walked_folds.len()
            ));
        }
        if current > 0 {
            let retain = matched as f64 / current as f64;
            if retain < REPOINT_RETAIN_FRACTION {
                return Err(format!(
                    "repoint_below_retain_threshold: current={current} walked={} matched={matched} would_remove={would_remove} retain={retain:.3}",
                    walked_folds.len()
                ));
            }
        }

        db.update_library_path(library_id, &candidate)?;
        let _ = db.repair_library_paths(library_id)?;
        let _ = pool.set_library_reachability(library_id, &candidate, true);
        pool.replace_walk_cache(library_id, dry_cache);
        run_index_pass(db, pool, job_id, library_id, Some(outcome))?
    };
    finish_scan_probes(db, pool, job_id, library_id, probe_queue)
}

fn run_scan_job(
    db: &Arc<Db>,
    pool: &Arc<LibraryPool>,
    job_id: i64,
    library_id: i64,
) -> Result<(), String> {
    db.set_scan_job_state(job_id, "indexing")?;
    run_index_and_probe(db, pool, job_id, library_id)
}

fn run_index_and_probe(
    db: &Arc<Db>,
    pool: &Arc<LibraryPool>,
    job_id: i64,
    library_id: i64,
) -> Result<(), String> {
    let probe_queue = {
        let _epoch = pool.enter_index_epoch();
        run_index_pass(db, pool, job_id, library_id, None)?
    };
    finish_scan_probes(db, pool, job_id, library_id, probe_queue)
}

fn finish_scan_probes(
    db: &Arc<Db>,
    pool: &Arc<LibraryPool>,
    job_id: i64,
    library_id: i64,
    probe_queue: Vec<pool::WorkItem>,
) -> Result<(), String> {
    if !pool.is_library_reachable(library_id) {
        db.complete_scan_job(job_id, 0)?;
        return Ok(());
    }

    let probe_started = Instant::now();
    let probe_ids: Vec<(i64, PathBuf)> = probe_queue
        .iter()
        .map(|item| (item.item_id, item.path.clone()))
        .collect();
    pool.enqueue_probe_batch(probe_queue).wait();
    for (item_id, path) in &probe_ids {
        pool.enqueue(pool::WorkItem::extract(*item_id, library_id, path.clone()));
        pool.enqueue_map_rebuild(*item_id, library_id, path.clone());
    }
    pool.drain_pending_extracts()?;
    pool.drain_pending_maps()?;
    let probe_duration_ms = probe_started.elapsed().as_millis() as u64;
    db.complete_scan_job(job_id, probe_duration_ms)?;

    tracing::info!(job_id, library_id, probe_duration_ms, "scan job completed");
    Ok(())
}

/// Walk + upsert only. Caller must hold [`LibraryPool::enter_index_epoch`].
///
/// When `prewalked` is `Some`, the file list is reused (repoint: same cold walk
/// as the retain dry-run). Caller must have reseeded WalkCache for the new root.
fn run_index_pass(
    db: &Arc<Db>,
    pool: &Arc<LibraryPool>,
    job_id: i64,
    library_id: i64,
    prewalked: Option<walk::WalkOutcome>,
) -> Result<Vec<pool::WorkItem>, String> {
    let lib = db
        .get_library(library_id)?
        .ok_or_else(|| format!("library {library_id} not found"))?;
    // Canonical root for under-root checks (macOS /var vs /private/var).
    let library_root = std::fs::canonicalize(&lib.path)
        .map(|p| nightjar_db::normalize_library_root(&p.to_string_lossy()))
        .unwrap_or_else(|_| nightjar_db::normalize_library_root(&lib.path));
    let root = Path::new(&library_root);
    let root_before = check_root(root);
    if !matches!(root_before, Reachability::Reachable) {
        let _ = pool.set_library_reachability(library_id, &library_root, false);
        return Err(format!("library path is not reachable: {library_root}"));
    }

    // Scan library (and poll) re-try availability failures; permanent error stays
    // until mtime change (ADR-0014). Must run before list_item_paths so the
    // unchanged branch sees probe_status=indexed and re-queues probes.
    let (rq_probes, rq_extracts, rq_maps) = db.requeue_unavailable_for_library(library_id)?;
    if rq_probes > 0 || rq_extracts > 0 || rq_maps > 0 {
        tracing::info!(
            library_id,
            probes = rq_probes,
            extracts = rq_extracts,
            maps = rq_maps,
            "scan re-queued availability failures"
        );
    }

    let existing_count = db.count_items(library_id)?;
    let index_started = Instant::now();
    // Caller holds IndexEpochGuard for this walk/upsert (ADR-0013/0015).
    #[allow(clippy::type_complexity)]
    let index_result = (|| -> Result<(u32, u32, u32, u32, Vec<pool::WorkItem>, u64), String> {
        let reused = prewalked.is_some();
        let (cache_warm, outcome) = if let Some(outcome) = prewalked {
            // Repoint reseeded cache from the dry-run; treat as warm for next poll.
            (true, outcome)
        } else {
            let cache_warm = pool.with_walk_cache(library_id, |cache| !cache.is_empty());
            let outcome = pool.with_walk_cache(library_id, |cache| {
                walk::walk_media_files_cached(root, Some(cache))
            })?;
            (cache_warm, outcome)
        };
        if reused {
            tracing::info!(
                library_id,
                job_id,
                files = outcome.files.len(),
                "index reusing repoint dry-run walk (no second readdir)"
            );
        }
        let files = outcome.files;
        let relisted_dirs = outcome.relisted_dirs;
        let listing_errors = outcome.listing_errors;
        let mut added = 0u32;
        let mut updated = 0u32;
        let mut unchanged = 0u32;
        let mut skipped_outside_root = 0i64;
        let mut fold_collisions = 0i64;
        let mut keep_folds: HashSet<String> = HashSet::with_capacity(files.len());
        let mut pending_upserts: Vec<UpsertItem> = Vec::with_capacity(INDEX_BATCH);
        let mut pending_were_existing: Vec<bool> = Vec::with_capacity(INDEX_BATCH);
        let mut probe_queue = Vec::new();
        // One listing per parent for the whole index job (flat 10k dirs).
        let mut sidecar_dirs = nightjar_transcode::SidecarDirCache::default();

        let mut by_fold: HashMap<String, Vec<ItemPathRow>> = HashMap::new();
        for row in db.list_item_paths(library_id)? {
            by_fold.entry(fold_path(&row.path)).or_default().push(row);
        }

        let flush = |db: &Db,
                     pool: &LibraryPool,
                     library_id: i64,
                     library_root: &str,
                     pending: &mut Vec<UpsertItem>,
                     were_existing: &mut Vec<bool>,
                     probe_queue: &mut Vec<pool::WorkItem>,
                     added: &mut u32,
                     updated: &mut u32,
                     sidecar_dirs: &mut nightjar_transcode::SidecarDirCache|
         -> Result<(), String> {
            if pending.is_empty() {
                return Ok(());
            }
            let abs_paths: Vec<PathBuf> = pending
                .iter()
                .map(|p| resolve_media_path(library_root, &p.path))
                .collect();
            let ids = db.upsert_items_indexed(library_id, pending)?;
            for (i, id) in ids.into_iter().enumerate() {
                if were_existing[i] {
                    *updated += 1;
                } else {
                    *added += 1;
                }
                probe_queue.push(pool::WorkItem::probe(
                    id,
                    library_id,
                    abs_paths[i].clone(),
                    Some(job_id),
                ));
                match associate_sidecars(db, id, library_root, &abs_paths[i], sidecar_dirs) {
                    Ok(true) => {
                        db.mark_items_subtitle_pending(&[id])?;
                        pool.enqueue(pool::WorkItem::extract(
                            id,
                            library_id,
                            abs_paths[i].clone(),
                        ));
                    }
                    Ok(false) => {}
                    Err(e) => tracing::warn!(
                        item_id = id,
                        path = %abs_paths[i].display(),
                        error = %e,
                        "sidecar association failed"
                    ),
                }
            }
            pending.clear();
            were_existing.clear();
            Ok(())
        };

        for file in &files {
            // Resolve symlinks before the under-root check (ADR-0030 §1).
            // Walk paths stay under the walked root as strings; the inode can
            // still escape via symlink / bind-mount. Fail closed to skip.
            let resolved = std::fs::canonicalize(&file.path).unwrap_or_else(|_| file.path.clone());
            let Some(rel) = to_relpath(&library_root, &resolved) else {
                skipped_outside_root += 1;
                continue;
            };
            let folded = fold_path(&rel);
            keep_folds.insert(folded.clone());

            let matched = by_fold.get(&folded).cloned();
            match matched.as_deref() {
                Some(rows) if rows.len() > 1 => {
                    fold_collisions += 1;
                    tracing::warn!(
                        library_id,
                        path = %rel,
                        count = rows.len(),
                        "fold-equal path collision; refusing upsert"
                    );
                    continue;
                }
                Some([row]) if row.mtime_ms == file.mtime_ms => {
                    unchanged += 1;
                    if row.probe_status == "indexed" {
                        let abs = resolve_media_path(&library_root, &row.path);
                        probe_queue.push(pool::WorkItem::probe(
                            row.id,
                            library_id,
                            abs,
                            Some(job_id),
                        ));
                    }
                }
                other => {
                    // Sticky spelling: keep existing path on fold match.
                    let store_path = match other {
                        Some([row]) => row.path.clone(),
                        _ => rel.clone(),
                    };
                    let were_existing = matches!(other, Some([_]));
                    let file_name = file
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| rel.clone());
                    let parsed = parse_filename(&file_name);
                    let content_id = match nightjar_db::content_id_for_path(&file.path) {
                        Ok(id) => Some(id),
                        Err(e) => {
                            tracing::warn!(
                                path = %file.path.display(),
                                error = %e,
                                "content_id read failed; map rebuild will retry"
                            );
                            None
                        }
                    };
                    pending_upserts.push(UpsertItem {
                        path: store_path.clone(),
                        mtime_ms: file.mtime_ms,
                        size_bytes: file.size_bytes,
                        title: parsed.title,
                        kind: parsed.kind.as_str().to_string(),
                        year: parsed.year,
                        season: parsed.season,
                        episode: parsed.episode,
                        content_id,
                    });
                    pending_were_existing.push(were_existing);
                    if !were_existing {
                        by_fold.insert(
                            folded,
                            vec![ItemPathRow {
                                id: 0,
                                path: store_path,
                                mtime_ms: file.mtime_ms,
                                probe_status: "indexed".into(),
                            }],
                        );
                    }
                    if pending_upserts.len() >= INDEX_BATCH {
                        flush(
                            db,
                            pool,
                            library_id,
                            &library_root,
                            &mut pending_upserts,
                            &mut pending_were_existing,
                            &mut probe_queue,
                            &mut added,
                            &mut updated,
                            &mut sidecar_dirs,
                        )?;
                    }
                }
            }
        }

        flush(
            db,
            pool,
            library_id,
            &library_root,
            &mut pending_upserts,
            &mut pending_were_existing,
            &mut probe_queue,
            &mut added,
            &mut updated,
            &mut sidecar_dirs,
        )?;

        let _ = fold_collisions;
        let root_after = check_root(root);
        let root_ok_after = matches!(root_after, Reachability::Reachable);
        if !root_ok_after {
            let _ = pool.set_library_reachability(library_id, &lib.path, false);
        }
        // First index after a successful repoint: report unmatched rows but do
        // not delete_missing (ADR-0030). Next ordinary scan deletes.
        // dirty_add (path-hint mid-walk): skip delete so keep-set cannot drop
        // the hinted row. Poll-while-active does not set dirty — long walks may
        // still delete (ADR-0015). Manual dirty also skips until follow-up.
        let job_kind = db
            .get_scan_job(job_id)?
            .map(|j| j.kind)
            .unwrap_or_else(|| "scan".into());
        let defer_repoint = job_kind == "repoint";
        let dirty_add = pool.take_dirty_add(library_id);
        let manual_dirty = pool.is_scan_dirty(library_id);
        let skip_delete_hint_or_manual = dirty_add || manual_dirty;
        let allow_delete = !defer_repoint
            && !skip_delete_hint_or_manual
            && allow_delete_missing(
                true,
                root_ok_after,
                listing_errors,
                keep_folds.is_empty(),
                existing_count,
            );
        let deferred_remove = if defer_repoint {
            db.count_missing_fold(library_id, &keep_folds)?
        } else {
            0
        };
        let (removed, deleted_ids) = if allow_delete {
            let deleted_ids = db.delete_missing_fold(library_id, &keep_folds)?;
            for item_id in &deleted_ids {
                if let Err(e) = pool.remove_item_subtitles(*item_id) {
                    tracing::warn!(item_id, error = %e, "remove deleted subtitle directory failed");
                }
            }
            (deleted_ids.len() as u32, deleted_ids)
        } else {
            if defer_repoint {
                tracing::info!(
                    library_id,
                    job_id,
                    deferred_remove,
                    "repoint index: deferring delete_missing until next scan"
                );
            } else if dirty_add {
                tracing::info!(
                    library_id,
                    job_id,
                    "skipping delete_missing; hint dirt during scan (next poll heals deletes)"
                );
            } else if manual_dirty {
                tracing::info!(
                    library_id,
                    job_id,
                    "skipping delete_missing; manual rescan pending follow-up"
                );
            } else {
                tracing::warn!(
                    library_id,
                    listing_errors,
                    existing_count,
                    files = keep_folds.len(),
                    root_ok_after,
                    "skipping delete_missing; reachability in doubt"
                );
            }
            (0, Vec::new())
        };
        let _ = db.set_scan_job_skipped_outside_root(job_id, skipped_outside_root);
        let _ = db.set_scan_job_deferred_remove(job_id, deferred_remove);
        if defer_repoint && deferred_remove > 0 {
            pool.set_repoint_delete_holdoff(library_id, REPOINT_DELETE_HOLDOFF);
            tracing::info!(
                library_id,
                job_id,
                deferred_remove,
                holdoff_s = REPOINT_DELETE_HOLDOFF.as_secs(),
                "repoint deferred_remove holdoff armed; poll will skip until clear or expiry"
            );
        }
        let unresolved = db
            .get_library(library_id)?
            .map(|l| l.paths_unresolved)
            .unwrap_or(0);
        let _ = db.set_library_path_counters(library_id, unresolved, skipped_outside_root);
        let _ = deleted_ids;
        if let Err(e) = pool.cleanup_orphan_subtitles() {
            tracing::warn!(error = %e, "subtitle orphan cleanup failed");
        }

        // Rediscover sidecars beside unchanged media only when the walk cache was
        // warm and the parent was re-listed (new .srt bumps dir mtime). A cold
        // cache would mark every dir relisted and re-pay ~20 min of SMB readdir;
        // existing sidecar rows stay in the DB across restarts, and add/update
        // already ran associate_sidecars in flush.
        let mut sidecar_checked = 0u32;
        if cache_warm && allow_delete {
            for file in &files {
                let Some(parent) = file.path.parent() else {
                    continue;
                };
                if !relisted_dirs.contains(parent) {
                    continue;
                }
                let Some(rel) = to_relpath(&library_root, &file.path) else {
                    continue;
                };
                let folded = fold_path(&rel);
                let Some(rows) = by_fold.get(&folded) else {
                    continue;
                };
                if rows.len() != 1 {
                    continue;
                }
                let row = &rows[0];
                if row.mtime_ms != file.mtime_ms {
                    continue;
                }
                let item_id = row.id;
                if item_id == 0 {
                    continue;
                }
                sidecar_checked += 1;
                match associate_sidecars(db, item_id, &library_root, &file.path, &mut sidecar_dirs)
                {
                    Ok(true) => {
                        db.mark_items_subtitle_pending(&[item_id])?;
                        pool.enqueue(pool::WorkItem::extract(
                            item_id,
                            library_id,
                            file.path.clone(),
                        ));
                    }
                    Ok(false) => {}
                    Err(e) => tracing::warn!(
                        item_id,
                        path = %file.path.display(),
                        error = %e,
                        "sidecar association failed"
                    ),
                }
            }
        }

        let index_duration_ms = index_started.elapsed().as_millis() as u64;
        pool.record_index_duration_ms(index_duration_ms);
        db.set_scan_job_index_done(
            job_id,
            added,
            updated,
            removed,
            unchanged,
            index_duration_ms,
        )?;

        tracing::info!(
            job_id,
            library_id,
            added,
            updated,
            removed,
            unchanged,
            sidecar_checked,
            relisted_dirs = relisted_dirs.len(),
            to_probe = probe_queue.len(),
            index_duration_ms,
            "index pass done"
        );
        Ok((
            added,
            updated,
            removed,
            unchanged,
            probe_queue,
            index_duration_ms,
        ))
    })();
    let (_added, _updated, _removed, _unchanged, probe_queue, _index_duration_ms) = index_result?;
    Ok(probe_queue)
}

fn associate_sidecars(
    db: &Db,
    item_id: i64,
    library_root: &str,
    video_path: &Path,
    cache: &mut nightjar_transcode::SidecarDirCache,
) -> Result<bool, String> {
    let found = nightjar_transcode::discover_sidecars_cached(video_path, Some(cache))?;
    let rows: Vec<nightjar_db::SidecarRow> = found
        .into_iter()
        .filter_map(|s| {
            let path = to_relpath(library_root, &s.path)?;
            Some(nightjar_db::SidecarRow {
                media_item_id: item_id,
                track_id: s.track_id,
                path,
                mtime_ms: s.mtime_ms,
                size_bytes: s.size_bytes,
                format: s.format,
                language: s.language,
                forced: s.forced,
                sdh: s.sdh,
            })
        })
        .collect();
    db.replace_item_sidecars(item_id, &rows)
}

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightjar_db::NewLibrary;
    use nightjar_transcode::SubsStore;
    use std::fs;
    use std::process::Command;

    fn test_pool(db: &Arc<Db>, data_dir: &Path) -> Arc<LibraryPool> {
        let subs = Arc::new(SubsStore::new(data_dir.join("subs")).unwrap());
        LibraryPool::spawn(Arc::clone(db), subs)
    }

    #[test]
    fn index_pass_lists_before_probe_and_broken_moov_errors() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();

        // Tiny valid-ish mp4 via ffmpeg if available; otherwise skip probe-success path.
        let good = media.join("Good Movie (2020).mp4");
        let ffmpeg_ok = Command::new("ffmpeg")
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=64x64:d=0.2",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=48000:cl=stereo",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-shortest",
                good.to_str().unwrap(),
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        let broken = media.join("broken_moov.mp4");
        fs::write(&broken, b"not a real mp4").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let job_id = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        // Wait for completion.
        for _ in 0..200 {
            let job = db.get_scan_job(job_id).unwrap().unwrap();
            if job.state == "completed" || job.state == "failed" {
                assert_eq!(job.state, "completed");
                assert!(job.index_duration_ms.is_some());
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        let items = db.list_items(lib.id).unwrap();
        assert!(!items.is_empty());
        let broken_item = items
            .iter()
            .find(|i| i.path.ends_with("broken_moov.mp4"))
            .expect("broken_moov indexed");
        assert_eq!(broken_item.probe_status, "error");
        assert!(broken_item.scan_error.is_some());

        if ffmpeg_ok {
            let good_item = items
                .iter()
                .find(|i| i.path.ends_with("Good Movie (2020).mp4"))
                .expect("good file");
            assert_eq!(good_item.probe_status, "probed");
            assert!(good_item.video_codec.is_some());
        }

        // Unchanged rescan: index fast, nothing to probe.
        let job2 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        for _ in 0..200 {
            let job = db.get_scan_job(job2).unwrap().unwrap();
            if job.state == "completed" {
                assert!(job.unchanged >= 1);
                assert_eq!(job.probed, 0);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[test]
    fn index_associates_sidecar_srt_not_as_media_item() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(media.join("Subs")).unwrap();
        let video = media.join("Movie.mp4");
        fs::write(&video, b"not a real mp4").unwrap();
        fs::write(
            media.join("Movie.en.srt"),
            b"1\n00:00:00,000 --> 00:00:01,000\nHi\n",
        )
        .unwrap();
        fs::write(
            media.join("Subs").join("Movie.en.srt"),
            b"1\n00:00:00,000 --> 00:00:01,000\nSubs\n",
        )
        .unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let job_id = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        for _ in 0..200 {
            let job = db.get_scan_job(job_id).unwrap().unwrap();
            if job.state == "completed" || job.state == "failed" {
                assert_eq!(job.state, "completed");
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        let items = db.list_items(lib.id).unwrap();
        assert_eq!(items.len(), 1, "srt must not become media items: {items:?}");
        let sidecars = db.list_item_sidecars(items[0].id).unwrap();
        let ids: Vec<_> = sidecars.iter().map(|s| s.track_id.as_str()).collect();
        assert!(ids.contains(&"s-en"), "{ids:?}");
        assert!(ids.contains(&"s-Subs.en"), "{ids:?}");
    }

    #[test]
    fn empty_walk_with_existing_items_does_not_delete() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let video = media.join("Keep.mp4");
        fs::write(&video, b"not a real mp4").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let job_id = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        for _ in 0..200 {
            let job = db.get_scan_job(job_id).unwrap().unwrap();
            if job.state == "completed" || job.state == "failed" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(db.count_items(lib.id).unwrap(), 1);

        // Simulate stale empty mount: wipe files but keep the directory.
        fs::remove_file(&video).unwrap();
        let before = pool.transition_count();
        let job2 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        for _ in 0..200 {
            let job = db.get_scan_job(job2).unwrap().unwrap();
            if job.state == "completed" || job.state == "failed" {
                assert_eq!(job.removed, 0, "empty walk must not delete under doubt");
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(db.count_items(lib.id).unwrap(), 1);
        let _ = before;
    }

    #[test]
    fn unavailable_root_dispatches_no_item_work() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("A.mp4"), b"x").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let job_id = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        for _ in 0..200 {
            let job = db.get_scan_job(job_id).unwrap().unwrap();
            if job.state == "completed" || job.state == "failed" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        // Mark items unavailable as if a prior mount flap wrote them.
        for item in db.list_items(lib.id).unwrap() {
            db.apply_probe_update(&nightjar_db::ProbeUpdate {
                item_id: item.id,
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
                scan_error: Some("unavailable: test".into()),
            })
            .unwrap();
            db.set_subtitle_status(item.id, "unavailable", None, None)
                .unwrap();
        }

        let before = pool.transition_count();
        // Point library at a missing path via DB (simulate unmount).
        {
            // recreate library path by renaming away
            let gone = dir.path().join("gone");
            fs::rename(&media, &gone).unwrap();
        }
        pool.set_library_reachability(lib.id, &lib.path, false)
            .unwrap();
        assert_eq!(pool.transition_count(), before + 1);
        assert!(!pool.is_library_reachable(lib.id));

        // Enqueue must be a no-op while paused.
        for item in db.list_items(lib.id).unwrap() {
            pool.enqueue(pool::WorkItem::probe(
                item.id,
                lib.id,
                PathBuf::from(&item.path),
                None,
            ));
            pool.enqueue(pool::WorkItem::extract(
                item.id,
                lib.id,
                PathBuf::from(&item.path),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        for item in db.list_items(lib.id).unwrap() {
            assert_eq!(item.probe_status, "unavailable");
            assert_ne!(item.probe_status, "error");
        }

        // Restore path and recover.
        fs::rename(dir.path().join("gone"), &media).unwrap();
        let path = media.to_string_lossy().into_owned();
        // Update stored path still points at media which exists again.
        pool.set_library_reachability(lib.id, &path, true).unwrap();
        assert!(pool.is_library_reachable(lib.id));
        for _ in 0..100 {
            let items = db.list_items(lib.id).unwrap();
            if items.iter().all(|i| i.probe_status != "unavailable") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let items = db.list_items(lib.id).unwrap();
        assert!(
            items.iter().all(|i| i.probe_status != "unavailable"),
            "recovery must clear unavailable: {items:?}"
        );
    }

    #[test]
    fn corrupt_file_stays_error_across_reachability_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("broken_moov.mp4"), b"not a real mp4").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let job_id = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        for _ in 0..200 {
            let job = db.get_scan_job(job_id).unwrap().unwrap();
            if job.state == "completed" || job.state == "failed" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let item = db.list_items(lib.id).unwrap().into_iter().next().unwrap();
        assert_eq!(item.probe_status, "error");

        pool.set_library_reachability(lib.id, &lib.path, false)
            .unwrap();
        pool.set_library_reachability(lib.id, &lib.path, true)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        let again = db.get_item(item.id).unwrap().unwrap();
        assert_eq!(
            again.probe_status, "error",
            "permanent errors must not be re-queued by reachability recovery"
        );
    }

    fn wait_scan(db: &Db, job_id: i64) {
        for _ in 0..200 {
            let job = db.get_scan_job(job_id).unwrap().unwrap();
            if job.state == "completed" || job.state == "failed" {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("scan job {job_id} did not finish");
    }

    /// Scan requeues probe_status=unavailable (ADR-0014 retryable) but not error.
    #[test]
    fn scan_requeues_unavailable_not_permanent_error() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("A.mp4"), b"x").unwrap();
        fs::write(media.join("broken_moov.mp4"), b"not a real mp4").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let job1 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_scan(&db, job1);

        let items = db.list_items(lib.id).unwrap();
        let a = items
            .iter()
            .find(|i| i.path.ends_with("A.mp4"))
            .expect("A.mp4");
        let broken = items
            .iter()
            .find(|i| i.path.ends_with("broken_moov.mp4"))
            .expect("broken");
        assert_eq!(broken.probe_status, "error");

        // Simulate prior ENOENT-class failures on A; leave broken as permanent error.
        db.apply_probe_update(&nightjar_db::ProbeUpdate {
            item_id: a.id,
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
            scan_error: Some("no such file or directory".into()),
        })
        .unwrap();
        db.set_subtitle_status(a.id, "unavailable", None, None)
            .unwrap();

        // Drain with stored relpath only (dogfood failure mode) must still resolve.
        let n = pool.drain_pending_probes().unwrap();
        assert_eq!(
            n, 0,
            "unavailable is not indexed; drain skips until requeue"
        );

        let job2 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_scan(&db, job2);

        let a2 = db.get_item(a.id).unwrap().unwrap();
        assert_ne!(
            a2.probe_status, "unavailable",
            "scan must requeue unavailable; got {:?}",
            a2
        );
        assert!(
            a2.probe_status == "error"
                || a2.probe_status == "probed"
                || a2.probe_status == "indexed",
            "expected re-probe outcome, got {}",
            a2.probe_status
        );

        let broken2 = db.get_item(broken.id).unwrap().unwrap();
        assert_eq!(
            broken2.probe_status, "error",
            "permanent error must not be cleared by scan requeue"
        );
    }

    /// drain_pending_probes joins library root to ADR-0030 relpaths.
    #[test]
    fn drain_pending_probes_resolves_relpath() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        // Real tiny mp4 when ffmpeg exists so probe can succeed; otherwise accept error.
        let good = media.join("clip.mp4");
        let _ = Command::new("ffmpeg")
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=64x64:d=0.2",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                good.to_str().unwrap(),
            ])
            .status();
        if !good.exists() {
            fs::write(&good, b"x").unwrap();
        }

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let job = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_scan(&db, job);

        let item = db.list_items(lib.id).unwrap().into_iter().next().unwrap();
        // Force indexed with clear error as if restart mid-scan left the row.
        db.apply_probe_update(&nightjar_db::ProbeUpdate {
            item_id: item.id,
            duration_ms: None,
            container: None,
            video_codec: None,
            audio_codec: None,
            audio_channels: None,
            width: None,
            height: None,
            video_bitrate_bps: None,
            hdr: None,
            probe_status: "indexed".into(),
            scan_error: None,
        })
        .unwrap();

        assert!(item.path == "clip.mp4" || item.path.ends_with("clip.mp4"));
        assert!(!item.path.starts_with('/'), "stored path should be relpath");

        let n = pool.drain_pending_probes().unwrap();
        assert_eq!(n, 1);
        for _ in 0..100 {
            let row = db.get_item(item.id).unwrap().unwrap();
            if row.probe_status != "indexed" {
                assert_ne!(
                    row.probe_status, "unavailable",
                    "relpath drain must not ENOENT: {:?}",
                    row.scan_error
                );
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("probe never left indexed");
    }

    #[test]
    fn triggers_during_scan_coalesce_to_one_follow_up() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        for i in 0..200 {
            let season = media.join(format!("Show/Season {}", i % 10));
            fs::create_dir_all(&season).unwrap();
            fs::write(season.join(format!("E{i:03}.mp4")), b"x").unwrap();
        }

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "shows".into(),
            })
            .unwrap();

        let job1 = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Manual,
        )
        .unwrap();

        let mut overlapped = false;
        for _ in 0..400 {
            if db.active_scan_job(lib.id).unwrap() == Some(job1) {
                for _ in 0..8 {
                    let id = request_scan(
                        Arc::clone(&db),
                        Arc::clone(&pool),
                        lib.id,
                        ScanTrigger::Manual,
                    )
                    .unwrap();
                    assert_eq!(id, job1, "must reuse active job");
                }
                overlapped = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            overlapped,
            "scan finished before overlap window; enlarge fixture if this flakes"
        );

        // Job1 clearing `active` can race the follow-up spawn: wait until we
        // either see two finished jobs or the follow-up has run and gone idle.
        let mut completed = 0;
        for _ in 0..800 {
            completed = 0;
            for id in job1..job1 + 8 {
                if let Ok(Some(j)) = db.get_scan_job(id)
                    && j.library_id == lib.id
                    && (j.state == "completed" || j.state == "failed")
                {
                    completed += 1;
                }
            }
            let active = db.active_scan_job(lib.id).unwrap();
            if active.is_none() && completed >= 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert_eq!(
            completed, 2,
            "one follow-up after coalesced triggers, got {completed}"
        );
    }

    #[test]
    fn symlink_escape_increments_skipped_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&media).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let escape_target = outside.join("escaped.mp4");
        fs::write(&escape_target, b"x").unwrap();
        let link = media.join("escaped.mp4");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&escape_target, &link).unwrap();
        #[cfg(not(unix))]
        {
            let _ = (escape_target, link);
            return;
        }
        fs::write(media.join("kept.mp4"), b"y").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let job_id = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();

        for _ in 0..200 {
            let job = db.get_scan_job(job_id).unwrap().unwrap();
            if job.state == "completed" || job.state == "failed" {
                assert_eq!(job.state, "completed");
                assert!(
                    job.skipped_outside_root >= 1,
                    "symlink escape should skip, got {}",
                    job.skipped_outside_root
                );
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let items = db.list_items(lib.id).unwrap();
        assert!(items.iter().any(|i| i.path == "kept.mp4"));
        assert!(!items.iter().any(|i| i.path.contains("escaped")));
        let lib = db.get_library(lib.id).unwrap().unwrap();
        assert!(lib.skipped_outside_root >= 1);
    }

    #[test]
    fn sticky_spelling_survives_case_only_walk() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let original = media.join("Title.mp4");
        fs::write(&original, b"x").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let job1 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job1);
        let before = db.list_items(lib.id).unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].path, "Title.mp4");
        let id = before[0].id;

        // Case-only rename of the directory entry (works on folding and
        // case-sensitive hosts). Sticky spelling must keep the first path.
        let renamed = media.join("title.mp4");
        let _ = fs::rename(&original, &renamed);
        // Bump mtime/size so the index treats it as changed content.
        fs::write(&renamed, b"xy").unwrap();

        let job2 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job2);
        let after = db.list_items(lib.id).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, id);
        assert_eq!(after[0].path, "Title.mp4");
    }

    #[test]
    fn fold_collision_refuses_upsert() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("a.mp4"), b"x").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let job1 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job1);
        assert_eq!(db.list_items(lib.id).unwrap().len(), 1);

        // Corrupt DB: two BINARY-distinct rows that fold-collide.
        db.upsert_items_indexed(
            lib.id,
            &[UpsertItem {
                path: "A.mp4".into(),
                mtime_ms: 1,
                size_bytes: 1,
                title: "A".into(),
                kind: "movie".into(),
                year: None,
                season: None,
                episode: None,
                content_id: None,
            }],
        )
        .unwrap();
        assert_eq!(db.list_items(lib.id).unwrap().len(), 2);

        let job2 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job2);
        // Upsert refused for the colliding fold; both corrupt rows may remain
        // until operator cleanup — walk must not silently pick one.
        let paths: Vec<_> = db
            .list_items(lib.id)
            .unwrap()
            .into_iter()
            .map(|i| i.path)
            .collect();
        assert!(
            paths.contains(&"a.mp4".into()) && paths.contains(&"A.mp4".into()),
            "collision must not collapse rows: {paths:?}"
        );
    }

    fn wait_job(db: &Db, job_id: i64) {
        for _ in 0..200 {
            let job = db.get_scan_job(job_id).unwrap().unwrap();
            if job.state == "completed" || job.state == "failed" {
                assert_eq!(
                    job.state, "completed",
                    "job {job_id}: {:?}",
                    job.error_message
                );
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("job {job_id} did not finish");
    }

    #[test]
    fn hint_ingest_upserts_media_and_skips_non_media() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let ep = media.join("Show.S01E01.mkv");
        fs::write(&ep, b"not empty").unwrap();
        fs::write(media.join("notes.txt"), b"x").unwrap();
        fs::write(media.join("Show.S01E01.en.srt"), b"1\n").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "shows".into(),
            })
            .unwrap();

        assert_eq!(
            hint_ingest(db.as_ref(), pool.as_ref(), lib.id, &media.join("notes.txt")).unwrap(),
            HintIngestOutcome::Ignored
        );
        assert_eq!(
            hint_ingest(
                db.as_ref(),
                pool.as_ref(),
                lib.id,
                &media.join("Show.S01E01.en.srt")
            )
            .unwrap(),
            HintIngestOutcome::Ignored
        );
        let out = hint_ingest(db.as_ref(), pool.as_ref(), lib.id, &ep).unwrap();
        let HintIngestOutcome::Upserted { item_id } = out else {
            panic!("expected Upserted, got {out:?}");
        };
        assert!(item_id > 0);
        let items = db.list_items(lib.id).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].path, "Show.S01E01.mkv");
        assert_eq!(items[0].probe_status, "indexed");

        // Zero-size skipped (copy-in-progress).
        let empty = media.join("empty.mp4");
        fs::write(&empty, b"").unwrap();
        assert_eq!(
            hint_ingest(db.as_ref(), pool.as_ref(), lib.id, &empty).unwrap(),
            HintIngestOutcome::Ignored
        );
        assert_eq!(db.list_items(lib.id).unwrap().len(), 1);
    }

    #[test]
    fn hint_ingest_unchanged_mtime_and_update_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let path = media.join("clip.mp4");
        fs::write(&path, b"v1").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let first = hint_ingest(db.as_ref(), pool.as_ref(), lib.id, &path).unwrap();
        let HintIngestOutcome::Upserted { item_id } = first else {
            panic!("expected Upserted, got {first:?}");
        };
        let second = hint_ingest(db.as_ref(), pool.as_ref(), lib.id, &path).unwrap();
        assert_eq!(second, HintIngestOutcome::Unchanged { item_id });
        assert_eq!(db.list_items(lib.id).unwrap().len(), 1);

        // Bump mtime/size so the short-circuit does not apply.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&path, b"v2-longer").unwrap();
        let third = hint_ingest(db.as_ref(), pool.as_ref(), lib.id, &path).unwrap();
        let HintIngestOutcome::Upserted { item_id: id2 } = third else {
            panic!("expected Upserted after mtime change, got {third:?}");
        };
        assert_eq!(id2, item_id, "same path must keep media_items.id");
        let row = db.get_item(item_id).unwrap().unwrap();
        assert_eq!(row.probe_status, "indexed");
        assert_eq!(row.path, "clip.mp4");
        assert_eq!(db.list_items(lib.id).unwrap().len(), 1);
    }

    #[test]
    fn hint_ingest_fold_collision_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("a.mp4"), b"data").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let job = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job);

        db.upsert_items_indexed(
            lib.id,
            &[UpsertItem {
                path: "A.mp4".into(),
                mtime_ms: 1,
                size_bytes: 1,
                title: "A".into(),
                kind: "movie".into(),
                year: None,
                season: None,
                episode: None,
                content_id: None,
            }],
        )
        .unwrap();
        assert_eq!(db.list_items(lib.id).unwrap().len(), 2);

        let out = hint_ingest(db.as_ref(), pool.as_ref(), lib.id, &media.join("a.mp4")).unwrap();
        assert_eq!(out, HintIngestOutcome::Collision);
        assert_eq!(db.list_items(lib.id).unwrap().len(), 2);
    }

    #[test]
    fn hint_ingest_does_not_delete_missing_rows() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("keep.mp4"), b"data").unwrap();
        fs::write(media.join("gone.mp4"), b"data").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let job = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job);
        assert_eq!(db.list_items(lib.id).unwrap().len(), 2);

        fs::remove_file(media.join("gone.mp4")).unwrap();
        let new_ep = media.join("new.mp4");
        fs::write(&new_ep, b"fresh").unwrap();
        // Hint only — no full scan. gone.mp4 must remain in DB.
        hint_ingest(db.as_ref(), pool.as_ref(), lib.id, &new_ep).unwrap();
        let paths: std::collections::HashSet<_> = db
            .list_items(lib.id)
            .unwrap()
            .into_iter()
            .map(|i| i.path)
            .collect();
        assert!(
            paths.contains("gone.mp4"),
            "hint must not delete_missing: {paths:?}"
        );
        assert!(paths.contains("new.mp4"), "hint must upsert: {paths:?}");
        assert!(paths.contains("keep.mp4"));
        assert_eq!(paths.len(), 3);
    }

    #[test]
    fn hint_during_active_scan_sets_dirty_add_not_follow_up() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        for i in 0..80 {
            fs::write(media.join(format!("f{i:03}.mp4")), b"x").unwrap();
        }

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let job1 = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Manual,
        )
        .unwrap();
        let mut overlapped = false;
        for _ in 0..400 {
            if db.active_scan_job(lib.id).unwrap() == Some(job1) {
                let late = media.join("late.mp4");
                fs::write(&late, b"late").unwrap();
                let out = hint_ingest(db.as_ref(), pool.as_ref(), lib.id, &late).unwrap();
                assert!(
                    matches!(
                        out,
                        HintIngestOutcome::Upserted { .. } | HintIngestOutcome::Unchanged { .. }
                    ),
                    "hint during scan: {out:?}"
                );
                if matches!(out, HintIngestOutcome::Upserted { .. }) {
                    assert!(
                        pool.is_dirty_add(lib.id),
                        "upsert hint during active scan must set dirty_add"
                    );
                    assert!(
                        !pool.is_scan_dirty(lib.id),
                        "hint must not set manual follow-up dirty"
                    );
                }
                // Poll while active is a dirty no-op.
                assert_eq!(
                    request_scan(
                        Arc::clone(&db),
                        Arc::clone(&pool),
                        lib.id,
                        ScanTrigger::Poll
                    )
                    .unwrap(),
                    job1
                );
                assert!(
                    !pool.is_scan_dirty(lib.id),
                    "poll while active must not set scan_dirty"
                );
                overlapped = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(overlapped, "scan finished before hint overlap");

        wait_job(&db, job1);
        // No automatic follow-up from hint-only dirt.
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            db.active_scan_job(lib.id).unwrap().is_none(),
            "hint dirt must not schedule a follow-up scan"
        );
        let paths: Vec<_> = db
            .list_items(lib.id)
            .unwrap()
            .into_iter()
            .map(|i| i.path)
            .collect();
        assert!(
            paths.iter().any(|p| p == "late.mp4"),
            "hinted file must survive: {paths:?}"
        );
    }

    #[test]
    fn poll_while_active_does_not_suppress_delete_missing() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("keep.mp4"), b"data").unwrap();
        fs::write(media.join("gone.mp4"), b"data").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let job0 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job0);
        assert_eq!(db.list_items(lib.id).unwrap().len(), 2);

        fs::remove_file(media.join("gone.mp4")).unwrap();
        // Many files so the walk stays active long enough for a mid-walk poll.
        for i in 0..120 {
            fs::write(media.join(format!("extra{i:03}.mp4")), b"x").unwrap();
        }

        let job1 = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Manual,
        )
        .unwrap();
        let mut polled = false;
        for _ in 0..500 {
            if db.active_scan_job(lib.id).unwrap() == Some(job1) {
                let id = request_scan(
                    Arc::clone(&db),
                    Arc::clone(&pool),
                    lib.id,
                    ScanTrigger::Poll,
                )
                .unwrap();
                assert_eq!(id, job1);
                assert!(!pool.is_scan_dirty(lib.id));
                assert!(!pool.is_dirty_add(lib.id));
                polled = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(polled, "job finished before mid-walk poll");

        wait_job(&db, job1);
        let job1_row = db.get_scan_job(job1).unwrap().unwrap();
        assert!(
            job1_row.removed >= 1,
            "poll-while-active must not suppress delete_missing; removed={}",
            job1_row.removed
        );
        let paths: std::collections::HashSet<_> = db
            .list_items(lib.id)
            .unwrap()
            .into_iter()
            .map(|i| i.path)
            .collect();
        assert!(
            !paths.contains("gone.mp4"),
            "gone.mp4 must be deleted: {paths:?}"
        );
    }

    #[test]
    fn manual_scan_while_active_still_coalesces_follow_up() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        for i in 0..100 {
            fs::write(media.join(format!("f{i:03}.mp4")), b"x").unwrap();
        }

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let job1 = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Manual,
        )
        .unwrap();
        let mut overlapped = false;
        for _ in 0..400 {
            if db.active_scan_job(lib.id).unwrap() == Some(job1) {
                assert_eq!(
                    request_scan(
                        Arc::clone(&db),
                        Arc::clone(&pool),
                        lib.id,
                        ScanTrigger::Manual
                    )
                    .unwrap(),
                    job1
                );
                assert!(pool.is_scan_dirty(lib.id));
                overlapped = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(overlapped);

        let mut completed = 0;
        for _ in 0..800 {
            completed = 0;
            for id in job1..job1 + 8 {
                if let Ok(Some(j)) = db.get_scan_job(id)
                    && j.library_id == lib.id
                    && (j.state == "completed" || j.state == "failed")
                {
                    completed += 1;
                }
            }
            if db.active_scan_job(lib.id).unwrap().is_none() && completed >= 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert_eq!(completed, 2, "manual dirty must spawn one follow-up");
    }

    #[test]
    fn create_then_request_scan_returns_job_without_blocking_on_probe() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("A.mp4"), b"x").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let t0 = std::time::Instant::now();
        let job_id = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Manual,
        )
        .unwrap();
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(2),
            "request_scan must return before the walk finishes"
        );
        assert!(job_id > 0);
        let job = db.get_scan_job(job_id).unwrap().unwrap();
        assert_eq!(job.library_id, lib.id);
    }

    #[test]
    fn repoint_holdoff_blocks_poll_not_manual() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("a.mp4"), b"data").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let job0 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job0);

        pool.set_repoint_delete_holdoff(lib.id, Duration::from_secs(3600));
        assert!(pool.repoint_delete_holdoff_active(lib.id));

        let polled = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Poll,
        )
        .unwrap();
        assert_eq!(polled, 0, "poll must no-op under holdoff");
        assert!(
            db.active_scan_job(lib.id).unwrap().is_none(),
            "poll must not start a job under holdoff"
        );

        let manual = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Manual,
        )
        .unwrap();
        assert!(manual > 0);
        wait_job(&db, manual);
        assert!(
            !pool.repoint_delete_holdoff_active(lib.id),
            "successful ordinary scan clears holdoff"
        );

        let after = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Poll,
        )
        .unwrap();
        assert!(after > 0, "poll works again after holdoff clear");
        wait_job(&db, after);
    }

    #[test]
    fn repoint_holdoff_expires() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("a.mp4"), b"data").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        pool.set_repoint_delete_holdoff(lib.id, Duration::from_millis(80));
        assert!(pool.repoint_delete_holdoff_active(lib.id));
        std::thread::sleep(Duration::from_millis(120));
        assert!(!pool.repoint_delete_holdoff_active(lib.id));
        let polled = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Poll,
        )
        .unwrap();
        assert!(polled > 0);
        wait_job(&db, polled);
    }

    #[test]
    fn repoint_with_deferred_remove_arms_holdoff() {
        let dir = tempfile::tempdir().unwrap();
        let old_root = dir.path().join("old");
        let new_root = dir.path().join("new");
        fs::create_dir_all(&old_root).unwrap();
        fs::create_dir_all(&new_root).unwrap();
        // 10 keep + 1 gone → retain 10/11 ≥ 0.90, deferred_remove = 1.
        for i in 0..10 {
            let name = format!("keep{i}.mp4");
            fs::write(old_root.join(&name), b"data").unwrap();
            fs::write(new_root.join(&name), b"data").unwrap();
        }
        fs::write(old_root.join("gone.mp4"), b"data").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: old_root.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let job0 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job0);
        assert_eq!(db.list_items(lib.id).unwrap().len(), 11);

        let repoint_id = request_repoint(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            &new_root.to_string_lossy(),
        )
        .unwrap();
        wait_job(&db, repoint_id);
        let job = db.get_scan_job(repoint_id).unwrap().unwrap();
        assert_eq!(job.state, "completed", "repoint: {:?}", job.error_message);
        assert_eq!(job.deferred_remove, 1);
        assert!(
            pool.repoint_delete_holdoff_active(lib.id),
            "deferred_remove > 0 must arm holdoff"
        );
        assert_eq!(
            request_scan(
                Arc::clone(&db),
                Arc::clone(&pool),
                lib.id,
                ScanTrigger::Poll
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn repoint_reseeds_walk_cache_under_new_root() {
        let dir = tempfile::tempdir().unwrap();
        let old_root = dir.path().join("old");
        let new_root = dir.path().join("new");
        fs::create_dir_all(old_root.join("Show")).unwrap();
        fs::create_dir_all(new_root.join("Show")).unwrap();
        fs::write(old_root.join("Show/ep.mp4"), b"data").unwrap();
        fs::write(new_root.join("Show/ep.mp4"), b"data").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: old_root.to_string_lossy().into_owned(),
                kind: "shows".into(),
            })
            .unwrap();
        let job0 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job0);

        let repoint_id = request_repoint(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            &new_root.to_string_lossy(),
        )
        .unwrap();
        wait_job(&db, repoint_id);
        let job = db.get_scan_job(repoint_id).unwrap().unwrap();
        assert_eq!(job.state, "completed", "{:?}", job.error_message);
        assert_eq!(job.unchanged + job.added + job.updated, 1);

        let dir_count = pool.with_walk_cache(lib.id, |c| c.dir_count());
        assert!(
            dir_count >= 1,
            "repoint must reseed WalkCache under new root, dir_count={dir_count}"
        );
        let lib_row = db.get_library(lib.id).unwrap().unwrap();
        let new_canon = std::fs::canonicalize(&new_root).unwrap();
        assert!(
            lib_row.path.contains("new")
                || Path::new(&lib_row.path) == new_canon.as_path()
                || lib_row.path == new_canon.to_string_lossy(),
            "library path updated: {}",
            lib_row.path
        );
    }
}
