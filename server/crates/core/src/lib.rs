//! Domain types, filename parsing, and playback helpers.

mod filename;
mod models;
mod playback;

pub use filename::{ParsedName, parse_filename};
pub use models::{LibraryKind, MediaKind};
pub use playback::{
    BROWSER_V0, ClientCapabilityProfile, HdrCapability, MEDIA3_V0, MPV_V0, PlaybackDecision,
    PlaybackMethod, decide_playback, known_profile, method_from_manifest_expect, mime_for_path,
    resolve_profile,
};

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
