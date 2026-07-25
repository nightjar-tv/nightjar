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
    RemuxKey, RemuxState, ensure_embedded_webvtt, ensure_sidecar_webvtt,
    is_serveable_sidecar_format, list_text_subtitles, warm_embedded_webvtts,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remux_state: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remux_error: Option<String>,
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
    pub subtitle_tracks: Vec<SubtitleTrackDto>,
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
    let (remux_state, remux_error, stream_url, sessions_url, mime_type) = match decision.method {
        PlaybackMethod::DirectPlay => (None, None, Some(stream_url), None, decision.mime_type),
        PlaybackMethod::Transcode => (
            None,
            None,
            None,
            Some(format!("/api/v0/items/{item_id}/sessions")),
            "application/vnd.apple.mpegurl".into(),
        ),
        PlaybackMethod::Remux => {
            let registry = Arc::clone(&state.remux);
            let key = remux_key(&row);
            let status = tokio::task::spawn_blocking(move || registry.status(&key))
                .await
                .map_err(|e| ApiError::internal(format!("remux status task: {e}")))?;
            let (state_s, err, url) = match status {
                RemuxState::Ready => (Some("ready"), None, Some(stream_url)),
                RemuxState::Preparing => (Some("preparing"), None, None),
                RemuxState::NotStarted { reason } => (Some("notStarted"), reason, None),
                RemuxState::Failed(e) => (Some("failed"), Some(e), None),
            };
            (state_s, err, url, None, decision.mime_type)
        }
    };

    let subtitle_tracks = match decision.method {
        PlaybackMethod::DirectPlay | PlaybackMethod::Remux => subtitle_tracks_for(&state, &row)
            .unwrap_or_else(|e| {
                tracing::warn!(item_id, error = %e, "subtitle list failed");
                Vec::new()
            }),
        PlaybackMethod::Transcode => Vec::new(),
    };

    Ok(Json(PlaybackInfoDto {
        item_id: row.id,
        playback_method: decision.method.as_str(),
        remux_state,
        remux_error,
        reason: decision.reason,
        stream_url,
        sessions_url,
        mime_type,
        duration_ms: row.duration_ms,
        container: row.container,
        video_codec: row.video_codec,
        audio_codec: row.audio_codec,
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
    let decision = decide(&row);
    if !matches!(
        decision.method,
        PlaybackMethod::DirectPlay | PlaybackMethod::Remux
    ) {
        return Err(ApiError::not_found(format!(
            "item {item_id} has no text subtitle sidecars for {}",
            decision.method.as_str()
        )));
    }

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

fn subtitle_tracks_for(
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
    let warm_src = src.clone();
    let started = tokio::task::spawn_blocking(move || registry.start(&key, &src))
        .await
        .map_err(|e| ApiError::internal(format!("remux start task: {e}")))?;

    // First <track> GET otherwise pays ~NAS demux before captions appear.
    // Warm in parallel with remux so VTT is ready when the MP4 is.
    let cache = Arc::clone(&state.subs);
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
