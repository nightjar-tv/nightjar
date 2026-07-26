//! FFmpeg orchestration: HLS playback sessions in copy or encode mode
//! (ADR-0007, ADR-0011), hardware encode detection (ADR-0009), and text
//! subtitle WebVTT sidecars (ADR-0010).

mod hls;
mod hwaccel;
mod subs;

pub use hls::{
    EncoderKind, HlsSessionRegistry, HlsSubtitleTrack, PlaylistError, SessionEncoder, SessionMode,
    StartSessionError, WindowAction, decide_window_action,
};
pub use hwaccel::{
    EncoderCandidate, EncoderStatus, TranscodeCapabilities, probe_h264_encoders,
    probe_h264_encoders_arc, select_preferred,
};
pub use subs::{
    DiscoveredSidecar, SubsCache, SubtitleSourceKind, TextSubtitleStream, decode_subtitle_bytes,
    discover_sidecars, ensure_embedded_webvtt, ensure_sidecar_webvtt, is_serveable_sidecar_format,
    is_text_subtitle_codec, list_text_subtitles, normalize_language, srt_to_webvtt,
    warm_embedded_webvtts,
};

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
