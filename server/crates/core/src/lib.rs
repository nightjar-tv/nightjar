//! Domain types, filename parsing, and playback helpers.

mod filename;
mod models;
mod playback;
mod scan_progress;
mod track_select;
mod watch;

mod certification;
mod kids_scope;

pub use certification::{
    CertificationLadder, CertificationTier, LadderError, LadderRung, RegionLadder,
};
pub use filename::{
    FolderContext, MAX_EPISODE_RANGE, ParsedName, leading_episode_number, parse_filename,
    parse_filename_in, parse_with_parent,
};
pub use kids_scope::{
    ItemCertification, KidsDenialReason, KidsScopeDecision, KidsScopeFacts, ViewerScope,
    decide_kids_scope,
};
pub use models::{LibraryKind, MediaKind, Role};
pub use playback::{
    AETHER_V0, AccountPlaybackPolicy, BROWSER_V0, ClientCapabilityProfile, ClientOrigin,
    HdrCapability, MEDIA3_V0, MPV_V0, PlaybackCeilings, PlaybackDecision, PlaybackMethod,
    VideoEncodePlan, classify_client_origin, compose_playback_ceilings, decide_playback,
    is_dolby_vision_profile5, known_profile, method_from_manifest_expect, mime_for_path,
    needs_standalone_subtitle_extract, resolve_profile, resolve_profile_bag, video_encode_plan,
};
pub use scan_progress::{
    IndexPass, PROBE_BAR_MIN_QUEUE_DEPTH, ProgressDisplay, metadata_display, probe_display,
    probe_total,
};
pub use track_select::{
    DEFAULT_PREFERENCE_LANGUAGE, TrackCandidate, TrackSelection, select_audio_for_description,
    select_audio_track, select_subtitle_for_description, select_subtitle_track, title_looks_forced,
    title_looks_sdh,
};
pub use watch::{PLAYED_AT_PERCENT, RESUME_FLOOR_PERCENT, WatchReport, watch_report};

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
