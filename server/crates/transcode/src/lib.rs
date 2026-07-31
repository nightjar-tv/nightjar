//! FFmpeg orchestration: HLS playback sessions in copy or encode mode
//! (ADR-0007, ADR-0011), hardware encode detection (ADR-0009), text
//! subtitle WebVTT at scan time (ADR-0010 / ADR-0013), audio track
//! selection with stereo downmix (ADR-0012), and ASS/PGS burn-in (ADR-0018).

mod audio;
mod hls;
mod hls_segment_map;
mod hwaccel;
mod subs;

pub use audio::{AudioStream, list_audio_tracks, stereo_downmix_filter};
pub use hls::{
    AudioSelection, EncoderKind, HlsSessionRegistry, HlsSubtitleTrack, PlaylistError,
    SegmentMissAction, SessionEncoder, SessionMode, SessionView, StartSessionError, WindowAction,
    decide_segment_miss, decide_window_action,
};
pub use hwaccel::{
    EncoderCandidate, EncoderStatus, TranscodeCapabilities, probe_h264_encoders,
    probe_h264_encoders_arc, select_preferred,
};
pub use subs::{
    BurnInKind, BurnInSelection, BurnInSubtitleStream, DiscoveredSidecar, ExtractOutcome,
    SessionSubInput, SidecarDirCache, SidecarInput, SubsStore, SubtitleRender, SubtitleSourceKind,
    TextSubtitleStream, TrackReadiness, burn_in_kind_for_codec, decode_subtitle_bytes,
    discover_sidecars, discover_sidecars_cached, extract_embedded_ass, extract_item_subtitles,
    io_error_is_availability, is_burn_in_codec, is_burn_in_sidecar_format,
    is_serveable_sidecar_format, is_text_subtitle_codec, list_burn_in_subtitles,
    list_text_subtitles, normalize_language, prepare_session_subtitles, slice_webvtt,
    srt_to_webvtt, stored_webvtt, webvtt_max_cue_end_ms,
};

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
