use crate::error::{ApiError, ApiResult};
use crate::routes::items::decide;
use crate::state::AppState;
use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    response::Response,
};
use nightjar_core::PlaybackMethod;
use nightjar_transcode::{PlaylistError, StartSessionError};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscodeSessionDto {
    pub session_id: String,
    pub item_id: i64,
    pub playlist_url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistQuery {
    pub start_ms: Option<u64>,
}

pub async fn start(
    State(state): State<AppState>,
    Path(item_id): Path<i64>,
) -> ApiResult<(StatusCode, Json<TranscodeSessionDto>)> {
    let row = state
        .db
        .get_item(item_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("item {item_id} not found")))?;
    let decision = decide(&row);
    if decision.method != PlaybackMethod::Transcode {
        return Err(ApiError {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            message: format!("item {item_id} does not use transcode: {}", decision.reason),
        });
    }
    if row.probe_status != "probed" {
        return Err(ApiError {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            message: format!(
                "item {item_id} is not ready to transcode: {}",
                decision.reason
            ),
        });
    }

    let hls = Arc::clone(&state.hls);
    let src = std::path::PathBuf::from(&row.path);
    let started = tokio::task::spawn_blocking(move || hls.start(item_id, &src))
        .await
        .map_err(|e| ApiError::internal(format!("hls start task: {e}")))?;

    match started {
        Ok(session_id) => Ok((
            StatusCode::ACCEPTED,
            Json(TranscodeSessionDto {
                playlist_url: format!("/api/v0/sessions/{session_id}/index.m3u8"),
                session_id,
                item_id,
            }),
        )),
        Err(StartSessionError::CapFull) => Err(ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "all transcode sessions are in use; retry shortly".into(),
        }),
        Err(StartSessionError::Spawn(e)) => Err(ApiError::internal(e)),
    }
}

pub async fn playlist(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<PlaylistQuery>,
) -> ApiResult<Response> {
    let hls = Arc::clone(&state.hls);
    let start_ms = query.start_ms.unwrap_or(0);
    let sid = session_id.clone();
    let result = tokio::task::spawn_blocking(move || {
        // After a seek restart the playlist is briefly absent; wait up to 2s
        // so clients (and Gate 2's 3s seek budget) see a 200 rather than a
        // thrash of 404s.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match hls.playlist(&sid, start_ms) {
                Err(PlaylistError::NotReady) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                other => return other,
            }
        }
    })
    .await
    .map_err(|e| ApiError::internal(format!("hls playlist task: {e}")))?;

    match result {
        Ok(bytes) => {
            let mut res = Response::new(Body::from(bytes));
            *res.status_mut() = StatusCode::OK;
            res.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/vnd.apple.mpegurl"),
            );
            res.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            Ok(res)
        }
        Err(PlaylistError::NotFound) => Err(ApiError::not_found(format!(
            "session {session_id} not found"
        ))),
        Err(PlaylistError::NotReady) => Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: format!("playlist for session {session_id} not ready yet"),
        }),
        Err(PlaylistError::Failed(e)) => Err(ApiError::internal(format!(
            "session {session_id} failed: {e}"
        ))),
    }
}

pub async fn asset(
    State(state): State<AppState>,
    Path((session_id, asset)): Path<(String, String)>,
) -> ApiResult<Response> {
    let hls = Arc::clone(&state.hls);
    let sid = session_id.clone();
    let name = asset.clone();
    let result = tokio::task::spawn_blocking(move || hls.asset(&sid, &name))
        .await
        .map_err(|e| ApiError::internal(format!("hls asset task: {e}")))?;

    match result {
        Ok(bytes) => {
            let mime = if asset.ends_with(".mp4") {
                "video/mp4"
            } else {
                "video/iso.segment"
            };
            let mut res = Response::new(Body::from(bytes));
            *res.status_mut() = StatusCode::OK;
            res.headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
            Ok(res)
        }
        Err(PlaylistError::NotFound) => Err(ApiError::not_found(format!(
            "asset {asset} for session {session_id} not found"
        ))),
        Err(PlaylistError::NotReady) => Err(ApiError::not_found(format!(
            "asset {asset} for session {session_id} not ready"
        ))),
        Err(PlaylistError::Failed(e)) => Err(ApiError::internal(e)),
    }
}

pub async fn delete(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> ApiResult<StatusCode> {
    let hls = Arc::clone(&state.hls);
    let sid = session_id.clone();
    let stopped = tokio::task::spawn_blocking(move || hls.stop(&sid))
        .await
        .map_err(|e| ApiError::internal(format!("hls stop task: {e}")))?;
    if stopped {
        Ok(StatusCode::NO_CONTENT)
    } else {
        // Idempotent teardown: already gone is fine for player unmount.
        Ok(StatusCode::NO_CONTENT)
    }
}
