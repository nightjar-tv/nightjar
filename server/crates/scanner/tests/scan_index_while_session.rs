//! R4-STORAGE-BOUNDS item 3: scan/index work while an existing playback
//! session advances and seeks.
//!
//! The plan names four observables: viewer/seek results, scanner terminal
//! state, FFmpeg descendants, and cleanup. This test records all four against
//! the real `LibraryPool` and the real `HlsSessionRegistry`, so the session is
//! a real `ffmpeg` child, not a policy helper. It is in its own binary because
//! it spawns long-lived registry and pool worker threads and real child
//! processes; the `PATH`-mutating extract tests stay in theirs.
#![cfg(unix)]

use nightjar_core::VideoEncodePlan;
use nightjar_db::{Db, NewLibrary};
use nightjar_scanner::{LibraryPool, start_scan_job};
use nightjar_transcode::{
    AudioSelection, HlsSessionRegistry, PlaylistError, SessionMode, SubsStore,
    parse_time_keyed_segment_name,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The session source is long enough to hold several 2 s segments, so a seek
/// past the first segment starts a real new run instead of landing at zero. It
/// is also large enough that a re-encode session keeps its `ffmpeg` child live
/// across the concurrent scan, which is what lets the descendant be observed
/// under the composed workload.
const SOURCE_SECS: u64 = 30;
/// Extra files indexed by the concurrent scan. Enough that the scan is still
/// walking and probing after the first seek, without a long test. They are
/// copies of a small corpus fixture, so the scan stays on disk cheaply.
const SCAN_FILES: usize = 600;
const SEEK_MS: u64 = 15_000;

fn tool_present(tool: &str) -> bool {
    if std::env::var_os("NIGHTJAR_TEST_REQUIRE_FFMPEG").is_some() {
        return true;
    }
    Command::new(tool)
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn corpus_fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../testdata/files")
        .join(name)
}

fn build_long_source(dest: &Path) -> bool {
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=1920x1080:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440",
            "-t",
            &SOURCE_SECS.to_string(),
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
        ])
        .arg(dest)
        .status()
        .expect("spawn ffmpeg to build the long source");
    status.success()
}

fn wait_job(db: &Db, job_id: i64) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let job = db.get_scan_job(job_id).unwrap().unwrap();
        if job.state == "completed" || job.state == "failed" {
            return job.state;
        }
        assert!(Instant::now() < deadline, "job {job_id} did not finish");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn stereo() -> AudioSelection {
    AudioSelection {
        stream_index: None,
        channels: 2,
        channel_layout: Some("stereo".into()),
        max_channels: 2,
    }
}

fn first_segment(playlist: &[u8]) -> Option<String> {
    for line in String::from_utf8_lossy(playlist).lines() {
        let base = line.rsplit('/').next().unwrap_or(line);
        if parse_time_keyed_segment_name(base).is_some() {
            return Some(base.to_string());
        }
    }
    None
}

fn wait_playlist(hls: &HlsSessionRegistry, id: &str) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match hls.playlist(id) {
            Ok(bytes) if first_segment(&bytes).is_some() => return bytes,
            Ok(_) if Instant::now() < deadline => {}
            Err(PlaylistError::NotReady | PlaylistError::NotFound) if Instant::now() < deadline => {
            }
            Ok(bytes) => panic!(
                "playlist ready without time-keyed segments: {}",
                String::from_utf8_lossy(&bytes)
            ),
            Err(e) => panic!("playlist: {e:?}"),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_asset(hls: &HlsSessionRegistry, id: &str, name: &str) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match hls.asset(id, name, None) {
            Ok(bytes) => return bytes,
            Err(PlaylistError::NotReady) if Instant::now() < deadline => {}
            Err(e) => panic!("asset {name}: {e:?}"),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn seek_and_fetch(hls: &HlsSessionRegistry, id: &str, start_ms: u64) -> (u64, usize) {
    let view = hls.seek(id, start_ms).expect("seek");
    assert_ne!(view.run_id, 0, "a seek must name a live run");
    let playlist = wait_playlist(hls, id);
    let seg = first_segment(&playlist).expect("a listed segment");
    let bytes = wait_asset(hls, id, &seg);
    assert!(!bytes.is_empty(), "the seek's segment must carry bytes");
    (view.run_id, bytes.len())
}

/// Direct `ffmpeg` children of this test process. The registry spawns the
/// session encoder with `Command::new("ffmpeg")`, so this is the OS view of
/// the session's FFmpeg descendants, not the registry's handle accounting.
fn ffmpeg_descendants() -> usize {
    let out = Command::new("pgrep")
        .arg("-P")
        .arg(std::process::id().to_string())
        .arg("ffmpeg")
        .output()
        .expect("run pgrep to read the process table");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
}

/// A real session advances and seeks while a real scan indexes and probes new
/// files. The session's item stays probed, every scanned file is indexed, the
/// scan reaches `completed`, and teardown leaves no session dir or encoder.
#[test]
fn scan_index_runs_while_a_session_seeks_and_cleans_up() {
    if !tool_present("ffmpeg") {
        eprintln!("skip: ffmpeg not on PATH");
        return;
    }
    if !tool_present("ffprobe") {
        eprintln!("skip: ffprobe not on PATH");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let media = dir.path().join("media");
    fs::create_dir_all(&media).unwrap();
    let source = media.join("long.mkv");
    if !build_long_source(&source) {
        eprintln!("skip: could not build the long source fixture");
        return;
    }

    let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
    let subs = Arc::new(SubsStore::new(dir.path().join("subs")).unwrap());
    let pool = LibraryPool::spawn(Arc::clone(&db), Arc::clone(&subs));
    let lib = db
        .create_library(&NewLibrary {
            name: "t".into(),
            path: media.to_string_lossy().into_owned(),
            kind: "movies".into(),
        })
        .unwrap();

    // First scan probes the session source so a session can start on it.
    let first = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
    assert_eq!(wait_job(&db, first), "completed");
    let items = db.list_items(lib.id).unwrap();
    assert_eq!(items.len(), 1, "{items:?}");
    let item = items[0].clone();
    assert_eq!(item.probe_status, "probed");
    let duration_ms = item.duration_ms.expect("probed duration") as u64;

    // The real registry, with real ffmpeg children.
    let hls_root = dir.path().join("hls");
    let hls = HlsSessionRegistry::with_cap(
        hls_root.clone(),
        1,
        "libx264",
        Some(Arc::clone(&subs)),
        Some(Arc::clone(&db)),
    )
    .unwrap();
    let session = hls
        .start(
            item.id,
            &source,
            0,
            duration_ms,
            SessionMode::Transcode,
            stereo(),
            vec![],
            None,
            None,
            VideoEncodePlan::default(),
            None,
        )
        .expect("start a real transcode session");

    // Viewer baseline: the session lists a segment and serves its bytes.
    let baseline_playlist = wait_playlist(&hls, &session);
    let baseline_seg = first_segment(&baseline_playlist).expect("a listed segment");
    let baseline_bytes = wait_asset(&hls, &session, &baseline_seg);
    assert!(
        !baseline_bytes.is_empty(),
        "the viewer gets a first segment"
    );

    // New files make the second scan do real index and probe work. The scan
    // starts while the session is live, so the descendant below is observed
    // under the composed workload.
    let scan_fixture = corpus_fixture("h264_aac_mkv.mkv");
    assert!(scan_fixture.is_file(), "missing {}", scan_fixture.display());
    for i in 0..SCAN_FILES {
        fs::copy(&scan_fixture, media.join(format!("copy_{i:04}.mkv"))).unwrap();
    }
    let second = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
    let descendants_during_scan = ffmpeg_descendants();
    assert!(
        descendants_during_scan >= 1,
        "a live session must have an ffmpeg descendant while the scan runs; \
         observed {descendants_during_scan}"
    );

    // Advance and seek while the scan is still running.
    let (seek_run, seek_bytes) = seek_and_fetch(&hls, &session, SEEK_MS);
    let descendants_at_seek = ffmpeg_descendants();
    let state_at_seek = db.get_scan_job(second).unwrap().unwrap().state;
    eprintln!(
        "R4-STORAGE-BOUNDS: scan_state_at_seek={state_at_seek} \
         descendants_during_scan={descendants_during_scan} descendants_at_seek={descendants_at_seek} \
         seek_run={seek_run} seek_bytes={seek_bytes}"
    );
    assert_ne!(seek_run, 0);
    assert!(seek_bytes > 0);
    assert!(
        state_at_seek != "completed" && state_at_seek != "failed",
        "the scan must still be running when the viewer seeks; observed {state_at_seek}"
    );

    // Scanner terminal state.
    assert_eq!(wait_job(&db, second), "completed");
    let after = db.list_items(lib.id).unwrap();
    assert_eq!(
        after.len(),
        1 + SCAN_FILES,
        "the concurrent scan must index every file"
    );
    let session_item = db.get_item(item.id).unwrap().unwrap();
    assert_eq!(
        session_item.probe_status, "probed",
        "the scanned item must not be corrupted by the session"
    );
    eprintln!(
        "R4-STORAGE-BOUNDS: scan_terminal=completed items={} session_item_probe={}",
        after.len(),
        session_item.probe_status
    );

    // Cleanup: stop removes the session and its dir; no encoder survives.
    assert!(hls.stop(&session), "stop must remove the live session");
    assert_eq!(hls.kill_all_encoders(), 0, "no encoder may survive stop");
    assert_eq!(
        ffmpeg_descendants(),
        0,
        "no ffmpeg descendant may survive stop (observed {descendants_at_seek} at seek)"
    );
    assert!(
        fs::read_dir(&hls_root).unwrap().next().is_none(),
        "the session dir must be removed on stop"
    );
}
