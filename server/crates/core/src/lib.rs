//! Domain types, filename parsing, and playback helpers.

mod filename;
mod models;
mod playback;
mod track_select;

pub use filename::{MAX_EPISODE_RANGE, ParsedName, parse_filename};
pub use models::{LibraryKind, MediaKind};
pub use playback::{
    AETHER_V0, BROWSER_V0, ClientCapabilityProfile, HdrCapability, MEDIA3_V0, MPV_V0,
    PlaybackDecision, PlaybackMethod, VideoEncodePlan, decide_playback, is_dolby_vision_profile5,
    known_profile, method_from_manifest_expect, mime_for_path, resolve_profile,
    resolve_profile_bag, video_encode_plan,
};
pub use track_select::{
    DEFAULT_PREFERENCE_LANGUAGE, TrackCandidate, TrackSelection, select_audio_track,
    select_subtitle_track, title_looks_forced, title_looks_sdh,
};

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
