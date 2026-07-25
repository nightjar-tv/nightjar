use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use nightjar_core::{BROWSER_V0, PlaybackDecision, PlaybackMethod, decide_playback};
use nightjar_db::MediaItemRow;
use nightjar_transcode::{RemuxKey, RemuxState};
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
pub struct PlaybackInfoDto {
    pub item_id: i64,
    pub playback_method: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remux_state: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remux_error: Option<String>,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_url: Option<String>,
    pub mime_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_codec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_codec: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemuxAcceptedDto {
    pub item_id: i64,
    pub remux_state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
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

    let stream_url = format!("/api/v0/items/{item_id}/stream");
    let (remux_state, remux_error, stream_url) = match decision.method {
        PlaybackMethod::DirectPlay => (None, None, Some(stream_url)),
        PlaybackMethod::Transcode => (None, None, None),
        PlaybackMethod::Remux => {
            let registry = Arc::clone(&state.remux);
            let key = remux_key(&row);
            let status = tokio::task::spawn_blocking(move || registry.status(&key))
                .await
                .map_err(|e| ApiError::internal(format!("remux status task: {e}")))?;
            match status {
                RemuxState::Ready => (Some("ready"), None, Some(stream_url)),
                RemuxState::Preparing => (Some("preparing"), None, None),
                RemuxState::NotStarted { reason } => (Some("notStarted"), reason, None),
                RemuxState::Failed(e) => (Some("failed"), Some(e), None),
            }
        }
    };

    Ok(Json(PlaybackInfoDto {
        item_id: row.id,
        playback_method: decision.method.as_str(),
        remux_state,
        remux_error,
        reason: decision.reason,
        stream_url,
        mime_type: decision.mime_type,
        duration_ms: row.duration_ms,
        container: row.container,
        video_codec: row.video_codec,
        audio_codec: row.audio_codec,
    }))
}

pub async fn start_remux(
    State(state): State<AppState>,
    Path(item_id): Path<i64>,
) -> ApiResult<(StatusCode, Json<RemuxAcceptedDto>)> {
    let row = state
        .db
        .get_item(item_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("item {item_id} not found")))?;
    let decision = decide(&row);
    if decision.method != PlaybackMethod::Remux {
        return Err(ApiError {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            message: format!("item {item_id} does not use remux: {}", decision.reason),
        });
    }

    let registry = Arc::clone(&state.remux);
    let key = remux_key(&row);
    let src = std::path::PathBuf::from(&row.path);
    let started = tokio::task::spawn_blocking(move || registry.start(&key, &src))
        .await
        .map_err(|e| ApiError::internal(format!("remux start task: {e}")))?;

    let (remux_state, reason) = match started {
        RemuxState::Ready => ("ready", None),
        RemuxState::Preparing => ("preparing", None),
        RemuxState::NotStarted { reason } => ("notStarted", reason),
        RemuxState::Failed(e) => ("failed", Some(e)),
    };
    Ok((
        StatusCode::ACCEPTED,
        Json(RemuxAcceptedDto {
            item_id: row.id,
            remux_state,
            reason,
        }),
    ))
}

pub fn decide(row: &MediaItemRow) -> PlaybackDecision {
    decide_playback(
        &row.path,
        row.container.as_deref(),
        row.video_codec.as_deref(),
        row.audio_codec.as_deref(),
        row.scan_error.as_deref(),
        &row.probe_status,
        &BROWSER_V0,
    )
}

pub fn remux_key(row: &MediaItemRow) -> RemuxKey {
    RemuxKey {
        item_id: row.id,
        mtime_ms: row.mtime_ms,
        size_bytes: row.size_bytes,
    }
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
