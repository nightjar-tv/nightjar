//! ADR-0041 Decision 1-2 acceptance: probe classification never invokes the
//! ffmpeg subtitle-extract path.
//!
//! **These live in their own test binary on purpose.** They install a fake
//! `ffmpeg` by prepending a directory to the process `PATH`, and an environment
//! is per-process, so no test outside this file can be steered by it however
//! cargo schedules the binaries. They used to live in `src/lib.rs`, sharing a
//! process with every `require_ffprobe()` guard and every fixture builder that
//! resolves `ffprobe` or `ffmpeg` through `PATH` - see OPEN-DEFECTS entry 23.
//! No count here on purpose: the first draft of this line said nine and seven,
//! and both were wrong within the hour.
//!
//! The two tests here still write `PATH`, so `with_fake_ffmpeg` takes
//! `PATH_LOCK`. Any test added to this file must go through that helper.

use nightjar_db::{Db, NewLibrary};
use nightjar_scanner::{LibraryPool, start_scan_job};
use nightjar_transcode::SubsStore;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Serialises every `PATH` write in this binary. See the module note.
static PATH_LOCK: Mutex<()> = Mutex::new(());

fn test_pool(db: &Arc<Db>, data_dir: &Path) -> Arc<LibraryPool> {
    let subs = Arc::new(SubsStore::new(data_dir.join("subs")).unwrap());
    LibraryPool::spawn(Arc::clone(db), subs)
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

fn corpus_fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../testdata/files")
        .join(name)
}

fn copy_corpus_into(media: &Path, name: &str) -> PathBuf {
    let src = corpus_fixture(name);
    assert!(
        src.exists(),
        "corpus fixture missing (run testdata/generate.sh): {}",
        src.display()
    );
    let dest = media.join(name);
    fs::copy(&src, &dest).unwrap();
    dest
}

fn require_ffprobe() -> bool {
    if std::env::var_os("NIGHTJAR_TEST_REQUIRE_FFMPEG").is_some() {
        return true;
    }
    Command::new("ffprobe")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Install an executable fake `ffmpeg` that logs invocations referencing
/// `marker_path` and exits 1, so the "extract path is never invoked"
/// assertion is a process-count check (subtitle demux is the only ffmpeg
/// user in the scan pipeline). The marker filter is what keeps one test's
/// spawn out of the other's log, not the other way round: each test names its
/// own fixture in its own tempdir, so a lingering pool thread cannot write a
/// line the sibling then reads.
fn with_fake_ffmpeg<T>(log: &Path, marker_path: &Path, f: impl FnOnce() -> T) -> T {
    use std::os::unix::fs::PermissionsExt;
    let bin = log.parent().unwrap().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let fake = bin.join("ffmpeg");
    fs::write(
        &fake,
        format!(
            "#!/bin/sh\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    *{}*) echo invoked >> '{}' ;;\n  esac\ndone\nexit 1\n",
            marker_path.display(),
            log.display()
        ),
    )
    .unwrap();
    let mut perms = fs::metadata(&fake).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&fake, perms).unwrap();

    // **The injected directory holds exactly one entry, named `ffmpeg`.**
    //
    // `require_ffprobe` resolves `ffprobe` through `PATH` and it does so
    // *outside* the lock below, so while this directory is on `PATH` a
    // concurrent test's guard reads it. Today that is safe only because there
    // is no `ffprobe` in here to find. Drop one in — the obvious next move for
    // a test wanting a deterministic probe — and the guard resolves the fake
    // instead of the real binary. It is entry 23's own failure mode, inside
    // the file written to prevent it.
    //
    // The comment above used to be the whole defence, and a comment is not a
    // guard. This is the assertion the slice plan deferred.
    //
    // **It checks the directory, not a name.** `bin.join("ffprobe").exists()`
    // would pass for `ffprobe.exe`, for a `python3` someone added, for
    // anything at all that is not literally called `ffprobe` — and every one
    // of those is on `PATH` for the same window. What must hold is that this
    // directory contributes one executable and no other, so that is what is
    // read back.
    let entries: Vec<String> = fs::read_dir(&bin)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        entries,
        vec!["ffmpeg".to_string()],
        "the fake bin goes on PATH while another test may resolve ffprobe \
         through it, so it must hold exactly one entry named ffmpeg \
         (OPEN-DEFECTS entry 23). Found: {entries:?}"
    );

    // Held across the write, the closure and the restore. Complete rather than
    // a discipline: this helper is the only thing in the binary that *writes*
    // PATH, so there is no third party to forget the lock (OPEN-DEFECTS entry
    // 23).
    let _guard = PATH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let old = std::env::var_os("PATH");
    let new_path = match &old {
        Some(v) => format!("{}:{}", bin.display(), v.to_string_lossy()),
        None => bin.display().to_string(),
    };
    unsafe { std::env::set_var("PATH", new_path) };
    let result = f();
    match old {
        Some(v) => unsafe { std::env::set_var("PATH", v) },
        None => unsafe { std::env::remove_var("PATH") },
    }
    result
}

/// No subtitle streams, no sidecar → `none`, and the ffmpeg subtitle-extract
/// path is never invoked (ADR-0041 Decision 2 acceptance).
#[test]
fn probe_no_subtitles_classifies_none_without_extract() {
    if !require_ffprobe() {
        eprintln!("skip: ffprobe not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let media = dir.path().join("media");
    fs::create_dir_all(&media).unwrap();
    copy_corpus_into(&media, "h264_aac_mkv.mkv");
    let log = dir.path().join("ffmpeg-invocations.log");
    let marker = media.join("h264_aac_mkv.mkv");

    with_fake_ffmpeg(&log, &marker, || {
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
        wait_job(&db, job_id);
        // Give a wrongly-enqueued extract time to run if the wiring regressed.
        std::thread::sleep(Duration::from_millis(300));

        let items = db.list_items(lib.id).unwrap();
        assert_eq!(items.len(), 1, "{items:?}");
        let item = &items[0];
        assert_eq!(item.probe_status, "probed");
        assert_eq!(item.subtitle_status, "none");
        assert!(
            db.list_item_subtitle_tracks(item.id).unwrap().is_empty(),
            "no subtitle streams must persist no inventory rows"
        );
        let invoked = fs::read_to_string(&log).unwrap_or_default();
        assert!(
            invoked.is_empty(),
            "ffmpeg subtitle-extract path must never run during probe classification: {invoked}"
        );
    });
}

/// Image-only (PGS) → `none`, no extract job, and the persisted inventory
/// row carries `kind = image` (ADR-0041 Decision 1–2 acceptance).
#[test]
fn probe_image_only_classifies_none_and_persists_kind_image() {
    if !require_ffprobe() {
        eprintln!("skip: ffprobe not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let media = dir.path().join("media");
    fs::create_dir_all(&media).unwrap();
    copy_corpus_into(&media, "h264_aac_pgs_mkv.mkv");
    let log = dir.path().join("ffmpeg-invocations.log");
    let marker = media.join("h264_aac_pgs_mkv.mkv");

    with_fake_ffmpeg(&log, &marker, || {
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
        wait_job(&db, job_id);
        std::thread::sleep(Duration::from_millis(300));

        let items = db.list_items(lib.id).unwrap();
        assert_eq!(items.len(), 1, "{items:?}");
        let item = &items[0];
        assert_eq!(item.probe_status, "probed");
        assert_eq!(item.subtitle_status, "none");
        let tracks = db.list_item_subtitle_tracks(item.id).unwrap();
        assert_eq!(tracks.len(), 1, "{tracks:?}");
        assert_eq!(tracks[0].kind, "image");
        assert_eq!(tracks[0].codec, "hdmv_pgs_subtitle");
        assert_eq!(tracks[0].stream_index, 2);
        let invoked = fs::read_to_string(&log).unwrap_or_default();
        assert!(
            invoked.is_empty(),
            "image-only must not enqueue an extract job: {invoked}"
        );
    });
}
