//! Fixture-backed ADR-0024 selection (probes corpus, ranks in core).

use nightjar_core::{
    TrackCandidate, select_audio_track, select_subtitle_track, title_looks_forced, title_looks_sdh,
};
use nightjar_transcode::{list_audio_tracks, list_text_subtitles};
use std::path::PathBuf;

fn corpus(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../testdata/files")
        .join(name)
}

fn skip_without_ffmpeg() -> bool {
    std::process::Command::new("ffprobe")
        .arg("-version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
}

fn audio_cands(path: &std::path::Path) -> Vec<TrackCandidate> {
    list_audio_tracks(path)
        .expect("list audio")
        .into_iter()
        .map(|t| TrackCandidate {
            track_id: t.track_id(),
            language: t.language,
            title: t.title,
            is_default: t.is_default,
            is_forced: false,
            is_image: false,
            stream_index: t.stream_index,
        })
        .collect()
}

fn sub_cands(path: &std::path::Path) -> Vec<TrackCandidate> {
    list_text_subtitles(path)
        .expect("list subs")
        .into_iter()
        .map(|t| TrackCandidate {
            track_id: t.track_id(),
            language: t.language,
            title: t.title.clone(),
            is_default: t.is_default,
            is_forced: t.is_forced || title_looks_forced(t.title.as_deref()),
            is_image: false,
            stream_index: t.stream_index,
        })
        .collect()
}

#[test]
fn adv_fixture_english_not_arabic_or_sdh() {
    if skip_without_ffmpeg() {
        return;
    }
    let path = corpus("h264_aac_adv_track_select_mkv.mkv");
    if !path.is_file() {
        panic!("missing {}; run testdata/generate.sh", path.display());
    }
    let subs = sub_cands(&path);
    assert_eq!(subs.len(), 32, "expected 32 soft subs");
    assert_eq!(subs[0].language.as_deref(), Some("ar"));
    assert!(!subs.iter().any(|s| s.is_default));

    let sel = select_subtitle_track(&subs, Some("en"), Some("en"));
    let id = sel.track_id.expect(&sel.reason);
    let chosen = subs.iter().find(|s| s.track_id == id).expect("chosen");
    assert_eq!(chosen.language.as_deref(), Some("en"));
    assert!(!title_looks_sdh(chosen.title.as_deref()), "{chosen:?}");
    assert!(
        sel.reason.contains("matched your preference"),
        "{}",
        sel.reason
    );

    let miss = select_subtitle_track(&subs, Some("xx"), Some("en"));
    assert_eq!(miss.track_id, None, "{miss:?}");
}

#[test]
fn adv_fixture_audio_main_not_commentary() {
    if skip_without_ffmpeg() {
        return;
    }
    let path = corpus("h264_aac_adv_track_select_mkv.mkv");
    if !path.is_file() {
        panic!("missing {}; run testdata/generate.sh", path.display());
    }
    let audio = audio_cands(&path);
    assert_eq!(audio.len(), 2);
    let sel = select_audio_track(&audio, Some("en"));
    let id = sel
        .track_id
        .as_deref()
        .unwrap_or_else(|| panic!("{}", sel.reason));
    let chosen = audio.iter().find(|a| a.track_id == id).expect("chosen");
    assert_eq!(chosen.title.as_deref(), Some("Main"), "{sel:?} {chosen:?}");
}

#[test]
fn forced_fixture_foreign_audio_picks_forced() {
    if skip_without_ffmpeg() {
        return;
    }
    let path = corpus("h264_aac_forced_track_select_mkv.mkv");
    if !path.is_file() {
        panic!("missing {}; run testdata/generate.sh", path.display());
    }
    let audio = audio_cands(&path);
    let subs = sub_cands(&path);
    assert_eq!(audio[0].language.as_deref(), Some("ja"));
    assert!(subs.iter().any(|s| s.is_forced));

    let foreign = select_subtitle_track(&subs, Some("en"), Some("ja"));
    let id = foreign.track_id.expect(&foreign.reason);
    let chosen = subs.iter().find(|s| s.track_id == id).expect("chosen");
    assert!(chosen.is_forced, "{chosen:?}");
    assert!(foreign.reason.contains("forced"), "{}", foreign.reason);

    let matching = select_subtitle_track(&subs, Some("ja"), Some("ja"));
    assert_eq!(matching.track_id, None, "{matching:?}");
}
