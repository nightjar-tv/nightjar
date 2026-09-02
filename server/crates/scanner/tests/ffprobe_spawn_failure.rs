//! A failed `ffprobe` spawn must read differently from a failed `ffprobe` run.
//!
//! **This is its own test binary on purpose.** It edits the process `PATH`, and
//! an environment is per-process, so nothing else can be steered by it however
//! cargo schedules the binaries. It used to live in `src/probe.rs`, where it
//! shared a process with tests that resolve `ffprobe` and `ffmpeg` through
//! `PATH` — see OPEN-DEFECTS entry 23.
//!
//! Keep this file to one test. A second one here would race the first, and the
//! move would have bought nothing.

use nightjar_scanner::ffprobe;
use std::path::Path;

#[test]
fn spawn_failure_message_is_distinct() {
    // Point PATH away so the spawn fails distinctly from a process exit.
    // SAFETY: the only test in this binary, so no thread can observe the gap.
    let err = {
        let old = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", "/var/empty-nightjar-no-ffprobe") };
        let r = ffprobe(Path::new("/tmp/x.mkv"), None);
        match old {
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }
        r.unwrap_err()
    };
    assert!(
        err.starts_with("spawn ffprobe"),
        "expected spawn message, got {err}"
    );
    assert!(!err.starts_with("ffprobe failed"));
}
