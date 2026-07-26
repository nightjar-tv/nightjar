use crate::error::{ApiError, ApiResult};
use crate::routes::items::{decide, subtitle_tracks_for};
use crate::state::AppState;
use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    response::Response,
};
use nightjar_core::{BROWSER_V0, PlaybackMethod};
use nightjar_db::MediaItemRow;
use nightjar_transcode::{
    AudioSelection, HlsSubtitleTrack, PlaylistError, SessionMode, StartSessionError,
    list_audio_tracks, warm_embedded_webvtts,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscodeSessionDto {
    pub session_id: String,
    pub item_id: i64,
    pub playlist_url: String,
    pub video_encoder: String,
    pub encoder_kind: &'static str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartQuery {
    pub start_ms: Option<u64>,
    pub audio_track_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistQuery {
    pub start_ms: Option<u64>,
}

#[derive(Clone, Copy)]
enum PlaylistKind {
    Master,
    Media,
}

pub async fn start(
    State(state): State<AppState>,
    Path(item_id): Path<i64>,
    Query(query): Query<StartQuery>,
) -> ApiResult<(StatusCode, Json<TranscodeSessionDto>)> {
    let row = state
        .db
        .get_item(item_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("item {item_id} not found")))?;
    let decision = decide(&row);
    let mode = match decision.method {
        PlaybackMethod::Remux => SessionMode::Copy,
        PlaybackMethod::Transcode => SessionMode::Transcode,
        PlaybackMethod::DirectPlay => {
            return Err(ApiError {
                status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
                message: format!(
                    "item {item_id} does not need a session: {}",
                    decision.reason
                ),
            });
        }
    };
    if row.probe_status != "probed" {
        return Err(ApiError {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            message: format!("item {item_id} is not ready to play: {}", decision.reason),
        });
    }

    let Some(duration_ms) = row.duration_ms.filter(|d| *d > 0) else {
        return Err(ApiError {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            message: format!(
                "item {item_id} has no probed duration; cannot build a session playlist"
            ),
        });
    };

    let subtitle_tracks = match subtitle_tracks_for(&state, &row) {
        Ok(tracks) => snapshot_hls_tracks(&tracks),
        Err(e) => {
            tracing::warn!(item_id, error = %e, "subtitle list failed at session start");
            Vec::new()
        }
    };

    let audio = resolve_audio(&row, query.audio_track_id.as_deref())?;

    let start_ms = query.start_ms.unwrap_or(0);
    let hls = Arc::clone(&state.hls);
    let hls_for_start = Arc::clone(&hls);
    let src = std::path::PathBuf::from(&row.path);
    let tracks_for_start = subtitle_tracks;
    let started = tokio::task::spawn_blocking(move || {
        hls_for_start.start(
            item_id,
            &src,
            start_ms,
            duration_ms as u64,
            mode,
            audio,
            tracks_for_start,
        )
    })
    .await
    .map_err(|e| ApiError::internal(format!("hls start task: {e}")))?;

    // First caption request otherwise pays a cold demux of the source; warm
    // races the session instead (ADR-0010).
    let cache = Arc::clone(&state.subs);
    let warm_src = std::path::PathBuf::from(&row.path);
    let warm_id = row.id;
    let warm_mtime = file_mtime_ms(&warm_src).unwrap_or(row.mtime_ms);
    let warm_size = row.size_bytes;
    tokio::task::spawn_blocking(move || {
        match warm_embedded_webvtts(&cache, warm_id, warm_mtime, warm_size, &warm_src) {
            Ok(n) if n > 0 => tracing::info!(
                item_id = warm_id,
                tracks = n,
                "warmed embedded subtitle cache"
            ),
            Ok(_) => {}
            Err(e) => tracing::warn!(
                item_id = warm_id,
                error = %e,
                "subtitle cache warm failed"
            ),
        }
    });

    match started {
        Ok(session_id) => {
            let encoder = hls.encoder(&session_id).ok_or_else(|| {
                ApiError::internal(format!("session {session_id} disappeared after start"))
            })?;
            Ok((
                StatusCode::ACCEPTED,
                Json(TranscodeSessionDto {
                    playlist_url: format!("/api/v0/sessions/{session_id}/master.m3u8"),
                    session_id,
                    item_id,
                    video_encoder: encoder.name,
                    encoder_kind: encoder.kind.as_str(),
                }),
            ))
        }
        Err(StartSessionError::CapFull) => Err(ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "all playback sessions are in use; retry shortly".into(),
        }),
        Err(StartSessionError::Spawn(e)) => Err(ApiError::internal(e)),
    }
}

/// Which audio stream this session maps (ADR-0012). No `audioTrackId` takes
/// the container default, else the first track.
fn resolve_audio(row: &MediaItemRow, requested: Option<&str>) -> Result<AudioSelection, ApiError> {
    let max_channels = BROWSER_V0.max_audio_channels.unwrap_or(u32::MAX);
    let tracks = match list_audio_tracks(std::path::Path::new(&row.path)) {
        Ok(tracks) => tracks,
        // Without a requested track the stored first-audio count still
        // applies the ceiling, so a failed inventory need not fail playback.
        Err(e) if requested.is_none() => {
            tracing::warn!(item_id = row.id, error = %e, "audio track list failed at session start");
            return Ok(AudioSelection {
                stream_index: None,
                channels: stored_channels(row),
                max_channels,
            });
        }
        Err(e) => return Err(ApiError::internal(e)),
    };

    let track = match requested {
        Some(id) => Some(tracks.iter().find(|t| t.track_id() == id).ok_or_else(|| {
            ApiError::not_found(format!("audio track {id} not found for item {}", row.id))
        })?),
        None => tracks.iter().find(|t| t.is_default),
    };
    Ok(match track {
        Some(t) => AudioSelection {
            stream_index: Some(t.stream_index),
            channels: t.channels,
            max_channels,
        },
        None => AudioSelection {
            stream_index: None,
            channels: stored_channels(row),
            max_channels,
        },
    })
}

fn stored_channels(row: &MediaItemRow) -> u32 {
    row.audio_channels
        .and_then(|c| u32::try_from(c).ok())
        .unwrap_or(0)
}

fn snapshot_hls_tracks(tracks: &[crate::routes::items::SubtitleTrackDto]) -> Vec<HlsSubtitleTrack> {
    let mut out = Vec::new();
    let mut saw_default = false;
    for t in tracks {
        if t.url.is_none() {
            continue;
        }
        let name = t
            .label
            .clone()
            .or_else(|| t.language.clone())
            .unwrap_or_else(|| t.track_id.clone());
        let is_default = !saw_default;
        if is_default {
            saw_default = true;
        }
        out.push(HlsSubtitleTrack {
            track_id: t.track_id.clone(),
            language: t.language.clone(),
            name,
            is_default,
            forced: t.forced,
            sdh: t.sdh,
        });
    }
    out
}

fn file_mtime_ms(path: &std::path::Path) -> Option<i64> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let dur = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(dur.as_millis() as i64)
}

pub async fn master(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<PlaylistQuery>,
) -> ApiResult<Response> {
    wait_playlist(state, session_id, query.start_ms, PlaylistKind::Master).await
}

pub async fn playlist(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<PlaylistQuery>,
) -> ApiResult<Response> {
    wait_playlist(state, session_id, query.start_ms, PlaylistKind::Media).await
}

pub async fn subtitle_playlist(
    State(state): State<AppState>,
    Path((session_id, asset)): Path<(String, String)>,
) -> ApiResult<Response> {
    let track_id = asset
        .strip_suffix(".m3u8")
        .filter(|id| is_valid_sub_track_id(id))
        .ok_or_else(|| ApiError::not_found(format!("subtitle playlist {asset} not found")))?
        .to_string();
    let hls = Arc::clone(&state.hls);
    let sid = session_id.clone();
    let result = tokio::task::spawn_blocking(move || hls.subtitle_playlist(&sid, &track_id))
        .await
        .map_err(|e| ApiError::internal(format!("hls subtitle playlist task: {e}")))?;
    match result {
        Ok(bytes) => m3u8_ok(bytes),
        Err(PlaylistError::NotFound) => Err(ApiError::not_found(format!(
            "subtitle playlist {asset} for session {session_id} not found"
        ))),
        Err(other) => map_playlist_err(&session_id, other),
    }
}

fn is_valid_sub_track_id(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        Some('e') | Some('s') => {
            chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        }
        _ => false,
    }
}

async fn wait_playlist(
    state: AppState,
    session_id: String,
    start_ms: Option<u64>,
    kind: PlaylistKind,
) -> ApiResult<Response> {
    let hls = Arc::clone(&state.hls);
    let sid = session_id.clone();
    let result = tokio::task::spawn_blocking(move || {
        // The playlist is held back until FFmpeg writes the init segment.
        // Cold 1080p software encodes need a couple of seconds; wait up to 5s
        // so clients see a 200 rather than a thrash of 404s.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let outcome = match kind {
                PlaylistKind::Master => hls.master(&sid, start_ms),
                PlaylistKind::Media => hls.playlist(&sid, start_ms),
            };
            match outcome {
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
        Ok(bytes) => m3u8_ok(bytes),
        Err(e) => map_playlist_err(&session_id, e),
    }
}

fn map_playlist_err(session_id: &str, err: PlaylistError) -> ApiResult<Response> {
    match err {
        PlaylistError::NotFound => Err(ApiError::not_found(format!(
            "session {session_id} not found"
        ))),
        PlaylistError::NotReady => Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: format!("playlist for session {session_id} not ready yet"),
        }),
        PlaylistError::Failed(e) => Err(ApiError::internal(format!(
            "session {session_id} failed: {e}"
        ))),
    }
}

fn m3u8_ok(bytes: Vec<u8>) -> ApiResult<Response> {
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
        // Not yet on disk: ask the player to retry. 404 makes hls.js / Safari
        // give up on the fragment; 503 is recoverable while FFmpeg catches up.
        Err(PlaylistError::NotReady) => Err(ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: format!("asset {asset} for session {session_id} not ready yet"),
        }),
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
