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
    list_audio_tracks,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic counter so Safari/Chrome attach→switch request order is
/// readable in the dogfood log (`rg hls_client_req /tmp/nightjar-dogfood.log`).
static HLS_CLIENT_REQ_SEQ: AtomicU64 = AtomicU64::new(1);

fn log_hls_client_req(session_id: &str, resource: &str, start_ms: Option<u64>, status: u16) {
    let seq = HLS_CLIENT_REQ_SEQ.fetch_add(1, Ordering::Relaxed);
    tracing::info!(
        seq,
        session_id,
        resource,
        start_ms,
        status,
        "hls_client_req"
    );
}

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
        Ok(tracks) => match snapshot_hls_tracks(&state, &row, &tracks) {
            Ok(snap) => snap,
            Err(e) => {
                tracing::warn!(item_id, error = %e, "subtitle snapshot failed at session start");
                Vec::new()
            }
        },
        Err(e) => {
            tracing::warn!(item_id, error = %e, "subtitle list failed at session start");
            Vec::new()
        }
    };

    // First-play: sessions (remux/transcode) hit the same cold-title latency
    // as direct play, so bump extract here too (ADR-0013 §11).
    if row.subtitle_status == "pending" {
        state.pool.prioritize_extract(
            row.id,
            row.library_id,
            std::path::PathBuf::from(&row.path),
        );
    }

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

    match started {
        Ok(session_id) => {
            let encoder = hls.encoder(&session_id).ok_or_else(|| {
                ApiError::internal(format!("session {session_id} disappeared after start"))
            })?;
            log_hls_client_req(&session_id, "POST /sessions", Some(start_ms), 202);
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
        Err(StartSessionError::CapFull) => {
            log_hls_client_req("-", "POST /sessions", Some(start_ms), 503);
            Err(ApiError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: "all playback sessions are in use; retry shortly".into(),
            })
        }
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
                channel_layout: None,
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
            channel_layout: t.channel_layout.clone(),
            max_channels,
        },
        None => AudioSelection {
            stream_index: None,
            channels: stored_channels(row),
            channel_layout: None,
            max_channels,
        },
    })
}

fn stored_channels(row: &MediaItemRow) -> u32 {
    // NULL after an additive migration must not read as "0 channels / under
    // ceiling": session start would skip the pan and copy multi-channel AAC.
    // Prefer a live inventory; this value is only the fallback when listing
    // failed. Over-ceiling forces the downmix path until the next probe.
    row.audio_channels
        .and_then(|c| u32::try_from(c).ok())
        .unwrap_or(u32::MAX)
}

fn snapshot_hls_tracks(
    state: &AppState,
    row: &MediaItemRow,
    tracks: &[crate::routes::items::SubtitleTrackDto],
) -> Result<Vec<HlsSubtitleTrack>, String> {
    let sidecars = state.db.list_item_sidecars(row.id)?;
    let mut out = Vec::new();
    let mut saw_default = false;
    for t in tracks {
        // HLS MEDIA only for fully extracted tracks. Declaring a cold
        // session-inline rendition re-demuxes the source beside the encode
        // and can block Safari start when seg000.vtt never lands (ADR-0013).
        // Pending/partial stay on play-priority extract + preparing UI;
        // captions appear on the next session once complete.
        if t.readiness != Some("complete") || t.url.is_none() {
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
        let (stream_index, sidecar_path) = if t.source == "sidecar" {
            let path = sidecars
                .iter()
                .find(|s| s.track_id == t.track_id)
                .map(|s| std::path::PathBuf::from(&s.path));
            (None, path)
        } else {
            (t.stream_index, None)
        };
        out.push(HlsSubtitleTrack {
            track_id: t.track_id.clone(),
            language: t.language.clone(),
            name,
            is_default,
            forced: t.forced,
            sdh: t.sdh,
            item_id: row.id,
            stream_index,
            sidecar_path,
            codec: t.codec.clone(),
            item_vtt_path: Some(state.subs.vtt_path(row.id, &t.track_id)),
        });
    }
    Ok(out)
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
    // `{trackId}.m3u8` or `{trackId}/segNNN.vtt` (plan item 2).
    // Parse to typed fields only — never join the catch-all string into a path.
    use super::track_ids::{SessionSubtitleAsset, parse_session_subtitle_asset};
    let parsed = parse_session_subtitle_asset(&asset)
        .ok_or_else(|| ApiError::not_found(format!("subtitle asset {asset} not found")))?;

    match parsed {
        SessionSubtitleAsset::Playlist { track_id } => {
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
                Err(PlaylistError::NotReady) => Err(ApiError {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    message: format!("subtitle playlist {asset} not ready"),
                }),
                Err(other) => map_playlist_err(&session_id, other),
            }
        }
        SessionSubtitleAsset::Segment { track_id, index } => {
            let hls = Arc::clone(&state.hls);
            let sid = session_id.clone();
            let result =
                tokio::task::spawn_blocking(move || hls.subtitle_segment(&sid, &track_id, index))
                    .await
                    .map_err(|e| ApiError::internal(format!("hls subtitle segment task: {e}")))?;
            match result {
                Ok(bytes) => {
                    let mut res = Response::new(Body::from(bytes));
                    *res.status_mut() = StatusCode::OK;
                    res.headers_mut().insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("text/vtt; charset=utf-8"),
                    );
                    res.headers_mut().insert(
                        header::CACHE_CONTROL,
                        HeaderValue::from_static("private, no-cache"),
                    );
                    Ok(res)
                }
                Err(PlaylistError::NotFound) => Err(ApiError::not_found(format!(
                    "subtitle asset {asset} for session {session_id} not found"
                ))),
                Err(PlaylistError::NotReady) => Err(ApiError {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    message: format!("subtitle segment {asset} not ready"),
                }),
                Err(other) => map_playlist_err(&session_id, other),
            }
        }
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
        // Mid-title hardware sessions on a real library can take longer than
        // 5s to produce the first segment, especially during an audio switch
        // if the old session has only just been reaped. Match SEGMENT_WAIT in
        // hls.rs so the browser sees one long request instead of repeated 503s.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
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
        Ok(bytes) => {
            let resource = match kind {
                PlaylistKind::Master => "master.m3u8",
                PlaylistKind::Media => "index.m3u8",
            };
            log_hls_client_req(&session_id, resource, start_ms, 200);
            m3u8_ok(bytes)
        }
        Err(e) => {
            let resource = match kind {
                PlaylistKind::Master => "master.m3u8",
                PlaylistKind::Media => "index.m3u8",
            };
            let status = match &e {
                PlaylistError::NotFound => 404,
                PlaylistError::NotReady => 503,
                PlaylistError::Failed(_) => 500,
            };
            log_hls_client_req(&session_id, resource, start_ms, status);
            map_playlist_err(&session_id, e)
        }
    }
}

fn map_playlist_err(session_id: &str, err: PlaylistError) -> ApiResult<Response> {
    match err {
        PlaylistError::NotFound => Err(ApiError::not_found(format!(
            "session {session_id} not found"
        ))),
        // Same as segment assets: 503 is retryable while FFmpeg catches up.
        // 404 was indistinguishable from a dead session and made audio-switch
        // waits look like hard failures in the browser console.
        PlaylistError::NotReady => Err(ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
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
            log_hls_client_req(&session_id, &asset, None, 200);
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
        Err(PlaylistError::NotFound) => {
            log_hls_client_req(&session_id, &asset, None, 404);
            Err(ApiError::not_found(format!(
                "asset {asset} for session {session_id} not found"
            )))
        }
        // Not yet on disk: ask the player to retry. 404 makes hls.js / Safari
        // give up on the fragment; 503 is recoverable while FFmpeg catches up.
        Err(PlaylistError::NotReady) => {
            log_hls_client_req(&session_id, &asset, None, 503);
            Err(ApiError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: format!("asset {asset} for session {session_id} not ready yet"),
            })
        }
        Err(PlaylistError::Failed(e)) => {
            log_hls_client_req(&session_id, &asset, None, 500);
            Err(ApiError::internal(e))
        }
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
        log_hls_client_req(&session_id, "DELETE /sessions", None, 204);
        Ok(StatusCode::NO_CONTENT)
    } else {
        // Idempotent teardown: already gone is fine for player unmount.
        log_hls_client_req(&session_id, "DELETE /sessions", None, 204);
        Ok(StatusCode::NO_CONTENT)
    }
}
