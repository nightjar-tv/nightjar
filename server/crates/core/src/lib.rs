//! Domain types, filename parsing, and playback helpers.

mod filename;
mod models;
mod playback;

pub use filename::{ParsedName, parse_filename};
pub use models::{LibraryKind, MediaKind};
pub use playback::{
    AETHER_V0, BROWSER_V0, ClientCapabilityProfile, HdrCapability, MEDIA3_V0, MPV_V0,
    PlaybackDecision, PlaybackMethod, VideoEncodePlan, decide_playback, known_profile,
    method_from_manifest_expect, mime_for_path, resolve_profile, resolve_profile_bag,
    video_encode_plan,
};

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
