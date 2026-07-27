//! Library scanner: index pass, then bounded ffprobe pool (ADR-0004).

mod pool;
mod probe;
mod reachability;
mod walk;
mod watch;

pub use pool::LibraryPool;
pub use reachability::{Reachability, allow_delete_missing, check_root};
pub use walk::{
    WalkCache, WalkOutcome, walk_concurrency, walk_media_files_cached,
    walk_media_files_cached_with_concurrency,
};
pub use watch::spawn_library_watcher;

use nightjar_core::parse_filename;
use nightjar_db::{Db, UpsertItem};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

const INDEX_BATCH: usize = 200;

/// Request a library scan (ADR-0015). Sole entry point for creation, notify,
/// poll, and manual scan. Returns the active or newly accepted job id.
///
/// If a scan is already running, marks the library dirty for exactly one
/// follow-up when it finishes and returns the active job id.
pub fn request_scan(db: Arc<Db>, pool: Arc<LibraryPool>, library_id: i64) -> Result<i64, String> {
    let lib = db
        .get_library(library_id)?
        .ok_or_else(|| format!("library {library_id} not found"))?;
    if matches!(check_root(Path::new(&lib.path)), Reachability::Unreachable) {
        let _ = pool.set_library_reachability(library_id, &lib.path, false);
        return Err(format!("library path is not reachable: {}", lib.path));
    }
    if let Some(existing) = db.active_scan_job(library_id)? {
        pool.mark_scan_dirty(library_id);
        return Ok(existing);
    }
    let job_id = db.create_scan_job(library_id)?;
    std::thread::Builder::new()
        .name(format!("scan-job-{job_id}"))
        .spawn(move || {
            if let Err(e) = run_scan_job(&db, &pool, job_id, library_id) {
                tracing::error!(job_id, library_id, error = %e, "scan job failed");
                let _ = db.fail_scan_job(job_id, &e);
            }
            if pool.take_scan_dirty(library_id) {
                tracing::info!(
                    library_id,
                    "library dirty after scan; starting follow-up job"
                );
                if let Err(e) = request_scan(Arc::clone(&db), Arc::clone(&pool), library_id) {
                    tracing::warn!(library_id, error = %e, "follow-up scan failed");
                }
            }
        })
        .map_err(|e| format!("spawn scan job {job_id}: {e}"))?;
    Ok(job_id)
}

/// Alias kept for tests and call sites that mean "start discovery".
pub fn start_scan_job(db: Arc<Db>, pool: Arc<LibraryPool>, library_id: i64) -> Result<i64, String> {
    request_scan(db, pool, library_id)
}

fn run_scan_job(
    db: &Arc<Db>,
    pool: &Arc<LibraryPool>,
    job_id: i64,
    library_id: i64,
) -> Result<(), String> {
    db.set_scan_job_state(job_id, "indexing")?;

    let lib = db
        .get_library(library_id)?
        .ok_or_else(|| format!("library {library_id} not found"))?;
    let root = Path::new(&lib.path);
    let root_before = check_root(root);
    if !matches!(root_before, Reachability::Reachable) {
        let _ = pool.set_library_reachability(library_id, &lib.path, false);
        return Err(format!("library path is not reachable: {}", lib.path));
    }

    let existing_count = db.count_items(library_id)?;
    let index_started = Instant::now();
    // Hold extract workers off the share for the whole indexing phase (walk,
    // upserts, sidecar rediscovery). One in-flight demux may still finish;
    // new extracts wait until end_index, after set_scan_job_index_done.
    pool.begin_index();
    #[allow(clippy::type_complexity)]
    let index_result = (|| -> Result<(u32, u32, u32, u32, Vec<pool::WorkItem>, u64), String> {
        let cache_warm = pool.with_walk_cache(library_id, |cache| !cache.is_empty());
        let outcome = pool.with_walk_cache(library_id, |cache| {
            walk::walk_media_files_cached(root, Some(cache))
        })?;
        let files = outcome.files;
        let relisted_dirs = outcome.relisted_dirs;
        let listing_errors = outcome.listing_errors;
        let mut added = 0u32;
        let mut updated = 0u32;
        let mut unchanged = 0u32;
        let mut keep_paths = Vec::with_capacity(files.len());
        let mut pending_upserts: Vec<UpsertItem> = Vec::with_capacity(INDEX_BATCH);
        let mut pending_were_existing: Vec<bool> = Vec::with_capacity(INDEX_BATCH);
        let mut probe_queue = Vec::new();

        let flush = |db: &Db,
                     pool: &LibraryPool,
                     library_id: i64,
                     pending: &mut Vec<UpsertItem>,
                     were_existing: &mut Vec<bool>,
                     probe_queue: &mut Vec<pool::WorkItem>,
                     added: &mut u32,
                     updated: &mut u32|
         -> Result<(), String> {
            if pending.is_empty() {
                return Ok(());
            }
            let paths: Vec<PathBuf> = pending.iter().map(|p| PathBuf::from(&p.path)).collect();
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
                    paths[i].clone(),
                    Some(job_id),
                ));
                // New or replaced media: discover sidecars now so we do not need a
                // full-library rediscovery after every cold walk.
                match associate_sidecars(db, id, &paths[i]) {
                    Ok(true) => {
                        db.mark_items_subtitle_pending(&[id])?;
                        pool.enqueue(pool::WorkItem::extract(id, library_id, paths[i].clone()));
                    }
                    Ok(false) => {}
                    Err(e) => tracing::warn!(
                        item_id = id,
                        path = %paths[i].display(),
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
            let path_str = path_to_string(&file.path);
            keep_paths.push(path_str.clone());

            match db.item_index_row(library_id, &path_str)? {
                Some((id, mtime, probe_status)) if mtime == file.mtime_ms => {
                    unchanged += 1;
                    // A prior run may have indexed this file and died before probe.
                    // Unchanged mtime alone must not leave it stranded forever.
                    if probe_status == "indexed" {
                        probe_queue.push(pool::WorkItem::probe(
                            id,
                            library_id,
                            file.path.clone(),
                            Some(job_id),
                        ));
                    }
                }
                existing => {
                    let file_name = file
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path_str.clone());
                    let parsed = parse_filename(&file_name);
                    pending_upserts.push(UpsertItem {
                        path: path_str,
                        mtime_ms: file.mtime_ms,
                        size_bytes: file.size_bytes,
                        title: parsed.title,
                        kind: parsed.kind.as_str().to_string(),
                        year: parsed.year,
                        season: parsed.season,
                        episode: parsed.episode,
                    });
                    pending_were_existing.push(existing.is_some());
                    if pending_upserts.len() >= INDEX_BATCH {
                        flush(
                            db,
                            pool,
                            library_id,
                            &mut pending_upserts,
                            &mut pending_were_existing,
                            &mut probe_queue,
                            &mut added,
                            &mut updated,
                        )?;
                    }
                }
            }
        }

        flush(
            db,
            pool,
            library_id,
            &mut pending_upserts,
            &mut pending_were_existing,
            &mut probe_queue,
            &mut added,
            &mut updated,
        )?;

        let root_after = check_root(root);
        let root_ok_after = matches!(root_after, Reachability::Reachable);
        if !root_ok_after {
            let _ = pool.set_library_reachability(library_id, &lib.path, false);
        }
        let allow_delete = allow_delete_missing(
            true,
            root_ok_after,
            listing_errors,
            keep_paths.is_empty(),
            existing_count,
        );
        let (removed, deleted_ids) = if allow_delete {
            let deleted_ids = db.delete_missing(library_id, &keep_paths)?;
            for item_id in &deleted_ids {
                if let Err(e) = pool.remove_item_subtitles(*item_id) {
                    tracing::warn!(item_id, error = %e, "remove deleted subtitle directory failed");
                }
            }
            (deleted_ids.len() as u32, deleted_ids)
        } else {
            tracing::warn!(
                library_id,
                listing_errors,
                existing_count,
                files = keep_paths.len(),
                root_ok_after,
                "skipping delete_missing; reachability in doubt"
            );
            (0, Vec::new())
        };
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
                let path_str = path_to_string(&file.path);
                let Some((item_id, mtime)) = db.item_mtime(library_id, &path_str)? else {
                    continue;
                };
                if mtime != file.mtime_ms {
                    continue;
                }
                sidecar_checked += 1;
                match associate_sidecars(db, item_id, &file.path) {
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
    pool.end_index();
    let (_added, _updated, _removed, _unchanged, probe_queue, _index_duration_ms) = index_result?;

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
    for (item_id, path) in probe_ids {
        pool.enqueue(pool::WorkItem::extract(item_id, library_id, path));
    }
    pool.drain_pending_extracts()?;
    let probe_duration_ms = probe_started.elapsed().as_millis() as u64;
    db.complete_scan_job(job_id, probe_duration_ms)?;

    tracing::info!(job_id, library_id, probe_duration_ms, "scan job completed");
    Ok(())
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn associate_sidecars(db: &Db, item_id: i64, video_path: &Path) -> Result<bool, String> {
    let found = nightjar_transcode::discover_sidecars(video_path)?;
    let rows: Vec<nightjar_db::SidecarRow> = found
        .into_iter()
        .map(|s| nightjar_db::SidecarRow {
            media_item_id: item_id,
            track_id: s.track_id,
            path: path_to_string(&s.path),
            mtime_ms: s.mtime_ms,
            size_bytes: s.size_bytes,
            format: s.format,
            language: s.language,
            forced: s.forced,
            sdh: s.sdh,
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

        let job1 = request_scan(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();

        let mut overlapped = false;
        for _ in 0..400 {
            if db.active_scan_job(lib.id).unwrap() == Some(job1) {
                for _ in 0..8 {
                    let id = request_scan(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
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

        for _ in 0..800 {
            if db.active_scan_job(lib.id).unwrap().is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(db.active_scan_job(lib.id).unwrap().is_none());

        let mut completed = 0;
        for id in job1..job1 + 8 {
            if let Ok(Some(j)) = db.get_scan_job(id)
                && j.library_id == lib.id
                && (j.state == "completed" || j.state == "failed")
            {
                completed += 1;
            }
        }
        assert_eq!(
            completed, 2,
            "one follow-up after coalesced triggers, got {completed}"
        );
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
        let job_id = request_scan(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(2),
            "request_scan must return before the walk finishes"
        );
        assert!(job_id > 0);
        let job = db.get_scan_job(job_id).unwrap().unwrap();
        assert_eq!(job.library_id, lib.id);
    }
}
