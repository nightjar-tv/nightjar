use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::{
    Json,
    extract::{Path, State},
};
use nightjar_core::decide_direct_play;
use nightjar_db::MediaItemRow;
use serde::Serialize;

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
    pub direct_play: bool,
    pub needs_transcode: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackInfoDto {
    pub item_id: i64,
    pub direct_play: bool,
    pub needs_transcode: bool,
    pub reason: String,
    pub stream_url: String,
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
    let decision = decide_direct_play(
        &row.path,
        row.container.as_deref(),
        row.video_codec.as_deref(),
        row.audio_codec.as_deref(),
        row.scan_error.as_deref(),
        &row.probe_status,
    );
    Ok(Json(PlaybackInfoDto {
        item_id: row.id,
        direct_play: decision.direct_play,
        needs_transcode: decision.needs_transcode,
        reason: decision.reason,
        stream_url: format!("/api/v0/items/{item_id}/stream"),
        mime_type: decision.mime_type,
        duration_ms: row.duration_ms,
        container: row.container,
        video_codec: row.video_codec,
        audio_codec: row.audio_codec,
    }))
}

pub fn to_dto(row: MediaItemRow) -> MediaItemDto {
    let decision = decide_direct_play(
        &row.path,
        row.container.as_deref(),
        row.video_codec.as_deref(),
        row.audio_codec.as_deref(),
        row.scan_error.as_deref(),
        &row.probe_status,
    );
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
        direct_play: decision.direct_play,
        needs_transcode: decision.needs_transcode,
    }
}
