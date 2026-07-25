//! Domain types, filename parsing, and playback helpers.

mod filename;
mod models;
mod playback;

pub use filename::{ParsedName, parse_filename};
pub use models::{LibraryKind, MediaKind};
pub use playback::{
    BROWSER_V0, ClientCapabilityProfile, PlaybackDecision, PlaybackMethod, decide_playback,
    mime_for_path,
};

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
