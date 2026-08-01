//! Gate 2: every corpus media row's manifest `expect` matches `decide_playback`
//! under `BROWSER_V0` after a real ffprobe (or a structured probe failure).

use nightjar_core::{BROWSER_V0, decide_playback, method_from_manifest_expect};
use nightjar_scanner::ffprobe;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Deserialize)]
struct Manifest {
    files: Vec<ManifestFile>,
}

#[derive(Debug, Deserialize)]
struct ManifestFile {
    path: Option<String>,
    expect: String,
    #[serde(default)]
    commit: Option<bool>,
    #[serde(default)]
    status: Option<String>,
}

fn repo_testdata() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../testdata")
}

fn ensure_corpus(testdata: &Path) {
    let marker = testdata.join("files/h264_aac_mp4.mp4");
    if marker.is_file() {
        return;
    }
    let generate = testdata.join("generate.sh");
    let status = Command::new("bash")
        .arg(&generate)
        .status()
        .expect("spawn testdata/generate.sh");
    assert!(status.success(), "testdata/generate.sh failed");
}

#[test]
fn corpus_manifest_expects_match_decide_playback() {
    if std::env::var_os("NIGHTJAR_TEST_REQUIRE_FFMPEG").is_none()
        && Command::new("ffprobe")
            .arg("-version")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
    {
        eprintln!("skip: ffprobe not on PATH (set NIGHTJAR_TEST_REQUIRE_FFMPEG=1 in CI)");
        return;
    }

    let testdata = repo_testdata();
    ensure_corpus(&testdata);
    let manifest_path = testdata.join("manifest.json");
    let raw = std::fs::read_to_string(&manifest_path).expect("read manifest.json");
    let manifest: Manifest = serde_json::from_str(&raw).expect("parse manifest.json");

    let mut checked = 0usize;
    for row in &manifest.files {
        let Some(rel) = row.path.as_deref() else {
            assert_eq!(
                method_from_manifest_expect(&row.expect),
                None,
                "pathless row must not claim a method: {}",
                row.expect
            );
            continue;
        };
        if row.status.as_deref() == Some("pending source") {
            continue;
        }
        let Some(want) = method_from_manifest_expect(&row.expect) else {
            continue;
        };

        let path = testdata.join(rel);
        // Non-committed rows (large-*, Dolby kit): exercise when present, skip if absent.
        if row.commit == Some(false) && !path.is_file() {
            continue;
        }
        assert!(
            path.is_file(),
            "corpus file missing (run testdata/generate.sh): {}",
            path.display()
        );

        let path_str = path.to_string_lossy();
        let decision = match ffprobe(&path) {
            Ok(p) => decide_playback(
                &path_str,
                p.container.as_deref(),
                p.video_codec.as_deref(),
                p.audio_codec.as_deref(),
                p.audio_channels.map(|c| c as u32),
                p.height.and_then(|h| u32::try_from(h).ok()),
                p.video_bitrate_bps.and_then(|b| u64::try_from(b).ok()),
                p.hdr.as_deref(),
                None,
                "probed",
                &BROWSER_V0,
            ),
            Err(err) => decide_playback(
                &path_str,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(err.as_str()),
                "error",
                &BROWSER_V0,
            ),
        };

        assert_eq!(
            decision.method, want,
            "{}: expect {:?} from {:?}, got {:?} ({})",
            rel, want, row.expect, decision.method, decision.reason
        );
        checked += 1;
    }

    assert!(
        checked >= 20,
        "expected to exercise most corpus media rows, checked {checked}"
    );
}
