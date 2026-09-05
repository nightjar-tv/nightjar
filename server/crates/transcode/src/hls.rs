//! HLS playback sessions (ADR-0007). A session either stream-copies the
//! source (remux) or re-encodes it (transcode); the two differ by
//! [`SessionMode`] and nothing else (ADR-0011).
//!
//! ADR-0020: producer-owned boundaries. Each encode/copy run writes into
//! `v<rung>/run_<n>/`; each rung has a time-keyed map
//! (`seg_<ms:011>.m4s`) as its serve truth. Playlists are assembled from that
//! map (EVENT while cooking, ENDLIST at run EOF) under a fresh URI per run.
//! Clients seek via
//! `POST /sessions/{id}/seek?startMs=` → fresh `playlistUrl`, not by
//! mutating one VOD or poking segment URIs.
//!
//! Fill-forward: FFmpeg starts at the play land ([`ENCODE_LEAD_SEGMENTS`] is
//! 0 under producer-truth playlists; the old Safari dig-back lead fitted the
//! synthetic full-title VOD and does not carry). Mapped segments from prior
//! runs stay on disk so scrub-back is a plain file serve. Per-run dirs count
//! against [`SESSION_RUN_CACHE_BUDGET_BYTES`]; oldest finished runs evict.

use super::audio::stereo_downmix_filter;
use super::subs::{
    BurnInKind, BurnInSelection, SessionSubInput, SubsStore, concat_webvtt_segments,
    extract_embedded_ass, prepare_session_subtitles, slice_webvtt, webvtt_max_cue_end_ms,
};
use crate::hls_grid::{GridCadence, grid_cadence_ms};
use crate::hls_master::VideoRung;
#[cfg(target_os = "linux")]
use crate::hls_memory::read_child_rss;
use crate::hls_memory::{EncoderMemory, encoder_memory_from_available, read_available_memory};
use crate::hls_policy;
use crate::hls_policy::{
    CoalesceDesire, PendingWaiterAction, SegmentMissAction, classify_restart_desire,
    coalesce_preempt_before_land, decide_segment_miss, digback_behind_committed, disable_preempt,
    no_fill_release_for_new_land, pending_restart_due, pending_waiter_action,
    prefetch_advances_pending, restart_spawn_gap, segment_miss_unreachable,
    serve_ok_after_pending_apply,
};
use nightjar_core::VideoEncodePlan;
use nightjar_db::Db;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const SINGLE_VIDEO_RUNG: VideoRung = VideoRung::SingleVideo;
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const REAPER_TICK: Duration = Duration::from_secs(5);
/// Per-session on-disk budget shared across every rung's run dirs (ADR-0020
/// §12, ADR-0051 amendment 2). Oldest finished (non-current) runs are evicted
/// first when exceeded.
const SESSION_RUN_CACHE_BUDGET_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// EOF this far short of probed duration → record usable extent (damaged).
const USABLE_SHORTFALL_MS: u64 = 30_000;
/// Locked HLS segment duration for **transcode** force-IDR / subtitle VTT
/// grid (ADR-0008 / ADR-0010). Copy segment durations come from the producer.
pub(crate) const SEGMENT_MS: u64 = 2000;
/// One copy/remux window. **Its own constant, not a multiple of
/// [`SEGMENT_MS`].**
///
/// They are different kinds of thing: `SEGMENT_MS` is an encoder IDR cadence,
/// this is how much media one copy window holds. Deriving it would let a
/// change to the IDR cadence silently move copy's scrub granularity.
///
/// **20 s is measured, not chosen.** `stay-ahead-vt-2026-08-20.md` §S8, a
/// human trial on a real title through the spike origin: a 2 s grid gave one
/// FFmpeg per skipped cue, a 4 s worst wait, short GOPs named as later URIs,
/// and on the second seek a video stall with audio that went robotic and
/// stayed robotic. One MPEG-TS per 20 s window was "stable" — 67 windows
/// listed, 20 on disk, last first byte 574 ms. §S8b confirmed it on iPhone
/// with `-c:a copy`.
///
/// **Scrub granularity is the window, not 2 s. Fine-grained seek stays
/// transcode.**
const COPY_WINDOW_MS: u64 = 20_000;
/// How long a segment or init fetch may block before returning 503. Mid-title
/// hardware transcodes on a NAS library can exceed 15s (dogfood: ~16s to
/// seg1098 after a Chrome seek on Up 1080p).
const SEGMENT_WAIT: Duration = Duration::from_secs(30);
const SEGMENT_POLL: Duration = Duration::from_millis(100);
/// Media seconds a session's encoder may run ahead of the playhead before it
/// is suspended, and the lead at which it is resumed (ADR-0050 §2). Product
/// constants: the knee replicated across two encoders, two operating systems
/// and both run orders. Below it a session pays half a second of latency for
/// no saving; above it, encode for no gain. Not settings (Rule 4.12).
const LEAD_TARGET_MS: u64 = 40_000;
const LEAD_FLOOR_MS: u64 = 20_000;
// The band must have room in it. Equal values would suspend and resume on
// adjacent ticks for a session's whole life.
const _: () = assert!(LEAD_FLOOR_MS < LEAD_TARGET_MS);
/// How often the throttle re-reads every session's lead. Fine enough that a
/// resumed encoder is producing again well inside one segment.
const THROTTLE_TICK: Duration = Duration::from_millis(250);
/// How long an encoder a seek replaced is kept before being terminated
/// (ADR-0050 §5).
///
/// Destroying an encoder context contends with creating one, so tearing the
/// old one down while the new one starts is paid for at the seek: 2187 ms for
/// kill-then-start, 1761 ms for start-then-kill, against 976-1132 ms once the
/// teardown is clear of the new encoder. The delay has to outlast the seek's
/// own first byte, which tops out near 2.4 s; at 2 s it still overlapped and
/// cost 462 ms. Five seconds is clear of that, not a tuned optimum.
const REAP_AFTER: Duration = Duration::from_secs(5);
// Useless if it does not outlast the seek it follows.
const _: () = assert!(REAP_AFTER.as_millis() > 2400);
/// Still justified under producer-truth: EVENT playlists list segments the
/// producer is still writing; Safari prefetches ~two past the on-disk
/// frontier. Those GETs Wait (cook), they do not scrub. Far scrub is
/// `POST /seek`, not a segment miss past this band.
pub(crate) const CATCH_UP_SEGMENTS: u64 = 2;
/// Safari retried refused segments at one-second intervals. A two-second floor
/// prevents adjacent prefetch misses from repeatedly moving the encode window.
pub(crate) const RESTART_MIN_INTERVAL: Duration = Duration::from_secs(2);
/// ADR-0023 §9.3: how long a seek waits for an in-flight keyframe-map build
/// before falling through to the §8 `-ss` path. ~1.5 s against the 1,545 ms
/// measured Matroska worst case (§2); waiting ~600 ms beats the 7.1 s cold
/// `-ss` baseline by an order of magnitude.
const MAP_BUILD_WAIT: Duration = Duration::from_millis(1500);
/// Poll interval while waiting for the build to land (§9.3).
const MAP_BUILD_WAIT_POLL: Duration = Duration::from_millis(50);
/// After the latest scrub intent while the prior encode has already landed,
/// wait this quiet period before killing FFmpeg. Rapid scrubs only update
/// the pending target (dogfood: three `seek restart` lines in ~9s; the last
/// fired 45ms after the previous `first_segment_ready`).
pub(crate) const RESTART_COALESCE_QUIET: Duration = Duration::from_millis(400);
/// Deleted under ADR-0020. Was the dig-back band for unlisted-but-requested
/// segments on the synthetic full-title VOD. Producer-truth playlists do not
/// list those URIs; far scrub is `POST /seek`. Kept as 0 so coalesce "far"
/// means any different pending land (see [`coalesce_preempt_before_land`]).
pub(crate) const ALIGN_BEHIND_SEGMENTS: u64 = 0;
/// Encode lead before play land. **0** under ADR-0020: the value 8 existed
/// so Safari dig-back behind `#EXT-X-START` on the synthetic full-title VOD
/// still hit lead-in files. That playlist is gone; window-relative START is
/// 0 and clients seek via the session API. Do not carry 8 as "still
/// meaningful as time." Override `NIGHTJAR_ENCODE_LEAD_SEGMENTS` only for
/// local experiments — not a shipped config surface.
pub(crate) const ENCODE_LEAD_SEGMENTS: u64 = 0;

/// Runtime lead-in. Default [`ENCODE_LEAD_SEGMENTS`].
fn encode_lead_segments() -> u64 {
    std::env::var("NIGHTJAR_ENCODE_LEAD_SEGMENTS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(ENCODE_LEAD_SEGMENTS)
}

/// Sum of bytes under every rung directory in a session cache directory.
fn session_disk_bytes(session: &Session) -> u64 {
    let mut total = 0u64;
    for rung in session.segment_maps.keys().copied() {
        let rung_dir = session.dir.join(crate::hls_segment_map::rung_rel_dir(rung));
        total = total.saturating_add(dir_tree_bytes(&rung_dir));
    }
    total
}

#[derive(Debug)]
pub enum StartSessionError {
    /// Measured memory reserve or the operator's explicit encoder cap refused
    /// this newcomer.
    AdmissionRefused,
    Spawn(String),
}

#[derive(Debug)]
pub enum PlaylistError {
    NotFound,
    NotReady,
    /// Abandoned / superseded miss hold reached [`IDLE_TIMEOUT`] while the
    /// session still exists. Mapped to empty HTTP 204 (ADR-0011 §7): not
    /// 4xx/5xx so Safari does not see an application-level media failure
    /// after a long hold. Session teardown uses [`NotFound`].
    AbandonedHoldEnded,
    Failed(String),
}

/// What FFmpeg does with the source for this session (ADR-0011).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMode {
    /// Stream copy: codecs already play, only the container changes.
    Copy,
    /// Re-encode to H.264 + AAC.
    Transcode,
}

/// Which audio track a session maps, and the ceiling it must fit (ADR-0012).
/// Switching tracks is a new session, so this never changes in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSelection {
    /// Absolute ffprobe stream index; `None` maps the first audio stream.
    pub stream_index: Option<u32>,
    /// Channel count of the selected track.
    pub channels: u32,
    /// ffprobe `channel_layout` when present (`5.1`, `6.0`, …).
    pub channel_layout: Option<String>,
    /// Client ceiling from the capability profile.
    pub max_channels: u32,
}

impl AudioSelection {
    pub fn needs_downmix(&self) -> bool {
        self.channels > self.max_channels
    }
}

pub struct HlsSessionRegistry {
    root: PathBuf,
    /// Explicit operator escape hatch. Normal admission has no configured cap
    /// and is governed by measured memory (ADR-0050 §7-§8, Rule 4.12).
    max_encoders: Option<usize>,
    /// The registry's worker threads own it for the process lifetime, so this
    /// high-water mark never falls until restart. One unusually large 4K
    /// software encode can therefore over-project for the rest of the run;
    /// that conservative cost is assigned to the newcomer (ADR-0050 §7).
    encoder_rss_high_water_bytes: AtomicU64,
    /// Session-shaped encode leg from ADR-0009 probe (shared with startup verify).
    encode_leg: crate::EncodeLeg,
    /// Library subtitle store for piggyback publish (ADR-0041 Decision 7).
    subs: Option<Arc<SubsStore>>,
    /// Item store for the piggyback `ready` flip at run EOF.
    db: Option<Arc<Db>>,
    /// ADR-0023 §9.3: whether a keyframe-map build for an item is queued or
    /// in flight. The API wires it to the library pool; a seek consults it
    /// before waiting, bounded, for a build a consumer already triggered.
    map_build_in_flight: Mutex<Option<Arc<MapBuildInFlight>>>,
    next_id: AtomicU64,
    sessions: Mutex<HashMap<String, Session>>,
}

/// ADR-0041 Decision 7: a piggyback target for a session on an `eligible`
/// item. When set, the session's ffmpeg gains `-map 0:s?` + `-c:s webvtt`
/// (a WebVTT side output alongside the video/audio maps); on a natural run
/// EOF that started at title 0 the assembled WebVTT is published to
/// `{subs}/{itemId}/{track_id}.vtt` and the item flips to `ready`. A killed
/// or offset run never publishes and leaves the item `eligible` for a later
/// pass (standalone or another piggyback) to finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiggybackExtract {
    /// Library track id (`e{stream_index}`) the side output is published as.
    pub track_id: String,
}

/// Serveable text track snapshot taken at session create (ADR-0010 / ADR-0013).
/// Mid-session sidecar additions do not appear until the next session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsSubtitleTrack {
    pub track_id: String,
    pub language: Option<String>,
    pub name: String,
    pub is_default: bool,
    pub forced: bool,
    pub sdh: bool,
    /// Item id (for logging / future use).
    pub item_id: i64,
    /// Embedded stream index, or None for a sidecar.
    pub stream_index: Option<u32>,
    /// Sidecar file path when `stream_index` is None.
    pub sidecar_path: Option<PathBuf>,
    /// Source codec / sidecar format (subrip, srt, vtt, …).
    pub codec: String,
    /// When set, 2s HLS segments are sliced from this on-disk item VTT
    /// (ready extract). No session demux. When None, session-inline demux
    /// writes `subs/{trackId}/full.vtt` instead.
    pub item_vtt_path: Option<PathBuf>,
}

struct Session {
    item_id: i64,
    src: PathBuf,
    dir: PathBuf,
    /// On-disk budget for this session's run dirs (ADR-0020 §12). A field
    /// rather than an environment read so a test names its own budget without
    /// steering every other test in the binary (OPEN-DEFECTS entry 22).
    run_cache_budget_bytes: u64,
    mode: SessionMode,
    audio: AudioSelection,
    /// Burn-in baked into this session's encode (ADR-0018). Seek restarts
    /// keep the same selection; switching burn-in is a fresh POST.
    burn_in: Option<BurnInSelection>,
    /// ADR-0022 scale / bitrate / tone-map knobs for Transcode mode.
    encode_plan: VideoEncodePlan,
    /// Keyframe map snapshot and the virtual file bound from it (ADR-0023).
    map_binding: MapBinding,
    /// Session-shaped encode leg (ADR-0009). Future fallback updates this field.
    encode_leg: crate::EncodeLeg,
    /// Encoder name for API / logs (`encode_leg.encoder`).
    video_encoder: String,
    /// Encode window start for the current run (`-ss` / lead-in).
    start_ms: u64,
    /// Client land point / seek intent (title-absolute).
    play_start_ms: u64,
    /// Producer-observed land (first mapped segment start after the latest
    /// run began). Exposed on the session API (ADR-0020).
    landed_ms: u64,
    /// Lazy usable extent when EOF is materially short of [`Self::duration_ms`].
    usable_extent_ms: Option<u64>,
    duration_ms: u64,
    /// One title-time-keyed segment map per video rung (ADR-0051 amendment 2).
    segment_maps: HashMap<VideoRung, crate::hls_segment_map::SegmentMap>,
    /// One live encoder and its run allocation per offered video rung
    /// (ADR-0051 decision 5). Keeping these facts together prevents a rung's
    /// child and run ids from being advanced independently.
    encoder_states: HashMap<VideoRung, EncoderState>,
    /// True after the current run's ffmpeg exited successfully (ENDLIST).
    current_run_eof: bool,
    last_access: Instant,
    /// Last encode-window restart (create counts as one) for the min-interval guard.
    last_restart: Instant,
    /// True after serving at least one segment at or past encode `start_ms`.
    primed: bool,
    /// Set once the play land is present in the segment map.
    first_segment_ready: bool,
    /// Latest aligned play land requested while coalescing rapid scrubs.
    pending_play_ms: Option<u64>,
    /// When [`Session::pending_play_ms`] was last updated (debounce clock).
    pending_since: Option<Instant>,
    failed: Option<String>,
    /// Tracks declared in the master, snapshotted at create.
    subtitle_tracks: Vec<HlsSubtitleTrack>,
    /// Piggyback target when this session runs on an `eligible` item
    /// (ADR-0041 Decision 7); `None` once the side output is published.
    piggyback: Option<PiggybackExtract>,
    /// Library subtitle store + item store for the piggyback publish.
    subs: Option<Arc<SubsStore>>,
    db: Option<Arc<Db>>,
    /// ADR-0023 §9.3: whether a keyframe-map build for this item is queued or
    /// in flight. The API wires it to the library pool; a seek consults it
    /// before deciding to wait, bounded, for the build to land.
    map_build_in_flight: Option<Arc<MapBuildInFlight>>,
    /// Title-absolute start of the furthest segment this session has been
    /// asked for. The playhead, as the server can see it (ADR-0050 §2).
    last_requested_ms: u64,
}

struct EncoderState {
    /// Current producer run id; playlist URI is per-run (ADR-0020).
    current_run_id: u64,
    /// Next run id to allocate on restart.
    next_run_id: u64,
    child: Option<Child>,
    /// Latest RSS sampled by the throttle worker without holding the sessions
    /// lock. `None` is distinct from a measured zero.
    child_rss_bytes: Option<u64>,
    /// True while this rung's encoder is SIGSTOPped by the throttle.
    throttled: bool,
    /// Encoders a seek replaced, kept until [`REAP_AFTER`] has put their
    /// teardown clear of the seek that replaced them (ADR-0050 §5).
    ///
    /// They keep **running**, not suspended. A client may still be waiting on
    /// a segment of that land which has not finished writing, and a suspended
    /// encoder never finishes it.
    superseded: Vec<SupersededEncoder>,
}

fn single_rung_segment_maps(
    map: crate::hls_segment_map::SegmentMap,
) -> HashMap<VideoRung, crate::hls_segment_map::SegmentMap> {
    HashMap::from([(SINGLE_VIDEO_RUNG, map)])
}

fn single_rung_encoder_states(
    current_run_id: u64,
    next_run_id: u64,
    child: Option<Child>,
) -> HashMap<VideoRung, EncoderState> {
    HashMap::from([(
        SINGLE_VIDEO_RUNG,
        EncoderState {
            current_run_id,
            next_run_id,
            child,
            child_rss_bytes: None,
            throttled: false,
            superseded: Vec::new(),
        },
    )])
}

impl Session {
    /// Returns the segment map for a rung known to belong to this session.
    ///
    /// A missing map is a session-construction bug, so this deliberately
    /// panics instead of turning every production lookup into a recoverable
    /// branch. When more production rungs land, construction must populate a
    /// map for every rung the session offers.
    fn segment_map(&self, rung: VideoRung) -> &crate::hls_segment_map::SegmentMap {
        let Some(map) = self.segment_maps.get(&rung) else {
            panic!("session has no segment map for rung {}", rung.as_str());
        };
        map
    }

    /// Mutable form of [`Self::segment_map`], with the same construction
    /// invariant and deliberate panic for a missing offered rung.
    fn segment_map_mut(&mut self, rung: VideoRung) -> &mut crate::hls_segment_map::SegmentMap {
        let Some(map) = self.segment_maps.get_mut(&rung) else {
            panic!("session has no segment map for rung {}", rung.as_str());
        };
        map
    }

    /// Returns the encoder state for a rung known to belong to this session.
    ///
    /// Encoder and segment-map keys are the session's construction invariant.
    /// A missing state is therefore a bug, matching [`Self::segment_map`].
    fn encoder_state(&self, rung: VideoRung) -> &EncoderState {
        let Some(state) = self.encoder_states.get(&rung) else {
            panic!("session has no encoder state for rung {}", rung.as_str());
        };
        state
    }

    /// Mutable form of [`Self::encoder_state`], with the same deliberate panic
    /// for a missing offered rung.
    fn encoder_state_mut(&mut self, rung: VideoRung) -> &mut EncoderState {
        let Some(state) = self.encoder_states.get_mut(&rung) else {
            panic!("session has no encoder state for rung {}", rung.as_str());
        };
        state
    }
}

/// An encoder a seek replaced, waiting out [`REAP_AFTER`].
struct SupersededEncoder {
    child: Child,
    /// Latest RSS sampled while this held child was still live.
    rss_bytes: Option<u64>,
    reap_at: Instant,
    /// The run this encoder is still writing into. Its directory is not the
    /// current run's any more, and every per-run cleanup path in this file
    /// reads "not the current run" as "finished". It is not finished: the
    /// process is alive until [`reap_at`](Self::reap_at).
    run_id: u64,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone)]
struct EncoderProcess {
    session_id: String,
    rung: VideoRung,
    pid: u32,
    /// `None` is the rung's current child; a run id identifies a held child.
    superseded_run_id: Option<u64>,
}

#[cfg(any(target_os = "linux", test))]
fn record_encoder_rss_sample(
    sessions: &mut HashMap<String, Session>,
    high_water_rss_bytes: &AtomicU64,
    process: &EncoderProcess,
    rss_bytes: Option<u64>,
) -> bool {
    let Some(session) = sessions.get_mut(&process.session_id) else {
        return false;
    };
    let state = session.encoder_state_mut(process.rung);
    let recorded = match process.superseded_run_id {
        None => {
            if state
                .child
                .as_ref()
                .is_none_or(|child| child.id() != process.pid)
            {
                false
            } else {
                state.child_rss_bytes = rss_bytes;
                true
            }
        }
        Some(run_id) => {
            let Some(held) = state
                .superseded
                .iter_mut()
                .find(|held| held.run_id == run_id && held.child.id() == process.pid)
            else {
                return false;
            };
            held.rss_bytes = rss_bytes;
            true
        }
    };
    if let (true, Some(rss_bytes)) = (recorded, rss_bytes) {
        high_water_rss_bytes.fetch_max(rss_bytes, Ordering::Relaxed);
    }
    recorded
}

/// Snapshot returned by start / seek / get (ADR-0020 wire fields).
#[derive(Debug, Clone)]
pub struct SessionView {
    pub session_id: String,
    pub item_id: i64,
    pub playlist_url: String,
    pub video_encoder: String,
    pub encoder_kind: EncoderKind,
    pub landed_ms: u64,
    /// Title time that element `currentTime` 0 means — see
    /// [`RunListing::media_origin_ms`]. Not the land.
    pub media_origin_ms: u64,
    pub usable_extent_ms: Option<u64>,
    pub run_id: u64,
}

/// The session's master playlist URI (ADR-0054 decision 5).
///
/// One URI for the life of the session, where this used to mint a fresh one per
/// run. What still changes per run is `EXT-X-MAP` inside the media playlist,
/// because the init carries the land in its `elst` empty edit and decision 4's
/// overturn measured that on all four paths.
///
/// **A client meets the changed map only through a re-attach.** Neither client
/// reloads a `VOD` playlist in place: hls.js gates it on `details.live`, and the
/// iPhone fetched one `index.m3u8` across four spawned runs. So the map is read
/// once per attach, alongside the playlist that names it, and the pairing is
/// always self-consistent.
fn playlist_url_for(session_id: &str) -> String {
    format!("/api/v0/sessions/{session_id}/master.m3u8")
}

fn run_dir(session: &Session, rung: VideoRung) -> PathBuf {
    run_path(
        &session.dir,
        rung,
        session.encoder_state(rung).current_run_id,
    )
}

fn rung_dir(session_dir: &Path, rung: VideoRung) -> PathBuf {
    session_dir.join(crate::hls_segment_map::rung_rel_dir(rung))
}

fn run_path(session_dir: &Path, rung: VideoRung, run_id: u64) -> PathBuf {
    session_dir.join(crate::hls_segment_map::run_rel_dir(rung, run_id))
}

fn write_run_encode_start(run_dir: &Path, start_ms: u64) -> Result<(), String> {
    fs::write(run_dir.join("encode_start_ms"), start_ms.to_string())
        .map_err(|e| format!("write encode_start_ms {}: {e}", run_dir.display()))
}

fn read_run_encode_start(run_dir: &Path) -> u64 {
    fs::read_to_string(run_dir.join("encode_start_ms"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn sync_segment_map(session: &mut Session, rung: VideoRung) {
    let run = run_dir(session, rung);
    let index_path = run.join("index.m3u8");
    let Ok(text) = fs::read_to_string(&index_path) else {
        return;
    };
    let encode_start_ms = read_run_encode_start(&run);
    let cadence = session_cadence_ms(session);
    let points = session_listed_points(session);
    let snap = session_key_snap(cadence, &points);
    let run_id = session.encoder_state(rung).current_run_id;
    let session_dir = session.dir.clone();
    if let Err(e) = crate::hls_segment_map::ingest_run_index(
        session.segment_map_mut(rung),
        &session_dir,
        rung,
        run_id,
        &text,
        encode_start_ms,
        snap,
    ) {
        tracing::warn!(
            run_id,
            error = %e,
            "hls map ingest failed"
        );
    }
}

/// Re-read the `index.m3u8` of every run a superseded encoder is still
/// writing into.
///
/// A seek keeps the prior encoder for [`REAP_AFTER`] and it keeps producing
/// into its rung's `run_<old>` (ADR-0050 §5). [`sync_segment_map`] reads the current run
/// only, so without this nothing re-ingests that file until the next
/// `restart_at`, and everything the held encoder writes after the seek is
/// invisible to a waiting request. The new encoder starts at the new land and
/// never produces behind it, so neither encoder can serve a want in the gap.
///
/// Reads [`SupersededEncoder::run_id`], never `read_dir`. The ids are already
/// in memory, so the runs touched are bounded by how many encoders are held,
/// not by how many seeks the session has made. [`sync_all_run_indexes`] walks
/// the whole session dir instead, which grows with session history, and it
/// stays where it is: once per seek.
///
/// **It is not one file read per held encoder.**
/// [`crate::hls_segment_map::ingest_run_index`] reads the index, the
/// `encode_start_ms`, and then **every segment file the index lists**, because
/// the map key comes from each segment's `sidx`. The cost is `2 + K` reads per
/// held run, measured 2026-08-30 at about 79 us per listed segment: 1.35 ms at
/// K=5, 6.1 ms at K=30, 23.7 ms at K=150, held under the sessions mutex.
///
/// The `is_empty` gate at the call site is what keeps that off the steady
/// state: with nothing held this costs one check, measured at 738 us against a
/// 737 us baseline. [`sync_segment_map`] already pays the same `2 + K` shape
/// for the current run, twice per poll iteration, which is the larger and
/// older cost — the map is rebuilt from disk rather than maintained
/// incrementally, and this function inherits that rather than introducing it.
fn sync_superseded_run_indexes(session: &mut Session, rung: VideoRung) {
    let cadence = session_cadence_ms(session);
    let points = session_listed_points(session);
    let snap = session_key_snap(cadence, &points);
    let run_ids: Vec<u64> = session
        .encoder_state(rung)
        .superseded
        .iter()
        .map(|s| s.run_id)
        .collect();
    for run_id in run_ids {
        let run_path = run_path(&session.dir, rung, run_id);
        let Ok(text) = fs::read_to_string(run_path.join("index.m3u8")) else {
            continue;
        };
        let encode_start_ms = read_run_encode_start(&run_path);
        let session_dir = session.dir.clone();
        if let Err(e) = crate::hls_segment_map::ingest_run_index(
            session.segment_map_mut(rung),
            &session_dir,
            rung,
            run_id,
            &text,
            encode_start_ms,
            snap,
        ) {
            tracing::warn!(run_id, error = %e, "hls map ingest failed (superseded run)");
        }
    }
}

/// Re-read every `v<rung>/run_*/index.m3u8` so scrub-back map hits see prior
/// runs even if the current run's index is empty after stop_child.
fn sync_all_run_indexes(session: &mut Session) {
    let cadence = session_cadence_ms(session);
    let points = session_listed_points(session);
    let snap = session_key_snap(cadence, &points);
    let session_dir = session.dir.clone();
    let rungs: Vec<VideoRung> = session.segment_maps.keys().copied().collect();
    for rung in rungs {
        let Ok(entries) = fs::read_dir(rung_dir(&session_dir, rung)) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(id_str) = name.strip_prefix("run_") else {
                continue;
            };
            let Ok(run_id) = id_str.parse::<u64>() else {
                continue;
            };
            let run_path = entry.path();
            let index_path = run_path.join("index.m3u8");
            let Ok(text) = fs::read_to_string(&index_path) else {
                continue;
            };
            let encode_start_ms = read_run_encode_start(&run_path);
            if let Err(e) = crate::hls_segment_map::ingest_run_index(
                session.segment_map_mut(rung),
                &session_dir,
                rung,
                run_id,
                &text,
                encode_start_ms,
                snap,
            ) {
                tracing::warn!(
                    rung = rung.as_str(),
                    run_id,
                    error = %e,
                    "hls map ingest failed (all-runs sync)"
                );
            }
        }
    }
}

fn latest_mapped_start_in_window(
    map: &crate::hls_segment_map::SegmentMap,
    window_start_ms: u64,
) -> Option<u64> {
    map.iter_ordered()
        .rev()
        .find(|s| s.start_ms >= window_start_ms)
        .map(|s| s.start_ms)
}

fn current_run_has_mapped_segment(session: &Session, rung: VideoRung) -> bool {
    // Match [`build_run_media_playlist`]: map rows without bytes must not
    // flip ready (header-only playlist / listed-404 class under ADR-0020).
    let in_playlist_window = |s: &crate::hls_segment_map::MappedSegment| {
        s.start_ms.saturating_add(s.duration_ms) > session.start_ms
            && session.dir.join(&s.rel_path).is_file()
    };
    if session
        .segment_map(rung)
        .iter_ordered()
        .any(|s| s.run_id == session.encoder_state(rung).current_run_id && in_playlist_window(s))
    {
        return true;
    }
    // Duplicate-write stop: fresh run id with no new producer bytes; playlist
    // is assembled from this rung's map (prior runs).
    session.encoder_state(rung).child.is_none()
        && session
            .segment_map(rung)
            .iter_ordered()
            .any(in_playlist_window)
}

fn first_current_run_start(session: &Session, rung: VideoRung) -> Option<u64> {
    let in_playlist_window = |s: &crate::hls_segment_map::MappedSegment| {
        s.start_ms.saturating_add(s.duration_ms) > session.start_ms
            && session.dir.join(&s.rel_path).is_file()
    };
    if let Some(ms) = session
        .segment_map(rung)
        .iter_ordered()
        .find(|s| s.run_id == session.encoder_state(rung).current_run_id && in_playlist_window(s))
        .map(|s| s.start_ms)
    {
        return Some(ms);
    }
    session
        .segment_map(rung)
        .iter_ordered()
        .find(|s| in_playlist_window(s))
        .map(|s| s.start_ms)
}

/// Copy's whole-title listing: the greedy [`COPY_WINDOW_MS`] walk of the
/// keyframe map.
///
/// **Copy cuts at source keyframes and cannot hold a grid, but its cut points
/// are known in advance** — the keyframe map already holds every one. Walk it
/// from 0, taking the first entry at or after each window boundary, and that
/// set is what a run will actually write: FFmpeg's `-hls_time` cuts at the
/// first keyframe at or after each boundary, and a seek to a listed point
/// lands exactly on it because the map entry is exact.
///
/// **The walk is run-independent** — it depends only on the map and on 0 —
/// which is what makes it listable before anything has been written. A listing
/// derived from where *this* run happened to start would differ per run.
///
/// `None` without a keyframe map: an `-ss` copy run's cut points are not
/// knowable ahead of time, so that session keeps the per-run listing.
///
/// **Unverified against the product.** §S8 measured this shape through the
/// spike origin, which never calls the session API. That `-hls_time` cuts
/// where the walk says is FFmpeg's documented stream-copy behaviour, checked
/// here against a fixture map and not against a real copy run.
fn copy_window_entries(session: &Session) -> Option<Vec<(u64, u64)>> {
    let map = session.map_binding.map.as_ref()?;
    let end = session.usable_extent_ms.unwrap_or(session.duration_ms);
    if end == 0 {
        return None;
    }
    let mut starts: Vec<u64> = Vec::new();
    let mut boundary = 0u64;
    for entry in &map.entries {
        if entry.pts_ms >= end {
            break;
        }
        if entry.pts_ms >= boundary {
            starts.push(entry.pts_ms);
            boundary = entry.pts_ms.saturating_add(COPY_WINDOW_MS);
        }
    }
    if starts.is_empty() {
        return None;
    }
    Some(
        starts
            .iter()
            .enumerate()
            .map(|(i, start)| {
                let next = starts.get(i + 1).copied().unwrap_or(end);
                (*start, next.saturating_sub(*start))
            })
            .collect(),
    )
}

/// The whole title on the grid this session's runs share (ADR-0054 decision 1).
///
/// `None` when there is no shared grid — copy and remux, or a leg with no
/// honest cadence — and the caller keeps the per-run listing.
///
/// The bound is `usable_extent_ms` when the producer reached EOF short of the
/// claimed duration, and `duration_ms` otherwise. `usable_extent_ms` is the
/// **session maximum**, the furthest point known reachable (#181). The per-run
/// reading is `0` when a run ends having produced nothing, and this would then
/// list nothing at all.
fn full_title_entries(session: &Session) -> Option<Vec<(u64, u64)>> {
    // Copy and remux list the same whole title on a different grid. That is
    // the only difference between the modes: both are full-title VOD, and
    // scrub granularity differs because copy cuts where the source does.
    let step = match session_grid_cadence(session) {
        // Copy and remux: the keyframe walk is their listing, by design.
        GridCadence::KeyframeWalk => return copy_window_entries(session),
        // Transcode with no listable grid. **Not the walk** — a transcode
        // encoder writes its own IDR grid and never lands on a source
        // keyframe, so the walk would name URIs it cannot fill. `None` here
        // drops `run_listing` into the run's own window listing.
        GridCadence::NoHonestGrid => return None,
        GridCadence::Cadence(step) => step,
    };
    let end = session.usable_extent_ms.unwrap_or(session.duration_ms);
    if end == 0 || step == 0 {
        return None;
    }
    Some(
        (0..end)
            .step_by(step as usize)
            .map(|start| (start, step.min(end - start)))
            .collect(),
    )
}

/// What a run's media playlist lists, and where its timeline starts.
struct RunListing {
    entries: Vec<(u64, u64)>,
    /// `EXT-X-START:TIME-OFFSET`, in milliseconds.
    start_offset_ms: u64,
    /// **The title time that element `currentTime` 0 means.**
    ///
    /// HLS media time runs from the first listed segment, so this is a
    /// property of the listing and not of the session: a full-title listing
    /// starts at 0 and a per-run listing starts at its first entry. The two
    /// shapes coexist by ADR-0054, and **which one a run serves can change
    /// mid-session** — copy's whole-title walk needs a keyframe map and the
    /// map arrives asynchronously. A client cannot derive it from the mode,
    /// from `duration` or from `seekable`, so the session view says it
    /// (Rule 2.1).
    ///
    /// **This is not `landed_ms`.** The land is where the producer started,
    /// title-absolute in both shapes; the origin is where the *element's*
    /// clock is zeroed. They are equal only in the per-run shape, and reading
    /// the land as the origin is what put `20:02` on a 15-minute title.
    media_origin_ms: u64,
}

/// One owner for the branch, so the playlist bytes and the origin the session
/// view reports cannot disagree (Rule 4.9).
/// What this run's media playlist lists, and where its timeline starts.
///
/// **Two shapes, and the choice is not stable for the life of a session.** A
/// full-title listing starts at 0 and says the land in `EXT-X-START`. The
/// fallback lists this run's window, with its own first entry as the origin,
/// and `full_title_entries` picks it whenever there is no honest grid.
///
/// **That matters to ADR-0054 decision 5 and it bounds where the decision may
/// go.** Under a session-scoped URI the playlist body is re-read only on a
/// re-attach, and the map is the *small* thing that changes between two reads:
/// the fallback changes the entire entry set and `media_origin_ms` with it.
/// Transcode never reaches the fallback, because `grid_cadence_ms` answers for
/// the leg, and decision 5 is transcode-only for exactly that reason.
///
/// **So do not widen the session-scoped URI to copy or remux on the strength of
/// decision 5.** Copy's whole-title walk waits on a keyframe map that arrives
/// asynchronously, which is the case where the shape flips mid-session, and
/// nothing has measured a client against a body that changes that much.
fn run_listing(session: &Session, rung: VideoRung) -> RunListing {
    match full_title_entries(session) {
        // A full-title listing starts at 0, so the attach point is the land
        // and must be said.
        Some(entries) => RunListing {
            entries,
            start_offset_ms: session.play_start_ms,
            media_origin_ms: 0,
        },
        None => {
            // No shared grid: list what the map holds for this window, as
            // before. ADR-0020: never list a URI whose bytes are gone.
            // Eviction updates the map, but defend in depth so a race cannot
            // reintroduce listed-404.
            let window = session.start_ms;
            let entries: Vec<(u64, u64)> = session
                .segment_map(rung)
                .iter_ordered()
                .filter(|s| s.start_ms.saturating_add(s.duration_ms) > window)
                .filter(|s| session.dir.join(&s.rel_path).is_file())
                .map(|s| (s.start_ms, s.duration_ms))
                .collect();
            // The per-run listing still begins at the land, where a zero
            // offset already means it — and where the first entry is the
            // origin. Listing nothing, the window is the honest answer for
            // the attach that follows.
            let media_origin_ms = entries.first().map_or(window, |(start, _)| *start);
            RunListing {
                entries,
                start_offset_ms: 0,
                media_origin_ms,
            }
        }
    }
}

fn build_run_media_playlist(session_id: &str, session: &Session, rung: VideoRung) -> Vec<u8> {
    // Path-absolute URIs (ADR-0008): run-dir depth cannot break resolution.
    let init_uri = format!(
        "/api/v0/sessions/{session_id}/runs/{}/init.mp4",
        session.encoder_state(rung).current_run_id
    );
    let listing = run_listing(session, rung);
    let bytes = crate::hls_segment_map::build_map_playlist(
        &listing.entries,
        &init_uri,
        listing.start_offset_ms,
    );
    with_session_absolute_segment_uris(session_id, &bytes)
}

/// Rewrite bare `seg_<ms>.m4s` lines to path-absolute session asset URLs.
/// Relative `../` climbs were the cutover failure class under `/runs/{n}/`.
fn with_session_absolute_segment_uris(session_id: &str, playlist: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(playlist);
    let mut out = String::with_capacity(text.len() + 64);
    for line in text.lines() {
        if let Some(ms) = crate::hls_segment_map::parse_time_keyed_segment_name(line) {
            out.push_str(&format!(
                "/api/v0/sessions/{session_id}/{}",
                crate::hls_segment_map::time_keyed_segment_name(ms)
            ));
            out.push('\n');
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.into_bytes()
}

fn dir_tree_bytes(path: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            total = total.saturating_add(dir_tree_bytes(&p));
        } else if let Ok(meta) = entry.metadata() {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

/// Per-run cache eviction (ADR-0020 §12). Map is authoritative:
/// - Prefer orphan run dirs (no map refs) so scrub-back stays a file serve.
/// - When a referenced run must go, `remove_run` before unlinking.
/// - Zero-byte dirs are reaped quietly — not budget evictions.
fn maybe_evict_finished_runs(session: &mut Session) {
    reap_empty_finished_run_dirs(session);
    let budget = session.run_cache_budget_bytes;
    loop {
        let total = session_disk_bytes(session);
        if total <= budget {
            return;
        }
        let rungs: Vec<VideoRung> = session.segment_maps.keys().copied().collect();
        let mut orphans: Vec<(VideoRung, u64, u64)> = Vec::new();
        let mut referenced_finished: Vec<(VideoRung, u64, u64)> = Vec::new();
        for rung in rungs {
            let live = live_run_ids(session);
            let referenced = session.segment_map(rung).referenced_run_ids();
            let Ok(entries) = fs::read_dir(rung_dir(&session.dir, rung)) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                let Some(id_str) = name.strip_prefix("run_") else {
                    continue;
                };
                let Ok(id) = id_str.parse::<u64>() else {
                    continue;
                };
                // Not just the current run: a superseded encoder is still
                // writing into its own run for up to `REAP_AFTER`.
                if live.contains(&id) {
                    continue;
                }
                let bytes = dir_tree_bytes(&entry.path());
                if bytes == 0 {
                    continue;
                }
                if referenced.contains(&id) {
                    referenced_finished.push((rung, id, bytes));
                } else {
                    orphans.push((rung, id, bytes));
                }
            }
        }
        orphans.sort_by_key(|(rung, id, _)| (*id, rung.as_str()));
        referenced_finished.sort_by_key(|(rung, id, _)| (*id, rung.as_str()));
        let victim = orphans
            .first()
            .copied()
            .or_else(|| referenced_finished.first().copied());
        let Some((victim_rung, victim_id, victim_bytes)) = victim else {
            return;
        };
        let path = run_path(&session.dir, victim_rung, victim_id);
        let had_map_refs = session
            .segment_map(victim_rung)
            .run_is_referenced(victim_id);
        // Drop map entries before unlinking so serve never sees a mapped
        // path whose file is already gone.
        session.segment_map_mut(victim_rung).remove_run(victim_id);
        if let Err(e) = fs::remove_dir_all(&path) {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "hls run eviction failed"
            );
            return;
        }
        let after = session_disk_bytes(session);
        tracing::info!(
            rung = victim_rung.as_str(),
            run_id = victim_id,
            evicted_bytes = victim_bytes,
            had_map_refs,
            orphan = !had_map_refs,
            session_disk_bytes_before = total,
            session_disk_bytes = after,
            budget_bytes = budget,
            session_dir = %session.dir.display(),
            "hls evicted finished run (cache budget)"
        );
    }
}

/// Remove finished run directories that hold no bytes. Not a budget eviction.
fn reap_empty_finished_run_dirs(session: &mut Session) {
    let rungs: Vec<VideoRung> = session.segment_maps.keys().copied().collect();
    for rung in rungs {
        let live = live_run_ids(session);
        let Ok(entries) = fs::read_dir(rung_dir(&session.dir, rung)) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(id_str) = name.strip_prefix("run_") else {
                continue;
            };
            let Ok(id) = id_str.parse::<u64>() else {
                continue;
            };
            // A live run is never empty today, because
            // `write_run_encode_start` seeds every run dir before the spawn.
            // That is a seeding detail in another function, not a property of
            // this one, so exclude live runs here rather than depending on it.
            if live.contains(&id) {
                continue;
            }
            if dir_tree_bytes(&entry.path()) > 0 {
                continue;
            }
            if session.segment_map(rung).run_is_referenced(id) {
                session.segment_map_mut(rung).remove_run(id);
            }
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

fn session_view(session_id: &str, session: &Session, rung: VideoRung) -> SessionView {
    let encoder_kind = if session.mode == SessionMode::Copy && session.burn_in.is_none() {
        EncoderKind::Copy
    } else if session.video_encoder == "libx264" {
        EncoderKind::Software
    } else {
        EncoderKind::Hardware
    };
    SessionView {
        session_id: session_id.to_string(),
        item_id: session.item_id,
        playlist_url: playlist_url_for(session_id),
        video_encoder: if session.mode == SessionMode::Copy && session.burn_in.is_none() {
            "copy".into()
        } else {
            session.video_encoder.clone()
        },
        encoder_kind,
        landed_ms: session.landed_ms,
        media_origin_ms: run_listing(session, rung).media_origin_ms,
        usable_extent_ms: session.usable_extent_ms,
        run_id: session.encoder_state(rung).current_run_id,
    }
}

/// Whether a leg of this shape runs an encoder at all: every transcode, and a
/// stream copy that burns in subtitles. Copy-only legs are a cheap remux.
/// This distinction does not change today's equal weights, but a future
/// measured weight will branch on it.
fn re_encodes(mode: SessionMode, burn_in: Option<&BurnInSelection>) -> bool {
    !(mode == SessionMode::Copy && burn_in.is_none())
}

/// Weighted live encoder load in hundredths of an encoder (ADR-0050 §7).
fn live_encoder_load_centi(sessions: &HashMap<String, Session>) -> u32 {
    sessions.values().fold(0u32, |total, session| {
        let weight =
            hls_policy::encoder_weight_centi(re_encodes(session.mode, session.burn_in.as_ref()));
        let session_load = session.encoder_states.values().fold(0u32, |load, state| {
            let encoder_count = u32::from(state.child.is_some())
                .saturating_add(state.superseded.len().min(u32::MAX as usize) as u32);
            load.saturating_add(weight.saturating_mul(encoder_count))
        });
        total.saturating_add(session_load)
    })
}

/// Sum the throttle worker's stored RSS samples. One unsampled child makes the
/// reading unmeasured rather than silently contributing zero (Rule 4.15).
fn live_encoder_rss(sessions: &HashMap<String, Session>) -> (Option<u64>, usize) {
    let mut live_rss_bytes = Some(0u64);
    let mut children = 0usize;
    for state in sessions
        .values()
        .flat_map(|session| session.encoder_states.values())
    {
        if state.child.is_some() {
            children = children.saturating_add(1);
            live_rss_bytes = live_rss_bytes
                .zip(state.child_rss_bytes)
                .map(|(total, rss)| total.saturating_add(rss));
        }
        for held in &state.superseded {
            children = children.saturating_add(1);
            live_rss_bytes = live_rss_bytes
                .zip(held.rss_bytes)
                .map(|(total, rss)| total.saturating_add(rss));
        }
    }
    (live_rss_bytes, children)
}

fn encoder_memory(
    sessions: &HashMap<String, Session>,
    available_bytes: &Result<u64, String>,
) -> EncoderMemory {
    let (live_rss_bytes, children) = live_encoder_rss(sessions);
    encoder_memory_from_available(live_rss_bytes, children, available_bytes)
}

fn encoder_memory_reserve_bytes(high_water_rss_bytes: u64) -> u64 {
    // ADR-0050 §4 spawns the seek encoder before reaping its predecessor, and
    // §5 measured a peak held set of two for one person scrubbing one session.
    high_water_rss_bytes.saturating_mul(2)
}

fn memory_admits_new_session(memory: EncoderMemory, high_water_rss_bytes: u64) -> bool {
    match memory {
        EncoderMemory::Unmeasured => true,
        EncoderMemory::Measured { children: 0, .. } => true,
        EncoderMemory::Measured { .. } if high_water_rss_bytes == 0 => true,
        EncoderMemory::Measured {
            available_bytes, ..
        } => available_bytes >= encoder_memory_reserve_bytes(high_water_rss_bytes),
    }
}

/// Whether a newcomer of this shape is admitted against measured memory and,
/// when configured, the operator's explicit live-encoder override. Every
/// weight is currently 1.0, so the override reads identically to a raw child
/// count today.
fn admits_new_session(
    sessions: &HashMap<String, Session>,
    mode: SessionMode,
    burn_in: Option<&BurnInSelection>,
    max_encoders: Option<usize>,
    memory: EncoderMemory,
    high_water_rss_bytes: u64,
) -> bool {
    let existing_centi = live_encoder_load_centi(sessions);
    let newcomer_centi = hls_policy::encoder_weight_centi(re_encodes(mode, burn_in));
    let cap_admits = max_encoders.is_none_or(|max_encoders| {
        hls_policy::admits_weighted_load(existing_centi, newcomer_centi, max_encoders)
    });
    cap_admits && memory_admits_new_session(memory, high_water_rss_bytes)
}

impl HlsSessionRegistry {
    /// Creates the HLS cache root, sweeps leftover session dirs from a prior
    /// process, and starts the idle reaper. `encode_leg` is the preferred
    /// session-shaped leg from ADR-0009 (`libx264` if nothing else works).
    pub fn new(
        root: PathBuf,
        encode_leg: impl Into<crate::EncodeLeg>,
    ) -> Result<Arc<Self>, String> {
        Self::with_measured_admission(root, encode_leg, None, None)
    }

    /// Construct the normal registry: no configured concurrency cap, with
    /// admission governed by the measured memory runaway guard.
    pub fn with_measured_admission(
        root: PathBuf,
        encode_leg: impl Into<crate::EncodeLeg>,
        subs: Option<Arc<SubsStore>>,
        db: Option<Arc<Db>>,
    ) -> Result<Arc<Self>, String> {
        Self::build(root, None, encode_leg, subs, db)
    }

    /// Construct a registry with an explicit operator/test encoder cap in
    /// addition to measured memory admission.
    pub fn with_cap(
        root: PathBuf,
        max_encoders: usize,
        encode_leg: impl Into<crate::EncodeLeg>,
        subs: Option<Arc<SubsStore>>,
        db: Option<Arc<Db>>,
    ) -> Result<Arc<Self>, String> {
        Self::build(root, Some(max_encoders), encode_leg, subs, db)
    }

    /// Session directories are process-owned caches, not restart state. The
    /// startup sweep deliberately removes both ADR-0051's rung layout and the
    /// old flat `run_*` layout; migration would imply resume support that the
    /// registry does not have. Removal is logged per directory with its byte
    /// count, so discarding an old-layout cache is explicit.
    fn build(
        root: PathBuf,
        max_encoders: Option<usize>,
        encode_leg: impl Into<crate::EncodeLeg>,
        subs: Option<Arc<SubsStore>>,
        db: Option<Arc<Db>>,
    ) -> Result<Arc<Self>, String> {
        fs::create_dir_all(&root)
            .map_err(|e| format!("create hls cache dir {}: {e}", root.display()))?;
        for entry in fs::read_dir(&root)
            .map_err(|e| format!("read hls cache dir {}: {e}", root.display()))?
            .flatten()
        {
            let path = entry.path();
            if path.is_dir() {
                let discarded_bytes = dir_tree_bytes(&path);
                if let Err(e) = fs::remove_dir_all(&path) {
                    tracing::warn!(
                        path = %path.display(),
                        discarded_bytes,
                        error = %e,
                        "hls startup sweep failed"
                    );
                } else {
                    tracing::info!(
                        path = %path.display(),
                        discarded_bytes,
                        "swept orphaned hls session dir"
                    );
                }
            }
        }

        let encode_leg = encode_leg.into();
        let registry = Arc::new(Self {
            root,
            max_encoders,
            encoder_rss_high_water_bytes: AtomicU64::new(0),
            encode_leg,
            subs,
            db,
            map_build_in_flight: Mutex::new(None),
            next_id: AtomicU64::new(1),
            sessions: Mutex::new(HashMap::new()),
        });
        let reaper = Arc::clone(&registry);
        std::thread::Builder::new()
            .name("hls-reaper".into())
            .spawn(move || reaper.reaper_loop())
            .map_err(|e| format!("spawn hls reaper: {e}"))?;
        let throttle = Arc::clone(&registry);
        std::thread::Builder::new()
            .name("hls-throttle".into())
            .spawn(move || throttle.throttle_loop())
            .map_err(|e| format!("spawn hls throttle: {e}"))?;
        Ok(registry)
    }

    /// ADR-0023 §9.3: attach the map-build-in-flight predicate (the API wires
    /// it to the library pool). A seek consults it before waiting, bounded,
    /// for a keyframe-map build a consumer already triggered. Without it,
    /// seeks never wait and fall straight to the §8 `-ss` plan.
    pub fn set_map_build_in_flight(&self, f: Option<Arc<MapBuildInFlight>>) {
        *self
            .map_build_in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = f;
    }

    /// Starts a session at `start_ms` (aligned). Every call creates its own
    /// session; seeking restarts that session in place (ADR-0011). Switching
    /// audio or burn-in does not: it starts a fresh session (ADR-0012 /
    /// ADR-0018). `subtitle_tracks` is snapshotted here and never revisited.
    /// `encode_plan` applies only in Transcode mode (ADR-0022). `piggyback`
    /// arms the ADR-0041 Decision 7 side output for an `eligible` item.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        &self,
        item_id: i64,
        src: &Path,
        start_ms: u64,
        duration_ms: u64,
        mode: SessionMode,
        audio: AudioSelection,
        subtitle_tracks: Vec<HlsSubtitleTrack>,
        burn_in: Option<BurnInSelection>,
        keyframe_map: Option<crate::virtual_input::KeyframeMap>,
        encode_plan: VideoEncodePlan,
        piggyback: Option<PiggybackExtract>,
    ) -> Result<String, StartSessionError> {
        let play_start_ms = align_to_segment(start_ms);
        // Read host and cgroup files before taking the sessions lock. The lock
        // is shared with segment-serving hot paths, and serializing filesystem
        // syscalls behind every session request would turn the guard into
        // contention on the path it protects.
        let available_memory = read_available_memory();
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| StartSessionError::Spawn("hls registry lock poisoned".into()))?;
        let (sampled_rss_bytes, children) = live_encoder_rss(&sessions);
        let memory = encoder_memory(&sessions, &available_memory);
        let high_water_rss_bytes = self.encoder_rss_high_water_bytes.load(Ordering::Relaxed);
        let reserve_bytes = encoder_memory_reserve_bytes(high_water_rss_bytes);
        let existing_centi = live_encoder_load_centi(&sessions);
        let newcomer_centi = hls_policy::encoder_weight_centi(re_encodes(mode, burn_in.as_ref()));
        let cap_admits = self.max_encoders.is_none_or(|max_encoders| {
            hls_policy::admits_weighted_load(existing_centi, newcomer_centi, max_encoders)
        });
        let memory_admits = memory_admits_new_session(memory, high_water_rss_bytes);
        let admitted = admits_new_session(
            &sessions,
            mode,
            burn_in.as_ref(),
            self.max_encoders,
            memory,
            high_water_rss_bytes,
        );
        let reason = if !cap_admits {
            "operator_override"
        } else if !memory_admits {
            "memory_reserve"
        } else if children == 0 {
            "no_live_children"
        } else if high_water_rss_bytes == 0 {
            "no_high_water_sample"
        } else if memory == EncoderMemory::Unmeasured {
            "unmeasured_admit"
        } else {
            "memory_available"
        };
        match memory {
            EncoderMemory::Measured {
                live_rss_bytes,
                available_bytes,
                children,
            } => tracing::info!(
                admitted,
                reason,
                live_rss_bytes,
                available_bytes,
                children,
                high_water_rss_bytes,
                reserve_bytes,
                operator_max_encoders = ?self.max_encoders,
                "hls session admission"
            ),
            EncoderMemory::Unmeasured => {
                let measurement_error = available_memory
                    .as_ref()
                    .err()
                    .map(String::as_str)
                    .unwrap_or("one or more live children have no positive RSS sample");
                tracing::warn!(
                    admitted,
                    reason,
                    live_rss_bytes = ?sampled_rss_bytes,
                    available_bytes = "unmeasured",
                    children,
                    high_water_rss_bytes,
                    reserve_bytes,
                    operator_max_encoders = ?self.max_encoders,
                    error = measurement_error,
                    "hls session admission"
                );
            }
        }
        if !admitted {
            return Err(StartSessionError::AdmissionRefused);
        }

        let id = format!("s{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let dir = self.root.join(&id);
        fs::create_dir_all(&dir).map_err(|e| {
            StartSessionError::Spawn(format!("create session dir {}: {e}", dir.display()))
        })?;
        let run_id = 0u64;
        let run_dir = run_path(&dir, SINGLE_VIDEO_RUNG, run_id);
        fs::create_dir_all(&run_dir).map_err(|e| {
            StartSessionError::Spawn(format!("create run dir {}: {e}", run_dir.display()))
        })?;
        // Release before ASS demux / ffmpeg spawn so a multi-minute NAS extract
        // does not freeze every other HLS request on this lock.
        drop(sessions);

        let spawn_started = Instant::now();
        let mut map_binding = MapBinding::new(keyframe_map);
        // ADR-0023 §9.3: a session create that arrives before the keyframe
        // map is ready waits, bounded, for the build the consumer already
        // triggered (playbackInfo or this session create), then uses the
        // fresh map. Position zero never waits — the Matroska path opens the
        // real file there. The §8 `-ss` fallback stays for genuine map
        // failure (§9.4), not for a bounded wait that has not yet elapsed.
        // The predicate is cloned out of the lock so the blocking wait below
        // never holds the registry mutex.
        if play_start_ms > 0 && map_binding.map.is_none() {
            let map_build_in_flight = self
                .map_build_in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            map_binding.map =
                wait_for_map_build(item_id, self.db.as_ref(), map_build_in_flight.as_ref());
        }
        let grid = grid_cadence_ms(
            mode,
            burn_in.is_some(),
            &self.encode_leg,
            &encode_plan,
            duration_ms,
        )
        .cadence();
        let want = grid.map_or(play_start_ms, |p| (play_start_ms / p) * p);
        let mut plan = map_binding.plan(src, want);
        if let Some(p) = grid {
            snap_plan_to_grid(&mut plan, p);
        }
        let start_ms = plan.window_start_ms;
        write_run_encode_start(&run_dir, start_ms).map_err(StartSessionError::Spawn)?;
        let burn_in =
            prepare_ass_burn_file(src, &dir, burn_in).map_err(StartSessionError::Spawn)?;
        let child = spawn_ffmpeg(
            &plan,
            &run_dir,
            mode,
            audio.clone(),
            &self.encode_leg,
            burn_in.as_ref(),
            encode_plan,
            piggyback.is_some(),
        )
        .map_err(StartSessionError::Spawn)?;
        map_binding.bound = plan.virtual_input.take();
        // Only cold (non-store) tracks need a session demux. Ready tracks
        // point MEDIA at the item VTT and must not re-read the source.
        spawn_session_subtitle_worker(src, &dir, &subtitle_tracks);
        let spawn_ms = spawn_started.elapsed().as_millis();
        tracing::info!(
            session_id = %id,
            item_id,
            start_ms,
            play_start_ms,
            run_id,
            encode_lead_segments = encode_lead_segments(),
            mode = ?mode,
            audio_stream = ?audio.stream_index,
            audio_channels = audio.channels,
            burn_in = burn_in.as_ref().map(|b| b.track_id.as_str()),
            encoder = %self.encode_leg.encoder,
            device = ?self.encode_leg.device,
            max_height = encode_plan.max_height,
            max_bitrate_bps = encode_plan.max_bitrate_bps,
            tone_map = encode_plan.tone_map,
            spawn_ms,
            start_path = plan.start_path,
            container_kind = plan.container_kind,
            fingerprint_cost_ms = plan.fingerprint_cost_ms,
            "hls session started"
        );
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| StartSessionError::Spawn("hls registry lock poisoned".into()))?;
        sessions.insert(
            id.clone(),
            Session {
                item_id,
                src: src.to_path_buf(),
                dir: dir.clone(),
                // One session budget is shared by every rung directory; it
                // is not reset for each rendition (ADR-0051 amendment 2).
                run_cache_budget_bytes: SESSION_RUN_CACHE_BUDGET_BYTES,
                mode,
                audio,
                burn_in,
                encode_plan,
                map_binding,
                encode_leg: self.encode_leg.clone(),
                video_encoder: self.encode_leg.encoder.clone(),
                start_ms,
                play_start_ms,
                landed_ms: start_ms,
                usable_extent_ms: None,
                duration_ms,
                segment_maps: single_rung_segment_maps(
                    crate::hls_segment_map::SegmentMap::default(),
                ),
                encoder_states: single_rung_encoder_states(run_id, 1, Some(child)),
                current_run_eof: false,
                last_access: Instant::now(),
                last_restart: Instant::now(),
                primed: false,
                first_segment_ready: false,
                pending_play_ms: None,
                pending_since: None,
                failed: None,
                subtitle_tracks,
                piggyback,
                subs: self.subs.clone(),
                db: self.db.clone(),
                map_build_in_flight: self
                    .map_build_in_flight
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
                // The playhead starts where the session was asked to start,
                // so a session created at a mid-title land does not read as
                // holding a title's worth of lead on its first tick.
                last_requested_ms: play_start_ms,
            },
        );
        Ok(id)
    }

    /// True when a spawn in this session held a keyframe map but still had
    /// to start with `-ss` (stale identity or a bind that would not hold).
    /// The caller enqueues a map rebuild (ADR-0023 §8).
    pub fn map_fallback(&self, session_id: &str) -> bool {
        self.sessions
            .lock()
            .ok()
            .and_then(|sessions| sessions.get(session_id).map(|s| s.map_binding.fell_back))
            .unwrap_or(false)
    }

    pub fn item_id(&self, session_id: &str) -> Option<i64> {
        self.sessions
            .lock()
            .ok()?
            .get(session_id)
            .map(|s| s.item_id)
    }

    pub fn encoder(&self, session_id: &str) -> Option<SessionEncoder> {
        let sessions = self.sessions.lock().ok()?;
        let session = sessions.get(session_id)?;
        Some(match session.mode {
            SessionMode::Copy => SessionEncoder {
                name: "copy".into(),
                kind: EncoderKind::Copy,
            },
            SessionMode::Transcode => SessionEncoder {
                name: session.video_encoder.clone(),
                kind: if session.video_encoder == "libx264" {
                    EncoderKind::Software
                } else {
                    EncoderKind::Hardware
                },
            },
        })
    }

    /// Returns the session's media playlist (ADR-0020, ADR-0054 decision 5).
    /// `start_ms` on this path is ignored for seek — use [`Self::seek`].
    pub fn playlist(&self, session_id: &str) -> Result<Vec<u8>, PlaylistError> {
        self.with_ready_session(session_id, |session| {
            let bytes = build_run_media_playlist(session_id, session, SINGLE_VIDEO_RUNG);
            log_playlist_serve(
                session_id,
                "index.m3u8",
                None,
                session.play_start_ms,
                session.pending_play_ms,
                &bytes,
            );
            Ok(bytes)
        })
    }

    /// Returns the session's HLS master playlist. Media and subtitle URIs are
    /// path-absolute under `/api/v0/sessions/…` (ADR-0008).
    pub fn master(&self, session_id: &str) -> Result<Vec<u8>, PlaylistError> {
        self.with_ready_session(session_id, |session| {
            let bytes = crate::hls_master::build_master(session_id, &session.subtitle_tracks);
            log_playlist_serve(
                session_id,
                "master.m3u8",
                None,
                session.play_start_ms,
                session.pending_play_ms,
                &bytes,
            );
            Ok(bytes)
        })
    }

    /// Apply a far scrub: new producer run + fresh playlist URI (ADR-0020).
    pub fn seek(&self, session_id: &str, start_ms: u64) -> Result<SessionView, PlaylistError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| PlaylistError::Failed("hls registry lock poisoned".into()))?;
        let session = sessions
            .get_mut(session_id)
            .ok_or(PlaylistError::NotFound)?;
        session.last_access = Instant::now();
        if let Some(err) = session.failed.clone() {
            return Err(PlaylistError::Failed(err));
        }
        let aligned = align_to_segment(start_ms);
        if aligned == session.play_start_ms {
            sync_segment_map(session, SINGLE_VIDEO_RUNG);
            return Ok(session_view(session_id, session, SINGLE_VIDEO_RUNG));
        }
        let leg = session.encode_leg.clone();
        // A seek always applies: nothing is destroyed, so nothing can be in
        // the way of destroying it (ADR-0050 §4).
        restart_at(session, aligned, &leg)?;
        maybe_evict_finished_runs(session);
        Ok(session_view(session_id, session, SINGLE_VIDEO_RUNG))
    }

    /// Current session wire snapshot (playlist URL, landed, usable extent).
    pub fn view(&self, session_id: &str) -> Result<SessionView, PlaylistError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| PlaylistError::Failed("hls registry lock poisoned".into()))?;
        let session = sessions
            .get_mut(session_id)
            .ok_or(PlaylistError::NotFound)?;
        session.last_access = Instant::now();
        let _ = note_child_exit(session);
        sync_segment_map(session, SINGLE_VIDEO_RUNG);
        Ok(session_view(session_id, session, SINGLE_VIDEO_RUNG))
    }

    /// Init (or other run-local file) under the requested run's rung directory.
    pub fn run_asset(
        &self,
        session_id: &str,
        run_id: u64,
        name: &str,
    ) -> Result<Vec<u8>, PlaylistError> {
        if name != "init.mp4" {
            return Err(PlaylistError::NotFound);
        }
        let deadline = Instant::now() + SEGMENT_WAIT;
        loop {
            {
                let mut sessions = self
                    .sessions
                    .lock()
                    .map_err(|_| PlaylistError::Failed("hls registry lock poisoned".into()))?;
                let session = sessions
                    .get_mut(session_id)
                    .ok_or(PlaylistError::NotFound)?;
                session.last_access = Instant::now();
                if let Some(err) = session.failed.clone() {
                    return Err(PlaylistError::Failed(err));
                }
                let path = run_path(&session.dir, SINGLE_VIDEO_RUNG, run_id).join("init.mp4");
                if let Ok(bytes) = fs::read(&path) {
                    return Ok(bytes);
                }
                if let Some(err) = note_child_exit(session) {
                    return Err(PlaylistError::Failed(err));
                }
            }
            if Instant::now() >= deadline {
                return Err(PlaylistError::NotReady);
            }
            std::thread::sleep(SEGMENT_POLL);
        }
    }

    /// Multi-segment subtitle media playlist for a snapshotted track (plan item 2).
    pub fn subtitle_playlist(
        &self,
        session_id: &str,
        track_id: &str,
    ) -> Result<Vec<u8>, PlaylistError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| PlaylistError::Failed("hls registry lock poisoned".into()))?;
        let session = sessions
            .get_mut(session_id)
            .ok_or(PlaylistError::NotFound)?;
        session.last_access = Instant::now();
        if !session
            .subtitle_tracks
            .iter()
            .any(|t| t.track_id == track_id)
        {
            return Err(PlaylistError::NotFound);
        }
        // Hold until video is ready so clients attach media + subs together.
        sync_segment_map(session, SINGLE_VIDEO_RUNG);
        if !current_run_has_mapped_segment(session, SINGLE_VIDEO_RUNG) {
            if let Some(err) = note_child_exit(session) {
                return Err(PlaylistError::Failed(err));
            }
            return Err(PlaylistError::NotReady);
        }
        let track = session
            .subtitle_tracks
            .iter()
            .find(|t| t.track_id == track_id)
            .expect("track checked above");
        Ok(build_subtitle_playlist_for(track, session.duration_ms))
    }

    /// Sliced WebVTT for one 2s window (`subs/{trackId}/segNNN.vtt`).
    pub fn subtitle_segment(
        &self,
        session_id: &str,
        track_id: &str,
        segment_idx: u64,
    ) -> Result<Vec<u8>, PlaylistError> {
        let deadline = Instant::now() + SEGMENT_WAIT;
        loop {
            let result = {
                let mut sessions = self
                    .sessions
                    .lock()
                    .map_err(|_| PlaylistError::Failed("hls registry lock poisoned".into()))?;
                let session = sessions
                    .get_mut(session_id)
                    .ok_or(PlaylistError::NotFound)?;
                session.last_access = Instant::now();
                if !session
                    .subtitle_tracks
                    .iter()
                    .any(|t| t.track_id == track_id)
                {
                    return Err(PlaylistError::NotFound);
                }
                read_subtitle_segment(session, track_id, segment_idx)
            };
            match result {
                Ok(bytes) => return Ok(bytes),
                Err(PlaylistError::NotReady) if Instant::now() < deadline => {
                    std::thread::sleep(SEGMENT_POLL);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Serves a playlist once the current run has something to list.
    ///
    /// **This used to refuse a URI whose run was not current, and that check is
    /// gone rather than relaxed.** It was not a choice: ADR-0054 decision 5 took
    /// the run out of both playlist paths, so there is no longer a run id to
    /// compare against `current_run_id`. Recorded here because a reader meeting
    /// the absence later would otherwise have to guess whether it was decided.
    ///
    /// **Nothing consumed it, and both clients misread it.** A 404 on a playlist
    /// URL is how each backend recognises a dead session and stops loading for
    /// good, so a stale-URI refusal produced a false "session gone" rather than
    /// a signal. `hlsPlayer.ts` had to stop the loader before teardown to keep
    /// hls.js off the 404 it would otherwise treat as fatal.
    ///
    /// What gates readiness is [`current_run_has_mapped_segment`], below, which
    /// is unaffected: a fetch before the new run produces still gets `NotReady`.
    fn with_ready_session<F>(&self, session_id: &str, build: F) -> Result<Vec<u8>, PlaylistError>
    where
        F: FnOnce(&Session) -> Result<Vec<u8>, PlaylistError>,
    {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| PlaylistError::Failed("hls registry lock poisoned".into()))?;
        let session = sessions
            .get_mut(session_id)
            .ok_or(PlaylistError::NotFound)?;
        session.last_access = Instant::now();

        if let Some(err) = session.failed.clone() {
            return Err(PlaylistError::Failed(err));
        }

        if let Some(err) = note_child_exit(session) {
            return Err(PlaylistError::Failed(err));
        }

        sync_segment_map(session, SINGLE_VIDEO_RUNG);
        if !current_run_has_mapped_segment(session, SINGLE_VIDEO_RUNG) {
            // Producer EOF with nothing in-window (damaged mid-title land):
            // serve empty ENDLIST playlists so the client can read
            // usableExtentMs instead of hanging on master 503.
            if session.current_run_eof {
                return build(session);
            }
            return Err(PlaylistError::NotReady);
        }
        note_first_segment_ready(session_id, session);
        maybe_apply_pending_restart(session)?;
        build(session)
    }

    /// Serves init/segment files. Retained segments from a previous encode
    /// window stay readable. Missing segments in a cold region of the
    /// full-title VOD return 503 while a guarded restart cooks them
    /// (ADR-0011 amendment). Safari native scrub often hits this path only.
    ///
    /// Logs terminal outcomes here (not only in the HTTP route after
    /// `.await`) so a client-aborted long-poll that still finishes cooking
    /// is visible as `hls asset ready` without a matching route 200.
    ///
    /// `fetcher` is log-only (optional `njFetcher` query): JS land-ensure /
    /// attach-wait probes set it; Safari's native HLS engine does not. Used
    /// to tell probe traffic from WebKit's own segment GETs in dogfood logs.
    /// (Native instant-503 while cooking was tried and rejected: broke
    /// fill-forward prefetch and double-scrub dig-back.)
    pub fn asset(
        &self,
        session_id: &str,
        name: &str,
        fetcher: Option<&str>,
    ) -> Result<Vec<u8>, PlaylistError> {
        let t0 = Instant::now();
        let result = self.asset_wait(session_id, name);
        // Always log not-ready/fail. Log ready only when we waited (long-poll /
        // cook) so aborted holds show up even if the HTTP route never runs;
        // skip hot-path disk hits (route 200 is enough).
        let waited = t0.elapsed() > Duration::from_millis(100);
        let fetcher = fetcher.unwrap_or("-");
        match &result {
            Ok(bytes) if waited => {
                tracing::info!(
                    session_id,
                    asset = %name,
                    fetcher,
                    bytes = bytes.len(),
                    waited_ms = t0.elapsed().as_millis(),
                    "hls asset ready"
                );
            }
            Ok(_) => {}
            Err(PlaylistError::NotReady) => {
                tracing::info!(
                    session_id,
                    asset = %name,
                    fetcher,
                    waited_ms = t0.elapsed().as_millis(),
                    "hls asset not ready"
                );
            }
            Err(PlaylistError::NotFound) => {
                tracing::info!(
                    session_id,
                    asset = %name,
                    fetcher,
                    "hls asset not found"
                );
            }
            Err(PlaylistError::AbandonedHoldEnded) => {
                tracing::info!(
                    session_id,
                    asset = %name,
                    fetcher,
                    waited_ms = t0.elapsed().as_millis(),
                    "hls asset abandoned hold ended"
                );
            }
            Err(PlaylistError::Failed(err)) => {
                tracing::warn!(
                    session_id,
                    asset = %name,
                    fetcher,
                    error = %err,
                    "hls asset failed"
                );
            }
        }
        result
    }

    fn asset_wait(&self, session_id: &str, name: &str) -> Result<Vec<u8>, PlaylistError> {
        if !is_safe_asset(name) {
            return Err(PlaylistError::NotFound);
        }
        let file_name = name.to_string();
        let requested_ms = crate::hls_segment_map::parse_time_keyed_segment_name(name);
        let mut deadline = Instant::now() + SEGMENT_WAIT;
        let mut holding_for_land = false;
        let mut holding_no_fill = false;
        // **Has this request already been held?** Set once the loop has slept a
        // poll without answering, which is the moment the session accepted the
        // want: it looked, decided the segment was still coming, and made the
        // client wait for it.
        //
        // **A want the session accepted is never 404 afterwards**
        // (ADR-0054 decision 3). 404 stays available on the *first* look, which
        // is the case decision 3 reserves it for - a URI outside the title or
        // off the grid, refused before anyone waits on it.
        let mut accepted_hold = false;
        let enter_no_fill = |reason: &str,
                             session_id: &str,
                             file_name: &str,
                             session: &Session,
                             holding_no_fill: &mut bool,
                             holding_for_land: &mut bool,
                             deadline: &mut Instant| {
            if !*holding_no_fill {
                *holding_no_fill = true;
                *holding_for_land = false;
                *deadline = Instant::now() + IDLE_TIMEOUT;
                tracing::info!(
                    session_id,
                    asset = %file_name,
                    play_start_ms = session.play_start_ms,
                    pending_play_ms = session.pending_play_ms,
                    hold_ms = IDLE_TIMEOUT.as_millis(),
                    reason,
                    "hls asset no-fill hold"
                );
            }
        };
        loop {
            {
                let mut sessions = self
                    .sessions
                    .lock()
                    .map_err(|_| PlaylistError::Failed("hls registry lock poisoned".into()))?;
                let session = sessions
                    .get_mut(session_id)
                    .ok_or(PlaylistError::NotFound)?;
                session.last_access = Instant::now();
                if let Some(ms) = requested_ms {
                    // Monotonic: a prefetch that runs ahead moves the
                    // playhead, a scrub back does not rewind it. A seek
                    // resets it explicitly, where the land is known.
                    session.last_requested_ms = session.last_requested_ms.max(ms);
                }
                if let Some(err) = session.failed.clone() {
                    return Err(PlaylistError::Failed(err));
                }
                note_first_segment_ready(session_id, session);
                if let Some(err) = session.failed.clone() {
                    return Err(PlaylistError::Failed(err));
                }
                if let Some(want_ms) = requested_ms
                    && holding_for_land
                {
                    let superseded = pending_waiter_action(session.pending_play_ms, want_ms)
                        == PendingWaiterAction::Release
                        || (session.pending_play_ms.is_none()
                            && align_to_segment(session.play_start_ms)
                                != align_to_segment(want_ms));
                    if superseded {
                        let far = match session.pending_play_ms {
                            Some(p) => coalesce_preempt_before_land(want_ms, p),
                            None => coalesce_preempt_before_land(want_ms, session.play_start_ms),
                        };
                        if far {
                            tracing::info!(
                                session_id,
                                asset = %file_name,
                                play_start_ms = session.play_start_ms,
                                pending_play_ms = session.pending_play_ms,
                                want_ms,
                                "hls asset superseded far — 503 (no hold)"
                            );
                            return Err(PlaylistError::NotReady);
                        }
                        enter_no_fill(
                            "superseded",
                            session_id,
                            &file_name,
                            session,
                            &mut holding_no_fill,
                            &mut holding_for_land,
                            &mut deadline,
                        );
                    }
                }

                let resolved = if file_name == "init.mp4" {
                    fs::read(run_dir(session, SINGLE_VIDEO_RUNG).join("init.mp4")).ok()
                } else if let Some(ms) = requested_ms {
                    sync_segment_map(session, SINGLE_VIDEO_RUNG);
                    // The gate is what keeps this honest. Outside the
                    // REAP_AFTER window after a seek the held set is empty and
                    // this costs one `is_empty()`; inside it, one file read per
                    // held encoder, on the only path where the ingest matters.
                    // The throttle tick would do the same work every 250 ms
                    // whether or not anyone is waiting, and would hand the
                    // waiter its bytes up to a tick late.
                    if !session
                        .encoder_state(SINGLE_VIDEO_RUNG)
                        .superseded
                        .is_empty()
                    {
                        sync_superseded_run_indexes(session, SINGLE_VIDEO_RUNG);
                    }
                    match session
                        .segment_map(SINGLE_VIDEO_RUNG)
                        .get(ms)
                        .map(|seg| seg.rel_path.clone())
                    {
                        Some(rel_path) => {
                            let abs = session.dir.join(rel_path);
                            match fs::read(&abs) {
                                Ok(bytes) => Some(bytes),
                                Err(_) => {
                                    // Map entry without bytes — drop this key
                                    // so we never keep advertising a dead URI.
                                    session.segment_map_mut(SINGLE_VIDEO_RUNG).remove_start(ms);
                                    None
                                }
                            }
                        }
                        None => None,
                    }
                } else {
                    None
                };

                if let Some(bytes) = resolved {
                    let play_before = session.play_start_ms;
                    if let Some(ms) = requested_ms
                        && ms >= session.start_ms
                    {
                        session.primed = true;
                    }
                    note_first_segment_ready(session_id, session);
                    maybe_apply_pending_restart(session)?;
                    if !serve_ok_after_pending_apply(
                        play_before,
                        session.play_start_ms,
                        requested_ms,
                    ) {
                        return Err(PlaylistError::NotReady);
                    }
                    return Ok(bytes);
                }
                if let Some(err) = note_child_exit(session) {
                    return Err(PlaylistError::Failed(err));
                }
                maybe_apply_pending_restart(session)?;
                if file_name == "init.mp4" {
                    // Rewritten on restart; wait for the new init.
                } else if let Some(want_ms) = requested_ms {
                    let window_start = session.start_ms;
                    let play_start = session.play_start_ms;
                    let latest = latest_segment_in_window(
                        session.segment_map(SINGLE_VIDEO_RUNG),
                        window_start,
                    );
                    let since = session.last_restart.elapsed();

                    if holding_no_fill
                        || segment_miss_unreachable(
                            want_ms,
                            session.play_start_ms,
                            session.pending_play_ms,
                            session.start_ms,
                            session.play_start_ms,
                            latest,
                            session.primed,
                            want_is_listed(session, want_ms),
                        )
                    {
                        if !holding_no_fill {
                            enter_no_fill(
                                "abandoned",
                                session_id,
                                &file_name,
                                session,
                                &mut holding_no_fill,
                                &mut holding_for_land,
                                &mut deadline,
                            );
                        } else if no_fill_release_for_new_land(
                            want_ms,
                            session.play_start_ms,
                            session.first_segment_ready,
                            session.start_ms,
                        ) {
                            tracing::info!(
                                session_id,
                                asset = %file_name,
                                play_start_ms = session.play_start_ms,
                                want_ms,
                                "hls asset no-fill release (new land ready)"
                            );
                            return Err(PlaylistError::NotReady);
                        }
                    } else {
                        let scrub_shaped = decide_segment_miss(
                            want_ms,
                            window_start,
                            play_start,
                            latest,
                            session.primed,
                            RESTART_MIN_INTERVAL,
                        ) == SegmentMissAction::Restart;
                        match decide_segment_miss(
                            want_ms,
                            window_start,
                            play_start,
                            latest,
                            session.primed,
                            since,
                        ) {
                            SegmentMissAction::Restart => {
                                if prefetch_advances_pending(session.pending_play_ms, want_ms) {
                                    return Err(PlaylistError::NotReady);
                                }
                                // `!want_is_listed` is the S3-vacuous half of
                                // this guard: under a full-title listing every
                                // want is listed, so this branch never runs
                                // and the supersede handling inside it never
                                // fires. **Left standing deliberately.** The
                                // case it protected — an unlisted want behind
                                // the window — is now answered earlier, by
                                // `segment_miss_unreachable`, which holds it
                                // before the match is reached. Deleting the
                                // condition here would route a *listed* want
                                // into supersede handling, which is the
                                // opposite of entry 13's decision. It stays,
                                // narrowed in meaning rather than silently
                                // dead: unlisted, not behind-window.
                                if digback_behind_committed(
                                    session.play_start_ms,
                                    session.pending_play_ms,
                                    want_ms,
                                ) && !want_is_listed(session, want_ms)
                                {
                                    if pending_waiter_action(session.pending_play_ms, want_ms)
                                        == PendingWaiterAction::Release
                                    {
                                        enter_no_fill(
                                            "superseded",
                                            session_id,
                                            &file_name,
                                            session,
                                            &mut holding_no_fill,
                                            &mut holding_for_land,
                                            &mut deadline,
                                        );
                                    } else if session.pending_play_ms.is_none()
                                        && align_to_segment(session.play_start_ms)
                                            != align_to_segment(want_ms)
                                    {
                                        return Err(PlaylistError::NotReady);
                                    }
                                } else {
                                    desire_restart(session, want_ms);
                                    holding_for_land = true;
                                    maybe_apply_pending_restart(session)?;
                                    deadline = Instant::now() + SEGMENT_WAIT;
                                }
                            }
                            SegmentMissAction::Wait => {
                                if scrub_shaped
                                    && !prefetch_advances_pending(session.pending_play_ms, want_ms)
                                    && !(digback_behind_committed(
                                        session.play_start_ms,
                                        session.pending_play_ms,
                                        want_ms,
                                    ) && !want_is_listed(session, want_ms))
                                {
                                    desire_restart(session, want_ms);
                                    holding_for_land = true;
                                }
                                maybe_apply_pending_restart(session)?;
                                if pending_waiter_action(session.pending_play_ms, want_ms)
                                    == PendingWaiterAction::Release
                                {
                                    enter_no_fill(
                                        "superseded",
                                        session_id,
                                        &file_name,
                                        session,
                                        &mut holding_no_fill,
                                        &mut holding_for_land,
                                        &mut deadline,
                                    );
                                } else if session.encoder_state(SINGLE_VIDEO_RUNG).child.is_none() {
                                    return Err(miss_refusal(accepted_hold));
                                } else if want_ms < window_start
                                    && !want_is_listed(session, want_ms)
                                {
                                    // Producer-truth: a URI behind the cooking
                                    // window was never listed *for this run*.
                                    //
                                    // The third site carrying ADR-0020's miss
                                    // policy, narrowed the same way as the
                                    // other two. With a full-title listing the
                                    // session did list it, so a 404 here
                                    // refuses a URI the playlist offers. An
                                    // unlisted want behind the window still
                                    // 404s, which is what this line was for.
                                    //
                                    // **`want_is_listed` is vacuously false
                                    // when the session has no honest grid**, so
                                    // the narrowing above protects nothing in
                                    // exactly the regime that needs it: a seek
                                    // moving the window turns a hold this
                                    // session already accepted into a 404. The
                                    // client cannot tell that session apart
                                    // from any other - hls.js and Safari
                                    // abandon the fragment on 404 whatever the
                                    // server's listing model says - so
                                    // acceptance decides here, not the listing.
                                    return Err(miss_refusal(accepted_hold));
                                }
                            }
                        }
                    }
                }
            }
            if Instant::now() >= deadline {
                if holding_no_fill {
                    return Err(PlaylistError::AbandonedHoldEnded);
                }
                return Err(PlaylistError::NotReady);
            }
            accepted_hold = true;
            std::thread::sleep(SEGMENT_POLL);
        }
    }

    /// Stops the session this player owns. One session per POST, so there is
    /// no other holder to consider.
    pub fn stop(&self, session_id: &str) -> bool {
        let mut sessions = match self.sessions.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        let Some(mut session) = sessions.remove(session_id) else {
            return false;
        };
        let rungs: Vec<VideoRung> = session.encoder_states.keys().copied().collect();
        for rung in rungs {
            stop_child(&mut session.encoder_state_mut(rung).child);
            reap_all_superseded(&mut session);
        }
        if let Err(e) = fs::remove_dir_all(&session.dir) {
            tracing::warn!(
                path = %session.dir.display(),
                error = %e,
                "hls session dir cleanup failed"
            );
        }
        tracing::info!(session_id, "hls session stopped");
        true
    }

    /// Sample current and held encoder RSS without keeping the registry lock
    /// across `/proc` reads. The 250 ms worker already visits every child, but
    /// a procfs read is still a syscall; doing it inline would serialize that
    /// I/O behind segment-serving paths that need the same lock.
    #[cfg(target_os = "linux")]
    fn sample_encoder_rss(&self) {
        let processes = {
            let Ok(sessions) = self.sessions.lock() else {
                return;
            };
            let mut processes = Vec::new();
            for (session_id, session) in sessions.iter() {
                for (&rung, state) in &session.encoder_states {
                    if let Some(child) = state.child.as_ref() {
                        processes.push(EncoderProcess {
                            session_id: session_id.clone(),
                            rung,
                            pid: child.id(),
                            superseded_run_id: None,
                        });
                    }
                    processes.extend(state.superseded.iter().map(|held| EncoderProcess {
                        session_id: session_id.clone(),
                        rung,
                        pid: held.child.id(),
                        superseded_run_id: Some(held.run_id),
                    }));
                }
            }
            processes
        };

        let mut samples = Vec::with_capacity(processes.len());
        for process in processes {
            match read_child_rss(process.pid) {
                Ok(rss_bytes) => samples.push((process, Some(rss_bytes))),
                Err(error) => {
                    tracing::warn!(
                        session_id = %process.session_id,
                        pid = process.pid,
                        error = %error,
                        "hls encoder RSS sample failed"
                    );
                    samples.push((process, None));
                }
            }
        }

        let Ok(mut sessions) = self.sessions.lock() else {
            return;
        };
        for (process, rss_bytes) in samples {
            record_encoder_rss_sample(
                &mut sessions,
                &self.encoder_rss_high_water_bytes,
                &process,
                rss_bytes,
            );
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn sample_encoder_rss(&self) {}

    /// Idle and failed sessions are reaped without a DELETE. Crashed or
    /// sleeping tabs never send one; without this Gate 2's zero-orphan
    /// criterion fails 48 hours later.
    /// Hold each session's encoder at [`LEAD_TARGET_MS`] and let it run again
    /// at [`LEAD_FLOOR_MS`] (ADR-0050 §2, §3).
    ///
    /// Suspending is what makes a lead a lead. An unthrottled encoder runs to
    /// EOF and produces media nobody has asked for. `-readrate` was measured
    /// as the declarative alternative and rejected, because it paces against
    /// wall clock and so cannot see a viewer who has stopped: in a 400 s pause
    /// its lead grew from 40 s to 418 s and never recovered.
    ///
    /// Applies to copy and remux sessions too. A remux is also one long
    /// FFmpeg holding a lead, and one concept gets one path (Rule 4.11).
    ///
    /// **This runs in tests.** The playhead only moves when a segment is
    /// fetched through [`HlsSessionRegistry::asset`], so a test that drives
    /// production without fetching never advances it, reaches
    /// [`LEAD_TARGET_MS`] and has its encoder suspended. That presents as a
    /// hang rather than as a throttle. No test does this today. If one starts
    /// to, fetch the segments rather than reaching for a switch to turn this
    /// off: a knob here would be standing in for a decision already made
    /// (Rule 4.12).
    fn throttle_loop(&self) {
        loop {
            std::thread::sleep(THROTTLE_TICK);
            self.sample_encoder_rss();
            let Ok(mut sessions) = self.sessions.lock() else {
                continue;
            };
            for (id, session) in sessions.iter_mut() {
                // The 250 ms tick already walks every session under the lock,
                // so the reap rides along rather than taking its own thread.
                let rungs: Vec<VideoRung> = session.encoder_states.keys().copied().collect();
                for rung in rungs {
                    reap_superseded(session);
                    let state = session.encoder_state(rung);
                    let Some(child) = state.child.as_ref() else {
                        // No producer: a finished run holds no lead, and a child
                        // that exited while suspended must not stay marked.
                        session.encoder_state_mut(rung).throttled = false;
                        continue;
                    };
                    let Some(lead_ms) = session_lead_ms(session) else {
                        continue;
                    };
                    let Some(stop) = throttle_action(state.throttled, lead_ms) else {
                        continue;
                    };
                    if signal_child(child, stop) {
                        session.encoder_state_mut(rung).throttled = stop;
                        let action = if stop { "suspend" } else { "resume" };
                        tracing::debug!(session_id = %id, lead_ms, action, "hls throttle");
                    }
                }
            }
        }
    }

    fn reaper_loop(&self) {
        loop {
            std::thread::sleep(REAPER_TICK);
            let stale: Vec<String> = {
                let Ok(sessions) = self.sessions.lock() else {
                    continue;
                };
                sessions
                    .iter()
                    .filter(|(_, s)| s.last_access.elapsed() > IDLE_TIMEOUT || s.failed.is_some())
                    .map(|(id, _)| id.clone())
                    .collect()
            };
            for id in stale {
                tracing::info!(session_id = %id, "hls session idle or failed force-reap");
                self.force_stop(&id);
            }
        }
    }

    fn force_stop(&self, session_id: &str) {
        let mut sessions = match self.sessions.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let Some(mut session) = sessions.remove(session_id) else {
            return;
        };
        let rungs: Vec<VideoRung> = session.encoder_states.keys().copied().collect();
        for rung in rungs {
            stop_child(&mut session.encoder_state_mut(rung).child);
            reap_all_superseded(&mut session);
        }
        let _ = fs::remove_dir_all(&session.dir);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEncoder {
    pub name: String,
    pub kind: EncoderKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderKind {
    Hardware,
    Software,
    Copy,
}

impl EncoderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hardware => "hardware",
            Self::Software => "software",
            Self::Copy => "copy",
        }
    }
}

fn restart_at(
    session: &mut Session,
    play_ms: u64,
    _encode_leg: &crate::EncodeLeg,
) -> Result<(), PlaylistError> {
    let play_start_ms = align_to_segment(play_ms);
    let prior_play = session.play_start_ms;
    let prior_ready = session.first_segment_ready;
    // No gate before the kill, because there is no kill. This used to ask
    // whether a client still held the cooking land, and defer if so, because
    // killing the encoder would strand that waiter. The encoder is now kept
    // and left running (ADR-0050 §4-§5), so it finishes the segment the
    // waiter is holding for and the question cannot arise.
    tracing::info!(
        prior_play_start_ms = prior_play,
        prior_first_segment_ready = prior_ready,
        superseding_encoder = session.encoder_state(SINGLE_VIDEO_RUNG).child.is_some(),
        held_encoders = session.encoder_state(SINGLE_VIDEO_RUNG).superseded.len(),
        new_play_start_ms = play_start_ms,
        "hls seek: supersede prior encode"
    );
    supersede_child(session);
    // `throttled` describes a live process. This session keeps going with a
    // new child, so leaving it set would make the next tick send a resume to
    // a child that was never suspended and skip the suspend it needs. The
    // other two `stop_child` callers remove the session outright.
    session.encoder_state_mut(SINGLE_VIDEO_RUNG).throttled = false;
    sync_segment_map(session, SINGLE_VIDEO_RUNG);
    sync_all_run_indexes(session);
    // Duplicate-write stop: scrub-back (or re-land) into media this rung's map
    // already holds — mint a fresh playlist URI, copy init, do not re-encode.
    if let Some(mapped) = map_segment_covering(session, play_start_ms) {
        let src_run = mapped.run_id;
        let run_id = session.encoder_state(SINGLE_VIDEO_RUNG).next_run_id;
        session.encoder_state_mut(SINGLE_VIDEO_RUNG).next_run_id += 1;
        let new_dir = run_path(&session.dir, SINGLE_VIDEO_RUNG, run_id);
        fs::create_dir_all(&new_dir).map_err(|e| {
            PlaylistError::Failed(format!("create run dir {}: {e}", new_dir.display()))
        })?;
        // Copy `src_run`'s init, not any run's: an init is not interchangeable.
        //
        // ADR-0054 decision 4 said inits are byte-identical across runs and
        // this copy was written on that. **It is false**, and was overturned
        // 2026-08-31: `-output_ts_offset` stamps the land into the init's
        // `elst` empty-edit, so two runs at different lands differ there by
        // construction. Measured on all four paths — QSV, libx264,
        // VideoToolbox, copy — every one distinct. On libx264 the whole file
        // differs by two bytes, one per track, both inside `elst`.
        //
        // **The copy is right for the segments this run will serve at its own
        // land, and wrong for segments from any other run.** Joining one run's
        // init to another run's segment **decodes cleanly** and reports the
        // *init's* start time: a segment holding the first two seconds of the
        // title, under a 60 s run's init, presents at 60 s. `elst` is a timing
        // field, so a test that asked only whether it decodes passes all four
        // pairings and calls this safe.
        //
        // **That pairing is reachable.** `EXT-X-MAP` names
        // `session.current_run_id`'s init while a playlist lists segments from
        // this rung's map, which holds whatever every live run wrote.
        // It predates S3 — the window listing filtered on time, never on run —
        // and S3 widened it from a window to the whole title.
        //
        // **What is not measured is the client.** The displacement above is
        // FFmpeg's. A browser appends to a MediaSource with one init per
        // track, but `tfdt` stays segment-local at 0 while `sidx` carries
        // title time, and which of those a player uses for placement has never
        // been measured here. **So this is not "the map-hit path is broken";
        // it is that the reason it was believed safe is false and the failure
        // mode is real in the one instrument used.** Do not restore the
        // identity claim without a player. Measured 2026-08-31 on `d341cc9`,
        // macOS Safari native and hls.js, across four spawned runs.
        let init_src = run_path(&session.dir, SINGLE_VIDEO_RUNG, src_run).join("init.mp4");
        let init_dst = new_dir.join("init.mp4");
        if init_src.exists() {
            fs::copy(&init_src, &init_dst).map_err(|e| {
                PlaylistError::Failed(format!(
                    "copy init {} -> {}: {e}",
                    init_src.display(),
                    init_dst.display()
                ))
            })?;
        }
        session.encoder_state_mut(SINGLE_VIDEO_RUNG).current_run_id = run_id;
        session.current_run_eof = true;
        session.start_ms = play_start_ms;
        session.play_start_ms = play_start_ms;
        // A seek moves the playhead, including backwards. Left at the old
        // high-water mark the lead reads as zero for the rest of the session
        // and the throttle never fires again (ADR-0050 §2).
        session.last_requested_ms = play_start_ms;
        session.landed_ms = mapped.start_ms;
        session.failed = None;
        session.last_restart = Instant::now();
        session.primed = true;
        session.first_segment_ready = true;
        if session.pending_play_ms == Some(play_start_ms) {
            session.pending_play_ms = None;
            session.pending_since = None;
        }
        release_overtaken_superseded(session);
        maybe_evict_finished_runs(session);
        tracing::info!(
            play_start_ms,
            run_id,
            src_run_id = src_run,
            mapped_start_ms = mapped.start_ms,
            session_disk_bytes = session_disk_bytes(session),
            path = %session.src.display(),
            "hls session seek map hit (duplicate-write stop)"
        );
        return Ok(());
    }
    if let Some(gap) = restart_spawn_gap() {
        tracing::info!(
            gap_ms = gap.as_millis(),
            play_start_ms,
            "hls restart spawn gap (NIGHTJAR_RESTART_SPAWN_GAP_MS)"
        );
        std::thread::sleep(gap);
    }
    // Gate 2 / fill-forward: do not wipe prior run dirs. Scrub-back into
    // mapped media is a plain file serve (ADR-0020 per-rung map). New producer
    // output goes in a fresh run directory under the active rung.
    let run_id = session.encoder_state(SINGLE_VIDEO_RUNG).next_run_id;
    session.encoder_state_mut(SINGLE_VIDEO_RUNG).next_run_id += 1;
    let run_dir = run_path(&session.dir, SINGLE_VIDEO_RUNG, run_id);
    fs::create_dir_all(&run_dir)
        .map_err(|e| PlaylistError::Failed(format!("create run dir {}: {e}", run_dir.display())))?;
    // ADR-0023 §9.3: a seek that arrives before the keyframe map is ready
    // waits, bounded, for the build the consumer already triggered
    // (playbackInfo or session create), then uses the fresh map. Position
    // zero never waits — the Matroska path opens the real file there. The
    // §8 `-ss` fallback stays for genuine map failure (§9.4), not for a
    // bounded wait that has not yet elapsed.
    if play_start_ms > 0 && session.map_binding.map.is_none() {
        session.map_binding.map = wait_for_map_build(
            session.item_id,
            session.db.as_ref(),
            session.map_build_in_flight.as_ref(),
        );
    }
    // Bind at the grid point at or before the land, not at the land, so the
    // cue this snaps to is at or before a grid point and the snap up can
    // never overshoot the play land.
    let grid = session_grid_cadence(session).cadence();
    let want = grid.map_or(play_start_ms, |p| (play_start_ms / p) * p);
    let mut plan = session.map_binding.plan(&session.src, want);
    if let Some(p) = grid {
        snap_plan_to_grid(&mut plan, p);
    }
    let start_ms = plan.window_start_ms;
    write_run_encode_start(&run_dir, start_ms).map_err(PlaylistError::Failed)?;
    let burn_in = prepare_ass_burn_file(&session.src, &session.dir, session.burn_in.clone())
        .map_err(PlaylistError::Failed)?;
    session.burn_in = burn_in;
    let child = spawn_ffmpeg(
        &plan,
        &run_dir,
        session.mode,
        session.audio.clone(),
        &session.encode_leg,
        session.burn_in.as_ref(),
        session.encode_plan,
        session.piggyback.is_some(),
    )
    .map_err(PlaylistError::Failed)?;
    session.map_binding.bound = plan.virtual_input.take();
    let state = session.encoder_state_mut(SINGLE_VIDEO_RUNG);
    state.child = Some(child);
    state.child_rss_bytes = None;
    session.encoder_state_mut(SINGLE_VIDEO_RUNG).current_run_id = run_id;
    session.current_run_eof = false;
    session.start_ms = start_ms;
    session.play_start_ms = play_start_ms;
    session.last_requested_ms = play_start_ms;
    session.landed_ms = start_ms;
    session.failed = None;
    session.last_restart = Instant::now();
    session.primed = false;
    session.first_segment_ready = false;
    if session.pending_play_ms == Some(play_start_ms) {
        session.pending_play_ms = None;
        session.pending_since = None;
    }
    release_overtaken_superseded(session);
    maybe_evict_finished_runs(session);
    tracing::info!(
        start_ms,
        play_start_ms,
        run_id,
        session_disk_bytes = session_disk_bytes(session),
        encoder = %session.encode_leg.encoder,
        device = ?session.encode_leg.device,
        start_path = plan.start_path,
        container_kind = plan.container_kind,
        fingerprint_cost_ms = plan.fingerprint_cost_ms,
        path = %session.src.display(),
        "hls session seek restart"
    );
    Ok(())
}

/// Mapped segment that already covers title-absolute `play_ms`.
///
/// Producer sidx can land a few tens of ms after `-ss` (dogfood: start 80 for
/// play 0). Treat the first mapped segment at or after `play` that starts
/// before `play + 2*SEGMENT_MS` as a hit so scrub-back does not re-encode.
fn map_segment_covering(
    session: &Session,
    play_ms: u64,
) -> Option<crate::hls_segment_map::MappedSegment> {
    let play = align_to_segment(play_ms);
    if let Some(exact) = session.segment_map(SINGLE_VIDEO_RUNG).get(play) {
        return Some(exact.clone());
    }
    if let Some(s) = session
        .segment_map(SINGLE_VIDEO_RUNG)
        .iter_ordered()
        .find(|s| {
            let end = s.start_ms.saturating_add(s.duration_ms.max(1));
            s.start_ms <= play && play < end
        })
    {
        return Some(s.clone());
    }
    let slack = SEGMENT_MS.saturating_mul(2);
    session
        .segment_map(SINGLE_VIDEO_RUNG)
        .iter_ordered()
        .find(|s| s.start_ms >= play && s.start_ms < play.saturating_add(slack))
        .cloned()
}

/// Record scrub intent. In-flight encodes keep cooking until land (or a far
/// pending preempts after [`RESTART_MIN_INTERVAL`]); after land, rapid intents
/// debounce into one restart (see [`RESTART_COALESCE_QUIET`]).
fn desire_restart(session: &mut Session, want_play_ms: u64) {
    let aligned = align_to_segment(want_play_ms);
    match classify_restart_desire(
        aligned,
        session.play_start_ms,
        session.start_ms,
        session.first_segment_ready,
    ) {
        CoalesceDesire::Nop => {
            if session.pending_play_ms == Some(aligned) {
                session.pending_play_ms = None;
                session.pending_since = None;
            }
        }
        CoalesceDesire::HoldInFlight => {
            // Same target: keep the pending clock so land still applies once.
            if session.pending_play_ms != Some(aligned) {
                session.pending_play_ms = Some(aligned);
                session.pending_since = Some(Instant::now());
                tracing::info!(
                    pending_play_ms = aligned,
                    cooking_play_ms = session.play_start_ms,
                    "hls seek restart coalesced (in flight)"
                );
            }
        }
        CoalesceDesire::HoldDebounce => {
            // Same target: do not reset the quiet clock on every 503 retry
            // (that would never elapse RESTART_COALESCE_QUIET).
            if session.pending_play_ms != Some(aligned) {
                session.pending_play_ms = Some(aligned);
                session.pending_since = Some(Instant::now());
                tracing::info!(
                    pending_play_ms = aligned,
                    play_start_ms = session.play_start_ms,
                    "hls seek restart coalesced (debounce)"
                );
            }
        }
    }
}

fn maybe_apply_pending_restart(session: &mut Session) -> Result<(), PlaylistError> {
    let elapsed = session.pending_since.map(|t| t.elapsed());
    // `pending_since == None` with a pending target means apply immediately
    // (used right after first_segment_ready for in-flight coalesce).
    let apply_immediate = session.pending_play_ms.is_some() && session.pending_since.is_none();
    let ready = session.first_segment_ready;
    let cooking = session.play_start_ms;
    let since = session.last_restart.elapsed();
    let allow_preempt = !disable_preempt();
    let Some(pending) = pending_restart_due(
        ready,
        session.pending_play_ms,
        elapsed,
        apply_immediate,
        cooking,
        since,
        allow_preempt,
    ) else {
        return Ok(());
    };
    // Do not clear pending before restart_at: a deferred land-waiter kill
    // must leave the far target recorded for land-then-yank.
    if encode_start_ms(pending) == session.start_ms && pending == session.play_start_ms {
        session.pending_play_ms = None;
        session.pending_since = None;
        return Ok(());
    }
    let preempt_before_land = !ready
        && allow_preempt
        && coalesce_preempt_before_land(cooking, pending)
        && since >= RESTART_MIN_INTERVAL;
    let leg = session.encode_leg.clone();
    restart_at(session, pending, &leg)?;
    if preempt_before_land {
        tracing::info!(
            pending_play_ms = pending,
            cooking_play_ms = cooking,
            since_last_restart_ms = since.as_millis(),
            "hls seek restart preempted (before land)"
        );
    }
    // restart_at clears pending when it matches the new play; clear any
    // leftover (e.g. already applied path).
    if session.pending_play_ms == Some(pending) {
        session.pending_play_ms = None;
        session.pending_since = None;
    }
    Ok(())
}

/// Logs once when the **play land** segment appears (not merely the lead-in
/// first window). Pending scrub apply waits for this when the new target is
/// near the cooking land so a coalesced restart does not yank before that
/// land exists — that left Safari retrying the prior land seg forever
/// (dogfood: seg415 after scrub to 1188). Far pending may preempt earlier
/// via [`coalesce_preempt_before_land`] once [`RESTART_MIN_INTERVAL`] elapses.
///
/// Called from playlist serve and from every `asset_wait` poll — not only when
/// the requested URI is the cooking land. Middle waiters may enter no-fill
/// before a 200 on that URI; final land-ensure must still notice.
fn note_first_segment_ready(session_id: &str, session: &mut Session) {
    sync_segment_map(session, SINGLE_VIDEO_RUNG);
    if session.first_segment_ready {
        return;
    }
    if !current_run_has_mapped_segment(session, SINGLE_VIDEO_RUNG) {
        return;
    }
    if let Some(landed) = first_current_run_start(session, SINGLE_VIDEO_RUNG) {
        session.landed_ms = landed;
    }
    session.first_segment_ready = true;
    let elapsed_ms = session.last_restart.elapsed().as_millis();
    let lead_ms = session.play_start_ms.saturating_sub(session.start_ms);
    let disk_bytes = session_disk_bytes(session);
    tracing::info!(
        session_id,
        elapsed_ms,
        start_ms = session.start_ms,
        play_start_ms = session.play_start_ms,
        landed_ms = session.landed_ms,
        lead_ms,
        session_disk_bytes = disk_bytes,
        encoder = %session.video_encoder,
        path = %session.src.display(),
        "hls_session_first_segment_ready"
    );
    // Scrubs that arrived while this encode was landing: apply latest now
    // (do not wait for debounce quiet — the client already waited on land).
    if session.pending_play_ms.is_some() {
        session.pending_since = None;
        if let Err(e) = maybe_apply_pending_restart(session) {
            session.failed = Some(match e {
                PlaylistError::Failed(msg) => msg,
                PlaylistError::NotFound => "pending seek restart: not found".into(),
                PlaylistError::NotReady => "pending seek restart: not ready".into(),
                PlaylistError::AbandonedHoldEnded => {
                    "pending seek restart: abandoned hold ended".into()
                }
            });
        }
    }
}

fn note_child_exit(session: &mut Session) -> Option<String> {
    let child = session
        .encoder_state_mut(SINGLE_VIDEO_RUNG)
        .child
        .as_mut()?;
    match child.try_wait() {
        Ok(Some(status)) if !status.success() => {
            let msg = format!("ffmpeg exited with {status}");
            session.failed = Some(msg.clone());
            Some(msg)
        }
        Ok(Some(_)) => {
            apply_run_eof(session);
            None
        }
        Ok(None) => None,
        Err(e) => {
            let msg = format!("ffmpeg wait: {e}");
            session.failed = Some(msg.clone());
            Some(msg)
        }
    }
}

/// Producer reached EOF: mark ENDLIST and record usable extent when the
/// farthest mapped end (or 0 if nothing was written) is materially short of
/// claimed duration. Empty map at a mid-title land is still damage — clients
/// must see usableExtentMs instead of hanging on master 503.
///
/// **The extent is this rung's map maximum, deliberately, not the ended
/// run's frontier.** #180 changed it to the per-run frontier on the reasoning
/// that the map holds every live run since #160, so the maximum could come
/// from a superseded run. That is true of the code and does not reach this
/// function:
///
/// - **This runs only on a clean exit.** [`note_child_exit`] routes a non-zero
///   status to `session.failed`. A run that exits cleanly read to the end of
///   its input, so it covers `[its land, media end]`, and **every run that
///   ends normally has the same frontier as every other**. A run that dies at
///   a hole in the source exits non-zero and never arrives here.
/// - **Eviction cannot shorten it.** [`live_run_ids`] includes
///   `current_run_id`, so a live run's segments are never evicted.
///
/// So the two readings differ in exactly one reachable shape: **the current
/// run exits clean having produced nothing**, which is a seek landing at or
/// past the true media end of a title claiming more. There the per-run
/// frontier is 0, `scrubRangeMs` returns it over `item.durationMs`, and
/// **nothing ever clears this field** — so the scrub bar collapses to zero for
/// the life of the session, surviving a seek back that plays fine. The
/// session maximum reports the furthest point known reachable, which is what
/// both the scrubber and a full-title listing want.
///
/// `usable_extent_zero_only_when_the_session_produced_nothing` pins that
/// shape. Reverted 2026-08-30; the reasoning is kept here so the change is not
/// made a second time from the same argument.
fn apply_run_eof(session: &mut Session) {
    let state = session.encoder_state_mut(SINGLE_VIDEO_RUNG);
    state.child = None;
    state.child_rss_bytes = None;
    session.current_run_eof = true;
    sync_segment_map(session, SINGLE_VIDEO_RUNG);
    let end = session
        .segment_map(SINGLE_VIDEO_RUNG)
        .iter_ordered()
        .next_back()
        .map(|last| last.start_ms.saturating_add(last.duration_ms))
        .unwrap_or(0);
    if session.duration_ms.saturating_sub(end) > USABLE_SHORTFALL_MS {
        session.usable_extent_ms = Some(end);
        tracing::info!(
            usable_extent_ms = end,
            duration_ms = session.duration_ms,
            run_id = session.encoder_state(SINGLE_VIDEO_RUNG).current_run_id,
            "hls usable extent recorded (EOF short of claimed duration)"
        );
    }
    publish_piggyback_if_complete(session);
}

/// ADR-0041 Decision 7: publish the piggybacked WebVTT side output once a
/// run that started at title 0 reaches natural EOF, and only then flip the
/// item to `ready`. A run killed by seek (ADR-0007) or stopped mid-way never
/// reaches this point, so the item stays `eligible` and no previously-good
/// track is deleted (the same invariant as Decision 8.5).
fn publish_piggyback_if_complete(session: &mut Session) {
    let Some(piggyback) = &session.piggyback else {
        return;
    };
    // A run that started at an offset only demuxed a suffix of the title.
    if session.start_ms != 0 {
        return;
    }
    let (Some(subs), Some(db)) = (&session.subs, &session.db) else {
        return;
    };
    let segments = vtt_segments_in(&run_dir(session, SINGLE_VIDEO_RUNG));
    if segments.is_empty() {
        return;
    }
    let mut bodies = Vec::with_capacity(segments.len());
    for path in &segments {
        match fs::read_to_string(path) {
            Ok(body) => bodies.push(body),
            Err(e) => {
                tracing::warn!(
                    item_id = session.item_id,
                    track_id = %piggyback.track_id,
                    path = %path.display(),
                    error = %e,
                    "piggyback subtitle segment read failed; item stays eligible"
                );
                return;
            }
        }
    }
    let body = concat_webvtt_segments(&bodies);
    if body.trim() == "WEBVTT" {
        tracing::warn!(
            item_id = session.item_id,
            track_id = %piggyback.track_id,
            "piggyback produced no cues; item stays eligible"
        );
        return;
    }
    if let Err(e) = subs.publish_item_vtt(session.item_id, &piggyback.track_id, &body) {
        tracing::warn!(
            item_id = session.item_id,
            track_id = %piggyback.track_id,
            error = %e,
            "piggyback publish failed; item stays eligible"
        );
        return;
    }
    if let Err(e) = db.set_subtitle_status(session.item_id, "ready") {
        tracing::warn!(
            item_id = session.item_id,
            track_id = %piggyback.track_id,
            error = %e,
            "piggyback ready flip failed; item stays eligible"
        );
        return;
    }
    tracing::info!(
        item_id = session.item_id,
        track_id = %piggyback.track_id,
        segment_count = segments.len(),
        cue_bytes = body.len(),
        run_id = session.encoder_state(SINGLE_VIDEO_RUNG).current_run_id,
        "piggyback extract published and item marked ready"
    );
    session.piggyback = None;
}

/// WebVTT side-output segments (`index{N}.vtt`) the HLS muxer wrote next to
/// `index.m3u8` in a run dir (ffmpeg names subtitle segments after the
/// playlist). Empty when the session had no subtitle output.
fn vtt_segments_in(run: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(run) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            name.starts_with("index") && name.ends_with(".vtt")
        })
        .collect();
    out.sort();
    out
}

/// Milliseconds of media in one segment this leg will actually produce.
///
/// [`SEGMENT_MS`] is the interval the session *asks* for. A leg that honours
/// `-force_key_frames` is given `expr:gte(t,n_forced*N)` and cuts on time, so
/// it hits it. A leg that discards the expression is given `-g <frames>`, and
/// a frame count only lands on `SEGMENT_MS` when the source rate divides it
/// into whole frames.
///
/// At `24000/1001` it does not. 2000 ms is 47.952 frames,
/// [`VideoEncodePlan::gop_frames`] rounds up to 48, and 48 frames is
/// `48 x 1001 / 24000 = 2002 ms`. **Measured on the N150 at `c43b440`,
/// `h264_qsv`, 1080p h264: 1061 of 1062 distinct segment starts were off the
/// 2000 ms grid, with a modal consecutive delta of 2002 ms across 923 pairs.**
/// A full-title listing built on `N x SEGMENT_MS` would have named a URI the
/// producer never writes at every entry but the first.
///
/// **Corrected 2026-08-30: this is every leg, not only the ones that discard
/// `-force_key_frames`.** It returned `SEGMENT_MS` for a leg that honours the
/// flag, on the reasoning that such a leg "cuts on time". It does not. **An
/// IDR can only be placed on a frame**, so the expression picks the nearest
/// one; it does not create a frame at 2.000 s. Measured through this crate's
/// own start path on `libx264`, which honours the flag: `24000/1001` produced
/// keys 83, 2085, 4087 — a cadence of **2002 ms**, not 2000. `25` and `60`
/// produced 2000, because at those rates 2000 ms is a whole number of frames
/// (50 and 120). `honours_force_key_frames` decides which arguments a leg is
/// given; it does not decide where the frames are.
///
/// `None` means there is no honest answer and the caller must not invent one:
/// either the source rate is unknown, or the frame count does not divide into
/// whole milliseconds, in which case no integer-ms grid exists at all.
// Wired to the spawn path by the phase change and to the playlist by the
// full-title listing; this commit lands the arithmetic and its controls alone,
// because the comment it corrects is what hid the defect and should not arrive
// buried in a behaviour change.
#[allow(dead_code)]
/// Put this run's output on the grid every other run shares.
///
/// Without this each run is phased to its own land — the cue the map snapped
/// it to — so two runs of the same session produce two unrelated sets of
/// segment times. Measured at `c43b440`, three runs of one session produced
/// first segments at `0`, `3780110` and `590632`: three phases, no shared
/// grid, and nothing a full-title listing could name in advance.
///
/// `want` is already a grid multiple at or before the play land, so the cue
/// (mapped) or lead-in start (`-ss`) is at or before it, and rounding that
/// **up** to the grid can never overshoot the play land.
///
/// Transcode only. Copy places no IDRs and re-encodes nothing, so it cannot
/// drop the `(cue, grid]` media and keeps its own phase.
/// The grid this session's runs share, or `None` when they cannot share one.
///
/// `None` for copy and remux — they place no IDRs and cannot drop the
/// `(cue, grid]` media — and for any session whose leg has no honest cadence,
/// which keeps a per-run listing rather than inventing a grid.
/// The cadence this session's producer writes at, for the map ingest.
///
/// Unlike [`grid_cadence_ms`] this is not gated on transcode: copy's keys are
/// source keyframes and have no cadence to snap to, so it answers `None` there
/// through [`produced_segment_ms`]'s own rate check only when a rate is
/// absent. Copy is excluded by the mode test below for the same reason it is
/// excluded from the grid — it places no IDRs.
fn session_cadence_ms(session: &Session) -> Option<u64> {
    session_grid_cadence(session).cadence()
}

/// This session's grid answer, with the three cases kept apart.
fn session_grid_cadence(session: &Session) -> GridCadence {
    grid_cadence_ms(
        session.mode,
        session.burn_in.is_some(),
        &session.encode_leg,
        &session.encode_plan,
        session.duration_ms,
    )
}

/// The points this session's listing names, for copy's key snap.
///
/// Copy's listing is the keyframe walk, which is irregular, so a produced key
/// rounds onto the nearest listed point rather than onto a cadence. Empty when
/// the session has no walk, and the caller then passes no snap at all.
fn session_listed_points(session: &Session) -> Vec<u64> {
    if session_cadence_ms(session).is_some() {
        return Vec::new();
    }
    copy_window_entries(session)
        .map(|entries| entries.into_iter().map(|(start, _)| start).collect())
        .unwrap_or_default()
}

/// How this session's producer keys are put onto its listing's keys.
fn session_key_snap<'a>(
    cadence: Option<u64>,
    points: &'a [u64],
) -> Option<crate::hls_segment_map::KeySnap<'a>> {
    match cadence {
        Some(c) => Some(crate::hls_segment_map::KeySnap::Cadence(c)),
        None if !points.is_empty() => Some(crate::hls_segment_map::KeySnap::Points(points)),
        None => None,
    }
}

/// Is this want one the session's own playlist offers?
///
/// **The narrowing that lets a cold URI spawn behind the committed land.**
/// [`digback_behind_committed`] declines a behind-committed GET as dig-back,
/// which was right while the only such GETs were WebKit asking for URIs the
/// session never offered. **A URI the playlist lists is not junk, it is the
/// contract** — and under ADR-0054 decision 3 it must be served, in either
/// direction.
///
/// So the guard keeps declining what it was built for (an unlisted want) and
/// stops declining what it never intended (our own listing). Off the grid or
/// past the extent is still unlisted and still 404s.
///
/// `false` for a session with no full-title listing — copy and remux, or a leg
/// with no honest cadence — which leaves the guard exactly as it was for them.
/// The code a segment miss refuses with, once the session has looked at it.
///
/// **A want this session accepted is never 404 afterwards** (ADR-0054
/// decision 3). `accepted_hold` is set once the wait loop has slept a poll
/// without answering, which is the moment the session accepted the want: it
/// looked, decided the segment was still coming, and made the client wait.
///
/// 404 stays available on the **first** look, which is the case decision 3
/// reserves it for - a URI outside the title or off the grid, refused before
/// anyone waits on it.
///
/// **This is a named function so the rule has one deterministic test.** Both
/// refusal sites in `asset_wait`'s `Wait` arm go through it, and reaching
/// either of them end to end depends on a seek landing inside one
/// `SEGMENT_POLL` of a held request - measured at 2 runs in 20, which is not
/// coverage. `a_want_this_session_accepted_is_never_404` pins the rule; the
/// integration tests around it cannot.
fn miss_refusal(accepted_hold: bool) -> PlaylistError {
    if accepted_hold {
        PlaylistError::NotReady
    } else {
        PlaylistError::NotFound
    }
}

fn want_is_listed(session: &Session, want_ms: u64) -> bool {
    // Ask the listing, not the cadence. A predicate that answers "on the grid"
    // claims wants the playlist does not offer, and every caller reads this as
    // "the session listed it".
    let Some(entries) = full_title_entries(session) else {
        return false;
    };
    entries
        .binary_search_by_key(&want_ms, |(start, _)| *start)
        .is_ok()
}

fn snap_plan_to_grid(plan: &mut StartPlan, produced_ms: u64) {
    let cue = plan.window_start_ms;
    let grid = cue.div_ceil(produced_ms) * produced_ms;
    plan.drop_ms = grid - cue;
    plan.window_start_ms = grid;
}

pub(crate) fn align_to_segment(ms: u64) -> u64 {
    (ms / SEGMENT_MS) * SEGMENT_MS
}

/// Encode window start for a play land: [`encode_lead_segments`] before the
/// aligned play point (Safari dig-back; see module docs / ADR-0011).
pub(crate) fn encode_start_ms(play_ms: u64) -> u64 {
    align_to_segment(play_ms.saturating_sub(encode_lead_segments() * SEGMENT_MS))
}

/// Highest mapped segment start at or after `window_start_ms`.
fn latest_segment_in_window(
    map: &crate::hls_segment_map::SegmentMap,
    window_start_ms: u64,
) -> Option<u64> {
    latest_mapped_start_in_window(map, window_start_ms)
}

/// Dogfood: what EXT-X-START / session land was when a playlist was served.
/// Full index bodies are huge (every seg URI); log header lines only.
fn log_playlist_serve(
    session_id: &str,
    resource: &str,
    req_start_ms: Option<u64>,
    play_start_ms: u64,
    pending_play_ms: Option<u64>,
    bytes: &[u8],
) {
    let text = String::from_utf8_lossy(bytes);
    let mut ext_x_start: Option<f64> = None;
    let mut head_lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        if head_lines.len() < 14 && (line.starts_with('#') || line.is_empty()) {
            head_lines.push(line);
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-START:TIME-OFFSET=") {
            let offset = rest.split(',').next().unwrap_or(rest);
            ext_x_start = offset.parse().ok();
        }
        if !line.starts_with('#') && !line.is_empty() {
            // First media URI — stop header capture.
            if head_lines.len() < 14 {
                head_lines.push(line);
            }
            break;
        }
    }
    let head = head_lines.join("|");
    tracing::info!(
        session_id,
        resource,
        req_start_ms,
        play_start_ms,
        pending_play_ms,
        ext_x_start_s = ext_x_start,
        play_land_ms = play_start_ms,
        head = %head,
        "hls playlist serve"
    );
}

/// Subtitle media playlist for one snapshotted track.
///
/// ADR-0020 §10 / ADR-0010: VTT stays on the fixed 2s index grid (`segNNN.vtt`)
/// even though video URIs are time-keyed (`seg_<ms:011>.m4s`). Do not "align"
/// subtitle segment names to producer video boundaries — cue slicing is
/// title-time on SEGMENT_MS, independent of copy GOP cuts.
fn build_subtitle_playlist_for(track: &HlsSubtitleTrack, duration_ms: u64) -> Vec<u8> {
    build_segmented_subtitle_playlist(&track.track_id, duration_ms)
}

/// Multi-segment VOD subtitle playlist aligned to SEGMENT_MS.
/// Segment URIs are relative to `subs/{trackId}.m3u8`.
fn build_segmented_subtitle_playlist(track_id: &str, duration_ms: u64) -> Vec<u8> {
    use std::fmt::Write;
    let full = duration_ms / SEGMENT_MS;
    let rem_ms = duration_ms % SEGMENT_MS;
    let segment_secs = SEGMENT_MS as f64 / 1000.0;
    let target = segment_secs.ceil() as u64;
    let mut out = format!(
        "#EXTM3U\n\
         #EXT-X-VERSION:6\n\
         #EXT-X-TARGETDURATION:{target}\n\
         #EXT-X-PLAYLIST-TYPE:VOD\n\
         #EXT-X-MEDIA-SEQUENCE:0\n"
    );
    for i in 0..full {
        let _ = writeln!(
            out,
            "#EXTINF:{segment_secs:.6},\n{track_id}/{}",
            segment_vtt_name(i)
        );
    }
    if rem_ms > 0 {
        let _ = writeln!(
            out,
            "#EXTINF:{:.6},\n{track_id}/{}",
            rem_ms as f64 / 1000.0,
            segment_vtt_name(full)
        );
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out.into_bytes()
}

fn segment_vtt_name(index: u64) -> String {
    format!("seg{index:03}.vtt")
}

fn spawn_session_subtitle_worker(src: &Path, dir: &Path, tracks: &[HlsSubtitleTrack]) {
    let inputs: Vec<SessionSubInput> = tracks
        .iter()
        .filter(|t| t.item_vtt_path.is_none())
        .map(|t| SessionSubInput {
            track_id: t.track_id.clone(),
            codec: t.codec.clone(),
            stream_index: t.stream_index,
            sidecar_path: t.sidecar_path.clone(),
        })
        .collect();
    if inputs.is_empty() {
        return;
    }
    let src = src.to_path_buf();
    let dir = dir.to_path_buf();
    let _ = std::thread::Builder::new()
        .name("hls-subs".into())
        .spawn(move || {
            if let Err(e) = prepare_session_subtitles(&src, &dir, &inputs) {
                tracing::warn!(
                    path = %src.display(),
                    error = %e,
                    "session subtitle prep failed"
                );
            }
        });
}

fn read_subtitle_segment(
    session: &Session,
    track_id: &str,
    segment_idx: u64,
) -> Result<Vec<u8>, PlaylistError> {
    // Belt: track_id must be a single Normal component (API allowlist is the
    // primary gate; this stops a regression from joining `..` into session.dir).
    use std::path::Component;
    let mut comps = Path::new(track_id).components();
    match (comps.next(), comps.next()) {
        (Some(Component::Normal(_)), None) => {}
        _ => return Err(PlaylistError::NotFound),
    }
    let track = session
        .subtitle_tracks
        .iter()
        .find(|t| t.track_id == track_id)
        .ok_or(PlaylistError::NotFound)?;

    let (full_path, done) = if let Some(path) = &track.item_vtt_path {
        // Ready extract: complete file, slice in-process (no session demux).
        (path.clone(), true)
    } else {
        let track_dir = session.dir.join("subs").join(track_id);
        (track_dir.join("full.vtt"), track_dir.join("done").exists())
    };
    if !full_path.exists() {
        return Err(PlaylistError::NotReady);
    }
    let body = fs::read_to_string(&full_path).map_err(|e| {
        PlaylistError::Failed(format!("read subtitle {}: {e}", full_path.display()))
    })?;
    let start_ms = segment_idx * SEGMENT_MS;
    let end_ms = start_ms + SEGMENT_MS;
    if !done {
        match webvtt_max_cue_end_ms(&body) {
            None => return Err(PlaylistError::NotReady),
            Some(max_end) if max_end < start_ms => return Err(PlaylistError::NotReady),
            Some(_) => {}
        }
    }
    Ok(slice_webvtt(&body, start_ms, end_ms).into_bytes())
}

/// ADR-0023 §9.3: whether a keyframe-map build for an item is queued or in
/// flight. The API wires it to the library pool so a seek can wait, bounded,
/// for a build a consumer already triggered.
type MapBuildInFlight = dyn Fn(i64) -> bool + Send + Sync;

/// What one producer run opens, and how that run is timed (ADR-0023 §3).
struct StartPlan {
    /// `-i` argument: the real file, or a session-scoped virtual-file URL.
    input: std::ffi::OsString,
    /// Title-absolute start of this run: `-output_ts_offset`, `-start_number`,
    /// the burn-in shift, and the run's recorded encode start. Equals the
    /// snapped map PTS on the mapped path.
    window_start_ms: u64,
    /// Whether FFmpeg seeks inside the input (`-ss`).
    seek_input: bool,
    /// `mapped` when the keyframe map placed this start, else `ss`.
    start_path: &'static str,
    container_kind: &'static str,
    /// Cost of the bind-time identity re-read (ADR-0023 §4).
    fingerprint_cost_ms: u128,
    /// Media to decode and discard between the splice point and
    /// [`window_start_ms`], so this run's output starts on the shared grid
    /// rather than on its own land. Zero when there is no grid to share.
    ///
    /// The spike's locked decision: *"Transcode must not emit the (cue, land]
    /// media: decode from the cue, drop until the grid."* Copy cannot do this
    /// — it re-encodes nothing — so it never gets a non-zero value here.
    drop_ms: u64,
    virtual_input: Option<crate::virtual_input::VirtualInput>,
}

/// The session's keyframe map and the virtual file bound from it. Dropping
/// it stops the session's range server.
#[derive(Default)]
struct MapBinding {
    /// Cleared when identity or a bind fails: a poisoned map is not retried
    /// for the life of the session.
    map: Option<crate::virtual_input::KeyframeMap>,
    bound: Option<crate::virtual_input::VirtualInput>,
    /// Set once a spawn fell back to `-ss` while holding a map (ADR-0023 §8);
    /// the API reads it to enqueue a map rebuild.
    fell_back: bool,
}

impl MapBinding {
    fn new(map: Option<crate::virtual_input::KeyframeMap>) -> Self {
        Self {
            map,
            bound: None,
            fell_back: false,
        }
    }

    /// Decides how the next run starts. Mapped when the map still matches
    /// the bytes on disk and binds; otherwise today's `-ss` on the real file
    /// (ADR-0023 §8). A map problem never fails the session.
    fn plan(&mut self, src: &Path, play_start_ms: u64) -> StartPlan {
        let Some(map) = self.map.take() else {
            tracing::info!(
                path = %src.display(),
                "no keyframe map yet; using -ss, build enqueued"
            );
            return ss_start_plan(src, play_start_ms, 0);
        };
        let cost_ms = match crate::virtual_input::verify_identity(src, &map.content_id) {
            Ok(cost_ms) => cost_ms,
            Err(e) => {
                tracing::info!(
                    path = %src.display(),
                    reason = %e,
                    "hls start: keyframe map identity stale; falling back to -ss"
                );
                self.bound = None;
                self.fell_back = true;
                return ss_start_plan(src, play_start_ms, 0);
            }
        };
        match crate::virtual_input::bind(src, &map, play_start_ms, self.bound.take()) {
            Ok(bind) => {
                let plan = StartPlan {
                    input: bind.input,
                    window_start_ms: bind.land_ms,
                    seek_input: bind.seek_input,
                    start_path: "mapped",
                    container_kind: map.container_kind.as_str(),
                    fingerprint_cost_ms: cost_ms,
                    drop_ms: 0,
                    virtual_input: bind.virtual_input,
                };
                self.map = Some(map);
                plan
            }
            Err(e) => {
                tracing::warn!(
                    path = %src.display(),
                    reason = %e,
                    "hls start: keyframe map bind failed; falling back to -ss"
                );
                self.fell_back = true;
                ss_start_plan(src, play_start_ms, cost_ms)
            }
        }
    }
}

/// Today's start: `-ss` on the real file at the encode window (ADR-0023 §8).
fn ss_start_plan(src: &Path, play_start_ms: u64, fingerprint_cost_ms: u128) -> StartPlan {
    StartPlan {
        input: src.as_os_str().to_owned(),
        window_start_ms: encode_start_ms(play_start_ms),
        seek_input: true,
        start_path: "ss",
        container_kind: "-",
        fingerprint_cost_ms,
        drop_ms: 0,
        virtual_input: None,
    }
}

/// ADR-0023 §9.3: bounded wait for an in-flight keyframe-map build.
///
/// Returns the freshly built map when it lands inside the bound; None when
/// no build is in flight to wait for, the bound expires, or the build failed
/// — the caller then falls through to the §8 `-ss` plan. The wait is only
/// worth paying while a build is actually running, so the predicate is
/// consulted; a never-built item falls through immediately (§9.4: the `-ss`
/// fallback is for genuine map failure, not for a wait that has not elapsed).
/// `start()` calls it before the session exists, so the three values a live
/// `Session` carries are passed explicitly.
fn wait_for_map_build(
    item_id: i64,
    db: Option<&Arc<Db>>,
    in_flight: Option<&Arc<MapBuildInFlight>>,
) -> Option<crate::virtual_input::KeyframeMap> {
    let db = db?;
    // The build may have landed since the session started (or since the last
    // seek): use it without waiting.
    if let Some(rows) = db.keyframe_map(item_id).ok().flatten() {
        return crate::virtual_input::KeyframeMap::from_db_rows(&rows);
    }
    let in_flight = in_flight?;
    if !in_flight(item_id) {
        return None;
    }
    let deadline = Instant::now() + MAP_BUILD_WAIT;
    loop {
        if let Some(rows) = db.keyframe_map(item_id).ok().flatten() {
            return crate::virtual_input::KeyframeMap::from_db_rows(&rows);
        }
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        std::thread::sleep((deadline - now).min(MAP_BUILD_WAIT_POLL));
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_ffmpeg(
    input: &StartPlan,
    dir: &Path,
    mode: SessionMode,
    audio: AudioSelection,
    encode_leg: &crate::EncodeLeg,
    burn_in: Option<&BurnInSelection>,
    encode_plan: VideoEncodePlan,
    piggyback_subs: bool,
) -> Result<Child, String> {
    let start_ms = input.window_start_ms;
    let start_secs = format!("{:.3}", start_ms as f64 / 1000.0);
    let start_number = (start_ms / SEGMENT_MS).to_string();
    let segment_secs = SEGMENT_MS as f64 / 1000.0;
    let force_kf = format!("expr:gte(t,n_forced*{segment_secs})");
    // Copy cuts at source keyframes, so `-hls_time` is a target it rounds up
    // to the next one. At 2 s that produced a segment per GOP — §S8 measured
    // robotic audio and a video stall — and at COPY_WINDOW_MS it produces the
    // windows the listing names. Transcode keeps SEGMENT_MS, where forced
    // IDRs make the target exact.
    let hls_time_ms = if mode == SessionMode::Copy {
        COPY_WINDOW_MS
    } else {
        SEGMENT_MS
    };
    let hls_time = format!("{}", hls_time_ms as f64 / 1000.0);
    // Burn-in always re-encodes video (ADR-0018).
    let mode = if burn_in.is_some() {
        SessionMode::Transcode
    } else {
        mode
    };
    let mut cmd = Command::new("ffmpeg");
    cmd.current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Discard stderr: a piped and unread stderr fills (~64KiB) and
        // deadlocks ffmpeg so the playlist never appears.
        .stderr(Stdio::null())
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"]);
    // Encode-leg pre-input (e.g. -vaapi_device) before any -i (ADR-0009).
    if mode == SessionMode::Transcode {
        encode_leg.push_pre_input(&mut cmd);
    }
    // The Matroska splice already starts at the land Cluster, so seeking
    // inside it would land a second time (ADR-0023 §3a). MP4 keeps `-ss`:
    // its virtual file spans the whole title (§3b).
    if start_ms > 0 && input.seek_input {
        cmd.args(["-ss", &start_secs]);
    }
    cmd.arg("-i").arg(&input.input);
    if input.drop_ms > 0 {
        // Output-side seek: decode from the splice, discard until the grid.
        // Before `-i` this would move the splice; after it, it moves where
        // output begins, which is the whole point — every run then starts on
        // the same grid instead of on its own land.
        let drop_secs = format!("{}.{:03}", input.drop_ms / 1000, input.drop_ms % 1000);
        cmd.args(["-ss", &drop_secs]);
    }
    if start_ms > 0 {
        // ADR-0020: load-bearing under copy. Does not rewrite tfdt/trun (those
        // stay segment-local at 0); it stamps title-absolute time into the
        // init `elst` empty-edit and each fragment's `sidx.earliest_presentation_time`,
        // which the rung map uses as the wire key. Removing or moving this
        // flag silently reintroduces zero-based / mislabelled segment times.
        cmd.args(["-output_ts_offset", &start_secs]);
    }
    let audio_map = match audio.stream_index {
        Some(index) => format!("0:{index}"),
        None => "0:a:0?".to_string(),
    };
    let downmix = if audio.needs_downmix() {
        let filter = stereo_downmix_filter(audio.channels, audio.channel_layout.as_deref());
        if filter.is_none() {
            tracing::warn!(
                channels = audio.channels,
                layout = audio.channel_layout.as_deref().unwrap_or("unknown"),
                input = %input.input.to_string_lossy(),
                "no downmix matrix for this layout; falling back to -ac 2"
            );
        }
        filter
    } else {
        None
    };

    // ASS uses libass `-vf` filters; PGS uses overlay `filter_complex`
    // (ADR-0018). Never overlay text ASS — sub2video draws blank.
    if let Some(burn) = burn_in {
        ensure_libass_for_ass(burn.kind, ffmpeg_has_libass_filters())?;
    }
    let pgs_overlay = burn_in.and_then(pgs_overlay_graph);
    let ass_vf = match burn_in {
        Some(burn) if burn.kind == BurnInKind::Ass => Some(ass_burn_vf(burn, start_ms)?),
        _ => None,
    };
    let software_chain = if mode == SessionMode::Transcode {
        Some(transcode_video_filter_chain(
            encode_plan,
            ass_vf.as_deref(),
        )?)
    } else {
        None
    };
    // Software scale/tonemap/burn then encode-leg upload (VAAPI hwupload).
    let video_vf = if mode == SessionMode::Transcode {
        encode_leg.compose_video_filter(software_chain.as_deref())
    } else {
        None
    };
    if let Some(ref complex) = pgs_overlay {
        let tail = video_vf.as_deref().unwrap_or(SDR_RETAG_CHAIN);
        let full = format!("{complex},{tail}[vout]");
        cmd.args(["-filter_complex", &full]);
        cmd.args(["-map", "[vout]", "-map", &audio_map]);
    } else {
        cmd.args(["-map", "0:v:0", "-map", &audio_map]);
    }

    match mode {
        // Hybrid: the codecs already copy and only the channel layout forces
        // work, so video still copies while audio is encoded (ADR-0012).
        SessionMode::Copy if audio.needs_downmix() => {
            cmd.args(["-c:v", "copy"]);
            push_audio_encode(&mut cmd, downmix.as_deref());
        }
        // Copy cannot place IDRs, so -hls_time is only a target: segments
        // break at source keyframes (ADR-0011).
        SessionMode::Copy => {
            cmd.args(["-c", "copy"]);
        }
        SessionMode::Transcode => {
            // Encode leg owns -c:v, pix_fmt, and encoder extras (ADR-0009).
            // No global non-x264 -pix_fmt yuv420p.
            encode_leg.push_encoder_args(&mut cmd);
            if pgs_overlay.is_some() {
                cmd.args(["-map_metadata", "-1"]);
            } else if let Some(ref vf) = video_vf {
                cmd.args(["-map_metadata", "-1", "-vf", vf]);
            }
            cmd.args([
                "-colorspace",
                "bt709",
                "-color_primaries",
                "bt709",
                "-color_trc",
                "bt709",
            ]);
            if let Some(bps) = encode_plan.max_bitrate_bps {
                let rate = bps.to_string();
                let buf = (bps.saturating_mul(2)).to_string();
                cmd.args(["-b:v", &rate, "-maxrate", &rate, "-bufsize", &buf]);
            }
            // IDR cadence is per encode leg (ADR-0052). The interval is
            // always SEGMENT_MS; how the leg is made to hit it differs.
            let gop = encode_plan.gop_frames(SEGMENT_MS);
            if encode_leg.honours_force_key_frames {
                // Time-based IDRs derived from SEGMENT_MS (same source as
                // -hls_time and the generated playlist EXTINF). A frame-count
                // -g alone is only 2s at 24 fps; at 60 fps it splits every
                // 0.8s (ADR-0008 §3).
                cmd.args(["-force_key_frames", force_kf.as_str()]);
                // Ceiling only; force_key_frames owns the cadence. Derived
                // from the source rate when known so it cannot land inside a
                // segment; otherwise a wide fallback that keeps the
                // expression as the binding constraint.
                let g = gop.map_or_else(|| "600".to_string(), |g| g.to_string());
                cmd.args(["-g", &g, "-sc_threshold", "0"]);
            } else {
                // This leg discards -force_key_frames, so -g is the cadence
                // and has to be exactly one segment of frames. Measured on
                // h264_qsv 2026-08-23: with -g 600 a 23.976 fps source cut
                // 25.025 s segments and the 2 s grid did not exist.
                match gop {
                    Some(g) => {
                        let g = g.to_string();
                        cmd.args(["-g", &g, "-keyint_min", &g, "-forced_idr", "1"]);
                    }
                    None => {
                        // No rate means no honest frame count. Send the
                        // expression so a leg that quietly does honour it
                        // still lands on the grid, and say so: this session's
                        // segments may not be SEGMENT_MS.
                        tracing::warn!(
                            encoder = %encode_leg.encoder,
                            "no source frame rate: cannot derive the IDR interval                              for a leg that ignores -force_key_frames; segment                              duration may not hold (ADR-0052)"
                        );
                        cmd.args(["-force_key_frames", force_kf.as_str()]);
                    }
                }
            }
            push_audio_encode(&mut cmd, downmix.as_deref());
        }
    }
    if piggyback_subs {
        // ADR-0041 Decision 7: the session's ffmpeg is already open on the
        // file, so the subtitle rendition is a free side output. The HLS
        // muxer writes WebVTT segments (`index{N}.vtt` + `index_vtt.m3u8`)
        // into the run dir; it accepts a single subtitle stream, which the
        // caller's gate guarantees (an image/unknown stream or a second
        // subtitle output fails the whole session). `-c:s webvtt` overrides
        // the global `-c copy` in Copy mode for subtitle streams only.
        cmd.args(["-map", "0:s?", "-c:s", "webvtt"]);
    }
    cmd.args([
        "-f",
        "hls",
        "-hls_time",
        hls_time.as_str(),
        "-hls_list_size",
        "0",
        "-hls_flags",
        "independent_segments+temp_file",
        "-hls_segment_type",
        "fmp4",
        "-hls_fmp4_init_filename",
        "init.mp4",
        "-hls_segment_filename",
        "seg%03d.m4s",
        "-start_number",
        &start_number,
        // Muxer index is ingested into the rung's time-keyed map
        // (ADR-0020); clients never fetch this file.
        "index.m3u8",
    ]);
    cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "ffmpeg not found on PATH".into()
        } else {
            format!("spawn ffmpeg for {}: {e}", input.input.to_string_lossy())
        }
    })
}

/// Error when ASS burn is requested but FFmpeg lacks libass filters.
const LIBASS_REQUIRED: &str =
    "ASS/SSA burn-in requires FFmpeg built with libass (ass and subtitles filters)";

/// Fail closed for ASS burn when libass filters are absent (ADR-0018).
fn ensure_libass_for_ass(kind: BurnInKind, has_libass: bool) -> Result<(), String> {
    if kind == BurnInKind::Ass && !has_libass {
        Err(LIBASS_REQUIRED.into())
    } else {
        Ok(())
    }
}

/// True when `ffmpeg -filters` lists both `ass` and `subtitles`.
fn libass_filters_listed(filters_text: &str) -> bool {
    let mut has_ass = false;
    let mut has_subtitles = false;
    for line in filters_text.lines() {
        match line.split_whitespace().nth(1) {
            Some("ass") => has_ass = true,
            Some("subtitles") => has_subtitles = true,
            _ => {}
        }
    }
    has_ass && has_subtitles
}

/// Retag-only SDR labels for non-HDR re-encodes. VideoToolbox otherwise copies
/// PQ/BT.2020 VUI onto an 8-bit encode; Safari native HLS rejects that.
const SDR_RETAG_CHAIN: &str =
    "sidedata=delete,setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709";

/// PQ/HLG → SDR via zscale linearise + hable tonemap (ADR-0022). Requires
/// FFmpeg built with libzimg (`zscale` in `-filters`).
const HDR_TONEMAP_CHAIN: &str = "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,\
tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuv420p,sidedata=delete";

/// Video filter chain for Transcode mode: optional scale, then tonemap or
/// SDR retag, with optional ASS burn prefix.
fn transcode_video_filter_chain(
    plan: VideoEncodePlan,
    ass_vf: Option<&str>,
) -> Result<String, String> {
    if plan.tone_map && !ffmpeg_has_zscale() {
        return Err(
            "HDR tone-map requires FFmpeg with libzimg (zscale filter); \
             install an FFmpeg build configured --enable-libzimg"
                .into(),
        );
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(ass) = ass_vf {
        parts.push(ass.to_string());
    }
    if let Some(h) = plan.max_height {
        // Shrink only; never upscale a source already under the ceiling.
        parts.push(format!("scale=-2:'min({h},ih)'"));
    }
    if plan.tone_map {
        parts.push(HDR_TONEMAP_CHAIN.to_string());
    } else {
        parts.push(SDR_RETAG_CHAIN.to_string());
    }
    Ok(parts.join(","))
}

/// Cached probe of the host FFmpeg filter list for zscale (libzimg).
pub fn host_tonemap_available() -> bool {
    ffmpeg_has_zscale()
}

fn ffmpeg_has_zscale() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let output = match Command::new("ffmpeg")
            .args(["-hide_banner", "-filters"])
            .output()
        {
            Ok(o) => o,
            Err(_) => return false,
        };
        let text = if output.stdout.is_empty() {
            String::from_utf8_lossy(&output.stderr)
        } else {
            String::from_utf8_lossy(&output.stdout)
        };
        text.lines().any(|line| {
            line.split_whitespace()
                .nth(1)
                .is_some_and(|name| name == "zscale")
        })
    })
}

/// Cached probe of the host FFmpeg filter list for libass.
fn ffmpeg_has_libass_filters() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let output = match Command::new("ffmpeg")
            .args(["-hide_banner", "-filters"])
            .output()
        {
            Ok(o) => o,
            Err(_) => return false,
        };
        // ffmpeg writes the filter table to stdout; some builds mix help on stderr.
        let text = if output.stdout.is_empty() {
            String::from_utf8_lossy(&output.stderr)
        } else {
            String::from_utf8_lossy(&output.stdout)
        };
        libass_filters_listed(&text)
    })
}

/// Escape a filesystem path for an FFmpeg filter option value.
fn escape_ffmpeg_filter_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '\\' | ':' | '\'' | '[' | ']' | ',' | ';' | ' ' | '(' | ')' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Ensure embedded ASS burn-in has a local `.ass` path (ADR-0018).
/// Sidecar and PGS selections pass through unchanged. Reuses an existing
/// session extract on seek restart.
fn prepare_ass_burn_file(
    src: &Path,
    session_dir: &Path,
    burn_in: Option<BurnInSelection>,
) -> Result<Option<BurnInSelection>, String> {
    let Some(mut burn) = burn_in else {
        return Ok(None);
    };
    if burn.kind != BurnInKind::Ass || burn.sidecar_path.is_some() {
        return Ok(Some(burn));
    }
    let stream_index = burn
        .stream_index
        .ok_or_else(|| "embedded ASS burn-in missing stream_index".to_string())?;
    let dest = session_dir.join(format!("burn_{}.ass", burn.track_id));
    let reuse = fs::metadata(&dest).ok().is_some_and(|m| m.len() > 0);
    if !reuse {
        tracing::info!(
            path = %src.display(),
            track_id = %burn.track_id,
            stream_index,
            dest = %dest.display(),
            "extracting embedded ASS for burn-in"
        );
        extract_embedded_ass(src, stream_index, &dest)?;
    }
    burn.sidecar_path = Some(dest);
    Ok(Some(burn))
}

/// libass `-vf` fragment for ASS/SSA burn-in (ADR-0018).
/// Always `ass=<local path>` — embedded tracks are demuxed first by
/// [`prepare_ass_burn_file`]. Mid-window `-ss` before `-i` resets frame PTS
/// to ~0; wrap with setpts so libass still matches absolute cue times, then
/// restore PTS for the muxer.
fn ass_burn_vf(burn: &BurnInSelection, start_ms: u64) -> Result<String, String> {
    if burn.kind != BurnInKind::Ass {
        return Err("ass_burn_vf called for non-ASS burn-in".into());
    }
    let path = burn
        .sidecar_path
        .as_ref()
        .ok_or_else(|| "ASS burn-in missing local .ass path (extract first)".to_string())?;
    let core = format!("ass={}", escape_ffmpeg_filter_path(path));
    if start_ms == 0 {
        return Ok(core);
    }
    let start_secs = start_ms as f64 / 1000.0;
    Ok(format!(
        "setpts=PTS+{start_secs}/TB,{core},setpts=PTS-{start_secs}/TB"
    ))
}

/// PGS overlay graph prefix (ADR-0018). SDR chain and `[vout]` are appended
/// by the caller. Embedded uses `0:s:N`.
fn pgs_overlay_graph(burn: &BurnInSelection) -> Option<String> {
    if burn.kind != BurnInKind::Pgs {
        return None;
    }
    let ordinal = burn.subtitle_ordinal?;
    Some(format!("[0:v:0][0:s:{ordinal}]overlay"))
}

/// Stereo AAC for the mapped track. With a matrix, `pan` does the mixdown;
/// without one, bare `-ac 2` is the fallback (ADR-0012) — swresample's
/// default matrix under-weights centre, which is why the matrix exists.
fn push_audio_encode(cmd: &mut Command, downmix: Option<&str>) {
    cmd.args(["-c:a", "aac", "-b:a", "192k"]);
    match downmix {
        Some(filter) => cmd.args(["-filter:a", filter]),
        None => cmd.args(["-ac", "2"]),
    };
}

/// Move this session's producer aside for a seek: keep it, reap it later.
///
/// It is left **running**. Suspending would free encoder time, but it also
/// stops production, and a client may be waiting on a segment of this land
/// that has not finished writing. Under kill-and-restart that case was
/// handled by deferring the kill (`may_kill_cooking_encode`); here it is
/// handled by letting the encoder finish. Leaving it running is also the
/// fastest of the three seek policies measured, at a 1132 ms median, so
/// correctness and speed agree (ADR-0050 §4-§5).
///
/// "Left running" includes resuming one the throttle had already suspended.
/// A session that has caught up sits SIGSTOPped at [`LEAD_TARGET_MS`], which
/// is the design's steady state, so that is the common case at a seek and not
/// the rare one. Without the SIGCONT this function would set aside a stopped
/// process for [`REAP_AFTER`] and then kill it — suspend-then-reap, the exact
/// policy ADR-0050 §5 was amended to forbid.
fn supersede_child(session: &mut Session) {
    let state = session.encoder_state_mut(SINGLE_VIDEO_RUNG);
    // Resume before setting it aside, while the child is still this state's
    // live child and the only thing that can signal it is this call under this lock.
    if state.throttled
        && let Some(child) = state.child.as_ref()
    {
        signal_child(child, false);
    }
    let Some(child) = state.child.take() else {
        return;
    };
    let rss_bytes = state.child_rss_bytes.take();
    // The flag described the child that just left.
    state.throttled = false;
    state.superseded.push(SupersededEncoder {
        child,
        rss_bytes,
        reap_at: Instant::now() + REAP_AFTER,
        // `restart_at` calls this before it assigns the new run, so
        // `current_run_id` here is exactly the run being set aside. Read it,
        // do not infer it later.
        run_id: state.current_run_id,
    });
}

/// Runs with an encoder still writing into them: the current one, plus every
/// superseded encoder that has not been reaped yet.
///
/// Every per-run cleanup in this file has to consult this rather than
/// `current_run_id` alone. A seek used to kill the prior encoder before any
/// cleanup ran, so "not current" meant "nothing is writing here". Under
/// ADR-0050 §4-§5 the prior encoder outlives the seek by [`REAP_AFTER`], and
/// unlinking its directory would take away the media the whole policy exists
/// to keep serving.
fn live_run_ids(session: &Session) -> Vec<u64> {
    let state = session.encoder_state(SINGLE_VIDEO_RUNG);
    let mut ids = Vec::with_capacity(state.superseded.len() + 1);
    ids.push(state.current_run_id);
    ids.extend(state.superseded.iter().map(|s| s.run_id));
    ids
}

/// Release every superseded encoder the new one has already overtaken.
///
/// **Segments within a run are sequential.** So a held encoder whose frontier
/// has reached [`Session::start_ms`] has already written everything behind
/// that land, and [`restart_at`] has already ingested it. The new encoder
/// covers from `start_ms` forward. The region only the held encoder serves is
/// `[frontier, start_ms)`, and this condition fires exactly when that region
/// is empty. **Nothing it could still produce is wanted by anyone**, so there
/// is no memory-against-a-wanted-segment trade to weigh here.
///
/// The boundary is `start_ms`, never `play_start_ms`. `window_start_ms` is a
/// lead-in at or before the land, so `play_start_ms >= start_ms`:
/// `play_start_ms` is safe but late, and holds the encoder past the point it
/// stopped being useful.
///
/// Called from **both** of [`restart_at`]'s exits, each after it assigns
/// `start_ms`, because they assign it differently and the map-hit branch
/// returns early — there is no common tail. [`restart_at`] refreshes the map
/// for every run ([`sync_segment_map`], [`sync_all_run_indexes`]) moments
/// before, so the frontier read here is freshly ingested, and this needs no
/// new state and no new ingest. It is not on the throttle tick for the
/// opposite reason: a held run's frontier there is only as fresh as the last
/// poll-path ingest, and with nobody polling it is frozen at seek time.
///
/// [`frontier_ms`] answers **per run**. A held encoder that has produced
/// nothing has no frontier and is left alone; reading it as zero would release
/// it on any seek.
///
/// It moves `reap_at`; it does not kill. [`reap_superseded`] does `kill()` and
/// `wait()`, and [`restart_at`] runs under the sessions mutex, so killing here
/// would put a blocking `wait()` on a just-signalled process into the seek
/// path. The next [`THROTTLE_TICK`] does the killing where it already lives.
fn release_overtaken_superseded(session: &mut Session) {
    let now = Instant::now();
    let start_ms = session.start_ms;
    // Disjoint fields: the segment map is read while encoder state is walked mutably.
    let Session {
        segment_maps,
        encoder_states,
        ..
    } = session;
    let Some(map) = segment_maps.get(&SINGLE_VIDEO_RUNG) else {
        panic!(
            "session has no segment map for rung {}",
            SINGLE_VIDEO_RUNG.as_str()
        );
    };
    let Some(state) = encoder_states.get_mut(&SINGLE_VIDEO_RUNG) else {
        panic!(
            "session has no encoder state for rung {}",
            SINGLE_VIDEO_RUNG.as_str()
        );
    };
    for held in state.superseded.iter_mut() {
        let Some(frontier) = frontier_ms(map.iter_ordered(), held.run_id) else {
            continue;
        };
        if frontier >= start_ms {
            // Never later than it already was.
            held.reap_at = held.reap_at.min(now);
        }
    }
}

/// Terminate superseded encoders whose delay has elapsed.
fn reap_superseded(session: &mut Session) {
    let now = Instant::now();
    session
        .encoder_state_mut(SINGLE_VIDEO_RUNG)
        .superseded
        .retain_mut(|s| {
            if s.reap_at > now {
                return true;
            }
            let _ = s.child.kill();
            let _ = s.child.wait();
            false
        });
}

/// Terminate every superseded encoder now, whatever their delay. Session
/// teardown: nothing may outlive the session that spawned it.
fn reap_all_superseded(session: &mut Session) {
    let state = session.encoder_state_mut(SINGLE_VIDEO_RUNG);
    for s in state.superseded.iter_mut() {
        let _ = s.child.kill();
        let _ = s.child.wait();
    }
    state.superseded.clear();
}

fn stop_child(child: &mut Option<Child>) {
    if let Some(mut c) = child.take() {
        // SIGKILL, deliberately. A SIGSTOPped child does not act on SIGTERM
        // until it is continued, which leaked 23 FFmpeg processes across one
        // bench sweep (ADR-0050 §3). SIGKILL cannot be blocked and terminates
        // a stopped process, so the throttle cannot strand a reap. Do not
        // "improve" this to terminate() without continuing the child first.
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// Suspend or resume one encoder. Unix only: ADR-0050 §9 scopes throttling to
/// Linux and macOS, because the property it depends on — that a suspended
/// encoder releases the hardware encoder — is a driver question that has not
/// been measured on Windows.
#[cfg(unix)]
fn signal_child(child: &Child, stop: bool) -> bool {
    // Take the numbers from libc, never by hand: SIGSTOP is 19 on Linux and
    // 17 on macOS, and 18 is SIGCONT on Linux but SIGTSTP on macOS. Written
    // out by hand they were inverted on Darwin, which suspends a session at
    // the floor and never resumes it.
    let sig = if stop { libc::SIGSTOP } else { libc::SIGCONT };
    // SAFETY: `kill` with a pid we own and a valid signal number. The pid
    // cannot have been recycled. This is only ever called on a rung's live
    // encoder child, and the three paths that reap a child all run under the
    // same lock as this call: `stop_child` takes the child out of encoder
    // state, and `reap_superseded` / `reap_all_superseded` only ever reap
    // children `supersede_child` already moved out of the rung's live state.
    // So a `Child` this call can see is not one any of them can be waiting on.
    unsafe { libc::kill(child.id() as libc::pid_t, sig) == 0 }
}

#[cfg(not(unix))]
fn signal_child(_child: &Child, _stop: bool) -> bool {
    false
}

/// Whether a session at `lead_ms` should change throttle state. `None` means
/// leave it alone.
///
/// The band is hysteresis, not a target: suspend at [`LEAD_TARGET_MS`], resume
/// at [`LEAD_FLOOR_MS`], do nothing between. A single threshold would suspend
/// and resume on adjacent ticks for the whole session.
fn throttle_action(throttled: bool, lead_ms: u64) -> Option<bool> {
    if !throttled && lead_ms >= LEAD_TARGET_MS {
        Some(true)
    } else if throttled && lead_ms <= LEAD_FLOOR_MS {
        Some(false)
    } else {
        None
    }
}

/// Media milliseconds produced beyond the playhead. `None` when nothing has
/// been produced, which is a session that has not started rather than one with
/// no lead: the throttle must leave it alone rather than read it as zero.
///
/// Saturating, because a playhead can legitimately sit past the frontier — a
/// client prefetching across the end of what is written asks for a segment
/// before it exists.
fn lead_ms(produced_end_ms: Option<u64>, last_requested_ms: u64) -> Option<u64> {
    Some(produced_end_ms?.saturating_sub(last_requested_ms))
}

/// [`lead_ms`] for one session, reading the frontier from the run that is
/// currently producing.
///
/// The rung map keeps prior runs' entries so scrub back stays a plain file
/// serve (ADR-0020 §3). Its maximum is therefore a frontier the live encoder
/// may be nowhere near after a backward seek, and using it would report no
/// lead for the rest of the session.
fn session_lead_ms(session: &Session) -> Option<u64> {
    // One live encoder per session, so the current run is the serving run.
    // ADR-0050 §4 breaks that: see the note on `frontier_ms`.
    let produced_end = frontier_ms(
        session.segment_map(SINGLE_VIDEO_RUNG).iter_ordered(),
        session.encoder_state(SINGLE_VIDEO_RUNG).current_run_id,
    );
    lead_ms(produced_end, session.last_requested_ms)
}

/// Furthest media end produced by `run_id`, ignoring every other run.
///
/// `run_id` is the run **serving this playhead**, which today is the same as
/// the session's current run because a seek kills the previous encoder before
/// starting the next. Under ADR-0050 §4 a seek spawns instead, so two runs are
/// live at once and "current" stops meaning "the one this playhead reads
/// from". Pass the serving run explicitly then; do not reach for
/// `session.current_run_id` here.
fn frontier_ms<'a>(
    mut segments: impl DoubleEndedIterator<Item = &'a crate::hls_segment_map::MappedSegment>,
    run_id: u64,
) -> Option<u64> {
    segments
        .rfind(|s| s.run_id == run_id)
        .map(|s| s.start_ms.saturating_add(s.duration_ms))
}

fn is_safe_asset(name: &str) -> bool {
    if name == "init.mp4" {
        return true;
    }
    crate::hls_segment_map::parse_time_keyed_segment_name(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BurnInKind;
    use crate::virtual_input::{KeyframeEntry, KeyframeMap, MapContainerKind};
    use std::process::Command;

    fn ffmpeg_available() -> bool {
        let ok = Command::new("ffmpeg")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok && std::env::var_os("NIGHTJAR_TEST_REQUIRE_FFMPEG").is_some() {
            panic!("NIGHTJAR_TEST_REQUIRE_FFMPEG is set but ffmpeg is not on PATH");
        }
        ok
    }

    /// A minimal session for unit tests that only touch process bookkeeping.
    fn make_test_session(dir: &Path) -> Session {
        Session {
            item_id: 1,
            src: PathBuf::from("/dev/null"),
            dir: dir.to_path_buf(),
            run_cache_budget_bytes: SESSION_RUN_CACHE_BUDGET_BYTES,
            mode: SessionMode::Copy,
            audio: stereo(),
            burn_in: None,
            encode_plan: VideoEncodePlan::default(),
            map_binding: MapBinding::default(),
            encode_leg: crate::EncodeLeg::software(),
            video_encoder: "copy".into(),
            start_ms: 0,
            play_start_ms: 0,
            landed_ms: 0,
            usable_extent_ms: None,
            duration_ms: 60_000,
            encoder_states: single_rung_encoder_states(0, 1, None),
            segment_maps: single_rung_segment_maps(Default::default()),
            current_run_eof: false,
            last_access: Instant::now(),
            last_restart: Instant::now(),
            primed: false,
            first_segment_ready: false,
            pending_play_ms: None,
            pending_since: None,
            failed: None,
            subtitle_tracks: vec![],
            last_requested_ms: 0,
            piggyback: None,
            subs: None,
            db: None,
            map_build_in_flight: None,
        }
    }

    /// A run advanced on one rung leaves every other rung's encoder alone.
    ///
    /// ADR-0051 amendment 4: encoder load is per rung, so run ids, the child
    /// and the superseded set are per rung too. Without that, a hop would
    /// renumber the rung it left. Collapse `encoder_state_mut` to one rung and
    /// this fails on the second rung's `current_run_id`.
    #[test]
    fn advancing_one_rungs_run_leaves_the_other_rung_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = make_test_session(dir.path());
        session.encoder_states = HashMap::from([
            (
                VideoRung::SingleVideo,
                EncoderState {
                    current_run_id: 0,
                    next_run_id: 1,
                    child: None,
                    child_rss_bytes: None,
                    throttled: false,
                    superseded: Vec::new(),
                },
            ),
            (
                VideoRung::SecondVideo,
                EncoderState {
                    current_run_id: 0,
                    next_run_id: 1,
                    child: None,
                    child_rss_bytes: None,
                    throttled: false,
                    superseded: Vec::new(),
                },
            ),
        ]);

        // Advance the *second* rung: a write that collapses to one rung lands
        // on the first, so the second reads back unchanged and this fails.
        {
            let state = session.encoder_state_mut(VideoRung::SecondVideo);
            state.current_run_id = state.next_run_id;
            state.next_run_id += 1;
            state.throttled = true;
        }

        let advanced = session.encoder_state(VideoRung::SecondVideo);
        assert_eq!(
            advanced.current_run_id, 1,
            "the rung that was advanced must carry the new run id"
        );
        assert!(advanced.throttled, "the advanced rung is the throttled one");

        let untouched = session.encoder_state(VideoRung::SingleVideo);
        assert_eq!(
            untouched.current_run_id, 0,
            "advancing one rung must not renumber another"
        );
        assert!(
            !untouched.throttled,
            "throttle is per rung, not per session"
        );
    }

    /// Equal title times remain distinct inside one session because every
    /// rung owns its own segment map (ADR-0051 amendment 2).
    #[test]
    fn equal_start_segments_survive_in_one_sessions_rung_maps() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = make_test_session(dir.path());
        session.segment_maps = HashMap::from([
            (VideoRung::SingleVideo, Default::default()),
            (VideoRung::SecondVideo, Default::default()),
        ]);

        for (rung, file_name) in [
            (VideoRung::SingleVideo, "single.m4s"),
            (VideoRung::SecondVideo, "second.m4s"),
        ] {
            let run = crate::hls_segment_map::run_rel_dir(rung, 0);
            fs::create_dir_all(dir.path().join(&run)).unwrap();
            fs::write(
                dir.path().join(&run).join(file_name),
                crate::hls_segment_map::fake_sidx_seg(42_000),
            )
            .unwrap();
            crate::hls_segment_map::ingest_run_index(
                session.segment_map_mut(rung),
                dir.path(),
                rung,
                0,
                &format!("#EXTM3U\n#EXTINF:2.000000,\n{file_name}\n"),
                42_000,
                None,
            )
            .unwrap();
        }

        for (rung, rel_path) in [
            (
                VideoRung::SingleVideo,
                PathBuf::from("vsingle/run_0/single.m4s"),
            ),
            (
                VideoRung::SecondVideo,
                PathBuf::from("vsecond/run_0/second.m4s"),
            ),
        ] {
            let map = session.segment_map(rung);
            assert_eq!(
                map.get(42_000).map(|segment| &segment.rel_path),
                Some(&rel_path)
            );
            assert_eq!(map.len(), 1);
        }
    }

    #[test]
    #[should_panic(expected = "session has no segment map for rung second")]
    fn missing_rung_map_is_a_session_construction_bug() {
        let dir = tempfile::tempdir().unwrap();
        let session = make_test_session(dir.path());

        let _ = session.segment_map(VideoRung::SecondVideo);
    }

    /// What a stand-in encoder is actually doing.
    ///
    /// `Running` and `Stopped` have to be separable. `kill(pid, 0)` succeeds
    /// for a SIGSTOPped process, so an existence check passes against a
    /// supersede that suspends the encoder instead of keeping it producing —
    /// which is the whole point of ADR-0050 §5.
    #[cfg(unix)]
    #[derive(Debug, PartialEq, Eq)]
    enum ChildState {
        Running,
        Stopped,
        Gone,
    }

    /// Read a stand-in encoder's state.
    ///
    /// The reap `wait()`s, so a reaped child is gone rather than a zombie and
    /// reads `Gone`. Assert on this rather than on
    /// `session.superseded.len()`: a reap that dropped the `Child` without
    /// killing it satisfies the length and leaks an FFmpeg per seek.
    ///
    /// `ps` rather than `waitpid`, deliberately. `waitpid` with `WUNTRACED`
    /// reports a stop **once** and then clears it, so a caller that has
    /// already waited for the child to stop reads the next check as `Running`
    /// and a test asserting "resumed" passes against code that never resumed
    /// it. That was written first and it did pass. `ps` reads the current
    /// state every time. `T` is stopped on both macOS and Linux; a running
    /// child is `S` or `R`.
    #[cfg(unix)]
    fn child_state(pid: u32) -> ChildState {
        // SAFETY: signal 0 delivers nothing. The pid comes from a child this
        // test spawned and has not waited on except through the reap under
        // test, so it is either ours or unallocated.
        if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
            return ChildState::Gone;
        }
        let out = std::process::Command::new("ps")
            .args(["-o", "state=", "-p", &pid.to_string()])
            .output()
            .expect("run ps");
        let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if state.is_empty() {
            return ChildState::Gone;
        }
        if state.starts_with('T') {
            ChildState::Stopped
        } else {
            ChildState::Running
        }
    }

    /// Wait, bounded, for a signal to be delivered and reflected.
    ///
    /// `kill` returns once the signal is queued, not once the target has acted
    /// on it, so reading the state straight after can race the delivery.
    #[cfg(unix)]
    fn wait_for_state(pid: u32, want: ChildState) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if child_state(pid) == want {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("pid {pid} never reached {want:?}");
    }

    /// Stands in for an encoder mid-run: alive, and long enough that only the
    /// reap can end it inside the test.
    #[cfg(unix)]
    fn spawn_stand_in_encoder() -> Child {
        std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep")
    }

    #[cfg(unix)]
    #[test]
    fn superseded_encoder_counts_against_live_encoder_cap() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = make_test_session(dir.path());
        let state = session.encoder_state_mut(SINGLE_VIDEO_RUNG);
        state.child = Some(spawn_stand_in_encoder());
        state.superseded.push(SupersededEncoder {
            child: spawn_stand_in_encoder(),
            rss_bytes: None,
            reap_at: Instant::now() + REAP_AFTER,
            run_id: 1,
        });
        let mut sessions = HashMap::from([("s1".to_string(), session)]);

        assert_eq!(live_encoder_load_centi(&sessions), 200);

        let session = sessions.get_mut("s1").unwrap();
        stop_child(&mut session.encoder_state_mut(SINGLE_VIDEO_RUNG).child);
        reap_all_superseded(session);
    }

    #[cfg(unix)]
    #[test]
    fn two_live_rungs_count_as_two_encoders() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = make_test_session(dir.path());
        session.encoder_states = HashMap::from([
            (
                VideoRung::SingleVideo,
                EncoderState {
                    current_run_id: 0,
                    next_run_id: 1,
                    child: Some(spawn_stand_in_encoder()),
                    child_rss_bytes: None,
                    throttled: false,
                    superseded: Vec::new(),
                },
            ),
            (
                VideoRung::SecondVideo,
                EncoderState {
                    current_run_id: 0,
                    next_run_id: 1,
                    child: Some(spawn_stand_in_encoder()),
                    child_rss_bytes: None,
                    throttled: false,
                    superseded: Vec::new(),
                },
            ),
        ]);
        let mut sessions = HashMap::from([("s1".to_string(), session)]);

        assert_eq!(live_encoder_load_centi(&sessions), 200);

        let session = sessions.get_mut("s1").unwrap();
        for rung in [VideoRung::SingleVideo, VideoRung::SecondVideo] {
            stop_child(&mut session.encoder_state_mut(rung).child);
        }
    }

    /// This is the boundary that M1's unit-mismatch mutation below breaks.
    /// It is deliberately in centi/cap units so a future unit mismatch is
    /// caught even though equal weights match a raw encoder count today.
    #[cfg(unix)]
    #[test]
    fn weighted_admission_respects_centi_cap_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let mut sessions = HashMap::new();
        for id in ["s1", "s2", "s3"] {
            let mut session = make_test_session(dir.path());
            session.encoder_state_mut(SINGLE_VIDEO_RUNG).child = Some(spawn_stand_in_encoder());
            sessions.insert(id.to_string(), session);
        }

        assert_eq!(live_encoder_load_centi(&sessions), 300);
        assert_eq!(
            u8::from(admits_new_session(
                &sessions,
                SessionMode::Copy,
                None,
                Some(3),
                EncoderMemory::Unmeasured,
                0,
            )),
            0
        );
        assert_eq!(
            u8::from(admits_new_session(
                &sessions,
                SessionMode::Copy,
                None,
                Some(4),
                EncoderMemory::Unmeasured,
                0,
            )),
            1
        );

        for session in sessions.values_mut() {
            stop_child(&mut session.encoder_state_mut(SINGLE_VIDEO_RUNG).child);
        }
    }

    #[cfg(unix)]
    #[test]
    fn no_configured_cap_admits_past_the_override_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = make_test_session(dir.path());
        session.encoder_state_mut(SINGLE_VIDEO_RUNG).child = Some(spawn_stand_in_encoder());
        let mut sessions = HashMap::from([("s1".to_string(), session)]);

        assert_eq!(
            [
                admits_new_session(
                    &sessions,
                    SessionMode::Copy,
                    None,
                    Some(1),
                    EncoderMemory::Unmeasured,
                    0,
                ),
                admits_new_session(
                    &sessions,
                    SessionMode::Copy,
                    None,
                    None,
                    EncoderMemory::Unmeasured,
                    0,
                ),
            ],
            [false, true]
        );

        let session = sessions.get_mut("s1").unwrap();
        stop_child(&mut session.encoder_state_mut(SINGLE_VIDEO_RUNG).child);
    }

    #[test]
    fn unmeasured_memory_admits() {
        assert!(memory_admits_new_session(EncoderMemory::Unmeasured, 1024));
    }

    #[test]
    fn zero_live_children_admit() {
        assert!(memory_admits_new_session(
            EncoderMemory::Measured {
                live_rss_bytes: 0,
                available_bytes: 0,
                children: 0,
            },
            1024,
        ));
    }

    #[test]
    fn no_high_water_sample_admits() {
        assert!(memory_admits_new_session(
            EncoderMemory::Measured {
                live_rss_bytes: 1024,
                available_bytes: 0,
                children: 1,
            },
            0,
        ));
    }

    #[test]
    fn memory_guard_reserves_two_high_water_children() {
        let memory = |available_bytes| EncoderMemory::Measured {
            live_rss_bytes: 100,
            available_bytes,
            children: 1,
        };

        assert_eq!(
            [
                memory_admits_new_session(memory(199), 100),
                memory_admits_new_session(memory(200), 100),
            ],
            [false, true]
        );
    }

    #[cfg(unix)]
    #[test]
    fn rss_high_water_counts_superseded_children_and_never_shrinks() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = make_test_session(dir.path());
        let current = spawn_stand_in_encoder();
        let current_pid = current.id();
        let held = spawn_stand_in_encoder();
        let held_pid = held.id();
        let state = session.encoder_state_mut(SINGLE_VIDEO_RUNG);
        state.child = Some(current);
        state.superseded.push(SupersededEncoder {
            child: held,
            rss_bytes: None,
            reap_at: Instant::now() + REAP_AFTER,
            run_id: 7,
        });
        let mut sessions = HashMap::from([("s1".to_string(), session)]);
        let high_water = AtomicU64::new(0);
        let current_process = EncoderProcess {
            session_id: "s1".into(),
            rung: SINGLE_VIDEO_RUNG,
            pid: current_pid,
            superseded_run_id: None,
        };
        let held_process = EncoderProcess {
            session_id: "s1".into(),
            rung: SINGLE_VIDEO_RUNG,
            pid: held_pid,
            superseded_run_id: Some(7),
        };
        record_encoder_rss_sample(&mut sessions, &high_water, &current_process, Some(100));
        record_encoder_rss_sample(&mut sessions, &high_water, &held_process, Some(300));
        record_encoder_rss_sample(&mut sessions, &high_water, &current_process, Some(50));
        record_encoder_rss_sample(&mut sessions, &high_water, &current_process, None);

        assert_eq!(
            (
                high_water.load(Ordering::Relaxed),
                live_encoder_rss(&sessions),
            ),
            (300, (None, 2))
        );

        let session = sessions.get_mut("s1").unwrap();
        stop_child(&mut session.encoder_state_mut(SINGLE_VIDEO_RUNG).child);
        reap_all_superseded(session);
    }

    /// A seek keeps the prior encoder alive and reaps it on its delay, not
    /// before (ADR-0050 §4-§5). Killing it at the seek is what the old shape
    /// did, and it cost 1055 ms.
    ///
    /// Every assertion here is on the process, not on the bookkeeping.
    #[cfg(unix)]
    #[test]
    fn a_superseded_encoder_lives_until_its_delay_is_up() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = make_test_session(dir.path());

        // Nothing held: reaping is a no-op rather than an error.
        reap_superseded(&mut session);
        assert!(
            session
                .encoder_state(SINGLE_VIDEO_RUNG)
                .superseded
                .is_empty()
        );

        session.encoder_state_mut(SINGLE_VIDEO_RUNG).current_run_id = 7;
        session.encoder_state_mut(SINGLE_VIDEO_RUNG).child = Some(spawn_stand_in_encoder());
        let pid = session
            .encoder_state(SINGLE_VIDEO_RUNG)
            .child
            .as_ref()
            .unwrap()
            .id();
        assert_eq!(child_state(pid), ChildState::Running);

        supersede_child(&mut session);
        assert_eq!(
            child_state(pid),
            ChildState::Running,
            "a seek supersedes the prior encoder, it does not kill it"
        );
        assert_eq!(
            session
                .encoder_state(SINGLE_VIDEO_RUNG)
                .superseded
                .first()
                .map(|s| s.run_id),
            Some(7),
            "the held encoder carries the run it is still writing into"
        );

        // Not due yet.
        reap_superseded(&mut session);
        assert_eq!(
            child_state(pid),
            ChildState::Running,
            "not past REAP_AFTER, so still producing"
        );
        assert_eq!(session.encoder_state(SINGLE_VIDEO_RUNG).superseded.len(), 1);

        // Due.
        session.encoder_state_mut(SINGLE_VIDEO_RUNG).superseded[0].reap_at =
            Instant::now() - Duration::from_millis(1);
        reap_superseded(&mut session);
        assert_eq!(
            child_state(pid),
            ChildState::Gone,
            "past its delay, so the process is gone"
        );
        assert!(
            session
                .encoder_state(SINGLE_VIDEO_RUNG)
                .superseded
                .is_empty()
        );
    }

    /// A seek on a session the throttle had suspended resumes the encoder
    /// before setting it aside.
    ///
    /// This is the common case, not the rare one: a session that has caught
    /// up sits SIGSTOPped at `LEAD_TARGET_MS`, which is the shape ADR-0050 §2
    /// designs for. Without the SIGCONT the seek sets aside a stopped process
    /// for `REAP_AFTER` and then kills it, which is suspend-then-reap — the
    /// policy §5 was amended to forbid, arriving through the throttle instead
    /// of through `supersede_child` asking for it.
    ///
    /// `kill(pid, 0)` cannot see this: it succeeds for a stopped process.
    #[cfg(unix)]
    #[test]
    fn superseding_a_throttled_encoder_resumes_it_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = make_test_session(dir.path());
        session.encoder_state_mut(SINGLE_VIDEO_RUNG).child = Some(spawn_stand_in_encoder());
        let pid = session
            .encoder_state(SINGLE_VIDEO_RUNG)
            .child
            .as_ref()
            .unwrap()
            .id();

        // What the throttle does at the lead target.
        assert!(signal_child(
            session
                .encoder_state(SINGLE_VIDEO_RUNG)
                .child
                .as_ref()
                .unwrap(),
            true
        ));
        session.encoder_state_mut(SINGLE_VIDEO_RUNG).throttled = true;
        wait_for_state(pid, ChildState::Stopped);

        supersede_child(&mut session);
        assert_eq!(
            child_state(pid),
            ChildState::Running,
            "a superseded encoder has to keep producing until it is reaped"
        );
        assert!(
            !session.encoder_state(SINGLE_VIDEO_RUNG).throttled,
            "the flag described the child that just left"
        );

        reap_all_superseded(&mut session);
        assert_eq!(child_state(pid), ChildState::Gone);
    }

    /// Session teardown takes them all, whatever their delay: nothing may
    /// outlive the session that spawned it.
    #[cfg(unix)]
    #[test]
    fn teardown_reaps_every_superseded_encoder() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = make_test_session(dir.path());
        let mut pids = Vec::new();
        for run_id in 0..3u64 {
            let child = spawn_stand_in_encoder();
            pids.push(child.id());
            session
                .encoder_state_mut(SINGLE_VIDEO_RUNG)
                .superseded
                .push(SupersededEncoder {
                    child,
                    rss_bytes: None,
                    reap_at: Instant::now() + Duration::from_secs(30),
                    run_id,
                });
        }
        assert!(pids.iter().all(|p| child_state(*p) == ChildState::Running));

        reap_all_superseded(&mut session);
        assert!(
            session
                .encoder_state(SINGLE_VIDEO_RUNG)
                .superseded
                .is_empty()
        );
        for pid in pids {
            assert_eq!(
                child_state(pid),
                ChildState::Gone,
                "teardown leaves no encoder behind"
            );
        }
    }

    /// The run a superseded encoder is still writing into is not evictable.
    ///
    /// Eviction excluded only `current_run_id`, which was safe while a seek
    /// killed the prior encoder before any cleanup ran. Under spawn-and-reap
    /// that run is live for [`REAP_AFTER`], and unlinking it takes away the
    /// media the policy exists to keep serving — silently, because nothing
    /// waits on a superseded child.
    #[cfg(unix)]
    #[test]
    fn eviction_leaves_a_live_superseded_run_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = make_test_session(dir.path());

        // Three runs on disk. run_1 is finished, run_2 is the one the seek
        // set aside, run_3 is the new producer.
        for run_id in 1..=3u64 {
            let run = run_path(dir.path(), SINGLE_VIDEO_RUNG, run_id);
            fs::create_dir_all(&run).unwrap();
            fs::write(run.join("seg.m4s"), vec![0u8; 4096]).unwrap();
        }
        session.encoder_state_mut(SINGLE_VIDEO_RUNG).current_run_id = 3;
        session
            .encoder_state_mut(SINGLE_VIDEO_RUNG)
            .superseded
            .push(SupersededEncoder {
                child: spawn_stand_in_encoder(),
                rss_bytes: None,
                reap_at: Instant::now() + Duration::from_secs(30),
                run_id: 2,
            });

        // A budget everything on disk exceeds, so eviction must pick a victim.
        session.run_cache_budget_bytes = 0;
        maybe_evict_finished_runs(&mut session);

        assert!(
            !run_path(dir.path(), SINGLE_VIDEO_RUNG, 1).exists(),
            "a finished run is still evictable"
        );
        assert!(
            run_path(dir.path(), SINGLE_VIDEO_RUNG, 2).exists(),
            "a superseded encoder is still writing into run_2"
        );
        assert!(
            run_path(dir.path(), SINGLE_VIDEO_RUNG, 3).exists(),
            "the current run stays"
        );

        reap_all_superseded(&mut session);
    }

    /// Write one producer run: the segment files, their `index.m3u8`, and the
    /// `encode_start_ms` beside them. `starts_ms` are title-absolute.
    fn write_producer_run(
        session_dir: &Path,
        run_id: u64,
        encode_start_ms: u64,
        starts_ms: &[u64],
    ) {
        let run = run_path(session_dir, SINGLE_VIDEO_RUNG, run_id);
        fs::create_dir_all(&run).unwrap();
        let mut index = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:2\n");
        for (i, start) in starts_ms.iter().enumerate() {
            let file = format!("seg{i:03}.m4s");
            fs::write(
                run.join(&file),
                crate::hls_segment_map::fake_sidx_seg(*start as u32),
            )
            .unwrap();
            index.push_str(&format!("#EXTINF:2.000000,\n{file}\n"));
        }
        fs::write(run.join("index.m3u8"), index).unwrap();
        write_run_encode_start(&run, encode_start_ms).unwrap();
    }

    /// A waiter can read what the superseded encoder wrote after the seek.
    ///
    /// The gap this covers is the one the hold exists for: the old encoder
    /// keeps producing into `run_0` for `REAP_AFTER`, and the new encoder
    /// starts at the new land and never produces behind it. Before the poll
    /// path ingested the held runs, the only refresh was `sync_segment_map`
    /// on the current run, so nothing that landed in `run_0` after the seek
    /// was ever mapped and neither encoder could serve the want.
    ///
    /// `restart_at` is `sync_all_run_indexes`'s one caller, so a test that
    /// seeks twice passes for the wrong reason. This one seeks once, by hand,
    /// and asserts the gap is absent from the map before it asks for it.
    #[cfg(unix)]
    #[test]
    fn a_waiter_reads_what_the_superseded_encoder_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264", None, None).unwrap();
        let session_dir = dir.path().join("hls").join("s1");
        fs::create_dir_all(&session_dir).unwrap();

        // run_0 produced to a frontier of 2000 (media ending at 4000) before
        // the seek. The seek lands at 20000, where run_1 starts producing.
        let land_ms = 20_000u64;
        let gap_ms = 4_000u64;
        write_producer_run(&session_dir, 0, 0, &[0, 2_000]);
        write_producer_run(&session_dir, 1, land_ms, &[land_ms]);

        let mut session = make_test_session(&session_dir);
        session.duration_ms = 120_000;
        // What `restart_at` leaves behind: run_0 held and still running,
        // run_1 current at the new land.
        let run0 = run_path(&session_dir, SINGLE_VIDEO_RUNG, 0);
        crate::hls_segment_map::ingest_run_index(
            session.segment_map_mut(SINGLE_VIDEO_RUNG),
            &session_dir,
            SINGLE_VIDEO_RUNG,
            0,
            &fs::read_to_string(run0.join("index.m3u8")).unwrap(),
            0,
            None,
        )
        .unwrap();
        session.encoder_state_mut(SINGLE_VIDEO_RUNG).current_run_id = 1;
        session.encoder_state_mut(SINGLE_VIDEO_RUNG).next_run_id = 2;
        session.start_ms = land_ms;
        session.play_start_ms = land_ms;
        session.last_requested_ms = land_ms;
        session.landed_ms = land_ms;
        session.encoder_state_mut(SINGLE_VIDEO_RUNG).child = Some(spawn_stand_in_encoder());
        session
            .encoder_state_mut(SINGLE_VIDEO_RUNG)
            .superseded
            .push(SupersededEncoder {
                child: spawn_stand_in_encoder(),
                rss_bytes: None,
                // The real delay is `REAP_AFTER`; parked out of reach so the
                // throttle tick cannot reap run_0 out from under the assertion.
                reap_at: Instant::now() + Duration::from_secs(30),
                run_id: 0,
            });
        // Both changes touch the same held set, so check them together. The
        // release condition must not fire here: run_0's frontier is 4000 and
        // the new land is 20000, so the gap below is exactly the region only
        // the held encoder serves.
        let held_reap_at = session.encoder_state(SINGLE_VIDEO_RUNG).superseded[0].reap_at;
        release_overtaken_superseded(&mut session);
        assert_eq!(
            session.encoder_state(SINGLE_VIDEO_RUNG).superseded[0].reap_at,
            held_reap_at,
            "the release condition must not fire while the gap is still unserved"
        );
        reg.sessions
            .lock()
            .unwrap()
            .insert("s1".to_string(), session);

        // The held encoder writes one more segment into its own run: past
        // everything run_0 had produced at the seek, and behind the new land.
        write_producer_run(&session_dir, 0, 0, &[0, 2_000, gap_ms]);
        {
            let sessions = reg.sessions.lock().unwrap();
            let session = sessions.get("s1").unwrap();
            assert!(
                session.segment_map(SINGLE_VIDEO_RUNG).get(gap_ms).is_none(),
                "the gap segment must be absent from the map before the request, \
                 or the request proves nothing"
            );
        }

        let name = crate::hls_segment_map::time_keyed_segment_name(gap_ms);
        let served = reg.asset("s1", &name, None);
        assert_eq!(
            served.as_deref().map_err(|e| format!("{e:?}")),
            Ok(crate::hls_segment_map::fake_sidx_seg(gap_ms as u32).as_slice()),
            "a request in the gap must be served the bytes the held encoder wrote"
        );

        reg.stop("s1");
    }

    /// A held encoder still short of the new land keeps its delay.
    ///
    /// This is the over-firing control. A forward seek past what the prior
    /// encoder has produced leaves `[frontier, start_ms)` non-empty, and only
    /// the held encoder will ever produce it, so releasing here would be the
    /// trade condition 1 does not make.
    #[cfg(unix)]
    #[test]
    fn a_held_encoder_short_of_the_new_land_keeps_its_delay() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = make_test_session(dir.path());

        // run_0 produced to a frontier of 4000; the seek lands at 20000.
        write_producer_run(dir.path(), 0, 0, &[0, 2_000]);
        let child = spawn_stand_in_encoder();
        let pid = child.id();
        let reap_at = Instant::now() + REAP_AFTER;
        session
            .encoder_state_mut(SINGLE_VIDEO_RUNG)
            .superseded
            .push(SupersededEncoder {
                child,
                rss_bytes: None,
                reap_at,
                run_id: 0,
            });
        sync_superseded_run_indexes(&mut session, SINGLE_VIDEO_RUNG);
        session.encoder_state_mut(SINGLE_VIDEO_RUNG).current_run_id = 1;
        session.encoder_state_mut(SINGLE_VIDEO_RUNG).next_run_id = 2;
        session.start_ms = 20_000;

        release_overtaken_superseded(&mut session);
        assert_eq!(
            session.encoder_state(SINGLE_VIDEO_RUNG).superseded[0].reap_at,
            reap_at,
            "a held encoder short of the new land keeps its original delay"
        );

        reap_superseded(&mut session);
        assert_eq!(
            child_state(pid),
            ChildState::Running,
            "the next reap must leave it producing"
        );

        reap_all_superseded(&mut session);
    }

    /// A held encoder the new one has already overtaken is released early.
    ///
    /// Segments within a run are sequential, so a frontier at or past
    /// `start_ms` means everything behind the new land is already written and
    /// already mapped. Everything the held encoder goes on to write is media
    /// the map has. This is the backward seek that hits the duplicate-write
    /// stop: nothing replaces the prior encoder there, so without this it does
    /// five seconds of entirely duplicate work.
    #[cfg(unix)]
    #[test]
    fn a_held_encoder_past_the_new_land_is_released_early() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = make_test_session(dir.path());

        // run_0 produced to a frontier of 6000; the seek lands back at 4000.
        write_producer_run(dir.path(), 0, 0, &[0, 2_000, 4_000]);
        let child = spawn_stand_in_encoder();
        let pid = child.id();
        session
            .encoder_state_mut(SINGLE_VIDEO_RUNG)
            .superseded
            .push(SupersededEncoder {
                child,
                rss_bytes: None,
                reap_at: Instant::now() + REAP_AFTER,
                run_id: 0,
            });
        sync_superseded_run_indexes(&mut session, SINGLE_VIDEO_RUNG);
        session.encoder_state_mut(SINGLE_VIDEO_RUNG).current_run_id = 1;
        session.encoder_state_mut(SINGLE_VIDEO_RUNG).next_run_id = 2;
        session.start_ms = 4_000;

        release_overtaken_superseded(&mut session);
        assert!(
            session.encoder_state(SINGLE_VIDEO_RUNG).superseded[0].reap_at <= Instant::now(),
            "a held encoder the new one has overtaken is due for reaping now"
        );

        // The tick that already does the killing takes it.
        reap_superseded(&mut session);
        assert_eq!(
            child_state(pid),
            ChildState::Gone,
            "the next reap releases the overtaken encoder"
        );
        assert!(
            session
                .encoder_state(SINGLE_VIDEO_RUNG)
                .superseded
                .is_empty()
        );
    }

    /// A held encoder that produced nothing has no frontier, and is not
    /// released.
    ///
    /// [`frontier_ms`] answers per run, and `None` is "has not started", not
    /// "is at zero". Reading it as zero would release every encoder that never
    /// wrote a segment on any seek, including a seek to the start of the
    /// title. The map here holds another run's segments out past the land, so
    /// this also fails if the frontier is read across the whole rung map.
    #[cfg(unix)]
    #[test]
    fn a_held_encoder_that_produced_nothing_is_not_released() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = make_test_session(dir.path());

        // run_0 is mapped out to 6000. run_9 is the held encoder, and it has
        // written nothing.
        write_producer_run(dir.path(), 0, 0, &[0, 2_000, 4_000]);
        let child = spawn_stand_in_encoder();
        let pid = child.id();
        let reap_at = Instant::now() + REAP_AFTER;
        session
            .encoder_state_mut(SINGLE_VIDEO_RUNG)
            .superseded
            .push(SupersededEncoder {
                child,
                rss_bytes: None,
                reap_at,
                run_id: 9,
            });
        session.encoder_state_mut(SINGLE_VIDEO_RUNG).current_run_id = 0;
        sync_segment_map(&mut session, SINGLE_VIDEO_RUNG);
        session.encoder_state_mut(SINGLE_VIDEO_RUNG).current_run_id = 10;
        session.encoder_state_mut(SINGLE_VIDEO_RUNG).next_run_id = 11;
        session.start_ms = 0;

        assert!(
            frontier_ms(session.segment_map(SINGLE_VIDEO_RUNG).iter_ordered(), 9).is_none(),
            "the held run must have produced nothing, or the test proves nothing"
        );
        release_overtaken_superseded(&mut session);
        assert_eq!(
            session.encoder_state(SINGLE_VIDEO_RUNG).superseded[0].reap_at,
            reap_at,
            "an encoder that produced nothing has no frontier to compare"
        );

        reap_superseded(&mut session);
        assert_eq!(child_state(pid), ChildState::Running);

        reap_all_superseded(&mut session);
    }

    /// The seek path itself releases the encoder it overtook.
    ///
    /// A backward seek into mapped media takes `restart_at`'s map-hit exit:
    /// it supersedes the prior encoder, sets `current_run_eof`, and **does not
    /// spawn**. So the prior encoder is held with nothing replacing it, and
    /// everything it goes on to write is already in the map. This asserts the
    /// call site, not just the condition — the two exits assign `start_ms`
    /// separately and the map-hit one returns early.
    #[cfg(unix)]
    #[test]
    fn a_backward_seek_releases_the_encoder_it_overtook() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("s1");
        fs::create_dir_all(&session_dir).unwrap();
        // run_0 has produced out to 6000 and is the current producer.
        write_producer_run(&session_dir, 0, 0, &[0, 2_000, 4_000]);

        let mut session = make_test_session(&session_dir);
        session.encoder_state_mut(SINGLE_VIDEO_RUNG).current_run_id = 0;
        session.encoder_state_mut(SINGLE_VIDEO_RUNG).next_run_id = 1;
        session.start_ms = 0;
        session.play_start_ms = 0;
        session.last_requested_ms = 4_000;
        session.encoder_state_mut(SINGLE_VIDEO_RUNG).child = Some(spawn_stand_in_encoder());
        let pid = session
            .encoder_state(SINGLE_VIDEO_RUNG)
            .child
            .as_ref()
            .unwrap()
            .id();

        restart_at(&mut session, 2_000, &crate::EncodeLeg::software()).unwrap();

        assert_eq!(
            session.start_ms, 2_000,
            "the map-hit exit lands on the mapped segment"
        );
        assert_eq!(
            session
                .encoder_state(SINGLE_VIDEO_RUNG)
                .superseded
                .first()
                .map(|s| s.run_id),
            Some(0),
            "the seek held the prior encoder"
        );
        assert!(
            session.encoder_state(SINGLE_VIDEO_RUNG).superseded[0].reap_at <= Instant::now(),
            "a backward seek into mapped media releases the encoder it overtook"
        );
        assert_eq!(
            child_state(pid),
            ChildState::Running,
            "the seek path moves reap_at; it does not kill under the lock"
        );

        reap_superseded(&mut session);
        assert_eq!(child_state(pid), ChildState::Gone);
        assert!(
            session
                .encoder_state(SINGLE_VIDEO_RUNG)
                .superseded
                .is_empty()
        );
    }

    /// ADR-0052: the frame count for one segment comes from the source rate,
    /// so it is a different number per source and never a constant. This is
    /// the arithmetic that `-g 48` got wrong by being written down once.
    /// ADR-0050 §2: the band is hysteresis. Suspend at the target, resume at
    /// the floor, and do nothing between, or a session at the threshold
    /// suspends and resumes on adjacent ticks for its whole life.
    #[test]
    fn throttle_band_is_hysteresis_not_a_threshold() {
        // Running, below the target: leave it alone.
        assert_eq!(throttle_action(false, 0), None);
        assert_eq!(throttle_action(false, LEAD_FLOOR_MS), None);
        assert_eq!(throttle_action(false, LEAD_TARGET_MS - 1), None);
        // Running, at or past the target: suspend.
        assert_eq!(throttle_action(false, LEAD_TARGET_MS), Some(true));
        assert_eq!(throttle_action(false, LEAD_TARGET_MS * 4), Some(true));
        // Suspended, still above the floor: stay suspended. This is the half
        // a single threshold would get wrong.
        assert_eq!(throttle_action(true, LEAD_TARGET_MS), None);
        assert_eq!(throttle_action(true, LEAD_FLOOR_MS + 1), None);
        // Suspended, at or below the floor: resume.
        assert_eq!(throttle_action(true, LEAD_FLOOR_MS), Some(false));
        assert_eq!(throttle_action(true, 0), Some(false));
    }

    /// The lead is measured from the furthest segment asked for, not from the
    /// encode start. A session that has produced nothing has no lead to read,
    /// which is different from having a lead of zero.
    #[test]
    fn lead_is_produced_media_beyond_the_playhead() {
        // Nothing produced is not a lead of zero: the throttle must not act.
        assert_eq!(lead_ms(None, 0), None);
        assert_eq!(lead_ms(None, 500_000), None);
        // Produced to 4 s with the playhead at the start.
        assert_eq!(lead_ms(Some(4000), 0), Some(4000));
        // Mid-title: the lead is the gap, not the frontier.
        assert_eq!(lead_ms(Some(640_000), 600_000), Some(40_000));
        // Playhead level with the frontier.
        assert_eq!(lead_ms(Some(4000), 4000), Some(0));
        // A prefetch past the frontier saturates rather than wrapping.
        assert_eq!(lead_ms(Some(4000), 10_000), Some(0));
    }

    /// The frontier is the producing run's, not the rung map's maximum.
    ///
    /// The map keeps prior runs so scrub back is a plain file serve
    /// (ADR-0020 §3). After a seek back from 50 min to 10 min it still holds
    /// segments out to 50 min while the live encoder is at 10. Reading the
    /// map maximum reports a frontier far past the playhead, the lead
    /// saturates to zero, and the throttle never fires again for that session.
    #[test]
    fn backward_seek_does_not_disable_the_throttle() {
        use crate::hls_segment_map::MappedSegment;
        let seg = |start_ms, run_id| MappedSegment {
            start_ms,
            duration_ms: 2000,
            run_id,
            rel_path: crate::hls_segment_map::run_rel_dir(SINGLE_VIDEO_RUNG, run_id)
                .join("seg.m4s"),
        };
        // Run 0 reached 50 minutes before the seek; run 1 landed at 10 and has
        // produced 40 s past it. Map order is by start time, so run 0's entry
        // sorts last.
        let segs = [seg(600_000, 1), seg(638_000, 1), seg(3_000_000, 0)];

        assert_eq!(
            frontier_ms(segs.iter(), 1),
            Some(640_000),
            "the producing run's frontier, not the map's maximum"
        );
        let lead = lead_ms(frontier_ms(segs.iter(), 1), 600_000);
        assert_eq!(lead, Some(40_000));
        assert_eq!(
            throttle_action(false, lead.unwrap()),
            Some(true),
            "a session 40 s ahead must suspend, whatever prior runs left behind"
        );

        // The bug this pins: the map maximum saturates the lead to zero.
        let stale = lead_ms(
            segs.iter().next_back().map(|s| s.start_ms + s.duration_ms),
            600_000,
        );
        assert_eq!(stale, Some(2_402_000));
        assert_eq!(
            throttle_action(false, 0),
            None,
            "a lead read as zero never suspends"
        );
    }

    /// Every run shares one grid, whatever land it was snapped to.
    ///
    /// Without this each run is phased to its own cue. Measured at `c43b440`,
    /// three runs of one session started at `0`, `3780110` and `590632`.
    #[test]
    fn snapping_puts_every_run_on_one_phase() {
        let plan_for = |cue: u64| StartPlan {
            input: std::ffi::OsString::from("/dev/null"),
            window_start_ms: cue,
            seek_input: false,
            start_path: "mapped",
            container_kind: "matroska",
            fingerprint_cost_ms: 0,
            drop_ms: 0,
            virtual_input: None,
        };

        // The three cues measured on the N150, against the 2002 ms cadence
        // that hardware actually produces.
        for cue in [0u64, 3_780_110, 590_632] {
            let mut plan = plan_for(cue);
            snap_plan_to_grid(&mut plan, 2002);
            assert_eq!(
                plan.window_start_ms % 2002,
                0,
                "cue {cue} must land on the shared grid, not its own phase"
            );
            assert_eq!(
                plan.window_start_ms - plan.drop_ms,
                cue,
                "the drop is exactly the media between the cue and the grid"
            );
            assert!(
                plan.drop_ms < 2002,
                "never drop a whole segment: {} at cue {cue}",
                plan.drop_ms
            );
        }

        // A cue already on the grid drops nothing.
        let mut on_grid = plan_for(4004);
        snap_plan_to_grid(&mut on_grid, 2002);
        assert_eq!(on_grid.window_start_ms, 4004);
        assert_eq!(on_grid.drop_ms, 0, "a cue on the grid has nothing to drop");
    }

    /// Copy never gets a grid: it re-encodes nothing, so it cannot drop the
    /// media between the cue and the grid.
    #[test]
    fn only_transcode_gets_a_shared_grid() {
        let film = VideoEncodePlan {
            source_frame_rate: Some((24000, 1001)),
            ..VideoEncodePlan::default()
        };
        const FILM: u64 = 7_200_000;
        // `libx264` honours `-force_key_frames`, so its grid is SEGMENT_MS.
        let sw = crate::EncodeLeg::software();
        assert_eq!(
            grid_cadence_ms(SessionMode::Transcode, false, &sw, &film, FILM),
            GridCadence::Cadence(SEGMENT_MS)
        );
        assert_eq!(
            grid_cadence_ms(SessionMode::Copy, false, &sw, &film, FILM),
            GridCadence::KeyframeWalk,
            "copy cannot drop to a grid and must keep its own phase"
        );
        // Burn-in re-encodes video whatever the mode says (ADR-0018).
        assert_eq!(
            grid_cadence_ms(SessionMode::Copy, true, &sw, &film, FILM),
            GridCadence::Cadence(SEGMENT_MS),
            "burn-in is a transcode for this question"
        );
        // **The two absences are different answers and must stay apart.** A
        // transcode with no listable grid used to arrive as the same `None`
        // copy does, and `full_title_entries` then handed it copy's keyframe
        // walk (entry 18). Copy's answer is a walk; this one is a per-run
        // window listing.
        assert_eq!(
            grid_cadence_ms(
                SessionMode::Transcode,
                false,
                &sw,
                &VideoEncodePlan::default(),
                FILM
            ),
            GridCadence::NoHonestGrid
        );
        assert_ne!(
            grid_cadence_ms(
                SessionMode::Transcode,
                false,
                &sw,
                &VideoEncodePlan::default(),
                FILM
            ),
            grid_cadence_ms(SessionMode::Copy, false, &sw, &film, FILM),
            "no-honest-grid must not be indistinguishable from copy"
        );
    }

    /// The narrowing keeps what the dig-back guard was built for.
    ///
    /// `digback_behind_committed` declines a behind-committed GET as junk. A
    /// URI the playlist lists is not junk, so the guard now yields to it — but
    /// **an unlisted want must still decline**, or a prefetching client turns
    /// a full-title listing into the restart storm the guard was measured to
    /// stop.
    #[test]
    fn only_a_listed_want_escapes_the_digback_guard() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = eof_test_session(dir.path(), 1_354_496);
        session.mode = SessionMode::Transcode;
        session.encode_plan = VideoEncodePlan {
            source_frame_rate: Some((24000, 1001)),
            ..VideoEncodePlan::default()
        };
        // The fixture session runs on `libx264`, which honours
        // `-force_key_frames`, so its grid is SEGMENT_MS. **This read 2002
        // until 2026-08-31**, from the frame count, which is the arm only a
        // leg that discards the flag takes (entry 20).
        let step = SEGMENT_MS;

        assert!(
            want_is_listed(&session, 40 * step),
            "on the grid and inside the title is listed"
        );
        assert!(
            !want_is_listed(&session, 40 * step + 1),
            "off the grid is not listed and must keep declining"
        );
        assert!(
            !want_is_listed(&session, 2_000_000),
            "past the extent is not listed and must keep declining"
        );

        // Copy has no full-title listing yet, so the guard is untouched for it.
        session.mode = SessionMode::Copy;
        assert!(
            !want_is_listed(&session, 40 * step),
            "a session with no full-title listing offers nothing to escape with"
        );
    }

    /// The playlist lists the whole title, not the run's window.
    ///
    /// **This needs a declared source rate.** Without one there is no honest
    /// cadence, so no grid, so no full-title listing — and 53 of this file's
    /// sessions pass `VideoEncodePlan::default()`, which reaches the per-run
    /// path instead. A green suite is not evidence for this listing; these
    /// are.
    #[test]
    fn the_playlist_lists_the_whole_title_not_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = eof_test_session(dir.path(), 1_354_496);
        // `eof_test_session` builds a Copy session; the full-title listing is
        // transcode's, so say so rather than inheriting it.
        session.mode = SessionMode::Transcode;
        session.encode_plan = plan_25fps();
        session.start_ms = 400_000;
        session.play_start_ms = 400_000;

        let pl = build_run_media_playlist("s1", &session, SINGLE_VIDEO_RUNG);
        let text = String::from_utf8_lossy(&pl);

        assert!(
            text.contains("/api/v0/sessions/s1/seg_00000000000.m4s"),
            "a run landed at 400 s still lists the title from 0: {text}"
        );
        assert!(
            text.contains("/api/v0/sessions/s1/seg_00001354000.m4s"),
            "and lists to the usable extent"
        );
        assert!(text.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(!text.contains("#EXT-X-PLAYLIST-TYPE:EVENT"));
        assert!(text.contains("#EXT-X-ENDLIST"));
        assert!(
            text.contains("#EXT-X-START:TIME-OFFSET=400.000,PRECISE=YES"),
            "the attach point is the land, not the first entry: {text}"
        );

        // 1354496 / 2000 entries, and the last one is short rather than past
        // the extent.
        let listed = text.matches("seg_").count();
        assert_eq!(listed, 678, "0..1354496 on a 2000 ms grid");
    }

    /// The origin a client converts by is the **listing's**, not the land.
    ///
    /// Reading the land as the origin is what put `20:02` on a 15-minute
    /// title: `?startMs=600000`, macOS Safari native, measured 2026-08-31 at
    /// `4c0c20f` before this existed. Element time is zeroed at the first
    /// segment the playlist lists, and a full-title listing lists from 0
    /// however far in the run landed.
    #[test]
    fn a_full_title_listing_has_media_origin_zero_however_far_it_landed() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = eof_test_session(dir.path(), 900_000);
        session.mode = SessionMode::Transcode;
        session.encode_plan = plan_25fps();
        session.start_ms = 600_000;
        session.play_start_ms = 600_000;
        session.landed_ms = 600_000;

        let listing = run_listing(&session, SINGLE_VIDEO_RUNG);
        assert_eq!(
            listing.media_origin_ms, 0,
            "the listing starts at 0, so element time is already title time"
        );
        assert_eq!(
            listing.start_offset_ms, 600_000,
            "the land is still said, in EXT-X-START, where it belongs"
        );
        assert_eq!(
            session.landed_ms, 600_000,
            "and the land is untouched: the two answer different questions"
        );

        // What the session view reports must be what the bytes did.
        let text =
            String::from_utf8_lossy(&build_run_media_playlist("s1", &session, SINGLE_VIDEO_RUNG))
                .to_string();
        assert!(text.contains("/api/v0/sessions/s1/seg_00000000000.m4s"));
        assert!(text.contains("#EXT-X-START:TIME-OFFSET=600.000,PRECISE=YES"));
    }

    /// A transcode session at a rounded rate lists a grid, and the ingest snap
    /// resolves the producer's drifted key onto it.
    ///
    /// **The two sites entry 18 turns on, asserted together.** The listing
    /// names multiples of the derived cadence; the ingest snaps producer keys
    /// onto the same multiples; the serve lookup is then an exact match on
    /// keys the ingest already normalised, which is why it needs no tolerance
    /// of its own. If the listing and the snap ever disagree the failure
    /// appears at segment 500, not segment 1.
    #[test]
    fn a_rounded_rate_lists_a_grid_the_snap_can_reach() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = eof_test_session(dir.path(), 7_200_000);
        session.mode = SessionMode::Transcode;
        session.encode_plan = VideoEncodePlan {
            source_frame_rate: Some((2997, 125)),
            ..VideoEncodePlan::default()
        };

        // Before entry 18's fix this was `NoHonestGrid` arriving as copy's
        // `None`, and the listing was a keyframe walk on 20 s boundaries.
        // The fixture leg honours `-force_key_frames`, so the grid is
        // SEGMENT_MS and the rate does not enter into it (entry 20).
        assert_eq!(
            session_grid_cadence(&session),
            GridCadence::Cadence(SEGMENT_MS)
        );

        let entries = full_title_entries(&session).expect("a rounded rate still lists");
        assert!(entries.len() > 3000, "a full title, not a window");
        for (i, (start, _)) in entries.iter().enumerate().take(2000) {
            assert_eq!(*start, i as u64 * SEGMENT_MS, "listing is the derived grid");
        }

        // The producer's key for a late segment carries the first-segment
        // offset plus the accumulated rounding drift, and snaps onto the key
        // the listing named.
        // The producer's key for a late segment is the first frame at or
        // after the listed multiple, so it sits within one frame period.
        let listed = 2000 * SEGMENT_MS;
        let produced = listed + 42;
        assert_eq!(
            crate::hls_segment_map::snap_to_cadence(produced, SEGMENT_MS),
            Some(listed),
            "the ingest resolves what the listing promised"
        );

        // And the snap is what the session would actually be given.
        let points = session_listed_points(&session);
        assert!(points.is_empty(), "a cadence session lists no walk points");
        assert!(matches!(
            session_key_snap(session_cadence_ms(&session), &points),
            Some(crate::hls_segment_map::KeySnap::Cadence(SEGMENT_MS))
        ));
    }

    /// A transcode session with no listable grid gets the **per-run window
    /// listing**, never copy's keyframe walk.
    ///
    /// **This is the defect itself, as a control.** Entry 18 was a transcode
    /// session handed a walk of the source's keyframes on 20 s boundaries: the
    /// listing named times the encoder never writes, every request held to
    /// `SEGMENT_WAIT`, and 129 of 1814 items could not play. The keyframe map
    /// below is populated deliberately, so `copy_window_entries` *would*
    /// return a walk if this branch reached it.
    #[test]
    fn a_transcode_with_no_grid_is_not_given_copys_walk() {
        let dir = tempfile::tempdir().unwrap();
        // Two hours, so 7/3's residual actually outruns the budget. At 100 s
        // it does not, which is the point of checking the title rather than
        // the rate alone.
        let mut session = eof_test_session(dir.path(), 7_200_000);
        session.mode = SessionMode::Transcode;
        // 7/3 rounds to 2143 ms and drifts past the budget: no listable grid.
        session.encode_plan = VideoEncodePlan {
            source_frame_rate: Some((7, 3)),
            ..VideoEncodePlan::default()
        };
        session.map_binding.map = Some(KeyframeMap {
            container_kind: MapContainerKind::Matroska,
            content_id: "probe".into(),
            entries: (0..13)
                .map(|i| KeyframeEntry {
                    pts_ms: i * 8_000,
                    byte_offset: i * 1_000,
                })
                .collect(),
        });

        assert_eq!(session_grid_cadence(&session), GridCadence::NoHonestGrid);
        // The walk is available and must not be taken.
        assert!(
            copy_window_entries(&session).is_some(),
            "fixture check: the walk exists, so this control is not vacuous"
        );
        assert!(
            full_title_entries(&session).is_none(),
            "a transcode encoder never lands on a source keyframe: the walk \
             would list URIs it cannot fill (entry 18)"
        );
    }

    /// The rate that already worked is untouched. **The `(a, a)` control.**
    #[test]
    fn an_exact_rate_lists_exactly_what_it_did_before() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = eof_test_session(dir.path(), 7_200_000);
        session.mode = SessionMode::Transcode;
        session.encode_plan = VideoEncodePlan {
            source_frame_rate: Some((24000, 1001)),
            ..VideoEncodePlan::default()
        };
        assert_eq!(
            session_grid_cadence(&session),
            GridCadence::Cadence(SEGMENT_MS)
        );

        let exact = full_title_entries(&session).expect("24000/1001 listed before and lists now");

        // Same rate written the other way must produce the identical listing,
        // which is the whole claim: 2997/125 and 24000/1001 are one rate.
        let mut other = eof_test_session(dir.path(), 7_200_000);
        other.mode = SessionMode::Transcode;
        other.encode_plan = VideoEncodePlan {
            source_frame_rate: Some((2997, 125)),
            ..VideoEncodePlan::default()
        };
        assert_eq!(
            full_title_entries(&other),
            Some(exact),
            "the same 23.976 written two ways must list the same grid"
        );
    }

    /// The per-run listing ADR-0020 kept begins at its first entry, so there
    /// the origin **is** the land — which is why one field cannot serve both
    /// and why the client cannot guess from the mode.
    #[test]
    fn a_per_run_listing_has_media_origin_at_its_first_entry() {
        let dir = tempfile::tempdir().unwrap();
        let run0 = run_path(dir.path(), SINGLE_VIDEO_RUNG, 0);
        fs::create_dir_all(&run0).unwrap();
        let seg_rel = crate::hls_segment_map::run_rel_dir(SINGLE_VIDEO_RUNG, 0).join("seg000.m4s");
        fs::write(dir.path().join(&seg_rel), [0u8; 64]).unwrap();

        let mut session = eof_test_session(dir.path(), 900_000);
        // Copy with no keyframe map: cut points are unknowable, so no
        // full-title listing exists to be had.
        session.mode = SessionMode::Copy;
        session.encode_plan = plan_25fps();
        session.start_ms = 600_000;
        session.play_start_ms = 600_000;
        session.landed_ms = 600_000;
        session
            .segment_map_mut(SINGLE_VIDEO_RUNG)
            .insert(crate::hls_segment_map::MappedSegment {
                start_ms: 600_000,
                duration_ms: 2_000,
                run_id: 0,
                rel_path: seg_rel,
            });
        assert!(
            full_title_entries(&session).is_none(),
            "an -ss copy run cannot say where it will cut"
        );

        let listing = run_listing(&session, SINGLE_VIDEO_RUNG);
        assert_eq!(listing.media_origin_ms, 600_000);
        assert_eq!(
            listing.start_offset_ms, 0,
            "a zero offset already means the land here"
        );
    }

    /// Listing nothing, the window is the honest origin: the attach that
    /// follows lands there, and a zero would claim title 0.
    #[test]
    fn an_empty_per_run_listing_falls_back_to_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = eof_test_session(dir.path(), 900_000);
        session.mode = SessionMode::Copy;
        session.encode_plan = plan_25fps();
        session.start_ms = 600_000;
        session.play_start_ms = 600_000;

        let listing = run_listing(&session, SINGLE_VIDEO_RUNG);
        assert!(listing.entries.is_empty());
        assert_eq!(listing.media_origin_ms, 600_000);
    }

    /// Copy lists the whole title too, on the greedy 20 s keyframe walk.
    ///
    /// Until this commit copy had no full-title listing and this test asserted
    /// that. **That was a waypoint, not a decision** — the only difference
    /// between the modes is the grid, and both are full-title VOD.
    #[test]
    fn copy_lists_the_whole_title_on_the_keyframe_walk() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = eof_test_session(dir.path(), 100_000);
        session.mode = SessionMode::Copy;
        session.encode_plan = plan_25fps();
        // Keyframes every 8 s, so a 20 s walk takes every third one.
        session.map_binding.map = Some(KeyframeMap {
            container_kind: MapContainerKind::Matroska,
            content_id: "probe".into(),
            entries: (0..13)
                .map(|i| KeyframeEntry {
                    pts_ms: i * 8_000,
                    byte_offset: i * 1_000,
                })
                .collect(),
        });

        let entries = copy_window_entries(&session).expect("copy lists from its map");
        let starts: Vec<u64> = entries.iter().map(|(s, _)| *s).collect();
        assert_eq!(
            starts,
            vec![0, 24_000, 48_000, 72_000, 96_000],
            "the first keyframe at or after each 20 s boundary, not the boundary"
        );
        assert!(
            starts.windows(2).all(|w| w[1] - w[0] >= COPY_WINDOW_MS),
            "every window holds at least COPY_WINDOW_MS of media"
        );
        assert_eq!(
            entries.last().map(|(s, d)| s + d),
            Some(100_000),
            "the last window runs to the extent, not past it"
        );

        // The walk depends only on the map and 0, which is what makes it
        // listable before anything is written. Where this run started must not
        // change it.
        session.start_ms = 48_000;
        session.play_start_ms = 48_000;
        let after_seek: Vec<u64> = copy_window_entries(&session)
            .expect("still lists")
            .iter()
            .map(|(s, _)| *s)
            .collect();
        assert_eq!(starts, after_seek, "the listing is run-independent");

        let pl = build_run_media_playlist("s1", &session, SINGLE_VIDEO_RUNG);
        let text = String::from_utf8_lossy(&pl);
        assert!(text.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(!text.contains("#EXT-X-PLAYLIST-TYPE:EVENT"));
        assert!(text.contains("/api/v0/sessions/s1/seg_00000000000.m4s"));
        assert!(text.contains("/api/v0/sessions/s1/seg_00000024000.m4s"));
    }

    /// Without a keyframe map, copy's cut points are not knowable ahead of
    /// time, so that session keeps the per-run listing.
    #[test]
    fn copy_without_a_map_keeps_the_per_run_listing() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = eof_test_session(dir.path(), 100_000);
        session.mode = SessionMode::Copy;
        session.encode_plan = plan_25fps();
        assert!(session.map_binding.map.is_none());
        assert!(
            full_title_entries(&session).is_none(),
            "an -ss copy run cannot say where it will cut"
        );
    }

    /// A cold listed URI starts an encoder (ADR-0054 decision 3).
    ///
    /// Each arm is a want no run is heading towards. Under the policy this
    /// reverses every one of them returned `Wait`, and a full-title listing
    /// would have held them to `IDLE_TIMEOUT`.
    #[test]
    fn a_cold_listed_uri_restarts_rather_than_waiting_forever() {
        let long_ago = RESTART_MIN_INTERVAL * 2;
        let frontier = Some(40_000);

        // Behind the encode window: this run produces forward and never
        // returns to it.
        assert_eq!(
            decide_segment_miss(10_000, 30_000, 40_000, frontier, true, long_ago),
            SegmentMissAction::Restart,
            "a want behind the window is never produced by this run"
        );

        // Further ahead than the catch-up band: waiting is a stall, not a cook.
        let far = 40_000 + (CATCH_UP_SEGMENTS + 3) * SEGMENT_MS;
        assert_eq!(
            decide_segment_miss(far, 30_000, 40_000, frontier, true, long_ago),
            SegmentMissAction::Restart,
            "a want past the catch-up band is not reachable by fill-forward"
        );

        // Inside the band is a cook, and still waits.
        let near = 40_000 + CATCH_UP_SEGMENTS * SEGMENT_MS;
        assert_eq!(
            decide_segment_miss(near, 30_000, 40_000, frontier, true, long_ago),
            SegmentMissAction::Wait,
            "the producer is heading for this one"
        );
    }

    /// The two guards that stop a prefetching client turning a full-title
    /// listing into a restart storm.
    #[test]
    fn a_cold_uri_does_not_restart_while_starting_or_inside_the_interval() {
        let far = 400_000;

        assert_eq!(
            decide_segment_miss(far, 30_000, 40_000, Some(40_000), true, Duration::ZERO),
            SegmentMissAction::Wait,
            "inside RESTART_MIN_INTERVAL nothing restarts, however cold the want"
        );

        assert_eq!(
            decide_segment_miss(far, 30_000, 40_000, None, false, RESTART_MIN_INTERVAL * 2),
            SegmentMissAction::Wait,
            "a run that has served nothing has no frontier to be far from"
        );
    }

    /// A leg that discards `-force_key_frames` must be given the cadence as a
    /// frame count plus `-forced_idr`. Measured on h264_qsv 2026-08-23: with
    /// `-g 600` a 23.976 fps source cut 25.025 s segments and the 2 s grid did
    /// not exist. Software honours the expression, so it keeps it.
    #[test]
    fn idr_arguments_differ_per_encode_leg() {
        assert!(crate::EncodeLeg::software().honours_force_key_frames);
        assert!(crate::EncodeLeg::videotoolbox().honours_force_key_frames);
        assert!(!crate::EncodeLeg::qsv_sysmem().honours_force_key_frames);
        // An unverified leg claims nothing and takes the explicit cadence.
        assert!(!crate::EncodeLeg::generic_hw("h264_nvenc", "nvenc").honours_force_key_frames);
    }

    #[test]
    fn video_filter_scales_and_retags_sdr() {
        let plan = VideoEncodePlan {
            max_height: Some(1080),
            max_bitrate_bps: Some(5_000_000),
            tone_map: false,
            source_frame_rate: None,
        };
        let vf = transcode_video_filter_chain(plan, None).unwrap();
        assert!(vf.contains("min(1080,ih)"), "{vf}");
        assert!(vf.contains("setparams=color_primaries=bt709"), "{vf}");
        assert!(!vf.contains("tonemap="), "{vf}");
    }

    #[test]
    fn video_filter_tone_map_requires_zscale() {
        let plan = VideoEncodePlan {
            tone_map: true,
            ..VideoEncodePlan::default()
        };
        let result = transcode_video_filter_chain(plan, None);
        if ffmpeg_has_zscale() {
            let vf = result.expect("zscale present");
            assert!(vf.contains("tonemap=tonemap=hable"), "{vf}");
            assert!(vf.contains("zscale="), "{vf}");
        } else {
            let err = result.expect_err("no zscale");
            assert!(err.contains("libzimg"), "{err}");
        }
    }

    /// Proves tonemap changed pixels vs retag — not beauty.
    /// Floor from the HDR tonemap-delta measurement, 2026-08-01 (MAD ≈ 11.08).
    #[test]
    fn tonemap_frame_differs_from_retag() {
        if !ffmpeg_available() || !ffmpeg_has_zscale() {
            eprintln!("skipping: need ffmpeg with zscale");
            return;
        }
        let src = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files/hevc_hdr10_mp4.mp4");
        if !src.exists() {
            eprintln!("skipping: missing {}", src.display());
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let retag_rgb = dir.path().join("retag.rgb");
        let tonemap_rgb = dir.path().join("tonemap.rgb");
        for (vf, out) in [
            (SDR_RETAG_CHAIN, &retag_rgb),
            (HDR_TONEMAP_CHAIN, &tonemap_rgb),
        ] {
            let status = Command::new("ffmpeg")
                .args([
                    "-y",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-ss",
                    "1",
                    "-t",
                    "0.05",
                    "-i",
                ])
                .arg(&src)
                .args(["-vf", &format!("{vf},format=rgb24"), "-frames:v", "1"])
                .arg(out)
                .status()
                .unwrap();
            assert!(status.success(), "frame extract failed for {vf}");
        }
        let a = fs::read(&retag_rgb).unwrap();
        let b = fs::read(&tonemap_rgb).unwrap();
        assert_eq!(a.len(), b.len());
        assert!(!a.is_empty());
        let sum: u64 = a
            .iter()
            .zip(b.iter())
            .map(|(x, y)| u64::from((*x as i16 - *y as i16).unsigned_abs()))
            .sum();
        let mad = sum as f64 / a.len() as f64;
        // Half of measured ~11.08 — HDR tonemap-delta measurement, 2026-08-01
        assert!(
            mad >= 5.0,
            "expected tonemap≠retag (MAD≥5.0), got MAD={mad}"
        );
    }

    /// End-to-end: committed HDR fixtures through spawn_ffmpeg → BT.709 labels.
    #[test]
    fn tonemap_session_marks_bt709_for_pq_and_hlg() {
        if !ffmpeg_available() || !ffmpeg_has_zscale() {
            eprintln!("skipping: need ffmpeg with zscale");
            return;
        }
        let testdata = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../testdata/files");
        let cases = [("hevc_hdr10_mp4.mp4", true), ("hevc_hlg_mp4.mp4", false)];
        for (name, required) in cases {
            let src = testdata.join(name);
            if !src.exists() {
                if required {
                    panic!("committed fixture missing: {}", src.display());
                }
                eprintln!("skipping optional fixture (land via testdata/hevc-hlg-fixture): {name}");
                continue;
            }
            let dir = tempfile::tempdir().unwrap();
            let enc = dir.path().join("enc");
            fs::create_dir_all(&enc).unwrap();
            let plan = VideoEncodePlan {
                tone_map: true,
                ..VideoEncodePlan::default()
            };
            let mut child = spawn_ffmpeg(
                &ss_start_plan(&src, 0, 0),
                &enc,
                SessionMode::Transcode,
                stereo(),
                &crate::EncodeLeg::software(),
                None,
                plan,
                false,
            )
            .unwrap_or_else(|e| panic!("spawn tonemap session for {name}: {e}"));
            let deadline = Instant::now() + Duration::from_secs(45);
            while Instant::now() < deadline {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            stop_child(&mut Some(child));
            assert!(
                enc.join("seg000.m4s").exists() || enc.join("index.m3u8").exists(),
                "{name}: tonemap session produced no HLS output"
            );
            let joined = dir.path().join("joined.mp4");
            let status = Command::new("ffmpeg")
                .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
                .arg(enc.join("index.m3u8"))
                .args(["-c", "copy"])
                .arg(&joined)
                .status()
                .unwrap();
            assert!(status.success(), "{name}: remux from HLS failed");
            let trc = probe_entry(&joined, "v:0", "color_transfer");
            let prim = probe_entry(&joined, "v:0", "color_primaries");
            let space = probe_entry(&joined, "v:0", "color_space");
            assert!(
                trc == "bt709" || trc == "1",
                "{name}: expected bt709 transfer, got {trc:?}"
            );
            assert!(
                prim == "bt709" || prim == "1",
                "{name}: expected bt709 primaries, got {prim:?}"
            );
            assert!(
                space == "bt709" || space == "1",
                "{name}: expected bt709 space, got {space:?}"
            );
        }
    }

    fn make_fixture(path: &Path) {
        make_fixture_secs(path, 4);
    }

    fn make_fixture_secs(path: &Path, secs: u32) {
        let d = secs.to_string();
        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                &format!("color=c=black:s=64x64:d={d}"),
                "-f",
                "lavfi",
                "-i",
                &format!("sine=frequency=440:duration={d}"),
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                path.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(status.success());
    }

    const FIXTURE_MS: u64 = 4000;

    /// First audio track, already inside the browser ceiling.
    fn stereo() -> AudioSelection {
        AudioSelection {
            stream_index: None,
            channels: 2,
            channel_layout: Some("stereo".into()),
            max_channels: 2,
        }
    }

    /// Polls the session's media playlist until it lists a time-keyed segment.
    ///
    /// **One helper, not two.** `wait_playlist_run` used to sit beside this to
    /// poll one specific run, because the URI named a run and a stale one 404ed.
    /// Under ADR-0054 decision 5 there is one URI, and `playlist` already holds
    /// until the *current* run has a mapped segment, so waiting for "the new
    /// run" and waiting for "the playlist" became the same wait (Rule 4.11).
    fn wait_playlist(reg: &HlsSessionRegistry, id: &str) -> Vec<u8> {
        let deadline = Instant::now() + SEGMENT_WAIT;
        loop {
            match reg.playlist(id) {
                Ok(bytes) => {
                    if first_listed_seg_opt(&bytes).is_some() {
                        return bytes;
                    }
                    if Instant::now() >= deadline {
                        panic!(
                            "playlist ready without time-keyed segments: {}",
                            String::from_utf8_lossy(&bytes)
                        );
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(PlaylistError::NotReady) | Err(PlaylistError::NotFound)
                    if Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => panic!("playlist: {e:?}"),
            }
        }
    }

    /// First time-keyed segment URI listed in a media playlist body.
    fn first_listed_seg_opt(playlist: &[u8]) -> Option<String> {
        for line in String::from_utf8_lossy(playlist).lines() {
            let base = line.rsplit('/').next().unwrap_or(line);
            if crate::hls_segment_map::parse_time_keyed_segment_name(base).is_some() {
                return Some(base.to_string());
            }
        }
        None
    }

    fn first_listed_seg(playlist: &[u8]) -> String {
        first_listed_seg_opt(playlist)
            .unwrap_or_else(|| panic!("no time-keyed segment in playlist"))
    }

    /// Producer land for a mid-start / seek window (may be tens of ms off the
    /// aligned play ms — do not hardcode `seg_00000040000`).
    ///
    /// **Read from the session view, not from the first listed URI.** The
    /// first entry was the land while the playlist listed one window; a
    /// full-title listing starts at 0 for every session, so the playlist no
    /// longer says where a run landed. `landedMs` is what a client reads, and
    /// after the grid snap it is exactly the first segment this run writes.
    fn wait_land_near(reg: &HlsSessionRegistry, id: &str, play_ms: u64) -> (String, u64) {
        let _ = wait_playlist(reg, id);
        let ms = reg.view(id).expect("session view").landed_ms;
        let name = crate::hls_segment_map::time_keyed_segment_name(ms);
        let slack = SEGMENT_MS.saturating_mul(2);
        assert!(
            ms + slack >= play_ms && ms < play_ms.saturating_add(slack),
            "land {ms} not near play {play_ms} (slack {slack}): {name}"
        );
        let _ = wait_asset(reg, id, &name);
        (name, ms)
    }

    fn wait_first_listed_asset(reg: &HlsSessionRegistry, id: &str) -> Vec<u8> {
        let pl = wait_playlist(reg, id);
        wait_asset(reg, id, &first_listed_seg(&pl))
    }

    fn wait_asset(reg: &HlsSessionRegistry, id: &str, name: &str) -> Vec<u8> {
        let deadline = Instant::now() + SEGMENT_WAIT + Duration::from_secs(5);
        loop {
            match reg.asset(id, name, None) {
                Ok(bytes) => return bytes,
                Err(PlaylistError::NotReady) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => panic!("asset {name}: {e:?}"),
            }
        }
    }

    /// `note_first_segment_ready` has three production call sites and this is
    /// its only unit test, so it outlives the stale guard it used to check.
    #[test]
    fn first_segment_ready_is_set_once_the_play_land_is_mapped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let play_ms = 2_538_000u64;
        let mut session = Session {
            item_id: 1,
            src: PathBuf::from("/dev/null"),
            dir: dir.path().to_path_buf(),
            run_cache_budget_bytes: SESSION_RUN_CACHE_BUDGET_BYTES,
            mode: SessionMode::Transcode,
            audio: stereo(),
            burn_in: None,
            encode_plan: VideoEncodePlan::default(),
            map_binding: MapBinding::default(),
            encode_leg: crate::EncodeLeg::software(),
            video_encoder: "libx264".into(),
            start_ms: play_ms - ENCODE_LEAD_SEGMENTS * SEGMENT_MS,
            play_start_ms: play_ms,
            // Not the answer: the assignment under test has to run for the
            // assertion below to hold. Seeded to `play_ms` this pinned nothing.
            landed_ms: 0,
            usable_extent_ms: None,
            duration_ms: 3_600_000,
            encoder_states: single_rung_encoder_states(0, 1, None),
            segment_maps: single_rung_segment_maps(crate::hls_segment_map::SegmentMap::default()),
            current_run_eof: false,
            last_access: Instant::now(),
            last_restart: Instant::now(),
            primed: false,
            first_segment_ready: false,
            pending_play_ms: None,
            pending_since: None,
            failed: None,
            subtitle_tracks: vec![],
            last_requested_ms: 0,
            piggyback: None,
            subs: None,
            db: None,
            map_build_in_flight: None,
        };
        session
            .segment_map_mut(SINGLE_VIDEO_RUNG)
            .insert(crate::hls_segment_map::MappedSegment {
                start_ms: play_ms,
                duration_ms: SEGMENT_MS,
                run_id: 0,
                rel_path: crate::hls_segment_map::run_rel_dir(SINGLE_VIDEO_RUNG, 0)
                    .join("seg000.m4s"),
            });
        let run0 = run_path(dir.path(), SINGLE_VIDEO_RUNG, 0);
        fs::create_dir_all(&run0).unwrap();
        fs::write(run0.join("seg000.m4s"), b"seg").unwrap();
        note_first_segment_ready("test", &mut session);
        assert!(session.first_segment_ready, "the play land is in the map");
        assert_eq!(
            session.landed_ms, play_ms,
            "land moves to the first segment of the current run"
        );
    }

    /// The race entry 13 exists to survive: **a run's final segment, ingest
    /// racing the reap, with the want landing precisely on it.**
    ///
    /// Observed once in three device runs and never reproduced synthetically,
    /// so it is pinned here as a shape rather than as a field report. The
    /// numbers are the ones measured on the iPhone: a run landed at 114 000
    /// that stopped at `seg089` (178 000), a want of 180 000 that would have
    /// been its next and last segment, and a session whose window had already
    /// moved to 600 000.
    ///
    /// **A candidate that only handles a want beyond a dead run's coverage
    /// does not address this.** The contested segment is the one the run was
    /// in the middle of committing: it exists, or is about to, and the window
    /// is already past it. A replay where the run stopped one segment *short*
    /// restarts and serves in under a second — that path was never broken.
    #[test]
    fn a_want_on_a_dead_runs_final_segment_reaches_a_producer() {
        let frontier = 178_000u64; // the last segment that run committed
        let want = 180_000u64; // its next one, which it may or may not have
        let window = 600_000u64; // the session has moved nine minutes on
        let play = window;

        // The playlist lists the whole title (ADR-0054), so it offers `want`.
        assert!(
            want < window,
            "the shape under test is a want below the current window"
        );

        assert!(
            !segment_miss_unreachable(
                want,
                play,
                None,
                window,
                play,
                Some(frontier),
                true,
                true, // listed
            ),
            "a listed want on a dead run's final segment must not be held:              nothing alive is producing it and the playlist named it"
        );

        assert_eq!(
            decide_segment_miss(
                want,
                window,
                play,
                Some(frontier),
                true,
                RESTART_MIN_INTERVAL * 2,
            ),
            SegmentMissAction::Restart,
            "and the miss policy that now owns the position says seek"
        );

        // The control that separates this from the old behaviour: the same
        // position, unlisted, is ADR-0020's per-run listing and still holds.
        assert!(
            segment_miss_unreachable(
                want,
                play,
                None,
                window,
                play,
                Some(frontier),
                true,
                false, // not listed
            ),
            "an unlisted want behind the window is still unreachable"
        );

        // And the same-run control: an ordinary in-window fetch must not turn
        // into a restart just because this changed.
        assert!(
            !segment_miss_unreachable(
                window + SEGMENT_MS,
                play,
                None,
                window,
                play,
                Some(window),
                true,
                true,
            ),
            "an in-window want is fill-forward, not a seek"
        );
        assert_eq!(
            decide_segment_miss(
                window + SEGMENT_MS,
                window,
                play,
                Some(window),
                true,
                RESTART_MIN_INTERVAL * 2,
            ),
            SegmentMissAction::Wait,
            "the (a,a) control: an ordinary fetch spawns nothing"
        );
    }

    /// ADR-0020: far scrub is POST /seek, not a segment GET. Seek then hold
    /// the land segment until the new run cooks it.
    #[test]
    fn scrub_segment_hold_returns_200_on_same_request() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture_secs(&src, 60);
        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264", None, None).unwrap();
        let id = reg
            .start(
                1,
                &src,
                0,
                60_000,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                None,
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        wait_playlist(&reg, &id);
        let _ = wait_first_listed_asset(&reg, &id);
        std::thread::sleep(RESTART_MIN_INTERVAL);

        let view = reg.seek(&id, 40_000).expect("seek");
        assert_ne!(view.run_id, 0, "fresh run after far seek");
        let t0 = Instant::now();
        let (land, _) = wait_land_near(&reg, &id, 40_000);
        assert!(!wait_asset(&reg, &id, &land).is_empty());
        assert!(
            t0.elapsed() < SEGMENT_WAIT + Duration::from_secs(5),
            "should finish within SEGMENT_WAIT"
        );
    }

    /// ADR-0020: producer-truth runs have no dig-back lead before their land.
    #[test]
    fn lead_zero_digback_before_land_is_not_covered() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture_secs(&src, 60);
        let play_ms = 40_000;
        assert_eq!(ENCODE_LEAD_SEGMENTS, 0);
        assert_eq!(encode_start_ms(play_ms), play_ms);
        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264", None, None).unwrap();
        let id = reg
            .start(
                1,
                &src,
                play_ms,
                60_000,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                None,
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        let (land, land_ms) = wait_land_near(&reg, &id, play_ms);
        assert!(!wait_asset(&reg, &id, &land).is_empty());

        let digback = crate::hls_segment_map::time_keyed_segment_name(land_ms - SEGMENT_MS);
        let t0 = Instant::now();
        match reg.asset(&id, &digback, None) {
            Err(PlaylistError::NotFound) | Err(PlaylistError::NotReady) => {
                assert!(
                    t0.elapsed() < Duration::from_secs(5),
                    "dig-back before land must fail quickly"
                );
            }
            Ok(_) => panic!("dig-back must not cook a retreated window"),
            Err(e) => panic!("unexpected dig-back error: {e:?}"),
        }
    }

    /// ADR-0020: a segment miss cannot make a new far-ahead producer run.
    /// Same-run fill-forward may eventually produce the bytes; that is not a
    /// scrub. What must not happen is a new `run_id` without POST /seek.
    #[test]
    fn a_far_ahead_listed_want_cooks_and_the_seek_path_still_works() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture_secs(&src, 60);
        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264", None, None).unwrap();
        let id = reg
            .start(
                1,
                &src,
                0,
                60_000,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                None,
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        let playlist = wait_playlist(&reg, &id);
        let _ = wait_asset(&reg, &id, &first_listed_seg(&playlist));
        std::thread::sleep(RESTART_MIN_INTERVAL);

        let run_before = {
            let sessions = reg.sessions.lock().unwrap();
            sessions
                .get(&id)
                .unwrap()
                .encoder_state(SINGLE_VIDEO_RUNG)
                .current_run_id
        };
        let land_ms = 40_000;
        let land = crate::hls_segment_map::time_keyed_segment_name(land_ms);
        assert!(
            !String::from_utf8_lossy(&playlist).contains(&land),
            "far-ahead segment must not already be listed"
        );
        // Overturned 2026-08-30: a listed want past the catch-up band is a
        // cold URI, and ADR-0054 decision 3 makes it a seek. It used to be
        // declined here, which a full-title listing turns into a hold that
        // never ends.
        let _ = reg.asset(&id, &land, None);
        let (run_after_miss, pending) = {
            let sessions = reg.sessions.lock().unwrap();
            let session = sessions.get(&id).unwrap();
            (
                session.encoder_state(SINGLE_VIDEO_RUNG).current_run_id,
                session.pending_play_ms,
            )
        };
        assert!(
            run_after_miss != run_before || pending == Some(land_ms),
            "a far-ahead listed want must cook: run {run_before} -> \
             {run_after_miss}, pending {pending:?}"
        );

        let view = reg.seek(&id, land_ms).expect("seek");
        assert_ne!(view.run_id, run_before, "seek starts a new producer run");
        let seek_playlist = wait_playlist(&reg, &id);
        let seek_land = first_listed_seg(&seek_playlist);
        assert!(!wait_asset(&reg, &id, &seek_land).is_empty());
    }

    /// **The rule #208 added, pinned where it is deterministic.**
    ///
    /// Reaching either refusal site end to end needs a seek to land inside one
    /// `SEGMENT_POLL` of a held request. Measured at **2 runs in 20** locally,
    /// so an integration test named for this rule would look like coverage and
    /// mostly not be it. The rule lives in [`miss_refusal`] for that reason.
    #[test]
    fn a_want_this_session_accepted_is_never_404() {
        assert!(
            matches!(miss_refusal(true), PlaylistError::NotReady),
            "a want the session accepted must answer 503, which hls.js and \
             Safari retry - not 404, which makes them abandon the fragment"
        );
        assert!(
            matches!(miss_refusal(false), PlaylistError::NotFound),
            "the first look keeps 404, which is the case ADR-0054 decision 3 \
             reserves it for: a URI outside the title or off the grid"
        );
    }

    /// **`want_is_listed` is vacuously false for a session with no honest
    /// grid**, which is the regime #208's guard was written for.
    ///
    /// `nightjar` #211 measured the live CI failure arriving through the
    /// behind-window branch, 20 of 20 times it fired - the branch whose
    /// narrowing this vacuity removes. `OPEN-DEFECTS` entry 27.
    ///
    /// Every step is asserted, so a change to any one of them fails here by
    /// name rather than silently moving a test onto the other regime - which is
    /// what `VideoEncodePlan::default()` did to
    /// `held_segment_waiter_no_fill_when_pending_moves`.
    #[test]
    fn no_source_rate_leaves_every_want_unlisted() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture_secs(&src, 60);
        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264", None, None).unwrap();
        let id = reg
            .start(
                1,
                &src,
                0,
                60_000,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                None,
                // No `source_frame_rate`. This is the regime, not an oversight.
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        wait_playlist(&reg, &id);

        let sessions = reg.sessions.lock().unwrap();
        let session = sessions.get(&id).unwrap();
        assert!(
            matches!(session_grid_cadence(session), GridCadence::NoHonestGrid),
            "no source frame rate must mean no honest grid"
        );
        assert!(
            full_title_entries(session).is_none(),
            "no honest grid must mean no full-title listing"
        );
        // 40000 is on the 2 s grid and is still not listed. That is the whole
        // point: the predicate is false because there is no listing at all,
        // not because this want is off it.
        assert!(
            !want_is_listed(session, 40_000),
            "want_is_listed must be vacuously false with no listing"
        );
        assert!(
            !want_is_listed(session, 0),
            "vacuously false means every want, including the first"
        );
        drop(sessions);
        let _ = reg.stop(&id);
    }

    /// A held want is never answered 404, by whichever path releases it.
    ///
    /// **Named for what it guarantees on every run**, not for the branch it
    /// sometimes reaches. The behind-window refusal is reached about 2 runs in
    /// 20 - the rest release through `no_fill_release_for_new_land` - so this
    /// is a smoke test over the whole hold, and
    /// [`a_want_this_session_accepted_is_never_404`] is what pins the rule.
    ///
    /// It orders the hold ahead of the seek, which
    /// `held_segment_waiter_no_fill_when_pending_moves` never did; that
    /// omission is what made it load-dependent (`OPEN-DEFECTS` entry 27).
    #[test]
    fn a_held_want_is_never_answered_404() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture_secs(&src, 60);
        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264", None, None).unwrap();
        let id = reg
            .start(
                1,
                &src,
                0,
                60_000,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                None,
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        wait_playlist(&reg, &id);
        let _ = wait_first_listed_asset(&reg, &id);
        std::thread::sleep(RESTART_MIN_INTERVAL);

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (tx, rx) = std::sync::mpsc::channel();
        let reg_hold = Arc::clone(&reg);
        let id_hold = id.clone();
        // Ahead of the initial window, so the first look Waits rather than
        // refusing: 40000 is not behind `window_start` until the seek moves it.
        let hold_name = crate::hls_segment_map::time_keyed_segment_name(40_000);
        std::thread::spawn(move || {
            let _ = ready_tx.send(());
            let result = reg_hold.asset(&id_hold, &hold_name, None);
            let _ = tx.send(result);
        });

        // The handshake takes thread-start latency out of the budget; the sleep
        // then covers one poll, so the request has slept without answering and
        // the session has accepted the want before the window moves.
        ready_rx.recv().expect("hold thread started");
        std::thread::sleep(SEGMENT_POLL + Duration::from_millis(40));
        let _ = reg.seek(&id, 50_000);

        let first = rx
            .recv_timeout(SEGMENT_WAIT + Duration::from_secs(20))
            .expect("held request returned");
        assert!(
            !matches!(first, Err(PlaylistError::NotFound)),
            "a want this session accepted must not 404 afterwards; got {:?}",
            first
                .as_ref()
                .map(|b| b.len())
                .map_err(|e| format!("{e:?}"))
        );
        let _ = reg.stop(&id);
    }

    /// While a waiter holds for land A, a newer scrub moves pending to B.
    /// Once B's encode window is ready, a behind-window hold on A must 503
    /// (`no_fill_release_for_new_land`) so WebKit leaves dig-back — not sit
    /// until teardown (desktop-native single scrub: held mid, zero land GETs).
    /// Immediate 204 on supersede stays rejected (wedged Safari on doubles).
    #[test]
    fn held_segment_waiter_no_fill_when_pending_moves() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture_secs(&src, 60);
        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264", None, None).unwrap();
        let id = reg
            .start(
                1,
                &src,
                0,
                60_000,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                None,
                // **The rate is the point.** `make_fixture_secs` writes 25 fps,
                // and this test used `VideoEncodePlan::default()`, whose
                // `source_frame_rate` is `None`. That gave the session
                // `NoHonestGrid`, so `full_title_entries` was `None` and
                // `want_is_listed` was false for *every* want - and the
                // contract below was being asserted in a regime it was never
                // written for. See `accepted_hold_is_never_404_with_no_honest_grid`
                // for that regime, tested on purpose and under its own name.
                VideoEncodePlan {
                    source_frame_rate: Some((25, 1)),
                    ..VideoEncodePlan::default()
                },
                None,
            )
            .unwrap();
        wait_playlist(&reg, &id);
        let _ = wait_first_listed_asset(&reg, &id);

        // **Pin the regime, do not assume it.** A future change to the plan or
        // to `produced_segment_ms` that drops this session back to
        // `NoHonestGrid` must fail here, not silently move the test onto the
        // other path the way `VideoEncodePlan::default()` did.
        {
            let sessions = reg.sessions.lock().unwrap();
            let session = sessions.get(&id).unwrap();
            assert!(
                matches!(session_grid_cadence(session), GridCadence::Cadence(_)),
                "the contract this test is named for is a full-title one; \
                 without an honest grid it exercises the other regime"
            );
            assert!(
                want_is_listed(session, 40_000),
                "segment 40000 must be listed for the supersede contract to be \
                 the thing under test"
            );
        }

        std::thread::sleep(RESTART_MIN_INTERVAL);

        let (tx, rx) = std::sync::mpsc::channel();
        let reg_hold = Arc::clone(&reg);
        let id_hold = id.clone();
        // Far of the initial window so the GET Waits; seek then supersedes.
        let hold_name = crate::hls_segment_map::time_keyed_segment_name(40_000);
        std::thread::spawn(move || {
            let t0 = Instant::now();
            let result = reg_hold.asset(&id_hold, &hold_name, None);
            let _ = tx.send((result, t0.elapsed()));
        });
        // Retarget within the fixture so the new run can land (EOF on a
        // past-duration seek never flips first_segment_ready). Budget covers
        // SEGMENT_WAIT on the hold plus land cook on the seek.
        let probe_until = Instant::now() + SEGMENT_WAIT + Duration::from_secs(20);
        while Instant::now() < probe_until {
            let _ = reg.seek(&id, 50_000);
            match rx.try_recv() {
                Ok((first, elapsed)) => {
                    assert!(
                        matches!(first, Err(PlaylistError::NotReady)) || first.is_ok(),
                        "superseded behind-window hold: 503 after new land, or 200 if A cooked first; got {:?} elapsed={elapsed:?}",
                        first
                            .as_ref()
                            .map(|b| b.len())
                            .map_err(|e| format!("{e:?}"))
                    );
                    let _ = reg.stop(&id);
                    return;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    panic!("hold thread disconnected without sending");
                }
            }
        }
        let _ = reg.stop(&id);
        panic!("hold did not finish within SEGMENT_WAIT+20s after supersede");
    }

    /// ADR-0020: far scrub is seek API. Pending apply from a second seek
    /// while land A cooks; land B segment then 200s from the new run.
    #[test]
    fn final_land_waiter_applies_pending_when_cooking_land_appears() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture_secs(&src, 120);
        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264", None, None).unwrap();
        let id = reg
            .start(
                1,
                &src,
                20_000,
                120_000,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                None,
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        {
            let sessions = reg.sessions.lock().unwrap();
            let s = sessions.get(&id).unwrap();
            assert_eq!(s.play_start_ms, 20_000);
            assert!(!s.first_segment_ready);
        }

        // Seek to B while A may still be cooking (may defer if land waiter).
        let _ = reg.seek(&id, 40_000).expect("seek B");
        // Drive readiness: playlist/asset poll notices land and applies pending.
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            let _ = reg.playlist(&id);
            let sessions = reg.sessions.lock().unwrap();
            let s = sessions.get(&id).unwrap();
            if s.play_start_ms == 40_000 && s.first_segment_ready {
                break;
            }
            drop(sessions);
            if Instant::now() >= deadline {
                panic!("pending B did not apply within 90s");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let bytes = wait_asset(&reg, &id, "seg_00000040000.m4s");
        assert!(!bytes.is_empty());
        let sessions = reg.sessions.lock().unwrap();
        assert_eq!(sessions.get(&id).unwrap().play_start_ms, 40_000);
    }

    #[test]
    fn encode_start_includes_lead_before_play() {
        assert_eq!(ENCODE_LEAD_SEGMENTS, 0);
        assert_eq!(encode_start_ms(1_264_000), 1_264_000);
        assert_eq!(encode_start_ms(1_000), 0);
        assert_eq!(encode_start_ms(0), 0);
        assert_eq!(encode_start_ms(4_000), 4_000);
        assert_eq!(encode_start_ms(16_000), 16_000);
        assert_eq!(encode_start_ms(18_000), 18_000);
    }

    /// Mid-title switch: encode starts at land. Behind-window dig-back must
    /// not retreat play land; real scrub-back is POST /seek.
    #[test]
    fn switch_session_serves_first_requested_segment() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture_secs(&src, 60);
        let duration_ms = 60_000;
        let play_ms = 40_000;
        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264", None, None).unwrap();

        let prior = reg
            .start(
                1,
                &src,
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
            .unwrap();
        wait_playlist(&reg, &prior);

        let switched = reg
            .start(
                1,
                &src,
                play_ms,
                duration_ms,
                SessionMode::Transcode,
                AudioSelection {
                    stream_index: Some(0),
                    channels: 2,
                    channel_layout: Some("stereo".into()),
                    max_channels: 2,
                },
                vec![],
                None,
                None,
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        let playlist = wait_playlist(&reg, &switched);
        let text = String::from_utf8_lossy(&playlist);
        assert!(
            text.contains("#EXT-X-START:TIME-OFFSET=0.000,PRECISE=YES"),
            "EXT-X-START is window-relative (ADR-0020): {text}"
        );
        let (land, land_ms) = wait_land_near(&reg, &switched, play_ms);
        assert!(
            !wait_asset(&reg, &switched, &land).is_empty(),
            "play-land segment servable"
        );
        // Overturned 2026-08-30: a want behind the window is one this run
        // never produces, so waiting on it never ends. Whether it actually
        // cooks is decided at the call site by the dig-back guard, which now
        // yields only to a want the playlist lists.
        assert_eq!(
            decide_segment_miss(0, 40_000, 40_000, None, false, RESTART_MIN_INTERVAL),
            SegmentMissAction::Restart,
            "seg at t=0 behind a 40s window is not reachable by fill-forward"
        );

        // Behind encode window: unlisted under producer-truth → 404, not a
        // retreated cook. Real scrub-back is POST /seek.
        let dig_ms = 5 * SEGMENT_MS;
        assert!(digback_behind_committed(play_ms, None, dig_ms));
        match reg.asset(&switched, "seg_00000010000.m4s", None) {
            Err(PlaylistError::NotFound) | Err(PlaylistError::NotReady) => {}
            Ok(_) => panic!("dig-back must not cook a retreated window"),
            Err(e) => panic!("unexpected dig-back error: {e:?}"),
        }
        assert!(
            !wait_asset(
                &reg,
                &switched,
                &crate::hls_segment_map::time_keyed_segment_name(land_ms)
            )
            .is_empty(),
            "switch land segment must serve"
        );

        assert!(reg.stop(&prior));
        assert!(reg.stop(&switched));
    }

    /// Fresh mid-title session: encode-at-land cooks near play first.
    #[test]
    fn new_session_serves_first_requested_segment() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture_secs(&src, 60);
        let duration_ms = 60_000;
        let play_ms = 40_000;
        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264", None, None).unwrap();
        let id = reg
            .start(
                1,
                &src,
                play_ms,
                duration_ms,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                None,
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        let playlist = wait_playlist(&reg, &id);
        let text = String::from_utf8_lossy(&playlist);
        assert!(
            text.contains("#EXT-X-START:TIME-OFFSET=0.000,PRECISE=YES"),
            "EXT-X-START is window-relative (ADR-0020): {text}"
        );
        let (land, _) = wait_land_near(&reg, &id, play_ms);
        assert!(
            !wait_asset(&reg, &id, &land).is_empty(),
            "land segment must be served"
        );
        assert!(reg.stop(&id));
    }

    #[test]
    fn session_produces_vod_playlist_and_stop_reaps() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture(&src);
        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264", None, None).unwrap();
        let id = reg
            .start(
                1,
                &src,
                0,
                FIXTURE_MS,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                None,
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        assert_eq!(
            reg.encoder(&id),
            Some(SessionEncoder {
                name: "libx264".into(),
                kind: EncoderKind::Software,
            })
        );
        let playlist = wait_playlist(&reg, &id);
        let text = String::from_utf8_lossy(&playlist);
        assert!(text.contains("#EXT-X-PLAYLIST-TYPE:VOD"), "{text}");
        assert!(
            !text.contains("#EXT-X-PLAYLIST-TYPE:EVENT"),
            "EVENT is gone from every mode: {text}"
        );
        assert!(text.contains("#EXT-X-START:TIME-OFFSET=0.000"), "{text}");
        let land = first_listed_seg(&playlist);
        assert!(reg.asset(&id, &land, None).is_ok(), "land={land}");
        assert!(reg.stop(&id));
        assert!(matches!(reg.playlist(&id), Err(PlaylistError::NotFound)));
    }

    /// Session-inline demux with no scan-time extract: the video segment and
    /// the first subtitle window both become servable from one session start.
    #[test]
    fn session_subtitle_segment_without_scan_extract() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files/h264_aac_srt_mkv.mkv");
        if !corpus.exists() {
            eprintln!("skipping: missing {}", corpus.display());
            return;
        }
        let streams = crate::list_text_subtitles(&corpus).expect("list");
        assert!(!streams.is_empty());
        let track = &streams[0];
        let track_id = track.track_id();
        let dir = tempfile::tempdir().unwrap();
        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 2, "libx264", None, None).unwrap();
        // Short window: we only need first video seg + first subtitle slice.
        let id = reg
            .start(
                1,
                &corpus,
                0,
                4000,
                SessionMode::Copy,
                stereo(),
                vec![HlsSubtitleTrack {
                    track_id: track_id.clone(),
                    language: track.language.clone(),
                    name: track_id.clone(),
                    is_default: true,
                    forced: false,
                    sdh: false,
                    item_id: 1,
                    stream_index: Some(track.stream_index),
                    sidecar_path: None,
                    codec: track.codec.clone(),
                    item_vtt_path: None,
                }],
                None,
                None,
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        wait_playlist(&reg, &id);
        let sub_pl = reg.subtitle_playlist(&id, &track_id).expect("sub playlist");
        let sub_text = String::from_utf8_lossy(&sub_pl);
        assert!(
            sub_text.contains(&format!("{track_id}/seg000.vtt")),
            "{sub_text}"
        );
        let mut seg = None;
        for _ in 0..100 {
            match reg.subtitle_segment(&id, &track_id, 0) {
                Ok(bytes) => {
                    seg = Some(bytes);
                    break;
                }
                Err(PlaylistError::NotReady) => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => panic!("subtitle segment: {e:?}"),
            }
        }
        let seg = String::from_utf8(seg.expect("subtitle seg000 not ready in time")).unwrap();
        assert!(seg.contains("\nNightjar SRT sample\n"), "{seg}");
        reg.stop(&id);
    }

    /// Open a database with one `probed` item carrying `subtitle_status`.
    fn item_db(dir: &Path, subtitle_status: &str) -> Arc<Db> {
        let db = Arc::new(nightjar_db::open(dir).unwrap());
        db.with_conn(|conn| {
            conn.execute_batch(&format!(
                "INSERT INTO libraries (name, path, kind) VALUES ('t', '/tmp/t', 'movies');
                 INSERT INTO media_items (
                    library_id, path, mtime_ms, size_bytes, title, kind, probe_status,
                    subtitle_status
                 ) VALUES (1, '/tmp/t/a.mkv', 1, 2, 'A', 'movie', 'probed', '{subtitle_status}');"
            ))
            .map_err(|e| e.to_string())?;
            Ok(())
        })
        .unwrap();
        db
    }

    /// Color+sine MKV with exactly one embedded SRT track.
    fn make_fixture_with_sub_secs(path: &Path, secs: u32) {
        let d = secs.to_string();
        let srt = path.with_extension("srt");
        fs::write(
            &srt,
            "1\n00:00:00,000 --> 00:00:01,000\nNightjar piggyback cue\n",
        )
        .unwrap();
        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                &format!("color=c=black:s=64x64:d={d}"),
                "-f",
                "lavfi",
                "-i",
                &format!("sine=frequency=440:duration={d}"),
                "-i",
            ])
            .arg(&srt)
            .args([
                "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-c:s", "srt", "-map",
                "0:v:0", "-map", "1:a:0", "-map", "2:0",
            ])
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success(), "fixture with subtitle encode failed");
        let _ = fs::remove_file(&srt);
        // No -shortest: an SRT stream's length is its last cue end, which
        // would truncate the fixture to the cue range instead of `secs`.
        let dur = ffprobe_duration_ms(path);
        assert!(
            dur >= secs.saturating_mul(900) as i64,
            "fixture too short for a mid-piggyback kill: {dur}ms for {secs}s"
        );
    }

    /// ADR-0041 Decision 7 acceptance (remux and transcode): a session on an
    /// `eligible` item writes the subtitle WebVTT under `{subs}/{itemId}/`
    /// with no standalone extract job in the loop — the session's own ffmpeg
    /// produced the rendition — and flips the item to `ready` only once the
    /// run reaches natural EOF from title 0.
    #[test]
    fn piggyback_session_publishes_item_vtt_and_flips_ready() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files/h264_aac_srt_mkv.mkv");
        if !corpus.exists() {
            eprintln!("skipping: missing {}", corpus.display());
            return;
        }
        let streams = crate::list_text_subtitles(&corpus).expect("list");
        assert_eq!(streams.len(), 1, "piggyback gate expects one text track");
        let track_id = streams[0].track_id();
        for mode in [SessionMode::Copy, SessionMode::Transcode] {
            let dir = tempfile::tempdir().unwrap();
            let db = item_db(dir.path(), "eligible");
            let subs = Arc::new(SubsStore::new(dir.path().join("subs")).unwrap());
            let reg = HlsSessionRegistry::with_cap(
                dir.path().join("hls"),
                2,
                "libx264",
                Some(subs.clone()),
                Some(db.clone()),
            )
            .unwrap();
            let id = reg
                .start(
                    1,
                    &corpus,
                    0,
                    4000,
                    mode,
                    stereo(),
                    vec![],
                    None,
                    None,
                    VideoEncodePlan::default(),
                    Some(PiggybackExtract {
                        track_id: track_id.clone(),
                    }),
                )
                .unwrap();
            // Drive to natural EOF: view/playlist polls observe the child
            // exit, which triggers the publish + ready flip.
            let deadline = Instant::now() + Duration::from_secs(60);
            let mut status = String::new();
            loop {
                if let Some(row) = db.get_item(1).unwrap() {
                    status = row.subtitle_status;
                }
                if status == "ready" && subs.has_vtt(1, &track_id) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "piggyback never published; status={status}"
                );
                let _ = reg.playlist(&id);
                std::thread::sleep(Duration::from_millis(50));
            }
            let body = fs::read_to_string(subs.vtt_path(1, &track_id)).unwrap();
            assert!(body.contains("Nightjar SRT sample"), "{body}");
            assert_eq!(
                db.get_item(1).unwrap().unwrap().subtitle_status,
                "ready",
                "complete piggyback run flips the item to ready"
            );
            reg.stop(&id);
        }
    }

    /// Counts `trak` boxes in an fMP4 init: one per track in `moov`.
    ///
    /// `traf` lives in `moof` and never in an init, so a byte scan is honest
    /// here without a box parser.
    fn init_track_count(init: &[u8]) -> usize {
        init.windows(4).filter(|w| *w == b"trak").count()
    }

    /// **ADR-0054 decision 5's one unmeasured assumption, pinned.**
    ///
    /// `session.piggyback` clears once the extract publishes, so a run spawned
    /// after that asks FFmpeg for one fewer output than run 0 did: the
    /// `-map 0:s? -c:s webvtt` pair is gone. Decision 5 rests on that not
    /// reaching `init.mp4`. If it did, a client holding the first init it saw
    /// would be holding one that describes a different track set.
    ///
    /// The HLS muxer is documented to write WebVTT to its own rendition, so the
    /// fMP4 init should carry video and audio either way, and the two-byte
    /// `libx264` diff in `init-identity-across-runs-2026-08-31.md` shows two
    /// tracks and no third. **Nothing varied it deliberately until this test.**
    ///
    /// **Two sessions on one source at one land, alike but for the piggyback
    /// request. Deliberately not a seek.** Once a run reaches EOF the whole
    /// title is mapped, so every in-range seek is a map hit that copies an init
    /// rather than spawning one, and a seek past the map produces no media to
    /// have an init for. The publish that clears `piggyback` needs that EOF, so
    /// the two cannot be staged in one session. What actually differs between
    /// the runs is the FFmpeg invocation, and this compares exactly that.
    #[test]
    fn piggyback_does_not_change_the_init_track_layout() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files/h264_aac_srt_mkv.mkv");
        if !corpus.exists() {
            eprintln!("skipping: missing {}", corpus.display());
            return;
        }
        let streams = crate::list_text_subtitles(&corpus).expect("list");
        let track_id = streams[0].track_id();

        for mode in [SessionMode::Copy, SessionMode::Transcode] {
            let mut inits: Vec<Vec<u8>> = Vec::new();
            for piggyback in [
                Some(PiggybackExtract {
                    track_id: track_id.clone(),
                }),
                None,
            ] {
                let dir = tempfile::tempdir().unwrap();
                let db = item_db(dir.path(), "eligible");
                let subs = Arc::new(SubsStore::new(dir.path().join("subs")).unwrap());
                let reg = HlsSessionRegistry::with_cap(
                    dir.path().join("hls"),
                    2,
                    "libx264",
                    Some(subs),
                    Some(db),
                )
                .unwrap();
                let id = reg
                    .start(
                        1,
                        &corpus,
                        0,
                        4000,
                        mode,
                        stereo(),
                        vec![],
                        None,
                        None,
                        VideoEncodePlan::default(),
                        piggyback,
                    )
                    .unwrap();
                wait_playlist(&reg, &id);
                inits.push(reg.run_asset(&id, 0, "init.mp4").expect("init.mp4 bytes"));
                reg.stop(&id);
            }

            let (with_subs, without) = (&inits[0], &inits[1]);
            assert!(
                init_track_count(with_subs) > 0,
                "{mode:?}: init must declare at least one track"
            );
            assert_eq!(
                init_track_count(with_subs),
                init_track_count(without),
                "{mode:?}: the piggyback subtitle output must not add a track to init.mp4 \
                 ({} bytes with, {} without)",
                with_subs.len(),
                without.len()
            );
            assert_eq!(
                with_subs, without,
                "{mode:?}: init.mp4 must not depend on the piggyback request at all"
            );
        }
    }

    /// ADR-0041 Decision 7 acceptance: a session killed mid-piggyback leaves
    /// the item `eligible`, never `ready`, and does not delete a
    /// previously-good track file for the item (Decision 8.5's invariant).
    #[test]
    fn piggyback_killed_session_leaves_eligible_and_keeps_prior_track() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("long_sub.mkv");
        make_fixture_with_sub_secs(&src, 60);
        let streams = crate::list_text_subtitles(&src).expect("list");
        assert_eq!(streams.len(), 1);
        let track_id = streams[0].track_id();
        let db = item_db(dir.path(), "eligible");
        let subs = Arc::new(SubsStore::new(dir.path().join("subs")).unwrap());
        // A previously-good track the killed piggyback must not touch.
        subs.publish_item_vtt(
            1,
            &track_id,
            "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nPrior ready cue\n",
        )
        .unwrap();
        let reg = HlsSessionRegistry::with_cap(
            dir.path().join("hls"),
            2,
            "libx264",
            Some(subs.clone()),
            Some(db.clone()),
        )
        .unwrap();
        let id = reg
            .start(
                1,
                &src,
                0,
                60_000,
                SessionMode::Copy,
                stereo(),
                vec![],
                None,
                None,
                VideoEncodePlan::default(),
                Some(PiggybackExtract {
                    track_id: track_id.clone(),
                }),
            )
            .unwrap();
        // Kill immediately, without any playlist/view poll: a poll would
        // observe the child exit and could publish before the kill. Stopping
        // right after spawn is mid-piggyback by construction — a 60s copy
        // cannot reach natural EOF in the milliseconds since spawn, and
        // nothing observed the child exit before the stop.
        assert!(reg.stop(&id), "kill mid-piggyback");
        let row = db.get_item(1).unwrap().expect("item row");
        assert_eq!(
            row.subtitle_status, "eligible",
            "killed piggyback must not flip the item to ready"
        );
        assert!(subs.has_vtt(1, &track_id), "prior track must survive");
        assert_eq!(
            fs::read_to_string(subs.vtt_path(1, &track_id)).unwrap(),
            "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nPrior ready cue\n",
            "killed piggyback must not overwrite the prior track"
        );
    }

    /// One session per start, even for the same item at the same offset, and
    /// stopping one leaves the other playing (ADR-0011).
    #[test]
    fn every_start_is_its_own_session() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture(&src);
        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 2, "libx264", None, None).unwrap();
        let a = reg
            .start(
                1,
                &src,
                0,
                FIXTURE_MS,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                None,
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        let b = reg
            .start(
                1,
                &src,
                0,
                FIXTURE_MS,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                None,
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        assert_ne!(a, b);
        assert!(matches!(
            reg.start(
                2,
                &src,
                0,
                FIXTURE_MS,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                None,
                VideoEncodePlan::default(),
                None,
            ),
            Err(StartSessionError::AdmissionRefused)
        ));
        assert!(reg.stop(&a));
        assert!(matches!(reg.playlist(&a), Err(PlaylistError::NotFound)));
        wait_playlist(&reg, &b);
        reg.stop(&b);
    }

    /// A copy session must never reach the configured encoder: an unusable
    /// encoder name still yields segments because video is stream-copied.
    #[test]
    fn copy_session_bypasses_the_encoder() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture(&src);
        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "no_such_encoder", None, None)
                .unwrap();
        let id = reg
            .start(
                1,
                &src,
                0,
                FIXTURE_MS,
                SessionMode::Copy,
                stereo(),
                vec![],
                None,
                None,
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        assert_eq!(
            reg.encoder(&id),
            Some(SessionEncoder {
                name: "copy".into(),
                kind: EncoderKind::Copy,
            })
        );
        wait_playlist(&reg, &id);
        assert!(
            reg.asset(&id, &first_listed_seg(&wait_playlist(&reg, &id)), None)
                .is_ok()
        );
        reg.stop(&id);
    }

    #[test]
    fn seek_retains_prior_window_segments() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture(&src);
        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264", None, None).unwrap();
        let id = reg
            .start(
                1,
                &src,
                0,
                FIXTURE_MS,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                None,
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        wait_playlist(&reg, &id);
        let early_name = first_listed_seg(&wait_playlist(&reg, &id));
        let early = wait_asset(&reg, &id, &early_name);
        // Move the window forward; stale-retain may 503 behind-play until the
        // new land is ready (ENCODE_LEAD=0). Then prior bytes stay readable.
        for _ in 0..100 {
            match reg.seek(&id, 2000) {
                Ok(_) => break,
                Err(PlaylistError::NotReady) => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => panic!("seek: {e:?}"),
            }
        }
        let _ = wait_playlist(&reg, &id);
        let still = wait_asset(&reg, &id, &early_name);
        assert_eq!(early.len(), still.len());
        assert!(reg.asset(&id, &early_name, None).is_ok());
        // Scrub-back to already-mapped media: duplicate-write stop (no ffmpeg).
        let view = reg.seek(&id, 0).expect("seek back");
        {
            let sessions = reg.sessions.lock().unwrap();
            let s = sessions.get(&id).unwrap();
            assert!(s.first_segment_ready, "expected map-hit ready");
            assert!(
                s.encoder_state(SINGLE_VIDEO_RUNG).child.is_none(),
                "map hit must not spawn ffmpeg"
            );
            assert_eq!(s.play_start_ms, 0);
        }
        assert!(reg.asset(&id, &early_name, None).is_ok());
        assert!(view.run_id >= 2, "fresh playlist URI even on map hit");
        reg.stop(&id);
    }

    /// Encodes a fixture whose audio streams carry `layouts`, one stream each.
    fn make_fixture_layouts(path: &Path, layouts: &[&str]) {
        let mut cmd = Command::new("ffmpeg");
        cmd.args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=64x64:d=4",
        ]);
        for layout in layouts {
            cmd.args([
                "-f",
                "lavfi",
                "-i",
                &format!("anullsrc=r=48000:cl={layout}:d=4"),
            ]);
        }
        cmd.args(["-map", "0:v:0"]);
        for i in 1..=layouts.len() {
            cmd.args(["-map", &format!("{i}:a:0")]);
        }
        cmd.args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"]);
        let status = cmd.arg(path).status().unwrap();
        assert!(status.success(), "fixture encode failed for {layouts:?}");
    }

    /// Runs one session encode to completion and joins its segments back into
    /// a single file so the delivered streams can be probed.
    fn encode_and_join(
        src: &Path,
        dir: &Path,
        mode: SessionMode,
        audio: AudioSelection,
        encoder: &str,
    ) -> PathBuf {
        let enc = dir.join("enc");
        fs::create_dir_all(&enc).unwrap();
        let leg = crate::EncodeLeg::from(encoder);
        let mut child = spawn_ffmpeg(
            &ss_start_plan(src, 0, 0),
            &enc,
            mode,
            audio,
            &leg,
            None,
            VideoEncodePlan::default(),
            false,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        stop_child(&mut Some(child));
        assert!(
            enc.join("seg000.m4s").exists(),
            "session produced no segments"
        );
        let joined = dir.join("joined.mp4");
        let status = Command::new("ffmpeg")
            .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
            .arg(enc.join("index.m3u8"))
            .args(["-c", "copy"])
            .arg(&joined)
            .status()
            .unwrap();
        assert!(status.success(), "remux from HLS failed");
        joined
    }

    fn probe_entry(path: &Path, select: &str, entry: &str) -> String {
        let out = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                select,
                "-show_entries",
                &format!("stream={entry}"),
                "-of",
                "csv=p=0",
            ])
            .arg(path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "ffprobe failed for {}",
            path.display()
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn ffprobe_duration_ms(path: &Path) -> i64 {
        let out = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "csv=p=0",
            ])
            .arg(path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "ffprobe duration failed for {}",
            path.display()
        );
        let text = String::from_utf8_lossy(&out.stdout);
        (text.trim().parse::<f64>().unwrap_or(0.0) * 1000.0) as i64
    }

    /// ADR-0012 decision 2: a 5.1 track above the ceiling forces an audio
    /// encode, never a video one. The registry encoder is unusable, so any
    /// attempt to re-encode video fails the whole session.
    #[test]
    fn copy_session_downmixes_audio_without_touching_video() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("surround.mp4");
        make_fixture_layouts(&src, &["5.1"]);
        let audio = AudioSelection {
            stream_index: None,
            channels: 6,
            channel_layout: Some("5.1".into()),
            max_channels: 2,
        };
        let joined = encode_and_join(
            &src,
            dir.path(),
            SessionMode::Copy,
            audio,
            "no_such_encoder",
        );
        assert_eq!(probe_entry(&joined, "v:0", "codec_name"), "h264");
        assert_eq!(probe_entry(&joined, "a:0", "channels"), "2");
    }

    /// 6.0 shares a channel count with 5.1 but not the index map; falling
    /// back to -ac 2 is the correct (if imperfect) path (ADR-0012).
    #[test]
    fn unknown_named_layout_falls_back_to_ac2() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("six_oh.mkv");
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=24:duration=1",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=channel_layout=6.0:sample_rate=48000:duration=1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "flac",
                "-shortest",
                src.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(status.success());
        assert!(
            stereo_downmix_filter(6, Some("6.0")).is_none(),
            "6.0 must not use the 5.1 pan table"
        );
        let audio = AudioSelection {
            stream_index: None,
            channels: 6,
            channel_layout: Some("6.0".into()),
            max_channels: 2,
        };
        let joined = encode_and_join(&src, dir.path(), SessionMode::Transcode, audio, "libx264");
        assert_eq!(probe_entry(&joined, "a:0", "channels"), "2");
    }

    /// A non-default track is reachable by absolute stream index (ADR-0012).
    #[test]
    fn session_maps_the_selected_audio_stream() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("two_audio.mp4");
        make_fixture_layouts(&src, &["stereo", "mono"]);
        let second = AudioSelection {
            stream_index: Some(2),
            channels: 1,
            channel_layout: Some("mono".into()),
            max_channels: 2,
        };
        let joined = encode_and_join(&src, dir.path(), SessionMode::Copy, second, "libx264");
        assert_eq!(
            probe_entry(&joined, "a:0", "channels"),
            "1",
            "expected the mono second track, not the stereo default"
        );
    }

    #[test]
    fn subtitle_media_playlist_is_segmented_vod() {
        let text = String::from_utf8(build_segmented_subtitle_playlist("e2", 5000)).unwrap();
        assert!(text.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(text.contains("e2/seg000.vtt"));
        assert!(text.contains("e2/seg001.vtt"));
        assert!(text.contains("e2/seg002.vtt"));
        assert!(!text.contains("/api/v0/items/"));
        assert!(text.ends_with("#EXT-X-ENDLIST\n"));
        assert_eq!(text.matches("#EXTINF:").count(), 3);
    }

    /// Ready-store path: slice cues from an on-disk item VTT without session demux.
    #[test]
    fn item_store_vtt_slices_into_hls_segments() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture_secs(&src, 4);
        let vtt = dir.path().join("e2.vtt");
        fs::write(
            &vtt,
            "WEBVTT\n\n1\n00:00:00.500 --> 00:00:01.500\nHello\n\n2\n00:00:02.500 --> 00:00:03.500\nWorld\n\n",
        )
        .unwrap();
        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 2, "libx264", None, None).unwrap();
        let id = reg
            .start(
                1,
                &src,
                0,
                4000,
                SessionMode::Copy,
                stereo(),
                vec![HlsSubtitleTrack {
                    track_id: "e2".into(),
                    language: Some("en".into()),
                    name: "en".into(),
                    is_default: true,
                    forced: false,
                    sdh: false,
                    item_id: 1,
                    stream_index: Some(2),
                    sidecar_path: None,
                    codec: "subrip".into(),
                    item_vtt_path: Some(vtt),
                }],
                None,
                None,
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        wait_playlist(&reg, &id);
        let sub_pl = String::from_utf8(reg.subtitle_playlist(&id, "e2").unwrap()).unwrap();
        assert!(sub_pl.contains("e2/seg000.vtt"), "{sub_pl}");
        assert!(!sub_pl.contains("/api/v0/items/"), "{sub_pl}");
        let seg0 = String::from_utf8(reg.subtitle_segment(&id, "e2", 0).unwrap()).unwrap();
        assert!(seg0.contains("Hello"), "{seg0}");
        assert!(!seg0.contains("World"), "{seg0}");
        let seg1 = String::from_utf8(reg.subtitle_segment(&id, "e2", 1).unwrap()).unwrap();
        assert!(seg1.contains("World"), "{seg1}");
        assert!(!seg1.contains("Hello"), "{seg1}");
        reg.stop(&id);
    }

    /// A session serves two asset names and this is where that is decided.
    ///
    /// It matters beyond path safety. The route the browser reaches these
    /// through, `GET /api/v0/sessions/{session_id}/{asset}`, is one of the
    /// cookie-accepted routes (ADR-0034 item 9), and it is a capture: the
    /// router cannot say which names it covers, because axum will not route
    /// `seg_{start_ms}.m4s`. So the size of that cookie-accepted set is
    /// whatever this function returns true for, and issue #96 is the record of
    /// that gap. A third shape arriving here widens a security surface, which
    /// is why the rejections below are asserted as thoroughly as the two
    /// acceptances: near-misses, wrong padding, suffixed names, the playlist
    /// names that belong to their own routes, and traversal.
    #[test]
    fn a_session_serves_exactly_two_asset_names() {
        assert!(is_safe_asset("init.mp4"));
        assert!(is_safe_asset("seg_00000008000.m4s"));
        for name in [
            "",
            "seg000.m4s",
            "seg_8000.m4s",
            "seg_000000080000.m4s",
            "seg_0000000800a.m4s",
            "seg_00000008000.m4s.tmp",
            "init.mp4.orig",
            "INIT.MP4",
            "master.m3u8",
            "index.m3u8",
            "../etc/passwd",
            "../../init.mp4",
            "subs/e0.vtt",
        ] {
            assert!(!is_safe_asset(name), "{name} must not be servable");
        }
        assert_eq!(
            crate::hls_segment_map::parse_time_keyed_segment_name("seg_00000008000.m4s"),
            Some(8000)
        );
    }

    /// Keyframe PTS must land on SEGMENT_MS boundaries regardless of source
    /// frame rate. A frame-count -g fails this at 60 fps and on VFR.
    #[test]
    fn keyframes_align_to_segment_duration() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let fps60 = dir.path().join("60fps.mp4");
        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=60:duration=6",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=6",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-shortest",
                fps60.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(status.success(), "60fps fixture encode failed");

        let vfr = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files/h264_aac_vfr_mp4.mp4");
        let cases: &[(&str, &Path)] = &[("60fps", fps60.as_path()), ("vfr", vfr.as_path())];

        for (name, src) in cases {
            if !src.exists() {
                eprintln!("skipping {name}: missing {}", src.display());
                continue;
            }
            let enc = dir.path().join(name);
            fs::create_dir_all(&enc).unwrap();
            let mut child = spawn_ffmpeg(
                &ss_start_plan(src, 0, 0),
                &enc,
                SessionMode::Transcode,
                stereo(),
                &crate::EncodeLeg::software(),
                None,
                VideoEncodePlan::default(),
                false,
            )
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            stop_child(&mut Some(child));
            assert!(
                enc.join("index.m3u8").exists(),
                "{name}: ffmpeg playlist missing"
            );
            assert!(
                enc.join("seg000.m4s").exists(),
                "{name}: expected at least seg000.m4s"
            );

            let joined = dir.path().join(format!("{name}-joined.mp4"));
            let status = Command::new("ffmpeg")
                .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
                .arg(enc.join("index.m3u8"))
                .args(["-c", "copy"])
                .arg(&joined)
                .status()
                .unwrap();
            assert!(status.success(), "{name}: remux from HLS failed");

            let out = Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-select_streams",
                    "v:0",
                    "-show_frames",
                    "-show_entries",
                    "frame=key_frame,pts_time",
                    "-of",
                    "csv=p=0",
                ])
                .arg(&joined)
                .output()
                .unwrap();
            assert!(out.status.success(), "{name}: ffprobe failed");
            let text = String::from_utf8_lossy(&out.stdout);
            let key_pts: Vec<f64> = text
                .lines()
                .filter_map(|line| {
                    let mut parts = line.split(',');
                    let key = parts.next()?;
                    let pts = parts.next()?;
                    if key == "1" { pts.parse().ok() } else { None }
                })
                .collect();
            assert!(!key_pts.is_empty(), "{name}: no keyframes\n{text}");

            let segment_s = SEGMENT_MS as f64 / 1000.0;
            for pts in &key_pts {
                let nearest = (pts / segment_s).round() * segment_s;
                assert!(
                    (pts - nearest).abs() < 0.05,
                    "{name}: keyframe at {pts} not on a {segment_s}s boundary ({key_pts:?})"
                );
            }
            assert!(
                key_pts.iter().any(|p| (*p - 0.0).abs() < 0.05),
                "{name}: missing IDR at 0 ({key_pts:?})"
            );
            assert!(
                key_pts.iter().any(|p| (*p - segment_s).abs() < 0.05),
                "{name}: missing IDR at {segment_s}s ({key_pts:?})"
            );
        }
    }

    #[test]
    fn escape_ffmpeg_filter_path_escapes_colon() {
        let escaped = escape_ffmpeg_filter_path(Path::new("/Volumes/NAS:share/a.ass"));
        assert!(escaped.contains(r"\:"), "{escaped}");
    }

    #[test]
    fn libass_filters_listed_requires_ass_and_subtitles() {
        let with = "\
 .. overlay           VV->V      Overlay a video source on top of the input.
 .. ass               V->V       Render ASS subtitles onto input video using the libass library.
 .. subtitles         V->V       Render text subtitles onto input video using the libass library.
";
        assert!(libass_filters_listed(with));
        let without_ass = "\
 .. overlay           VV->V      Overlay a video source on top of the input.
 .. subtitles         V->V       Render text subtitles onto input video using the libass library.
";
        assert!(!libass_filters_listed(without_ass));
        let without_both = "\
 .. overlay           VV->V      Overlay a video source on top of the input.
";
        assert!(!libass_filters_listed(without_both));
    }

    #[test]
    fn libass_filters_listed_live_ffmpeg_and_stripped() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let output = Command::new("ffmpeg")
            .args(["-hide_banner", "-filters"])
            .output()
            .expect("ffmpeg -filters");
        let text = if output.stdout.is_empty() {
            String::from_utf8_lossy(&output.stderr).into_owned()
        } else {
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        // Equipped host (this dogfood machine): both filters present.
        // Lacking host: strip those lines from the same real table shape.
        let live = libass_filters_listed(&text);
        let stripped: String = text
            .lines()
            .filter(|line| {
                !matches!(
                    line.split_whitespace().nth(1),
                    Some("ass") | Some("subtitles")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !libass_filters_listed(&stripped),
            "stripping ass/subtitles from live -filters must fail closed"
        );
        if live {
            assert!(
                ensure_libass_for_ass(BurnInKind::Ass, live).is_ok()
                    && ensure_libass_for_ass(BurnInKind::Ass, false).is_err()
            );
        } else {
            assert!(ensure_libass_for_ass(BurnInKind::Ass, live).is_err());
        }
    }

    #[test]
    fn ensure_libass_for_ass_fails_closed_without_filters() {
        assert!(ensure_libass_for_ass(BurnInKind::Ass, false).is_err());
        assert!(ensure_libass_for_ass(BurnInKind::Ass, true).is_ok());
        assert!(ensure_libass_for_ass(BurnInKind::Pgs, false).is_ok());
    }

    #[test]
    fn ass_burn_vf_embedded_and_sidecar() {
        let side = BurnInSelection {
            track_id: "s-en".into(),
            kind: BurnInKind::Ass,
            stream_index: None,
            subtitle_ordinal: None,
            sidecar_path: Some(PathBuf::from("/tmp/a.ass")),
        };
        assert_eq!(ass_burn_vf(&side, 0).unwrap(), "ass=/tmp/a.ass");
        let mid = ass_burn_vf(&side, 10_000).unwrap();
        assert!(
            mid.starts_with("setpts=PTS+10/TB,ass=") && mid.ends_with(",setpts=PTS-10/TB"),
            "{mid}"
        );
        let spaced = BurnInSelection {
            track_id: "s-en".into(),
            kind: BurnInKind::Ass,
            stream_index: None,
            subtitle_ordinal: None,
            sidecar_path: Some(PathBuf::from("/tmp/The Movie (2007)/a.ass")),
        };
        let vf = ass_burn_vf(&spaced, 0).unwrap();
        assert!(
            vf.contains(r"\(") && vf.contains(r"\)") && vf.contains(r"\ "),
            "{vf}"
        );
        assert!(
            ass_burn_vf(
                &BurnInSelection {
                    track_id: "e2".into(),
                    kind: BurnInKind::Ass,
                    stream_index: Some(2),
                    subtitle_ordinal: Some(0),
                    sidecar_path: None,
                },
                0
            )
            .is_err()
        );
        assert!(pgs_overlay_graph(&side).is_none());
    }

    #[test]
    fn pgs_overlay_graph_embedded_only() {
        let pgs = BurnInSelection {
            track_id: "e3".into(),
            kind: BurnInKind::Pgs,
            stream_index: Some(3),
            subtitle_ordinal: Some(0),
            sidecar_path: None,
        };
        assert_eq!(
            pgs_overlay_graph(&pgs).as_deref(),
            Some("[0:v:0][0:s:0]overlay")
        );
    }

    fn rgb24_abs_diff(a: &[u8], b: &[u8]) -> u64 {
        assert_eq!(a.len(), b.len());
        a.iter()
            .zip(b.iter())
            .map(|(x, y)| u64::from(x.abs_diff(*y)))
            .sum()
    }

    fn ffmpeg_rgb24_frame(src: &Path, vf: Option<&str>, ss_ms: u64) -> Vec<u8> {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-hide_banner", "-loglevel", "error", "-y"]);
        if ss_ms > 0 {
            cmd.args(["-ss", &format!("{:.3}", ss_ms as f64 / 1000.0)]);
        }
        cmd.arg("-i").arg(src).args(["-an", "-frames:v", "1"]);
        if let Some(vf) = vf {
            cmd.args(["-vf", vf]);
        }
        cmd.args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-"]);
        let out = cmd.output().expect("spawn ffmpeg");
        assert!(
            out.status.success(),
            "ffmpeg frame failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    #[test]
    fn burn_in_ass_corpus_changes_pixels() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        if !ffmpeg_has_libass_filters() {
            eprintln!("skipping: host ffmpeg lacks libass filters");
            return;
        }
        let corpus = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files/h264_aac_ass_mkv.mkv");
        if !corpus.exists() {
            eprintln!("skipping: missing {}", corpus.display());
            return;
        }
        let streams = crate::list_burn_in_subtitles(&corpus).expect("list");
        let burn = streams
            .iter()
            .find(|s| s.kind == BurnInKind::Ass)
            .expect("ass track");
        let dir = tempfile::tempdir().unwrap();
        let extracted = dir.path().join("burn_e.ass");
        crate::extract_embedded_ass(&corpus, burn.stream_index, &extracted).expect("extract");
        let selection = BurnInSelection {
            track_id: burn.track_id(),
            kind: BurnInKind::Ass,
            stream_index: Some(burn.stream_index),
            subtitle_ordinal: Some(burn.subtitle_ordinal),
            sidecar_path: Some(extracted.clone()),
        };
        let vf = ass_burn_vf(&selection, 0).unwrap();
        assert!(vf.starts_with("ass="), "{vf}");
        let plain = ffmpeg_rgb24_frame(&corpus, None, 0);
        let burned = ffmpeg_rgb24_frame(&corpus, Some(&vf), 0);
        let diff = rgb24_abs_diff(&plain, &burned);
        assert!(
            diff > 100_000,
            "embedded ASS burn should change pixels; diff_sum={diff}"
        );

        // Sidecar path: extract ASS, burn via ass=
        let side = dir.path().join("track.ass");
        let extract = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(&corpus)
            .args(["-map", "0:s:0", "-c", "copy"])
            .arg(&side)
            .output()
            .unwrap();
        assert!(extract.status.success());
        let side_sel = BurnInSelection {
            track_id: "s-en".into(),
            kind: BurnInKind::Ass,
            stream_index: None,
            subtitle_ordinal: None,
            sidecar_path: Some(side),
        };
        let side_vf = ass_burn_vf(&side_sel, 0).unwrap();
        assert!(side_vf.starts_with("ass="), "{side_vf}");
        let side_burned = ffmpeg_rgb24_frame(&corpus, Some(&side_vf), 0);
        let side_diff = rgb24_abs_diff(&plain, &side_burned);
        assert!(
            side_diff > 100_000,
            "sidecar ASS burn should change pixels; diff_sum={side_diff}"
        );
    }

    #[test]
    fn burn_in_ass_session_produces_segments() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        if !ffmpeg_has_libass_filters() {
            eprintln!("skipping: host ffmpeg lacks libass filters");
            return;
        }
        let corpus = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files/h264_aac_ass_mkv.mkv");
        if !corpus.exists() {
            eprintln!("skipping: missing {}", corpus.display());
            return;
        }
        let streams = crate::list_burn_in_subtitles(&corpus).expect("list");
        let burn = streams
            .iter()
            .find(|s| s.kind == BurnInKind::Ass)
            .expect("ass track");
        let selection = BurnInSelection {
            track_id: burn.track_id(),
            kind: BurnInKind::Ass,
            stream_index: Some(burn.stream_index),
            subtitle_ordinal: Some(burn.subtitle_ordinal),
            sidecar_path: None,
        };
        let dir = tempfile::tempdir().unwrap();
        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 2, "libx264", None, None).unwrap();
        let id = reg
            .start(
                1,
                &corpus,
                0,
                2000,
                SessionMode::Copy,
                stereo(),
                vec![],
                Some(selection),
                None,
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        {
            let sessions = reg.sessions.lock().unwrap();
            let s = sessions.get(&id).unwrap();
            assert_eq!(s.mode, SessionMode::Copy);
            assert!(s.burn_in.is_some());
        }
        wait_playlist(&reg, &id);
        let _ = wait_asset(&reg, &id, "init.mp4");
        let _ = wait_first_listed_asset(&reg, &id);
        reg.stop(&id);
    }

    #[test]
    fn burn_in_sidecar_ass_session_produces_segments() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        if !ffmpeg_has_libass_filters() {
            eprintln!("skipping: host ffmpeg lacks libass filters");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("movie.mp4");
        make_fixture_secs(&src, 2);
        let ass = dir.path().join("movie.ass");
        fs::write(
            &ass,
            "[Script Info]\nScriptType: v4.00+\n\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: Default,Arial,48,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,2,10,10,20,1\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:00.00,0:00:02.00,Default,,0,0,0,,Sidecar ASS\n",
        )
        .unwrap();
        let selection = BurnInSelection {
            track_id: "s-en".into(),
            kind: BurnInKind::Ass,
            stream_index: None,
            subtitle_ordinal: None,
            sidecar_path: Some(ass),
        };
        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 2, "libx264", None, None).unwrap();
        let id = reg
            .start(
                1,
                &src,
                0,
                2000,
                SessionMode::Transcode,
                stereo(),
                vec![],
                Some(selection),
                None,
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        wait_playlist(&reg, &id);
        let _ = wait_first_listed_asset(&reg, &id);
        reg.stop(&id);
    }

    #[test]
    fn burn_in_ass_mid_start_matches_late_cue() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        if !ffmpeg_has_libass_filters() {
            eprintln!("skipping: host ffmpeg lacks libass filters");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("movie.mp4");
        make_fixture_secs(&src, 15);
        let ass = dir.path().join("late.ass");
        fs::write(
            &ass,
            "[Script Info]\nScriptType: v4.00+\nPlayResX: 320\nPlayResY: 240\n\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: Default,Arial,24,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,2,10,10,20,1\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:10.00,0:00:12.00,Default,,0,0,0,,LATE CUE\n",
        )
        .unwrap();
        let selection = BurnInSelection {
            track_id: "s-en".into(),
            kind: BurnInKind::Ass,
            stream_index: None,
            subtitle_ordinal: None,
            sidecar_path: Some(ass.clone()),
        };
        let vf = ass_burn_vf(&selection, 10_000).unwrap();
        let plain = ffmpeg_rgb24_frame(&src, None, 10_000);
        let burned = ffmpeg_rgb24_frame(&src, Some(&vf), 10_000);
        let diff = rgb24_abs_diff(&plain, &burned);
        assert!(
            diff > 10_000,
            "mid-start ASS burn must show late cue; diff_sum={diff} vf={vf}"
        );
    }

    #[test]
    fn burn_in_pgs_session_produces_segments() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let corpus = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files/h264_aac_pgs_mkv.mkv");
        if !corpus.exists() {
            eprintln!("skipping: missing {}", corpus.display());
            return;
        }
        let streams = crate::list_burn_in_subtitles(&corpus).expect("list");
        let burn = streams
            .iter()
            .find(|s| s.kind == BurnInKind::Pgs)
            .expect("pgs track");
        let selection = BurnInSelection {
            track_id: burn.track_id(),
            kind: BurnInKind::Pgs,
            stream_index: Some(burn.stream_index),
            subtitle_ordinal: Some(burn.subtitle_ordinal),
            sidecar_path: None,
        };
        let dir = tempfile::tempdir().unwrap();
        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 2, "libx264", None, None).unwrap();
        let id = reg
            .start(
                1,
                &corpus,
                0,
                2000,
                SessionMode::Copy,
                stereo(),
                vec![],
                Some(selection),
                None,
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        wait_playlist(&reg, &id);
        let _ = wait_first_listed_asset(&reg, &id);
        reg.stop(&id);
    }

    /// Segment URIs in the media playlist are path-absolute under the session
    /// root so run-directory depth cannot break resolution (ADR-0008).
    #[test]
    fn media_playlist_segment_uris_are_session_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let run0 = run_path(dir.path(), SINGLE_VIDEO_RUNG, 0);
        fs::create_dir_all(&run0).unwrap();
        let seg_rel =
            crate::hls_segment_map::run_rel_dir(SINGLE_VIDEO_RUNG, 0).join("seg_disk.m4s");
        fs::write(dir.path().join(&seg_rel), [0u8; 64]).unwrap();
        let mut map = crate::hls_segment_map::SegmentMap::default();
        map.insert(crate::hls_segment_map::MappedSegment {
            start_ms: 21,
            duration_ms: 2002,
            run_id: 0,
            rel_path: seg_rel,
        });
        let session = Session {
            item_id: 33,
            src: PathBuf::from("/dev/null"),
            dir: dir.path().to_path_buf(),
            run_cache_budget_bytes: SESSION_RUN_CACHE_BUDGET_BYTES,
            mode: SessionMode::Copy,
            audio: stereo(),
            burn_in: None,
            encode_plan: VideoEncodePlan::default(),
            map_binding: MapBinding::default(),
            encode_leg: crate::EncodeLeg::software(),
            video_encoder: "copy".into(),
            start_ms: 0,
            play_start_ms: 0,
            landed_ms: 21,
            usable_extent_ms: None,
            duration_ms: 60_000,
            encoder_states: single_rung_encoder_states(0, 1, None),
            segment_maps: single_rung_segment_maps(map),
            current_run_eof: false,
            last_access: Instant::now(),
            last_restart: Instant::now(),
            primed: false,
            first_segment_ready: false,
            pending_play_ms: None,
            pending_since: None,
            failed: None,
            subtitle_tracks: vec![],
            last_requested_ms: 0,
            piggyback: None,
            subs: None,
            db: None,
            map_build_in_flight: None,
        };
        let pl = build_run_media_playlist("s1", &session, SINGLE_VIDEO_RUNG);
        let text = String::from_utf8_lossy(&pl);
        let uri = text
            .lines()
            .find(|l| l.contains("seg_"))
            .expect("listed segment URI");
        assert_eq!(
            uri, "/api/v0/sessions/s1/seg_00000000021.m4s",
            "must be path-absolute under the session"
        );
        assert!(
            text.contains("#EXT-X-MAP:URI=\"/api/v0/sessions/s1/runs/0/init.mp4\""),
            "MAP URI must be path-absolute, got {text}"
        );
    }

    /// Client-shaped link walk: master → media → MAP → first segment, each
    /// hop resolved the way a browser resolves relative HLS URIs, then served
    /// as real bytes (not string-only checks). Locks the relative-URI class
    /// that produced three cutover defects (segment depth, sub climb, and a
    /// master that pointed at the wrong index).
    ///
    /// **Rewritten 2026-08-31 with ADR-0054 decision 5.** This read: *"the
    /// mistaken `master points at session-root index` hypothesis — master is
    /// per-run, so bare `index.m3u8` is correct"*. The premise was true and is
    /// not any more. The master is the session's, and the session-root index is
    /// exactly what it points at.
    ///
    /// The walk now runs twice, either side of a seek, and asserts the two
    /// things decision 5 turns on: the URI does not change, and the
    /// `EXT-X-MAP` inside it does.
    #[test]
    fn session_hls_link_walk_resolves_to_real_bytes() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.mp4");
        make_fixture_secs(&src, 12);
        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 2, "libx264", None, None).unwrap();
        let id = reg
            .start(
                1,
                &src,
                0,
                12_000,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                None,
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        wait_playlist(&reg, &id);
        let _ = wait_first_listed_asset(&reg, &id);
        let view = reg.view(&id).expect("view");
        let init_before = walk_run_playlist_chain(&reg, &id, view.run_id, &view.playlist_url);

        std::thread::sleep(RESTART_MIN_INTERVAL);
        let after = reg.seek(&id, 4_000).expect("seek");
        assert_ne!(after.run_id, view.run_id, "seek must mint a fresh run");
        // Hold until the new run's media playlist lists a segment, then walk.
        let pl = wait_playlist(&reg, &id);
        let _ = wait_asset(&reg, &id, &first_listed_seg(&pl));
        let init_after = walk_run_playlist_chain(&reg, &id, after.run_id, &after.playlist_url);

        // **The shape ADR-0054 decision 5 was held back for, pinned.** One URI
        // across a seek, and the `EXT-X-MAP` inside it naming a different init
        // on either side. The ADR called this unmeasured and would not call it
        // coherent; what settles it is that neither client re-reads a `VOD`
        // playlist in place, so the changed map is only ever met through a
        // re-attach that reads playlist and map together.
        assert_eq!(
            view.playlist_url, after.playlist_url,
            "the playlist URI must not change across a seek"
        );
        assert_ne!(
            init_before, init_after,
            "EXT-X-MAP must name the new run's init after a seek"
        );
        reg.stop(&id);
    }

    /// Resolve `relative` against an absolute path URL (no scheme), the same
    /// way `urljoin` / browsers resolve HLS playlist references.
    fn resolve_hls_uri(base_url: &str, relative: &str) -> String {
        if relative.starts_with('/') {
            return relative.to_string();
        }
        let base_dir = base_url
            .rsplit_once('/')
            .map(|(d, _)| d)
            .unwrap_or(base_url);
        let mut parts: Vec<&str> = base_dir.split('/').filter(|p| !p.is_empty()).collect();
        for seg in relative.split('/') {
            match seg {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                other => parts.push(other),
            }
        }
        format!("/{}", parts.join("/"))
    }

    /// Walks master → media → `EXT-X-MAP` → init the way a client resolves it,
    /// and returns the init URI the map named.
    ///
    /// `run_id` is what the *session* says the current run is, not something the
    /// URL carries. The walk asserts the map still reaches that run's init after
    /// the playlists stopped naming it (ADR-0054 decision 5).
    fn walk_run_playlist_chain(
        reg: &HlsSessionRegistry,
        id: &str,
        run_id: u64,
        master_url: &str,
    ) -> String {
        assert_eq!(
            master_url,
            format!("/api/v0/sessions/{id}/master.m3u8"),
            "playlistUrl must be the session master, with no run segment"
        );
        assert!(
            !master_url.contains("/runs/"),
            "dead class: the master URI is not per-run, got {master_url}"
        );
        let master = reg.master(id).expect("master bytes");
        let master_text = String::from_utf8_lossy(&master);
        assert!(master_text.starts_with("#EXTM3U"), "{master_text}");
        let media_rel = master_text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .expect("master must list a media playlist URI");
        // Content types match the HTTP layer (sessions.rs): playlists →
        // application/vnd.apple.mpegurl; init → video/mp4; segments →
        // video/iso.segment. Asserted here as path→kind, bytes below.
        assert!(
            media_rel.ends_with(".m3u8") || media_rel == "index.m3u8",
            "media playlist URI must be m3u8, got {media_rel}"
        );
        let media_url = resolve_hls_uri(master_url, media_rel);
        assert_eq!(
            media_url,
            format!("/api/v0/sessions/{id}/index.m3u8"),
            "master must emit the path-absolute session-scoped media URI"
        );
        // Dead class, inverted 2026-08-31 by ADR-0054 decision 5. This
        // previously asserted the opposite, that a session-root index "is not on
        // the wire", and the doc comment above the calling test argued for it in
        // prose. Both were correct while the master was per-run.
        assert!(
            !media_url.contains("/runs/"),
            "the media URI is the session's, got {media_url}"
        );

        let media = reg.playlist(id).expect("media playlist");
        let media_text = String::from_utf8_lossy(&media);
        assert!(media_text.contains("#EXTINF:"), "{media_text}");
        let map_uri = media_text
            .lines()
            .find_map(|l| {
                l.trim()
                    .strip_prefix("#EXT-X-MAP:URI=\"")
                    .map(|rest| rest.trim_end_matches('"').to_string())
            })
            .expect("EXT-X-MAP");
        let init_url = resolve_hls_uri(&media_url, &map_uri);
        assert_eq!(
            init_url,
            format!("/api/v0/sessions/{id}/runs/{run_id}/init.mp4")
        );
        let init = reg
            .run_asset(id, run_id, "init.mp4")
            .expect("init.mp4 bytes");
        assert!(
            init.len() > 8 && &init[4..8] == b"ftyp",
            "init must be fMP4"
        );

        let seg_rel = media_text
            .lines()
            .map(str::trim)
            .find(|l| l.contains("seg_") && l.ends_with(".m4s"))
            .expect("first segment URI")
            .to_string();
        let seg_url = resolve_hls_uri(&media_url, &seg_rel);
        assert!(
            seg_url.starts_with(&format!("/api/v0/sessions/{id}/seg_")),
            "segment must resolve to session-root asset route, got {seg_url} from {seg_rel}"
        );
        let seg_name = seg_url.rsplit('/').next().expect("seg name");
        let seg = reg.asset(id, seg_name, None).expect("segment bytes");
        assert!(!seg.is_empty(), "first listed segment must have bytes");
        init_url
    }

    /// Empty mid-title EOF must record usable extent (even with an empty map)
    /// so clients see damage instead of hanging on master 503 (DEF-8519 mask).
    #[test]
    fn empty_eof_records_usable_extent_zero() {
        let dir = tempfile::tempdir().unwrap();
        let run0 = run_path(dir.path(), SINGLE_VIDEO_RUNG, 0);
        fs::create_dir_all(&run0).unwrap();
        let mut session = Session {
            item_id: 8519,
            src: PathBuf::from("/dev/null"),
            dir: dir.path().to_path_buf(),
            run_cache_budget_bytes: SESSION_RUN_CACHE_BUDGET_BYTES,
            mode: SessionMode::Copy,
            audio: stereo(),
            burn_in: None,
            encode_plan: VideoEncodePlan::default(),
            map_binding: MapBinding::default(),
            encode_leg: crate::EncodeLeg::software(),
            video_encoder: "copy".into(),
            start_ms: 1_014_000,
            play_start_ms: 1_014_000,
            landed_ms: 1_014_000,
            usable_extent_ms: None,
            duration_ms: 1_354_496,
            encoder_states: single_rung_encoder_states(0, 1, None),
            segment_maps: single_rung_segment_maps(crate::hls_segment_map::SegmentMap::default()),
            current_run_eof: false,
            last_access: Instant::now(),
            last_restart: Instant::now(),
            primed: false,
            first_segment_ready: false,
            pending_play_ms: None,
            pending_since: None,
            failed: None,
            subtitle_tracks: vec![],
            last_requested_ms: 0,
            piggyback: None,
            subs: None,
            db: None,
            map_build_in_flight: None,
        };
        apply_run_eof(&mut session);
        assert!(session.current_run_eof);
        assert_eq!(
            session.usable_extent_ms,
            Some(0),
            "empty map + mid-title EOF → usableExtentMs=0"
        );
        let pl = build_run_media_playlist("s1", &session, SINGLE_VIDEO_RUNG);
        let text = String::from_utf8_lossy(&pl);
        assert!(text.contains("#EXT-X-ENDLIST"), "empty ENDLIST for clients");
        assert!(
            !text.contains("seg_"),
            "no listed URIs when nothing is on disk"
        );
    }

    /// A minimal session for the EOF-extent tests. Fields the extent does not
    /// read are defaults; the ones it does — `duration_ms`, `current_run_id`,
    /// `segment_maps` — are set by the caller.
    fn eof_test_session(dir: &Path, duration_ms: u64) -> Session {
        Session {
            item_id: 8519,
            src: PathBuf::from("/dev/null"),
            dir: dir.to_path_buf(),
            run_cache_budget_bytes: SESSION_RUN_CACHE_BUDGET_BYTES,
            mode: SessionMode::Copy,
            audio: stereo(),
            burn_in: None,
            encode_plan: VideoEncodePlan::default(),
            map_binding: MapBinding::default(),
            encode_leg: crate::EncodeLeg::software(),
            video_encoder: "copy".into(),
            start_ms: 0,
            play_start_ms: 0,
            landed_ms: 0,
            usable_extent_ms: None,
            duration_ms,
            encoder_states: single_rung_encoder_states(0, 1, None),
            segment_maps: single_rung_segment_maps(crate::hls_segment_map::SegmentMap::default()),
            current_run_eof: false,
            last_access: Instant::now(),
            last_restart: Instant::now(),
            primed: false,
            first_segment_ready: false,
            pending_play_ms: None,
            pending_since: None,
            failed: None,
            subtitle_tracks: vec![],
            last_requested_ms: 0,
            piggyback: None,
            subs: None,
            db: None,
            map_build_in_flight: None,
        }
    }

    /// The extent is 0 only when the session produced nothing.
    ///
    /// The shape no test covered until 2026-08-30, and the one #180 regressed:
    /// a session that has already played, then a seek landing at or past the
    /// true media end of a title claiming more. The run exits clean having
    /// written nothing.
    ///
    /// Reading the ended run's frontier here gives 0, and `scrubRangeMs`
    /// returns `usableExtentMs` over `item.durationMs` whenever it is set —
    /// so the scrub bar would collapse to zero and stay there, because
    /// nothing clears the field. The session maximum reports what is still
    /// reachable.
    #[test]
    fn usable_extent_zero_only_when_the_session_produced_nothing() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(run_path(dir.path(), SINGLE_VIDEO_RUNG, 1)).unwrap();

        let mut map = crate::hls_segment_map::SegmentMap::default();
        // run 0 played the first 400 s of a title claiming 1354 s.
        map.insert(crate::hls_segment_map::MappedSegment {
            start_ms: 398_000,
            duration_ms: 2_000,
            run_id: 0,
            rel_path: crate::hls_segment_map::run_rel_dir(SINGLE_VIDEO_RUNG, 0)
                .join("seg_00000398000.m4s"),
        });

        // run 1 landed past the real media end and exited clean, writing
        // nothing of its own.
        let mut session = eof_test_session(dir.path(), 1_354_496);
        session.segment_maps = single_rung_segment_maps(map);
        session.encoder_state_mut(SINGLE_VIDEO_RUNG).current_run_id = 1;
        session.encoder_state_mut(SINGLE_VIDEO_RUNG).next_run_id = 2;
        session.start_ms = 1_014_000;
        session.play_start_ms = 1_014_000;

        apply_run_eof(&mut session);

        assert_eq!(
            session.usable_extent_ms,
            Some(400_000),
            "400 s is still reachable; reporting the empty run's own frontier \
             would collapse the scrub bar to zero for the session's life"
        );
    }

    /// Eviction: orphans first; map-referenced run survives while orphans
    /// remain; after a referenced eviction the map no longer points at gone
    /// files; playlist never lists missing bytes.
    #[test]
    fn eviction_map_authoritative_orphans_first() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path();
        // run_0: referenced map bytes (large)
        let run0 = run_path(session_dir, SINGLE_VIDEO_RUNG, 0);
        fs::create_dir_all(&run0).unwrap();
        let seg_path = run0.join("seg.m4s");
        fs::write(&seg_path, vec![0u8; 50_000]).unwrap();
        // run_1: orphan (no map refs) with bytes
        let run1 = run_path(session_dir, SINGLE_VIDEO_RUNG, 1);
        fs::create_dir_all(&run1).unwrap();
        fs::write(run1.join("init.mp4"), vec![0u8; 10_000]).unwrap();
        // run_2: empty finished (noise)
        let run2 = run_path(session_dir, SINGLE_VIDEO_RUNG, 2);
        fs::create_dir_all(&run2).unwrap();
        // run_3: current (small)
        let run3 = run_path(session_dir, SINGLE_VIDEO_RUNG, 3);
        fs::create_dir_all(&run3).unwrap();
        fs::write(run3.join("init.mp4"), vec![0u8; 100]).unwrap();

        let mut map = crate::hls_segment_map::SegmentMap::default();
        map.insert(crate::hls_segment_map::MappedSegment {
            start_ms: 0,
            duration_ms: 2000,
            run_id: 0,
            rel_path: crate::hls_segment_map::run_rel_dir(SINGLE_VIDEO_RUNG, 0).join("seg.m4s"),
        });

        let mut session = Session {
            item_id: 1,
            src: PathBuf::from("/dev/null"),
            dir: session_dir.to_path_buf(),
            run_cache_budget_bytes: SESSION_RUN_CACHE_BUDGET_BYTES,
            mode: SessionMode::Transcode,
            audio: stereo(),
            burn_in: None,
            encode_plan: VideoEncodePlan::default(),
            map_binding: MapBinding::default(),
            encode_leg: crate::EncodeLeg::software(),
            video_encoder: "libx264".into(),
            start_ms: 0,
            play_start_ms: 0,
            landed_ms: 0,
            usable_extent_ms: None,
            duration_ms: 60_000,
            encoder_states: single_rung_encoder_states(3, 4, None),
            segment_maps: single_rung_segment_maps(map),
            current_run_eof: true,
            last_access: Instant::now(),
            last_restart: Instant::now(),
            primed: true,
            first_segment_ready: true,
            pending_play_ms: None,
            pending_since: None,
            failed: None,
            subtitle_tracks: vec![],
            last_requested_ms: 0,
            piggyback: None,
            subs: None,
            db: None,
            map_build_in_flight: None,
        };

        // Budget between orphan (10k) and total (~60k): one eviction of orphan.
        session.run_cache_budget_bytes = 55_000;
        maybe_evict_finished_runs(&mut session);
        assert!(!run1.exists(), "orphan run_1 must be evicted first");
        assert!(
            run0.exists() && session.segment_map(SINGLE_VIDEO_RUNG).run_is_referenced(0),
            "map-referenced run_0 must survive while orphans remain"
        );
        assert!(!run2.exists(), "empty run_2 reaped quietly");
        assert!(
            session
                .dir
                .join(
                    &session
                        .segment_map(SINGLE_VIDEO_RUNG)
                        .get(0)
                        .unwrap()
                        .rel_path,
                )
                .is_file(),
            "mapped file still on disk"
        );

        // Force referenced eviction: budget below run_0 size.
        session.run_cache_budget_bytes = 1_000;
        maybe_evict_finished_runs(&mut session);
        assert!(!run0.exists(), "referenced run evicted under hard pressure");
        assert!(
            !session.segment_map(SINGLE_VIDEO_RUNG).run_is_referenced(0),
            "map must drop entries before/with delete"
        );
        let pl = build_run_media_playlist("s1", &session, SINGLE_VIDEO_RUNG);
        assert!(
            !String::from_utf8_lossy(&pl).contains("seg_"),
            "playlist must not list URIs whose files are gone"
        );
    }

    /// Twelve seconds with a keyframe every second, so a keyframe map has
    /// mid-title lands to snap to. Matroska gets one Cluster per keyframe.
    fn make_mapped_fixture(path: &Path, secs: u32) {
        let d = secs.to_string();
        let mut cmd = Command::new("ffmpeg");
        cmd.args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size=64x64:rate=25:d={d}"),
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:duration={d}"),
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-pix_fmt",
            "yuv420p",
            "-force_key_frames",
            "expr:gte(t,n_forced*1)",
            "-sc_threshold",
            "0",
            "-c:a",
            "aac",
        ]);
        if path.extension().and_then(|e| e.to_str()) == Some("mkv") {
            cmd.args(["-cluster_time_limit", "1000"]);
        }
        assert!(cmd.arg(path).status().unwrap().success());
    }

    fn keyframe_map(
        kind: MapContainerKind,
        src: &Path,
        entries: Vec<KeyframeEntry>,
    ) -> KeyframeMap {
        assert!(entries.len() > 4, "fixture needs mid-title lands");
        KeyframeMap {
            container_kind: kind,
            content_id: nightjar_db::content_id_for_path(src).unwrap(),
            entries,
        }
    }

    /// EBML variable-size integer at `at`: value with the marker bit
    /// stripped, and how many bytes it occupied.
    fn read_vint(bytes: &[u8], at: usize) -> Option<(u64, usize)> {
        let first = *bytes.get(at)?;
        if first == 0 {
            return None;
        }
        let len = first.leading_zeros() as usize + 1;
        let mut value = u64::from(first) & (0xFF >> len);
        for step in 1..len {
            value = (value << 8) | u64::from(*bytes.get(at + step)?);
        }
        Some((value, len))
    }

    /// Cluster offsets and Cluster timestamps, the pair a Matroska map
    /// carries. Timestamps are raw TimestampScale units, which the fixture
    /// leaves at the 1ms default.
    fn matroska_entries(src: &Path) -> Vec<KeyframeEntry> {
        let bytes = fs::read(src).unwrap();
        let mut entries = Vec::new();
        let mut at = 0usize;
        while at + 4 <= bytes.len() {
            if bytes[at..at + 4] != [0x1F, 0x43, 0xB6, 0x75] {
                at += 1;
                continue;
            }
            // A Cluster ID that is really frame data will not be followed by
            // a size and a Timestamp child, so the parse rejects it.
            let mut cursor = at + 4;
            let Some((_, size_len)) = read_vint(&bytes, cursor) else {
                at += 1;
                continue;
            };
            cursor += size_len;
            // FFmpeg writes a CRC-32 child ahead of the Cluster Timestamp.
            if bytes.get(cursor) == Some(&0xBF) {
                let Some((crc_len, crc_len_len)) = read_vint(&bytes, cursor + 1) else {
                    at += 1;
                    continue;
                };
                cursor += 1 + crc_len_len + crc_len as usize;
            }
            if bytes.get(cursor) != Some(&0xE7) {
                at += 1;
                continue;
            }
            cursor += 1;
            let Some((len, len_len)) = read_vint(&bytes, cursor) else {
                at += 1;
                continue;
            };
            cursor += len_len;
            let end = cursor + len as usize;
            let pts_ms = bytes[cursor..end]
                .iter()
                .fold(0u64, |acc, b| (acc << 8) | u64::from(*b));
            entries.push(KeyframeEntry {
                pts_ms,
                byte_offset: at as u64,
            });
            at = end;
        }
        entries
    }

    /// Sync sample PTS and position, the pair an MP4 map carries.
    fn mp4_entries(src: &Path) -> Vec<KeyframeEntry> {
        let out = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_packets",
                "-show_entries",
                "packet=pts_time,pos,flags",
                "-of",
                "default=noprint_wrappers=1",
            ])
            .arg(src)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "ffprobe packets for {}",
            src.display()
        );
        let mut entries = Vec::new();
        let (mut pts_ms, mut byte_offset) = (None, None);
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key {
                "pts_time" => pts_ms = value.parse::<f64>().ok().map(|s| (s * 1000.0) as u64),
                "pos" => byte_offset = value.parse::<u64>().ok(),
                "flags" if value.starts_with('K') => {
                    if let (Some(pts_ms), Some(byte_offset)) = (pts_ms, byte_offset) {
                        entries.push(KeyframeEntry {
                            pts_ms,
                            byte_offset,
                        });
                    }
                }
                _ => {}
            }
        }
        entries
    }

    /// Drives a started session to producer EOF and joins the run so the
    /// delivered stream can be probed as one file.
    fn run_to_eof_and_join(reg: &HlsSessionRegistry, id: &str, out: &Path) -> PathBuf {
        let deadline = Instant::now() + Duration::from_secs(120);
        let run_index = loop {
            let (run_id, dir, failed) = {
                let sessions = reg.sessions.lock().unwrap();
                let session = sessions.get(id).expect("session live");
                (
                    session.encoder_state(SINGLE_VIDEO_RUNG).current_run_id,
                    session.dir.clone(),
                    session.failed.clone(),
                )
            };
            if let Some(err) = failed {
                panic!("session failed: {err}");
            }
            // Keeps last_access fresh so the reaper leaves the session alone.
            let _ = reg.playlist(id);
            let index = run_path(&dir, SINGLE_VIDEO_RUNG, run_id).join("index.m3u8");
            if fs::read_to_string(&index)
                .map(|text| text.contains("#EXT-X-ENDLIST"))
                .unwrap_or(false)
            {
                break index;
            }
            assert!(Instant::now() < deadline, "producer never reached EOF");
            std::thread::sleep(Duration::from_millis(100));
        };
        let joined = out.join("joined.mp4");
        // -copyts: without it the join rebases to zero and the title-absolute
        // land the session encoded at would not survive into the probe.
        let status = Command::new("ffmpeg")
            .args(["-y", "-hide_banner", "-loglevel", "error", "-copyts", "-i"])
            .arg(&run_index)
            .args(["-c", "copy"])
            .arg(&joined)
            .status()
            .unwrap();
        assert!(status.success(), "join {}", run_index.display());
        joined
    }

    fn probe_secs(path: &Path, select: &str, entry: &str) -> f64 {
        let raw = probe_entry(path, select, entry);
        raw.parse()
            .unwrap_or_else(|_| panic!("{select} {entry} was {raw:?}"))
    }

    /// Decodes the audio the session delivered. A byte splice that lands
    /// mid-frame still probes as AAC and still lists a duration; only a
    /// decode says whether the frames survived.
    fn assert_audio_decodes(path: &Path) {
        let out = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(path)
            .args(["-map", "0:a:0", "-f", "null", "-"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success() && stderr.trim().is_empty(),
            "audio decode errors in {}: {stderr}",
            path.display()
        );
    }

    /// Video and audio must start together at the land: the splice is only
    /// honest if both tracks begin where the map said (ADR-0023 §3).
    fn assert_av_landed(joined: &Path, land_ms: u64, fixture_ms: u64) {
        let land = land_ms as f64 / 1000.0;
        let video_start = probe_secs(joined, "v:0", "start_time");
        let audio_start = probe_secs(joined, "a:0", "start_time");
        assert!(
            (video_start - land).abs() < 0.2,
            "video starts at {video_start}s, land was {land}s"
        );
        assert!(
            (video_start - audio_start).abs() < 0.15,
            "A/V offset {:.3}s (video {video_start}s, audio {audio_start}s)",
            video_start - audio_start
        );
        let want = (fixture_ms - land_ms) as f64 / 1000.0;
        let video_duration = probe_secs(joined, "v:0", "duration");
        let audio_duration = probe_secs(joined, "a:0", "duration");
        assert!(
            (video_duration - want).abs() < 0.5,
            "video runs {video_duration}s, expected {want}s from the land to EOF"
        );
        assert!(
            (audio_duration - want).abs() < 0.5,
            "audio runs {audio_duration}s, expected {want}s from the land to EOF"
        );
        assert_audio_decodes(joined);
    }

    /// Where a transcode run whose splice landed on `cue_ms` now starts.
    ///
    /// Runs no longer keep the cue's own phase. `snap_plan_to_grid` moves the
    /// output up to the grid every run shares and drops the media between, so
    /// a full-title listing can name the entries in advance. The splice still
    /// opens at the cue — that is what `map_binding.bound` and `!fell_back`
    /// assert beside this.
    /// The mapped fixtures are `testsrc rate=25`, where 2000 ms is exactly 50
    /// frames, so the produced cadence is `SEGMENT_MS`. Declaring the rate is
    /// what gives the session a grid at all: without it `produced_segment_ms`
    /// is `None` and no run is snapped.
    fn plan_25fps() -> VideoEncodePlan {
        VideoEncodePlan {
            source_frame_rate: Some((25, 1)),
            ..VideoEncodePlan::default()
        }
    }

    fn grid_start(cue_ms: u64) -> u64 {
        cue_ms.div_ceil(SEGMENT_MS) * SEGMENT_MS
    }

    const MAPPED_FIXTURE_MS: u64 = 12_000;

    /// ADR-0023 §3a: the Matroska splice opens at the land Cluster with no
    /// `-ss`, and what comes out still decodes on both tracks.
    #[test]
    fn mapped_matroska_session_starts_at_the_land_cluster() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("clip.mkv");
        make_mapped_fixture(&src, 12);
        let map = keyframe_map(MapContainerKind::Matroska, &src, matroska_entries(&src));
        let land_ms = map.entry_at_or_before(6000).unwrap().pts_ms;

        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264", None, None).unwrap();
        let id = reg
            .start(
                1,
                &src,
                6000,
                MAPPED_FIXTURE_MS,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                Some(map),
                plan_25fps(),
                None,
            )
            .unwrap();
        {
            let sessions = reg.sessions.lock().unwrap();
            let session = sessions.get(&id).unwrap();
            assert!(
                session.map_binding.bound.is_some(),
                "Matroska map start must open the spliced virtual file"
            );
            assert!(!session.map_binding.fell_back);
            assert_eq!(
                session.start_ms,
                grid_start(land_ms),
                "the splice opens at the land Cluster and the output starts on \
                 the shared grid above it, not on an -ss lead-in"
            );
        }
        let joined = run_to_eof_and_join(&reg, &id, dir.path());
        assert_av_landed(&joined, grid_start(land_ms), MAPPED_FIXTURE_MS);
    }

    /// A seek re-splices at the new land: the session keeps the map, so the
    /// second run opens a fresh virtual file rather than falling back.
    #[test]
    fn seek_rebinds_the_splice_at_the_new_land() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("clip.mkv");
        make_mapped_fixture(&src, 12);
        let map = keyframe_map(MapContainerKind::Matroska, &src, matroska_entries(&src));
        let land_ms = map.entry_at_or_before(2000).unwrap().pts_ms;

        // Start late and scrub back: the running encode cannot cover the new
        // land, so the seek has to respawn rather than retarget.
        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264", None, None).unwrap();
        let id = reg
            .start(
                1,
                &src,
                8000,
                MAPPED_FIXTURE_MS,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                Some(map),
                plan_25fps(),
                None,
            )
            .unwrap();
        wait_playlist(&reg, &id);
        std::thread::sleep(RESTART_MIN_INTERVAL);
        let view = reg.seek(&id, 2000).expect("seek");
        assert_ne!(view.run_id, 0, "fresh run after seek");
        {
            let sessions = reg.sessions.lock().unwrap();
            let session = sessions.get(&id).unwrap();
            assert!(session.map_binding.bound.is_some(), "seek re-splices");
            assert_eq!(session.start_ms, grid_start(land_ms));
        }
        assert!(!reg.map_fallback(&id), "rebind must hold across a seek");
        let joined = run_to_eof_and_join(&reg, &id, dir.path());
        assert_av_landed(&joined, grid_start(land_ms), MAPPED_FIXTURE_MS);
    }

    /// Copy leaves the source frames untouched, so a splice that lands
    /// mid-frame shows up as drift between the tracks. Checked here rather
    /// than inferred from the transcode run, which would re-time both.
    #[test]
    fn mapped_matroska_copy_session_keeps_audio_with_video() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("clip.mkv");
        make_mapped_fixture(&src, 12);
        let map = keyframe_map(MapContainerKind::Matroska, &src, matroska_entries(&src));
        let land_ms = map.entry_at_or_before(4000).unwrap().pts_ms;

        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "copy", None, None).unwrap();
        let id = reg
            .start(
                1,
                &src,
                4000,
                MAPPED_FIXTURE_MS,
                SessionMode::Copy,
                stereo(),
                vec![],
                None,
                Some(map),
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        let joined = run_to_eof_and_join(&reg, &id, dir.path());
        assert_eq!(probe_entry(&joined, "a:0", "codec_name"), "aac");
        assert_av_landed(&joined, land_ms, MAPPED_FIXTURE_MS);
    }

    /// ADR-0023 §3b: an end-moov MP4 is served as faststart and still seeks
    /// with `-ss`. The naive splice broke AAC here, so copy mode carries the
    /// original frames through to a decode.
    #[test]
    fn mapped_mp4_session_keeps_aac_intact() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("clip.mp4");
        make_mapped_fixture(&src, 12);
        let map = keyframe_map(MapContainerKind::Mp4, &src, mp4_entries(&src));
        let land_ms = map.entry_at_or_before(6000).unwrap().pts_ms;

        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "copy", None, None).unwrap();
        let id = reg
            .start(
                1,
                &src,
                6000,
                MAPPED_FIXTURE_MS,
                SessionMode::Copy,
                stereo(),
                vec![],
                None,
                Some(map),
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        {
            let sessions = reg.sessions.lock().unwrap();
            let session = sessions.get(&id).unwrap();
            assert!(
                session.map_binding.bound.is_some(),
                "end-moov MP4 must be served through the faststart layout"
            );
            assert!(!session.map_binding.fell_back);
            assert_eq!(session.start_ms, land_ms);
        }
        let joined = run_to_eof_and_join(&reg, &id, dir.path());
        assert_eq!(probe_entry(&joined, "a:0", "codec_name"), "aac");
        assert_av_landed(&joined, land_ms, MAPPED_FIXTURE_MS);
    }

    /// A faststart MP4 needs no virtual file: the map snap is the whole win
    /// and FFmpeg seeks the real path (ADR-0023 §3b).
    #[test]
    fn faststart_mp4_maps_without_a_virtual_file() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("staged.mp4");
        make_mapped_fixture(&staged, 12);
        let src = dir.path().join("clip.mp4");
        let status = Command::new("ffmpeg")
            .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
            .arg(&staged)
            .args(["-c", "copy", "-movflags", "+faststart"])
            .arg(&src)
            .status()
            .unwrap();
        assert!(status.success());
        let map = keyframe_map(MapContainerKind::Mp4, &src, mp4_entries(&src));
        let land_ms = map.entry_at_or_before(6000).unwrap().pts_ms;

        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264", None, None).unwrap();
        let id = reg
            .start(
                1,
                &src,
                6000,
                MAPPED_FIXTURE_MS,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                Some(map),
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        {
            let sessions = reg.sessions.lock().unwrap();
            let session = sessions.get(&id).unwrap();
            assert!(
                session.map_binding.bound.is_none(),
                "no server for faststart"
            );
            assert!(!session.map_binding.fell_back);
            assert_eq!(session.start_ms, land_ms);
        }
        let (_, listed_ms) = wait_land_near(&reg, &id, 6000);
        assert!(
            listed_ms + SEGMENT_MS >= land_ms,
            "land {listed_ms} before map"
        );
    }

    /// ADR-0023 §8: no map is not a failure, it is today's start.
    #[test]
    fn unmapped_session_starts_with_ss() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("clip.mp4");
        make_mapped_fixture(&src, 12);
        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264", None, None).unwrap();
        let id = reg
            .start(
                1,
                &src,
                6000,
                MAPPED_FIXTURE_MS,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                None,
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        {
            let sessions = reg.sessions.lock().unwrap();
            let session = sessions.get(&id).unwrap();
            assert!(session.map_binding.bound.is_none());
        }
        assert!(
            !reg.map_fallback(&id),
            "no map held means nothing to rebuild from this signal"
        );
        let _ = wait_land_near(&reg, &id, 6000);
    }

    /// ADR-0023 §4: the file was replaced under the map. The session starts
    /// with `-ss` on the real bytes and flags the rebuild the API enqueues.
    #[test]
    fn replaced_file_degrades_to_ss_and_flags_rebuild() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("clip.mkv");
        make_mapped_fixture(&src, 12);
        let map = keyframe_map(MapContainerKind::Matroska, &src, matroska_entries(&src));
        make_mapped_fixture(&src, 10);

        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264", None, None).unwrap();
        let id = reg
            .start(
                1,
                &src,
                6000,
                10_000,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                Some(map),
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        {
            let sessions = reg.sessions.lock().unwrap();
            let session = sessions.get(&id).unwrap();
            assert!(
                session.map_binding.bound.is_none(),
                "stale identity must not serve map offsets into new bytes"
            );
        }
        assert!(reg.map_fallback(&id), "stale map must enqueue a rebuild");
        let _ = wait_land_near(&reg, &id, 6000);
    }

    /// A packet-walk map records block positions inside a Cluster. Splicing
    /// one hands FFmpeg garbage that still parses, so the bind refuses it and
    /// the session falls back (ADR-0023 §8).
    #[test]
    fn offsets_that_are_not_clusters_degrade_to_ss_and_flag_rebuild() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("clip.mkv");
        make_mapped_fixture(&src, 12);
        let mut map = keyframe_map(MapContainerKind::Matroska, &src, matroska_entries(&src));
        for entry in &mut map.entries {
            entry.byte_offset += 18;
        }

        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264", None, None).unwrap();
        let id = reg
            .start(
                1,
                &src,
                6000,
                MAPPED_FIXTURE_MS,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                Some(map),
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        {
            let sessions = reg.sessions.lock().unwrap();
            assert!(sessions.get(&id).unwrap().map_binding.bound.is_none());
        }
        assert!(reg.map_fallback(&id), "bind refusal must enqueue a rebuild");
        let _ = wait_land_near(&reg, &id, 6000);
    }

    /// One probed item stamped with `content_id` and no map — the item store
    /// state after an index+probe pass under ADR-0023 §9 (the map builds on
    /// demand, never at scan).
    fn map_item_db(dir: &Path, content_id: &str) -> Arc<Db> {
        let db = Arc::new(nightjar_db::open(dir).unwrap());
        db.with_conn(|conn| {
            conn.execute_batch(&format!(
                "INSERT INTO libraries (name, path, kind) VALUES ('t', '/tmp/t', 'movies');
                 INSERT INTO media_items (
                    library_id, path, mtime_ms, size_bytes, title, kind, probe_status, content_id
                 ) VALUES (1, '/tmp/t/a.mkv', 1, 2, 'A', 'movie', 'probed', '{content_id}');"
            ))
            .map_err(|e| e.to_string())?;
            Ok(())
        })
        .unwrap();
        db
    }

    /// ADR-0023 §9.2/§9.3: a play-from-start session never waits on the map.
    /// A position-zero session on an unmapped item must not consult the
    /// map-build-in-flight state at all — the Matroska path opens the real
    /// file with no `-ss`, so session start latency is unaffected by build
    /// state. (The build may be queued by playbackInfo; start just does not
    /// look at it.)
    #[test]
    fn position_zero_session_start_never_consults_map_build_state() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("clip.mkv");
        make_mapped_fixture(&src, 12);
        let db = map_item_db(dir.path(), &nightjar_db::content_id_for_path(&src).unwrap());

        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264", None, Some(db))
                .unwrap();
        let consulted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&consulted);
        reg.set_map_build_in_flight(Some(Arc::new(move |_item_id: i64| {
            flag.store(true, Ordering::SeqCst);
            true
        })));

        let id = reg
            .start(
                1,
                &src,
                0,
                MAPPED_FIXTURE_MS,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                None,
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        assert!(
            !consulted.load(Ordering::SeqCst),
            "position-zero start must not consult the map build state"
        );
        {
            let sessions = reg.sessions.lock().unwrap();
            let session = sessions.get(&id).unwrap();
            assert_eq!(session.play_start_ms, 0);
            assert!(
                session.map_binding.bound.is_none(),
                "real file at zero; no virtual file"
            );
        }
        reg.stop(&id);
    }

    /// ADR-0023 §9.3: a seek that arrives before the keyframe map is ready
    /// waits, bounded, for the in-flight build to land, then starts from the
    /// fresh map instead of paying the §8 `-ss` cost. The session starts late
    /// and seeks back so the cooking run does not cover the new land (a
    /// covered seek would be served from the segment map, not this path).
    #[test]
    fn seek_waits_for_in_flight_map_build_then_lands_mapped() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("clip.mkv");
        make_mapped_fixture(&src, 12);
        let content_id = nightjar_db::content_id_for_path(&src).unwrap();
        let db = map_item_db(dir.path(), &content_id);
        let entries: Vec<(i64, i64)> = matroska_entries(&src)
            .iter()
            .map(|e| {
                (
                    i64::try_from(e.pts_ms).unwrap(),
                    i64::try_from(e.byte_offset).unwrap(),
                )
            })
            .collect();
        let land_ms = matroska_entries(&src)
            .iter()
            .rev()
            .find(|e| e.pts_ms <= 2000)
            .map(|e| e.pts_ms)
            .expect("land at or before the seek point");

        let reg = HlsSessionRegistry::with_cap(
            dir.path().join("hls"),
            3,
            "libx264",
            None,
            Some(db.clone()),
        )
        .unwrap();
        // The predicate is what a seek consults to decide the wait is worth
        // it; it also tells the build thread the seek has started asking.
        let build_go = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let build_go_in_pred = Arc::clone(&build_go);
        let consulted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let consulted_in_pred = Arc::clone(&consulted);
        reg.set_map_build_in_flight(Some(Arc::new(move |_item_id: i64| {
            consulted_in_pred.store(true, Ordering::SeqCst);
            build_go_in_pred.store(true, Ordering::SeqCst);
            true
        })));
        let build_db = Arc::clone(&db);
        let build_entries = entries.clone();
        std::thread::spawn(move || {
            while !build_go.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            // Land inside the §9.3 bound, the way a real index read does.
            std::thread::sleep(Duration::from_millis(250));
            build_db
                .replace_keyframe_map(1, &content_id, "matroska", &build_entries, None)
                .unwrap();
        });

        let id = reg
            .start(
                1,
                &src,
                8000,
                MAPPED_FIXTURE_MS,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                None,
                plan_25fps(),
                None,
            )
            .unwrap();
        wait_playlist(&reg, &id);
        std::thread::sleep(RESTART_MIN_INTERVAL);
        let view = reg.seek(&id, 2000).expect("seek");
        assert_ne!(view.run_id, 0, "fresh run after seek");
        assert!(
            consulted.load(Ordering::SeqCst),
            "seek must consult the build-in-flight state"
        );
        {
            let sessions = reg.sessions.lock().unwrap();
            let session = sessions.get(&id).unwrap();
            assert!(
                session.map_binding.bound.is_some(),
                "seek must use the map the bounded wait produced"
            );
            assert_eq!(
                session.start_ms,
                grid_start(land_ms),
                "the splice opens at the land Cluster and the output starts on \
                 the shared grid above it, not on an -ss lead-in"
            );
        }
        assert!(!reg.map_fallback(&id), "map landed; no §8 fallback");
        reg.stop(&id);
    }

    /// ADR-0023 §9.3/§9.4: the wait is bounded. A build that never lands
    /// (the damaged-index class) must fall through to the §8 `-ss` plan after
    /// the ~1.5 s bound — not hang, and not fall through early. The session
    /// starts late and seeks back so the seek must respawn rather than be
    /// served from the segment map.
    #[test]
    fn seek_wait_is_bounded_when_build_never_lands() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("clip.mkv");
        make_mapped_fixture(&src, 12);
        let db = map_item_db(dir.path(), &nightjar_db::content_id_for_path(&src).unwrap());

        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264", None, Some(db))
                .unwrap();
        reg.set_map_build_in_flight(Some(Arc::new(|_item_id: i64| true)));

        let id = reg
            .start(
                1,
                &src,
                8000,
                MAPPED_FIXTURE_MS,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                None,
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        wait_playlist(&reg, &id);
        std::thread::sleep(RESTART_MIN_INTERVAL);
        let before = Instant::now();
        let view = reg.seek(&id, 2000).expect("seek");
        let waited = before.elapsed();
        assert_ne!(view.run_id, 0, "fresh run after seek");
        assert!(
            waited >= Duration::from_millis(1200),
            "seek must wait the bound for the in-flight build: {waited:?}"
        );
        assert!(
            waited < Duration::from_secs(5),
            "wait must be bounded, not hang: {waited:?}"
        );
        {
            let sessions = reg.sessions.lock().unwrap();
            let session = sessions.get(&id).unwrap();
            assert!(
                session.map_binding.bound.is_none(),
                "no map landed; the run must be a plain -ss start"
            );
            assert_eq!(session.start_ms, 2000, "-ss respawn at the seek point");
        }
        reg.stop(&id);
    }

    /// ADR-0023 §9.3: when no map build is in flight there is nothing to
    /// wait for. A never-built item's seek falls through to the §8 `-ss`
    /// plan immediately — the bounded wait is only worth paying while a
    /// build the consumer already triggered is actually running. The session
    /// starts late and seeks back so the seek respawns rather than being
    /// served from the segment map.
    #[test]
    fn seek_with_no_build_in_flight_falls_to_ss_without_waiting() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("clip.mkv");
        make_mapped_fixture(&src, 12);
        let db = map_item_db(dir.path(), &nightjar_db::content_id_for_path(&src).unwrap());

        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264", None, Some(db))
                .unwrap();
        reg.set_map_build_in_flight(Some(Arc::new(|_item_id: i64| false)));

        let id = reg
            .start(
                1,
                &src,
                8000,
                MAPPED_FIXTURE_MS,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                None,
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        wait_playlist(&reg, &id);
        std::thread::sleep(RESTART_MIN_INTERVAL);
        let before = Instant::now();
        let view = reg.seek(&id, 2000).expect("seek");
        let waited = before.elapsed();
        assert_ne!(view.run_id, 0, "fresh run after seek");
        assert!(
            waited < Duration::from_millis(1200),
            "no build in flight: seek must not wait: {waited:?}"
        );
        {
            let sessions = reg.sessions.lock().unwrap();
            let session = sessions.get(&id).unwrap();
            assert!(
                session.map_binding.bound.is_none(),
                "no map and no build: plain -ss start"
            );
            assert_eq!(session.start_ms, 2000, "-ss respawn at the seek point");
            assert!(!session.primed, "a respawn, not a segment-map serve");
        }
        reg.stop(&id);
    }

    /// ADR-0023 §9.3: the bounded wait is not only for seeks. A session
    /// create at a mid-title position whose keyframe map has not been built
    /// waits, bounded, for the build a consumer already triggered, then
    /// starts from the fresh map instead of paying the §8 `-ss` cost. This
    /// is `seek_waits_for_in_flight_map_build_then_lands_mapped` applied to
    /// `start()` directly, without the initial zero-start run.
    #[test]
    fn start_waits_for_in_flight_map_build_then_lands_mapped() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("clip.mkv");
        make_mapped_fixture(&src, 12);
        let content_id = nightjar_db::content_id_for_path(&src).unwrap();
        let db = map_item_db(dir.path(), &content_id);
        let entries: Vec<(i64, i64)> = matroska_entries(&src)
            .iter()
            .map(|e| {
                (
                    i64::try_from(e.pts_ms).unwrap(),
                    i64::try_from(e.byte_offset).unwrap(),
                )
            })
            .collect();
        let land_ms = matroska_entries(&src)
            .iter()
            .rev()
            .find(|e| e.pts_ms <= 2000)
            .map(|e| e.pts_ms)
            .expect("land at or before the start point");

        let reg = HlsSessionRegistry::with_cap(
            dir.path().join("hls"),
            3,
            "libx264",
            None,
            Some(db.clone()),
        )
        .unwrap();
        // The predicate is what start consults to decide the wait is worth
        // it; it also tells the build thread the session is asking.
        let build_go = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let build_go_in_pred = Arc::clone(&build_go);
        let consulted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let consulted_in_pred = Arc::clone(&consulted);
        reg.set_map_build_in_flight(Some(Arc::new(move |_item_id: i64| {
            consulted_in_pred.store(true, Ordering::SeqCst);
            build_go_in_pred.store(true, Ordering::SeqCst);
            true
        })));
        let build_db = Arc::clone(&db);
        let build_entries = entries.clone();
        std::thread::spawn(move || {
            while !build_go.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            // Land inside the §9.3 bound, the way a real index read does.
            std::thread::sleep(Duration::from_millis(250));
            build_db
                .replace_keyframe_map(1, &content_id, "matroska", &build_entries, None)
                .unwrap();
        });

        let id = reg
            .start(
                1,
                &src,
                2000,
                MAPPED_FIXTURE_MS,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                None,
                plan_25fps(),
                None,
            )
            .unwrap();
        assert!(
            consulted.load(Ordering::SeqCst),
            "mid-title start must consult the build-in-flight state"
        );
        {
            let sessions = reg.sessions.lock().unwrap();
            let session = sessions.get(&id).unwrap();
            assert!(
                session.map_binding.bound.is_some(),
                "start must use the map the bounded wait produced"
            );
            assert_eq!(
                session.start_ms,
                grid_start(land_ms),
                "the splice opens at the land Cluster and the output starts on \
                 the shared grid above it, not on an -ss lead-in"
            );
        }
        assert!(!reg.map_fallback(&id), "map landed; no §8 fallback");
        reg.stop(&id);
    }

    /// ADR-0023 §9.3/§9.4: the create-path wait is bounded too. A build that
    /// never lands (the damaged-index class) must fall through to the §8
    /// `-ss` plan after the ~1.5 s bound — not hang, and not fall through
    /// early. This is `seek_wait_is_bounded_when_build_never_lands` applied
    /// to `start()` directly.
    #[test]
    fn start_wait_is_bounded_when_build_never_lands() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("clip.mkv");
        make_mapped_fixture(&src, 12);
        let db = map_item_db(dir.path(), &nightjar_db::content_id_for_path(&src).unwrap());

        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264", None, Some(db))
                .unwrap();
        reg.set_map_build_in_flight(Some(Arc::new(|_item_id: i64| true)));

        let before = Instant::now();
        let id = reg
            .start(
                1,
                &src,
                2000,
                MAPPED_FIXTURE_MS,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                None,
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        let waited = before.elapsed();
        assert!(
            waited >= Duration::from_millis(1200),
            "start must wait the bound for the in-flight build: {waited:?}"
        );
        assert!(
            waited < Duration::from_secs(5),
            "start wait must be bounded, not hang: {waited:?}"
        );
        {
            let sessions = reg.sessions.lock().unwrap();
            let session = sessions.get(&id).unwrap();
            assert!(
                session.map_binding.bound.is_none(),
                "no map landed; the run must be a plain -ss start"
            );
            assert_eq!(session.start_ms, 2000, "-ss start at the seek point");
        }
        reg.stop(&id);
    }

    /// ADR-0023 §9.2: a position-zero session create never waits on the map.
    /// When the build-in-flight predicate says a build is running, `start()`
    /// at `start_ms: 0` still returns near-instantly (no `MAP_BUILD_WAIT`
    /// delay) and lands `-ss` — the Matroska path opens the real file there.
    /// This is `seek_with_no_build_in_flight_falls_to_ss_without_waiting`'s
    /// sibling for the create path.
    #[test]
    fn position_zero_start_does_not_wait_for_map_build() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("clip.mkv");
        make_mapped_fixture(&src, 12);
        let db = map_item_db(dir.path(), &nightjar_db::content_id_for_path(&src).unwrap());

        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264", None, Some(db))
                .unwrap();
        // A build is running: if start waited, this call would block for the
        // whole bound.
        reg.set_map_build_in_flight(Some(Arc::new(|_item_id: i64| true)));

        let before = Instant::now();
        let id = reg
            .start(
                1,
                &src,
                0,
                MAPPED_FIXTURE_MS,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                None,
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        let waited = before.elapsed();
        assert!(
            waited < Duration::from_millis(1200),
            "position-zero start must not wait on the build: {waited:?}"
        );
        {
            let sessions = reg.sessions.lock().unwrap();
            let session = sessions.get(&id).unwrap();
            assert_eq!(session.play_start_ms, 0);
            assert_eq!(session.start_ms, 0, "-ss start at the play point");
            assert!(
                session.map_binding.bound.is_none(),
                "real file at zero; no virtual file"
            );
        }
        reg.stop(&id);
    }

    /// ADR-0023 §9.4 / §8: a genuinely failed map build (`map_status =
    /// 'error'`, the end state of a demand-triggered build on a damaged
    /// index) falls through to today's `-ss` start and the session still
    /// plays. Confirms the §8 fallback holds through the new on-demand
    /// trigger path.
    #[test]
    fn failed_map_build_falls_through_to_ss_and_plays() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("clip.mkv");
        make_mapped_fixture(&src, 12);
        let db = map_item_db(dir.path(), &nightjar_db::content_id_for_path(&src).unwrap());
        db.set_map_status(1, "error").unwrap();

        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "libx264", None, Some(db))
                .unwrap();
        let id = reg
            .start(
                1,
                &src,
                6000,
                MAPPED_FIXTURE_MS,
                SessionMode::Transcode,
                stereo(),
                vec![],
                None,
                None,
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        {
            let sessions = reg.sessions.lock().unwrap();
            let session = sessions.get(&id).unwrap();
            assert!(
                session.map_binding.bound.is_none(),
                "a failed map must not serve byte offsets"
            );
            assert_eq!(
                session.start_ms, 6000,
                "-ss start at the request on the real file"
            );
        }
        let _ = wait_land_near(&reg, &id, 6000);
        reg.stop(&id);
    }

    /// The naive MP4 splice stamped honest sidx and broke AAC on real
    /// interleaved titles. Synthetic fixtures do not reproduce that pattern:
    /// this path must run against a household end-moov file when the NAS is
    /// mounted. Skips cleanly in CI.
    ///
    /// # Run this once, first, or not at all
    ///
    /// **Its result depends on run order, so consecutive runs are not
    /// independent samples and a failure count over a loop of them measures the
    /// mount rather than the code.**
    ///
    /// It reads a 727 MB file over an SMB mount and probes it with `ffmpeg`,
    /// then asserts where the video lands:
    ///
    /// ```text
    /// video starts at 63.646s, land was 58.975s
    /// ```
    ///
    /// Measured 2026-08-23, eight A/B pairs across two trees, each pair run
    /// back to back, with the order reversed for the second four:
    ///
    /// | | ran first | ran second |
    /// |---|---|---|
    /// | tree A | 2/4 failed | 2/4 failed |
    /// | tree B | 1/4 failed | 4/4 failed |
    /// | **either tree** | **3/8** | **6/8** |
    ///
    /// **By tree the two are 4/8 and 5/8 — indistinguishable. By position, 3/8
    /// against 6/8.** Tree B looked like a regression at 4/4 purely because it
    /// had been placed second every time; moving it to the front took it to 1/4.
    /// The preceding run leaves the mount in a state the next one inherits.
    ///
    /// **This has now been misread as a regression four times**, in four
    /// different sessions, each in a different way, and every reader was
    /// looking at this test rather than at anyone's notes — which is why the
    /// finding is written here.
    ///
    /// The fourth (2026-08-24) is worth its own line, because the note above
    /// did not stop it. The reader saw the failure print the same numbers on
    /// every run, took a constant value as evidence of determinism, and
    /// bisected: pass on the parent commit, fail on the child, therefore the
    /// child caused it. Then the parent failed too, on a repeat.
    ///
    /// **The constant value is this test's signature, not a fingerprint of a
    /// cause.** It is quoted verbatim eight lines above. Two stable outcomes
    /// is what an order-dependent mount produces, and a failure that always
    /// reads the same is indistinguishable by symptom from a regression.
    ///
    /// **Only a same-commit repeat separates the two.** A bisect changes
    /// commits between samples, so it cannot: one sample per commit will
    /// happily draw a clean line through noise. Run the suspect commit twice
    /// before running any other.
    ///
    /// A skip is honest and a pass is honest. A failure means *this test ran
    /// second, or the NAS was slow*, until an order-controlled A/B says
    /// otherwise. Before attributing one to a change, check that the change can
    /// reach this crate at all: it depends only on `nightjar-core` and
    /// `nightjar-db`, and only on `VideoEncodePlan`, `content_id_for_path`, `Db`
    /// and `open` from them.
    #[test]
    fn mapped_real_library_end_moov_mp4_copy_keeps_aac() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let src = Path::new(
            "/Volumes/media/TV Shows/Greys Anatomy/Season 6/\
             Grey's Anatomy - 6x14 - Valentine's Day Massacre - WEBRip-1080p.mp4",
        );
        if !src.is_file() {
            eprintln!(
                "skipping: dogfood end-moov MP4 not mounted at {}",
                src.display()
            );
            return;
        }

        let duration_ms = probe_duration_ms(src);
        assert!(duration_ms > 120_000, "unexpected short dogfood title");
        // Keyframes around the mid land only — a full packet walk of a
        // 40-minute WEBRip is not what this assertion needs.
        let entries = mp4_entries_near(src, 60.0, 15.0);
        let map = keyframe_map(MapContainerKind::Mp4, src, entries);
        let request_ms = 60_000;
        let land_ms = map.entry_at_or_before(request_ms).unwrap().pts_ms;

        let dir = tempfile::tempdir().unwrap();
        let reg =
            HlsSessionRegistry::with_cap(dir.path().join("hls"), 3, "copy", None, None).unwrap();
        let id = reg
            .start(
                1,
                src,
                request_ms,
                duration_ms,
                SessionMode::Copy,
                stereo(),
                vec![],
                None,
                Some(map),
                VideoEncodePlan::default(),
                None,
            )
            .unwrap();
        {
            let sessions = reg.sessions.lock().unwrap();
            let session = sessions.get(&id).unwrap();
            assert!(
                session.map_binding.bound.is_some(),
                "real end-moov must go through virtual faststart"
            );
            assert!(!session.map_binding.fell_back);
            assert_eq!(session.start_ms, land_ms);
        }

        let joined = wait_land_and_join_window(&reg, &id, dir.path(), 8);
        assert_eq!(probe_entry(&joined, "a:0", "codec_name"), "aac");
        assert_audio_decodes(&joined);
        let video_start = probe_secs(&joined, "v:0", "start_time");
        let audio_start = probe_secs(&joined, "a:0", "start_time");
        let land = land_ms as f64 / 1000.0;
        // Copy `-ss` may still open on the previous source IDR when the map
        // PTS and the demuxer disagree by a frame; keep the land within one
        // segment of the snap, not frame-exact.
        assert!(
            (video_start - land).abs() < 2.0,
            "video starts at {video_start}s, land was {land}s"
        );
        // Copy residual: elst / priming (~83 ms) is in-family; keep room for
        // real interleaving without letting a broken splice pass.
        assert!(
            (video_start - audio_start).abs() < 0.2,
            "A/V offset {:.3}s on real end-moov copy (video {video_start}s, audio {audio_start}s)",
            video_start - audio_start
        );
    }

    fn probe_duration_ms(path: &Path) -> u64 {
        let out = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "csv=p=0",
            ])
            .arg(path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "ffprobe duration for {}",
            path.display()
        );
        let secs: f64 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("duration for {}", path.display()));
        (secs * 1000.0) as u64
    }

    /// Sync samples near `center_s` (±`window_s`) via ffprobe read intervals.
    fn mp4_entries_near(src: &Path, center_s: f64, window_s: f64) -> Vec<KeyframeEntry> {
        let start = (center_s - window_s).max(0.0);
        let end = center_s + window_s;
        let out = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "packet=pts_time,pos,flags",
                "-of",
                "csv=p=0",
                "-read_intervals",
                &format!("{start}%{end}"),
            ])
            .arg(src)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "ffprobe near-land packets for {}",
            src.display()
        );
        let mut entries = Vec::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let mut parts = line.split(',');
            let (Some(pts), Some(pos), Some(flags)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if !flags.contains('K') {
                continue;
            }
            let Ok(pts_s) = pts.parse::<f64>() else {
                continue;
            };
            let Ok(byte_offset) = pos.parse::<u64>() else {
                continue;
            };
            entries.push(KeyframeEntry {
                pts_ms: (pts_s * 1000.0) as u64,
                byte_offset,
            });
        }
        assert!(
            entries.len() > 1,
            "need keyframes around {center_s}s on {}",
            src.display()
        );
        entries
    }

    /// Wait for the land segment, then join enough of the producer run to
    /// decode audio — without cooking a full episode to EOF on the assertion.
    fn wait_land_and_join_window(
        reg: &HlsSessionRegistry,
        id: &str,
        out: &Path,
        window_secs: u32,
    ) -> PathBuf {
        let land_ms = {
            let sessions = reg.sessions.lock().unwrap();
            sessions.get(id).unwrap().landed_ms
        };
        let (name, _) = wait_land_near(reg, id, land_ms);
        let _ = wait_asset(reg, id, &name);
        let deadline = Instant::now() + Duration::from_secs(90);
        let run_index = loop {
            let (run_id, dir, failed) = {
                let sessions = reg.sessions.lock().unwrap();
                let session = sessions.get(id).expect("session live");
                (
                    session.encoder_state(SINGLE_VIDEO_RUNG).current_run_id,
                    session.dir.clone(),
                    session.failed.clone(),
                )
            };
            if let Some(err) = failed {
                panic!("session failed: {err}");
            }
            let _ = reg.playlist(id);
            let index = run_path(&dir, SINGLE_VIDEO_RUNG, run_id).join("index.m3u8");
            let text = fs::read_to_string(&index).unwrap_or_default();
            let listed_secs: f64 = text
                .lines()
                .filter_map(|l| l.strip_prefix("#EXTINF:"))
                .filter_map(|l| l.trim().trim_end_matches(',').parse::<f64>().ok())
                .sum();
            if listed_secs >= f64::from(window_secs) || text.contains("#EXT-X-ENDLIST") {
                break index;
            }
            if Instant::now() >= deadline {
                panic!(
                    "producer never listed {window_secs}s; playlist:\n{text}\nfailed={failed:?}"
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let joined = out.join("joined_window.mp4");
        // Join the producer playlist with copyts so title-absolute lands
        // survive. Do not `-t` from zero — that yields an empty file when the
        // land is a minute in.
        let err = out.join("join.err");
        let status = Command::new("ffmpeg")
            .args(["-y", "-hide_banner", "-loglevel", "error", "-copyts", "-i"])
            .arg(&run_index)
            .args(["-c", "copy"])
            .arg(&joined)
            .stderr(std::fs::File::create(&err).unwrap())
            .status()
            .unwrap();
        let err_text = fs::read_to_string(&err).unwrap_or_default();
        assert!(
            status.success(),
            "join window {}: {err_text}",
            run_index.display()
        );
        let meta = fs::metadata(&joined).unwrap();
        assert!(
            meta.len() > 10_000,
            "joined window too small ({} bytes): {err_text}",
            meta.len()
        );
        joined
    }
}
