use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::{
    Json,
    body::Body,
    extract::{Path, State},
    http::{HeaderValue, StatusCode, header},
    response::Response,
};
use nightjar_core::{BROWSER_V0, PlaybackDecision, PlaybackMethod, decide_playback};
use nightjar_db::{MediaItemRow, SidecarRow};
use nightjar_transcode::{
    ensure_embedded_webvtt, ensure_sidecar_webvtt, is_serveable_sidecar_format, list_audio_tracks,
    list_text_subtitles,
};
use serde::Serialize;
use std::sync::Arc;

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

pub async fn playback_info(
    State(state): State<AppState>,
    Path(item_id): Path<i64>,
) -> ApiResult<Json<PlaybackInfoDto>> {
    let row = state
        .db
        .get_item(item_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("item {item_id} not found")))?;
    let decision = decide(&row);

    // Remux and transcode both play through a session; only direct play is
    // served from the file itself (ADR-0011).
    let (stream_url, sessions_url) = match decision.method {
        PlaybackMethod::DirectPlay => (Some(format!("/api/v0/items/{item_id}/stream")), None),
        PlaybackMethod::Remux | PlaybackMethod::Transcode => {
            (None, Some(format!("/api/v0/items/{item_id}/sessions")))
        }
    };

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

    Ok(Json(PlaybackInfoDto {
        item_id: row.id,
        playback_method: decision.method.as_str(),
        reason: decision.reason,
        stream_url,
        sessions_url,
        mime_type: decision.mime_type,
        duration_ms: row.duration_ms,
        container: row.container,
        video_codec: row.video_codec,
        audio_codec: row.audio_codec,
        audio_tracks,
        subtitle_tracks,
    }))
}

pub async fn subtitle_vtt(
    State(state): State<AppState>,
    Path((item_id, asset)): Path<(i64, String)>,
) -> ApiResult<Response> {
    let track_id = asset
        .strip_suffix(".vtt")
        .filter(|id| is_valid_track_id(id))
        .ok_or_else(|| ApiError::not_found(format!("subtitle asset {asset} not found")))?
        .to_string();
    let row = state
        .db
        .get_item(item_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("item {item_id} not found")))?;

    let cache = Arc::clone(&state.subs);
    let path = if let Some(stream_index) = parse_embedded_track_id(&track_id) {
        let src = std::path::PathBuf::from(&row.path);
        let mtime_ms = file_mtime_ms(&src).unwrap_or(0);
        let size_bytes = row.size_bytes;
        tokio::task::spawn_blocking(move || {
            ensure_embedded_webvtt(&cache, item_id, mtime_ms, size_bytes, &src, stream_index)
        })
        .await
        .map_err(|e| ApiError::internal(format!("subtitle extract task: {e}")))?
        .map_err(ApiError::not_found)?
    } else {
        let sidecar = state
            .db
            .get_item_sidecar(item_id, &track_id)
            .map_err(ApiError::internal)?
            .ok_or_else(|| {
                ApiError::not_found(format!(
                    "subtitle track {track_id} not found for item {item_id}"
                ))
            })?;
        if !is_serveable_sidecar_format(&sidecar.format) {
            return Err(ApiError::not_found(format!(
                "subtitle track {track_id} is not served as WebVTT"
            )));
        }
        let sidecar_path = std::path::PathBuf::from(sidecar.path);
        let format = sidecar.format;
        let mtime_ms = sidecar.mtime_ms;
        let size_bytes = sidecar.size_bytes;
        let track_id = track_id.clone();
        tokio::task::spawn_blocking(move || {
            ensure_sidecar_webvtt(
                &cache,
                item_id,
                &track_id,
                &sidecar_path,
                &format,
                mtime_ms,
                size_bytes,
            )
        })
        .await
        .map_err(|e| ApiError::internal(format!("subtitle convert task: {e}")))?
        .map_err(ApiError::not_found)?
    };

    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| ApiError::internal(format!("read subtitle {}: {e}", path.display())))?;
    let mut res = Response::new(Body::from(bytes));
    *res.status_mut() = StatusCode::OK;
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/vtt; charset=utf-8"),
    );
    res.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=3600"),
    );
    Ok(res)
}

fn is_valid_track_id(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        Some('e') | Some('s') => {
            chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        }
        _ => false,
    }
}

fn parse_embedded_track_id(track_id: &str) -> Option<u32> {
    track_id.strip_prefix('e')?.parse().ok()
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
        let track_id = s.track_id();
        tracks.push(SubtitleTrackDto {
            url: Some(format!(
                "/api/v0/items/{}/subtitles/{}.vtt",
                row.id, track_id
            )),
            track_id,
            source: "embedded",
            codec: s.codec,
            language: s.language,
            label: s.title,
            forced: false,
            sdh: false,
            stream_index: Some(s.stream_index),
        });
    }
    for s in state.db.list_item_sidecars(row.id)? {
        tracks.push(sidecar_to_dto(row.id, &s));
    }
    Ok(tracks)
}

fn sidecar_to_dto(item_id: i64, s: &SidecarRow) -> SubtitleTrackDto {
    let served = is_serveable_sidecar_format(&s.format);
    SubtitleTrackDto {
        url: served.then(|| format!("/api/v0/items/{item_id}/subtitles/{}.vtt", s.track_id)),
        track_id: s.track_id.clone(),
        source: "sidecar",
        codec: s.format.clone(),
        language: s.language.clone(),
        label: None,
        forced: s.forced,
        sdh: s.sdh,
        stream_index: None,
    }
}

fn file_mtime_ms(path: &std::path::Path) -> Option<i64> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let dur = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(dur.as_millis() as i64)
}

pub fn decide(row: &MediaItemRow) -> PlaybackDecision {
    decide_playback(
        &row.path,
        row.container.as_deref(),
        row.video_codec.as_deref(),
        row.audio_codec.as_deref(),
        row.audio_channels.and_then(|c| u32::try_from(c).ok()),
        row.scan_error.as_deref(),
        &row.probe_status,
        &BROWSER_V0,
    )
}

pub fn to_dto(row: MediaItemRow) -> MediaItemDto {
    let decision = decide(&row);
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
        scan_error: row.scan_error,
        playback_method: decision.method.as_str(),
    }
}
