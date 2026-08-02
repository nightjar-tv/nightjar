//! Gate 2: every corpus media row's manifest `expect` matches `decide_playback`
//! under `BROWSER_V0` after a real ffprobe (or a structured probe failure).
//!
//! HDR/DV axis rows are also asserted against `BROWSER_V0`, `AETHER_V0`, and a
//! wide-codec no-HDR profile (ADR-0022). DV profiles are separate rows (P4, P5,
//! P7 MEL, P7 FEL, P8.1, P8.4). Method and rendered outcome are distinct:
//! Apple may DirectPlay a file while rendering HDR10 fallback (P7) or true
//! Dolby Vision (P5 / P8.x when the device allows).
//!
//! `provisional` means unmeasured, not "probably fine" (Rule 4.8). Every
//! `AETHER_V0` row is provisional: `t1_profile_counts.py` scores decide
//! *method* against the codec/container set only (no AetherEngine load, no
//! play). It is not a per-DV-profile render proof.
//!
//! `BROWSER_V0` / `NO_HDR_WIDE` rows assert routing (method + that a tonemap
//! path was selected), not colour fidelity. Profile 5 is refuse-with-reason
//! (no tonemap attempt); other DV rows may still select tonemap.
//!
//! Wrong current behaviour is encoded as the correct expectation with
//! `HdrCase.ignore` set (row skipped), not as an assertion of the bug.

use nightjar_core::{
    decide_playback, method_from_manifest_expect, video_encode_plan, ClientCapabilityProfile,
    HdrCapability, PlaybackMethod, AETHER_V0, BROWSER_V0,
};
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

/// Wide codecs like `AETHER_V0`, but `hdr: None` — isolates the HDR ceiling.
const NO_HDR_WIDE: ClientCapabilityProfile = ClientCapabilityProfile {
    hdr: HdrCapability::None,
    ..AETHER_V0
};

/// What the client is expected to put on screen, separate from playback method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenderedOutcome {
    DolbyVision,
    Hdr10Fallback,
    TonemappedSdr,
}

impl RenderedOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::DolbyVision => "dolbyVision",
            Self::Hdr10Fallback => "hdr10Fallback",
            Self::TonemappedSdr => "tonemappedSdr",
        }
    }
}

#[derive(Clone, Copy)]
struct ProfileExpect {
    method: PlaybackMethod,
    /// Absent for plain SDR (no HDR path).
    rendered: Option<RenderedOutcome>,
    /// Unmeasured — not "probably fine" (Rule 4.8). Server `decide_playback`
    /// method is still checked when this is true.
    provisional: bool,
}

#[derive(Clone, Copy)]
struct HdrCase {
    /// Short label for the report table (e.g. `P7 MEL`, `HDR10`).
    label: &'static str,
    /// Path relative to `testdata/`.
    rel: &'static str,
    /// When true, absent file skips (fetched / kit-local only).
    optional: bool,
    browser: ProfileExpect,
    aether: ProfileExpect,
    no_hdr: ProfileExpect,
    /// Source is HDR for encode-plan / refuse-with-reason checks.
    hdr_source: bool,
    /// Set when the correct expectation differs from today's engine; test is ignored.
    ignore: Option<&'static str>,
}

const fn verified(method: PlaybackMethod, rendered: Option<RenderedOutcome>) -> ProfileExpect {
    ProfileExpect {
        method,
        rendered,
        provisional: false,
    }
}

/// AETHER_V0 row: method is the server floor; render claim is provisional.
const fn aether_prov(method: PlaybackMethod, rendered: Option<RenderedOutcome>) -> ProfileExpect {
    ProfileExpect {
        method,
        rendered,
        provisional: true,
    }
}

const TM: Option<RenderedOutcome> = Some(RenderedOutcome::TonemappedSdr);
const DV: Option<RenderedOutcome> = Some(RenderedOutcome::DolbyVision);
const H10: Option<RenderedOutcome> = Some(RenderedOutcome::Hdr10Fallback);

/// ADR-0022 expectations for the HDR/DV corpus axis. DV profiles are one row
/// each — method and rendered outcome are not collapsed.
const HDR_AXIS: &[HdrCase] = &[
    // --- controls ---
    HdrCase {
        label: "SDR BT.709",
        rel: "files/h264_sdr_bt709_mp4.mp4",
        optional: false,
        browser: verified(PlaybackMethod::DirectPlay, None),
        aether: aether_prov(PlaybackMethod::DirectPlay, None),
        no_hdr: verified(PlaybackMethod::DirectPlay, None),
        hdr_source: false,
        ignore: None,
    },
    HdrCase {
        label: "HDR10",
        rel: "files/hevc_hdr10_mp4.mp4",
        optional: false,
        browser: verified(PlaybackMethod::Transcode, TM),
        aether: aether_prov(PlaybackMethod::DirectPlay, H10),
        no_hdr: verified(PlaybackMethod::Transcode, TM),
        hdr_source: true,
        ignore: None,
    },
    HdrCase {
        label: "HLG",
        rel: "files/hevc_hlg_mp4.mp4",
        optional: false,
        browser: verified(PlaybackMethod::Transcode, TM),
        // HLG passthrough is not one of the DV render tokens; AETHER DP of
        // plain HLG is still provisional (unmeasured on device).
        aether: aether_prov(PlaybackMethod::DirectPlay, None),
        no_hdr: verified(PlaybackMethod::Transcode, TM),
        hdr_source: true,
        ignore: None,
    },
    HdrCase {
        label: "HDR10+",
        rel: "files/hevc_hdr10plus_mp4.mp4",
        optional: false,
        browser: verified(PlaybackMethod::Transcode, TM),
        aether: aether_prov(PlaybackMethod::DirectPlay, H10),
        no_hdr: verified(PlaybackMethod::Transcode, TM),
        hdr_source: true,
        ignore: None,
    },
    // --- Dolby Vision per profile ---
    HdrCase {
        label: "P4",
        rel: "files/dolby-vision-makemkv/P4_LG_Dolby_Trailer_4K_Demo.mkv",
        optional: true,
        browser: verified(PlaybackMethod::Transcode, TM),
        // SDR-compatible BL (compat id 2). Whether Apple engages DV for P4 is
        // unmeasured; method DirectPlay is the server floor only.
        aether: aether_prov(PlaybackMethod::DirectPlay, DV),
        no_hdr: verified(PlaybackMethod::Transcode, TM),
        hdr_source: true,
        ignore: None,
    },
    HdrCase {
        label: "P5",
        rel: "files/dolby-vision-makemkv/P5_Dolby_Amaze.mkv",
        optional: true,
        browser: verified(PlaybackMethod::Transcode, TM),
        aether: aether_prov(PlaybackMethod::DirectPlay, DV),
        no_hdr: verified(PlaybackMethod::Transcode, TM),
        hdr_source: true,
        ignore: None,
    },
    HdrCase {
        label: "P7 MEL",
        rel: "files/dolby-vision-makemkv/P7_MEL_GIJoe_The_Rise_of_Cobra.mkv",
        optional: true,
        browser: verified(PlaybackMethod::Transcode, TM),
        // Apple has no P7 decoder (Dolby licence). DirectPlay serves the
        // HDR10 base; EL is ignored — by design, not a Nightjar bug.
        aether: aether_prov(PlaybackMethod::DirectPlay, H10),
        no_hdr: verified(PlaybackMethod::Transcode, TM),
        hdr_source: true,
        ignore: None,
    },
    HdrCase {
        label: "P7 FEL",
        rel: "files/dolby-vision-makemkv/P7_FEL_GIJoe_The_Rise_of_Cobra.mkv",
        optional: true,
        browser: verified(PlaybackMethod::Transcode, TM),
        aether: aether_prov(PlaybackMethod::DirectPlay, H10),
        no_hdr: verified(PlaybackMethod::Transcode, TM),
        hdr_source: true,
        ignore: None,
    },
    HdrCase {
        label: "P8.1",
        rel: "files/dolby-vision-makemkv/P81_GlassBlowing2_3840x2160@59_94fps_15200kbps.mkv",
        optional: true,
        browser: verified(PlaybackMethod::Transcode, TM),
        // Real DV when the device generation / tvOS allow; otherwise HDR10 BL.
        // Listed as dolbyVision provisionally — support varies.
        aether: aether_prov(PlaybackMethod::DirectPlay, DV),
        no_hdr: verified(PlaybackMethod::Transcode, TM),
        hdr_source: true,
        ignore: None,
    },
    HdrCase {
        label: "P8.1 pair mkv",
        rel: "files/hevc_dv_p81_pair.mkv",
        optional: false,
        browser: verified(PlaybackMethod::Transcode, TM),
        aether: aether_prov(PlaybackMethod::DirectPlay, DV),
        no_hdr: verified(PlaybackMethod::Transcode, TM),
        hdr_source: true,
        ignore: None,
    },
    HdrCase {
        label: "P8.1 pair mp4",
        rel: "files/hevc_dv_p81_pair.mp4",
        optional: false,
        browser: verified(PlaybackMethod::Transcode, TM),
        aether: aether_prov(PlaybackMethod::DirectPlay, DV),
        no_hdr: verified(PlaybackMethod::Transcode, TM),
        hdr_source: true,
        ignore: None,
    },
    HdrCase {
        label: "P8.4",
        rel: "files/hevc_dv_p84_hlg_mkv.mkv",
        optional: false,
        browser: verified(PlaybackMethod::Transcode, TM),
        aether: aether_prov(PlaybackMethod::DirectPlay, DV),
        no_hdr: verified(PlaybackMethod::Transcode, TM),
        hdr_source: true,
        ignore: None,
    },
    HdrCase {
        label: "P8.4 mp4",
        rel: "files/hevc_dv_p84_hlg_mp4.mp4",
        optional: false,
        browser: verified(PlaybackMethod::Transcode, TM),
        aether: aether_prov(PlaybackMethod::DirectPlay, DV),
        no_hdr: verified(PlaybackMethod::Transcode, TM),
        hdr_source: true,
        ignore: None,
    },
];

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

fn ffprobe_missing() -> bool {
    std::env::var_os("NIGHTJAR_TEST_REQUIRE_FFMPEG").is_none()
        && Command::new("ffprobe")
            .arg("-version")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
}

fn decide_for(
    path: &Path,
    profile: &ClientCapabilityProfile,
    tonemap_available: bool,
) -> nightjar_core::PlaybackDecision {
    let path_str = path.to_string_lossy();
    match ffprobe(path) {
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
            profile,
            tonemap_available,
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
            profile,
            tonemap_available,
        ),
    }
}

fn assert_profile(
    case: &HdrCase,
    profile_id: &str,
    want: ProfileExpect,
    got: &nightjar_core::PlaybackDecision,
) {
    if want.provisional {
        eprintln!(
            "provisional {profile_id} {}: method {:?} rendered {:?} — \
             AETHER_V0 unmeasured for per-profile HDR/DV render \
             (t1_profile_counts.py / ADR-0021; ADR-0022; Rule 4.8)",
            case.label,
            want.method,
            want.rendered.map(RenderedOutcome::as_str)
        );
    }
    assert_eq!(
        got.method, want.method,
        "{} {}: got {:?} ({}) — provisional={}",
        case.rel, profile_id, got.method, got.reason, want.provisional
    );
}

#[test]
fn corpus_manifest_expects_match_decide_playback() {
    if ffprobe_missing() {
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
        // Non-committed rows (large-*, Dolby kit / MakeMKV): exercise when present.
        if row.commit == Some(false) && !path.is_file() {
            continue;
        }
        assert!(
            path.is_file(),
            "corpus file missing (run testdata/generate.sh): {}",
            path.display()
        );

        let decision = decide_for(&path, &BROWSER_V0, true);
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

#[test]
fn hdr_axis_decide_table_browser_aether_no_hdr() {
    if ffprobe_missing() {
        eprintln!("skip: ffprobe not on PATH (set NIGHTJAR_TEST_REQUIRE_FFMPEG=1 in CI)");
        return;
    }

    let testdata = repo_testdata();
    ensure_corpus(&testdata);

    let mut checked = 0usize;
    for case in HDR_AXIS {
        if let Some(reason) = case.ignore {
            eprintln!("ignore {}: {reason}", case.rel);
            continue;
        }
        let path = testdata.join(case.rel);
        if case.optional && !path.is_file() {
            eprintln!("skip {}: optional corpus file absent", case.rel);
            continue;
        }
        assert!(
            path.is_file(),
            "HDR axis file missing (run testdata/generate.sh): {}",
            path.display()
        );

        let browser = decide_for(&path, &BROWSER_V0, true);
        let aether = decide_for(&path, &AETHER_V0, true);
        let no_hdr = decide_for(&path, &NO_HDR_WIDE, true);

        assert_profile(case, "BROWSER_V0", case.browser, &browser);
        assert_profile(case, "AETHER_V0", case.aether, &aether);
        assert_profile(case, "NO_HDR_WIDE", case.no_hdr, &no_hdr);

        if case.hdr_source {
            let probe = ffprobe(&path).expect("ffprobe HDR axis file");
            let plan = video_encode_plan(
                probe.height.and_then(|h| u32::try_from(h).ok()),
                probe.video_bitrate_bps.and_then(|b| u64::try_from(b).ok()),
                probe.hdr.as_deref(),
                &BROWSER_V0,
            );
            let is_p5 = probe.hdr.as_deref() == Some("dolby_vision_p5");
            if is_p5 {
                assert!(
                    !plan.tone_map,
                    "{}: P5 encode plan must not tonemap",
                    case.rel
                );
                assert_eq!(
                    probe.hdr.as_deref(),
                    Some("dolby_vision_p5"),
                    "{}: probe must store dolby_vision_p5",
                    case.rel
                );
            } else {
                assert!(
                    plan.tone_map,
                    "{}: encode plan must tone-map HDR sources",
                    case.rel
                );
            }

            // refuse-with-reason: session start can 415 on the named gap.
            if case.browser.method == PlaybackMethod::Transcode
                || case.no_hdr.method == PlaybackMethod::Transcode
            {
                let refused = decide_for(&path, &NO_HDR_WIDE, false);
                assert_eq!(refused.method, PlaybackMethod::Transcode);
                if is_p5 {
                    assert!(
                        refused.reason.contains("Profile 5"),
                        "{}: expected P5 refuse-with-reason, got {}",
                        case.rel,
                        refused.reason
                    );
                } else {
                    assert!(
                        refused.reason.contains("lacks zscale"),
                        "{}: expected refuse-with-reason naming zscale, got {}",
                        case.rel,
                        refused.reason
                    );
                }
            }
        }

        checked += 1;
    }

    assert!(
        checked >= 4,
        "expected to exercise HDR controls at minimum, checked {checked}"
    );
}
