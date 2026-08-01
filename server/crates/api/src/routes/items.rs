use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    response::Response,
};
use nightjar_core::{
    BROWSER_V0, ClientCapabilityProfile, PlaybackDecision, PlaybackMethod, decide_playback,
    known_profile, resolve_profile_bag,
};
use nightjar_db::{MediaItemRow, SidecarRow};
use nightjar_transcode::{
    TrackReadiness, is_burn_in_sidecar_format, is_serveable_sidecar_format, list_audio_tracks,
    list_burn_in_subtitles, list_text_subtitles, stored_webvtt,
};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaItemDto {
    pub id: i64,
    pub library_id: i64,
    pub path: String,
    pub title: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub season: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_codec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_codec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<i32>,
    pub size_bytes: i64,
    pub probe_status: String,
    pub subtitle_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scan_error: Option<String>,
    pub playback_method: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioTrackDto {
    pub track_id: String,
    pub codec: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub channels: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_layout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub default: bool,
    pub stream_index: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleTrackDto {
    pub track_id: String,
    pub source: &'static str,
    pub codec: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub forced: bool,
    pub sdh: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_index: Option<u32>,
    /// soft | burnIn (ADR-0018).
    pub render: &'static str,
    /// Present for serveable text tracks only (ADR-0013 §11).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readiness: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackInfoDto {
    pub item_id: i64,
    pub playback_method: &'static str,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sessions_url: Option<String>,
    pub mime_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_codec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_codec: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub audio_tracks: Vec<AudioTrackDto>,
    pub subtitle_status: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub subtitle_tracks: Vec<SubtitleTrackDto>,
}

pub async fn get(
    State(state): State<AppState>,
    Path(item_id): Path<i64>,
) -> ApiResult<Json<MediaItemDto>> {
    let row = state
        .db
        .get_item(item_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("item {item_id} not found")))?;
    Ok(Json(to_dto(row)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileQuery {
    pub profile_id: Option<String>,
    pub max_bitrate_bps: Option<u64>,
    pub max_height: Option<u32>,
    pub hdr: Option<String>,
}

/// Resolve `profileId` plus optional field-bag ceilings (ADR-0022).
pub fn profile_from_query(
    profile_id: Option<&str>,
    max_bitrate_bps: Option<u64>,
    max_height: Option<u32>,
    hdr: Option<&str>,
) -> ClientCapabilityProfile {
    let unknown = profile_id
        .filter(|s| !s.is_empty())
        .is_some_and(|id| known_profile(id).is_none());
    let bag = max_bitrate_bps.is_some() || max_height.is_some() || hdr.is_some();
    if unknown && !bag {
        tracing::warn!(
            profile_id = profile_id.unwrap_or(""),
            "unknown client profile id with no field bag; using BROWSER_V0"
        );
    } else if unknown && bag {
        tracing::info!(
            profile_id = profile_id.unwrap_or(""),
            "unknown client profile id; applying field bag on BROWSER_V0 codec floor"
        );
    }
    resolve_profile_bag(profile_id, max_bitrate_bps, max_height, hdr)
}

pub async fn playback_info(
    State(state): State<AppState>,
    Path(item_id): Path<i64>,
    Query(query): Query<ProfileQuery>,
) -> ApiResult<Json<PlaybackInfoDto>> {
    let row = state
        .db
        .get_item(item_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("item {item_id} not found")))?;
    let profile = profile_from_query(
        query.profile_id.as_deref(),
        query.max_bitrate_bps,
        query.max_height,
        query.hdr.as_deref(),
    );
    let decision = decide(&row, &profile, state.tonemap_available);
    let subtitle_tracks = subtitle_tracks_for(&state, &row).unwrap_or_else(|e| {
        tracing::warn!(item_id, error = %e, "subtitle list failed");
        Vec::new()
    });
    // Listed the same for every method: the client asks for a track and never
    // reasons about delivery to find one (ADR-0012).
    let audio_tracks = audio_tracks_for(&row).unwrap_or_else(|e| {
        tracing::warn!(item_id, error = %e, "audio track list failed");
        Vec::new()
    });

    // Remux and transcode both play through a session; only direct play is
    // served from the file itself (ADR-0011). sessionsUrl still appears on
    // DirectPlay when a track selection can force encode (ADR-0012 / ADR-0018).
    let max_channels = profile.max_audio_channels.unwrap_or(u32::MAX);
    let selection_may_need_session = subtitle_tracks.iter().any(|t| t.render == "burnIn")
        || audio_tracks.iter().any(|a| a.channels > max_channels);
    let (stream_url, sessions_url) = match decision.method {
        PlaybackMethod::DirectPlay => (
            Some(format!("/api/v0/items/{item_id}/stream")),
            if selection_may_need_session {
                Some(format!("/api/v0/items/{item_id}/sessions"))
            } else {
                None
            },
        ),
        PlaybackMethod::Remux | PlaybackMethod::Transcode => {
            (None, Some(format!("/api/v0/items/{item_id}/sessions")))
        }
    };

    // First-play: bump this title's extract ahead of the background drain so
    // progressive cues can start while the user is watching (ADR-0013 §11).
    if row.subtitle_status == "pending" {
        state
            .pool
            .prioritize_extract(row.id, row.library_id, std::path::PathBuf::from(&row.path));
    }

    Ok(Json(PlaybackInfoDto {
        item_id: row.id,
        playback_method: decision.method.as_str(),
        reason: decision.reason,
        stream_url,
        sessions_url,
        mime_type: decision.mime_type,
        duration_ms: row.duration_ms,
        container: row.container.clone(),
        video_codec: row.video_codec.clone(),
        audio_codec: row.audio_codec.clone(),
        audio_tracks,
        subtitle_status: row.subtitle_status.clone(),
        subtitle_tracks,
    }))
}

pub async fn subtitle_vtt(
    State(state): State<AppState>,
    Path((item_id, asset)): Path<(i64, String)>,
) -> ApiResult<Response> {
    let track_id = asset
        .strip_suffix(".vtt")
        .filter(|id| super::track_ids::is_valid_track_id(id))
        .ok_or_else(|| ApiError::not_found(format!("subtitle asset {asset} not found")))?
        .to_string();
    let row = state
        .db
        .get_item(item_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("item {item_id} not found")))?;

    // ADR-0013: playback never extracts; serve a stored file or 404.
    let path = stored_webvtt(&state.subs, item_id, &track_id).map_err(ApiError::not_found)?;

    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| ApiError::internal(format!("read subtitle {}: {e}", path.display())))?;
    let (readiness, _) = state
        .subs
        .track_readiness(item_id, &track_id, &row.subtitle_status);
    // A growing partial must not be cached: the next GET needs the newer body.
    let cache = match readiness {
        TrackReadiness::Complete if row.subtitle_status == "ready" => "private, max-age=3600",
        _ => "private, no-cache",
    };
    let mut res = Response::new(Body::from(bytes));
    *res.status_mut() = StatusCode::OK;
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/vtt; charset=utf-8"),
    );
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    Ok(res)
}

pub(crate) fn audio_tracks_for(row: &MediaItemRow) -> Result<Vec<AudioTrackDto>, String> {
    let tracks = list_audio_tracks(std::path::Path::new(&row.path))?
        .into_iter()
        .map(|a| AudioTrackDto {
            track_id: a.track_id(),
            codec: a.codec,
            language: a.language,
            channels: a.channels,
            channel_layout: a.channel_layout,
            label: a.title,
            default: a.is_default,
            stream_index: a.stream_index,
        })
        .collect();
    Ok(tracks)
}

pub(crate) fn subtitle_tracks_for(
    state: &AppState,
    row: &MediaItemRow,
) -> Result<Vec<SubtitleTrackDto>, String> {
    let mut tracks = Vec::new();
    let src = std::path::Path::new(&row.path);
    for s in list_text_subtitles(src)? {
        tracks.push(serveable_track_dto(
            state,
            row,
            ServeableTrack {
                track_id: s.track_id(),
                source: "embedded",
                codec: s.codec,
                language: s.language,
                label: s.title,
                forced: false,
                sdh: false,
                stream_index: Some(s.stream_index),
            },
        ));
    }
    for s in list_burn_in_subtitles(src)? {
        tracks.push(burn_in_track_dto(ServeableTrack {
            track_id: s.track_id(),
            source: "embedded",
            codec: s.codec,
            language: s.language,
            label: s.title,
            forced: false,
            sdh: false,
            stream_index: Some(s.stream_index),
        }));
    }
    for s in state.db.list_item_sidecars(row.id)? {
        tracks.push(sidecar_to_dto(state, row, &s));
    }
    Ok(tracks)
}

struct ServeableTrack {
    track_id: String,
    source: &'static str,
    codec: String,
    language: Option<String>,
    label: Option<String>,
    forced: bool,
    sdh: bool,
    stream_index: Option<u32>,
}

/// Readiness-aware DTO for a track the server can actually serve (embedded
/// text or a convertible sidecar). `url` only appears once cues exist.
fn serveable_track_dto(
    state: &AppState,
    row: &MediaItemRow,
    t: ServeableTrack,
) -> SubtitleTrackDto {
    let (readiness, revision) =
        state
            .subs
            .track_readiness(row.id, &t.track_id, &row.subtitle_status);
    let url = match readiness {
        TrackReadiness::Preparing => None,
        TrackReadiness::Partial | TrackReadiness::Complete => Some(format!(
            "/api/v0/items/{}/subtitles/{}.vtt",
            row.id, t.track_id
        )),
    };
    SubtitleTrackDto {
        url,
        readiness: Some(readiness.as_str()),
        revision: Some(revision),
        track_id: t.track_id,
        source: t.source,
        codec: t.codec,
        language: t.language,
        label: t.label,
        forced: t.forced,
        sdh: t.sdh,
        stream_index: t.stream_index,
        render: "soft",
    }
}

fn burn_in_track_dto(t: ServeableTrack) -> SubtitleTrackDto {
    SubtitleTrackDto {
        url: None,
        readiness: None,
        revision: None,
        track_id: t.track_id,
        source: t.source,
        codec: t.codec,
        language: t.language,
        label: t.label,
        forced: t.forced,
        sdh: t.sdh,
        stream_index: t.stream_index,
        render: "burnIn",
    }
}

fn sidecar_to_dto(state: &AppState, row: &MediaItemRow, s: &SidecarRow) -> SubtitleTrackDto {
    if is_serveable_sidecar_format(&s.format) {
        return serveable_track_dto(
            state,
            row,
            ServeableTrack {
                track_id: s.track_id.clone(),
                source: "sidecar",
                codec: s.format.clone(),
                language: s.language.clone(),
                label: None,
                forced: s.forced,
                sdh: s.sdh,
                stream_index: None,
            },
        );
    }
    if is_burn_in_sidecar_format(&s.format) {
        return burn_in_track_dto(ServeableTrack {
            track_id: s.track_id.clone(),
            source: "sidecar",
            codec: s.format.clone(),
            language: s.language.clone(),
            label: None,
            forced: s.forced,
            sdh: s.sdh,
            stream_index: None,
        });
    }
    // Unknown sidecar format: listed without delivery path.
    burn_in_track_dto(ServeableTrack {
        track_id: s.track_id.clone(),
        source: "sidecar",
        codec: s.format.clone(),
        language: s.language.clone(),
        label: None,
        forced: s.forced,
        sdh: s.sdh,
        stream_index: None,
    })
}

pub fn decide(
    row: &MediaItemRow,
    profile: &ClientCapabilityProfile,
    tonemap_available: bool,
) -> PlaybackDecision {
    decide_playback(
        &row.path,
        row.container.as_deref(),
        row.video_codec.as_deref(),
        row.audio_codec.as_deref(),
        row.audio_channels.and_then(|c| u32::try_from(c).ok()),
        row.height.and_then(|h| u32::try_from(h).ok()),
        row.video_bitrate_bps.and_then(|b| u64::try_from(b).ok()),
        row.hdr.as_deref(),
        row.scan_error.as_deref(),
        &row.probe_status,
        profile,
        tonemap_available,
    )
}

pub fn to_dto(row: MediaItemRow) -> MediaItemDto {
    let decision = decide(&row, &BROWSER_V0, true);
    MediaItemDto {
        id: row.id,
        library_id: row.library_id,
        path: row.path,
        title: row.title,
        kind: row.kind,
        year: row.year,
        season: row.season,
        episode: row.episode,
        duration_ms: row.duration_ms,
        container: row.container,
        video_codec: row.video_codec,
        audio_codec: row.audio_codec,
        width: row.width,
        height: row.height,
        size_bytes: row.size_bytes,
        probe_status: row.probe_status,
        subtitle_status: row.subtitle_status,
        scan_error: row.scan_error,
        playback_method: decision.method.as_str(),
    }
}
