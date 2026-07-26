//! Library scanner: index pass, then bounded ffprobe pool (ADR-0004).

mod probe;
mod walk;
mod watch;

pub use watch::spawn_library_watcher;

use nightjar_core::parse_filename;
use nightjar_db::{Db, ProbeUpdate, UpsertItem};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

const INDEX_BATCH: usize = 200;

#[derive(Debug, Clone)]
struct ProbeWork {
    item_id: i64,
    path: PathBuf,
}

/// Start (or reuse) an async scan job. Returns the job id immediately.
pub fn start_scan_job(db: Arc<Db>, library_id: i64) -> Result<i64, String> {
    if db.get_library(library_id)?.is_none() {
        return Err(format!("library {library_id} not found"));
    }
    if let Some(existing) = db.active_scan_job(library_id)? {
        return Ok(existing);
    }
    let job_id = db.create_scan_job(library_id)?;
    std::thread::Builder::new()
        .name(format!("scan-job-{job_id}"))
        .spawn(move || {
            if let Err(e) = run_scan_job(&db, job_id, library_id) {
                tracing::error!(job_id, library_id, error = %e, "scan job failed");
                let _ = db.fail_scan_job(job_id, &e);
            }
        })
        .map_err(|e| format!("spawn scan job {job_id}: {e}"))?;
    Ok(job_id)
}

fn run_scan_job(db: &Arc<Db>, job_id: i64, library_id: i64) -> Result<(), String> {
    db.set_scan_job_state(job_id, "indexing")?;

    let lib = db
        .get_library(library_id)?
        .ok_or_else(|| format!("library {library_id} not found"))?;
    let root = Path::new(&lib.path);
    if !root.is_dir() {
        return Err(format!("library path is not a directory: {}", lib.path));
    }

    let index_started = Instant::now();
    let files = walk::walk_media_files(root)?;
    let mut added = 0u32;
    let mut updated = 0u32;
    let mut unchanged = 0u32;
    let mut keep_paths = Vec::with_capacity(files.len());
    let mut pending_upserts: Vec<UpsertItem> = Vec::with_capacity(INDEX_BATCH);
    let mut pending_were_existing: Vec<bool> = Vec::with_capacity(INDEX_BATCH);
    let mut probe_queue: Vec<ProbeWork> = Vec::new();

    let flush = |db: &Db,
                 library_id: i64,
                 pending: &mut Vec<UpsertItem>,
                 were_existing: &mut Vec<bool>,
                 probe_queue: &mut Vec<ProbeWork>,
                 added: &mut u32,
                 updated: &mut u32|
     -> Result<(), String> {
        if pending.is_empty() {
            return Ok(());
        }
        let ids = db.upsert_items_indexed(library_id, pending)?;
        for (i, id) in ids.into_iter().enumerate() {
            if were_existing[i] {
                *updated += 1;
            } else {
                *added += 1;
            }
            probe_queue.push(ProbeWork {
                item_id: id,
                path: PathBuf::from(&pending[i].path),
            });
        }
        pending.clear();
        were_existing.clear();
        Ok(())
    };

    for file in &files {
        let path_str = path_to_string(&file.path);
        keep_paths.push(path_str.clone());

        match db.item_mtime(library_id, &path_str)? {
            Some((_, mtime)) if mtime == file.mtime_ms => {
                unchanged += 1;
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
        library_id,
        &mut pending_upserts,
        &mut pending_were_existing,
        &mut probe_queue,
        &mut added,
        &mut updated,
    )?;

    let removed = db.delete_missing(library_id, &keep_paths)? as u32;

    // Sidecar association always runs, including for unchanged media, so a
    // new .srt beside an untouched video is stored without waiting for mtime.
    for file in &files {
        let path_str = path_to_string(&file.path);
        let Some((item_id, _)) = db.item_mtime(library_id, &path_str)? else {
            continue;
        };
        match associate_sidecars(db, item_id, &file.path) {
            Ok(()) => {}
            Err(e) => tracing::warn!(
                item_id,
                path = %file.path.display(),
                error = %e,
                "sidecar association failed"
            ),
        }
    }

    let index_duration_ms = index_started.elapsed().as_millis() as u64;
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
        to_probe = probe_queue.len(),
        index_duration_ms,
        "index pass done"
    );

    let probe_started = Instant::now();
    run_probe_pool(db, job_id, probe_queue)?;
    let probe_duration_ms = probe_started.elapsed().as_millis() as u64;
    db.complete_scan_job(job_id, probe_duration_ms)?;

    tracing::info!(job_id, library_id, probe_duration_ms, "scan job completed");
    Ok(())
}

fn run_probe_pool(db: &Arc<Db>, job_id: i64, work: Vec<ProbeWork>) -> Result<(), String> {
    if work.is_empty() {
        return Ok(());
    }

    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(2, 16);
    let (tx, rx) = mpsc::channel::<ProbeWork>();
    let rx = Arc::new(Mutex::new(rx));
    let mut handles = Vec::with_capacity(workers);

    for i in 0..workers {
        let rx = Arc::clone(&rx);
        let db = Arc::clone(db);
        let handle = std::thread::Builder::new()
            .name(format!("ffprobe-{job_id}-{i}"))
            .spawn(move || {
                loop {
                    let next = {
                        let guard = match rx.lock() {
                            Ok(g) => g,
                            Err(_) => break,
                        };
                        guard.recv()
                    };
                    let Ok(item) = next else { break };
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
                            tracing::warn!(
                                path = %item.path.display(),
                                error = %e,
                                "ffprobe failed"
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
                                probe_status: "error".into(),
                                scan_error: Some(e),
                            }
                        }
                    };
                    let is_err = update.scan_error.is_some();
                    if let Err(e) = db.apply_probe_update(&update) {
                        tracing::warn!(item_id = item.item_id, error = %e, "probe update failed");
                    }
                    if let Err(e) = db.bump_scan_job_probe(job_id, is_err) {
                        tracing::warn!(job_id, error = %e, "probe counter bump failed");
                    }
                }
            })
            .map_err(|e| format!("spawn ffprobe worker: {e}"))?;
        handles.push(handle);
    }

    for item in work {
        tx.send(item)
            .map_err(|e| format!("enqueue probe work: {e}"))?;
    }
    drop(tx);

    for handle in handles {
        handle
            .join()
            .map_err(|_| "ffprobe worker panicked".to_string())?;
    }
    Ok(())
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn associate_sidecars(db: &Db, item_id: i64, video_path: &Path) -> Result<(), String> {
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
    use std::fs;
    use std::process::Command;

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
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let job_id = start_scan_job(Arc::clone(&db), lib.id).unwrap();
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
        let job2 = start_scan_job(Arc::clone(&db), lib.id).unwrap();
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
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let job_id = start_scan_job(Arc::clone(&db), lib.id).unwrap();
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
}
