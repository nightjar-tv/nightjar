use crate::error::{ApiError, ApiResult};
use crate::routes::items::{
    abs_path, decide, library_root, profile_from_query, subtitle_tracks_for,
};
use crate::state::AppState;
use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    response::Response,
};
use nightjar_core::{
    ClientCapabilityProfile, DEFAULT_PREFERENCE_LANGUAGE, PlaybackMethod, TrackCandidate,
    select_audio_track, select_subtitle_track, video_encode_plan,
};
use nightjar_db::MediaItemRow;
use nightjar_db::SubtitleTrackRow;
use nightjar_db::resolve_media_path;
use nightjar_transcode::{
    AudioSelection, BurnInKind, BurnInSelection, HlsSubtitleTrack, KeyframeMap, PiggybackExtract,
    PlaylistError, SessionMode, StartSessionError, burn_in_kind_for_codec, list_audio_tracks,
    list_burn_in_subtitles,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic counter so Safari/Chrome attach→switch request order is
/// readable in the dogfood log (`rg hls_client_req /tmp/nightjar-dogfood.log`).
static HLS_CLIENT_REQ_SEQ: AtomicU64 = AtomicU64::new(1);

fn log_hls_client_req(
    session_id: &str,
    resource: &str,
    start_ms: Option<u64>,
    status: u16,
    fetcher: Option<&str>,
) {
    let seq = HLS_CLIENT_REQ_SEQ.fetch_add(1, Ordering::Relaxed);
    tracing::info!(
        seq,
        session_id,
        resource,
        start_ms,
        status,
        fetcher = fetcher.unwrap_or("-"),
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
    pub landed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usable_extent_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartQuery {
    pub start_ms: Option<u64>,
    pub audio_track_id: Option<String>,
    pub subtitle_track_id: Option<String>,
    pub profile_id: Option<String>,
    pub max_bitrate_bps: Option<u64>,
    pub max_height: Option<u32>,
    pub hdr: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeekQuery {
    pub start_ms: u64,
}

/// Log-only marker on segment GETs (`njFetcher`). Serving ignores it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetQuery {
    pub nj_fetcher: Option<String>,
}

#[derive(Clone, Copy)]
enum PlaylistKind {
    Master,
    Media,
}

fn dto_from_view(view: nightjar_transcode::SessionView) -> TranscodeSessionDto {
    TranscodeSessionDto {
        session_id: view.session_id,
        item_id: view.item_id,
        playlist_url: view.playlist_url,
        video_encoder: view.video_encoder,
        encoder_kind: view.encoder_kind.as_str(),
        landed_ms: view.landed_ms,
        usable_extent_ms: view.usable_extent_ms,
    }
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
    let profile = profile_from_query(
        query.profile_id.as_deref(),
        query.max_bitrate_bps,
        query.max_height,
        query.hdr.as_deref(),
    );
    let decision = decide(&row, &profile, state.tonemap_available);
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

    let audio = resolve_audio(&state, &row, query.audio_track_id.as_deref(), &profile)?;
    let burn_in = resolve_burn_in(&state, &row, query.subtitle_track_id.as_deref())?;

    // DirectPlay is allowed when a track selection requires encode work the
    // progressive path cannot do (ADR-0012 hybrid / ADR-0018 burn-in).
    let needs_encode_selection = burn_in.is_some() || audio.needs_downmix();
    let mut mode = match decision.method {
        PlaybackMethod::Remux => SessionMode::Copy,
        PlaybackMethod::Transcode => SessionMode::Transcode,
        PlaybackMethod::DirectPlay if needs_encode_selection => SessionMode::Transcode,
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
    if burn_in.is_some() {
        mode = SessionMode::Transcode;
    }

    let lib_root = library_root(&state, row.library_id)?;
    let subtitle_tracks = match subtitle_tracks_for(&state, &row, &lib_root) {
        Ok(tracks) => match snapshot_hls_tracks(&state, &row, &lib_root, &tracks) {
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

    let start_ms = query.start_ms.unwrap_or(0);
    let keyframe_map = keyframe_map_for(&state, &row);
    let encode_plan = video_encode_plan(
        row.height.and_then(|h| u32::try_from(h).ok()),
        row.video_bitrate_bps.and_then(|b| u64::try_from(b).ok()),
        row.hdr.as_deref(),
        &profile,
    );
    // Profile 5: no tonemap attempt (decide already names the refuse reason).
    if mode == SessionMode::Transcode && nightjar_core::is_dolby_vision_profile5(row.hdr.as_deref())
    {
        return Err(ApiError {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            message: decision.reason.clone(),
        });
    }
    if encode_plan.tone_map && !state.tonemap_available {
        return Err(ApiError {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            message: decision.reason.clone(),
        });
    }
    let piggyback = match state.db.list_item_subtitle_tracks(row.id) {
        Ok(tracks) => piggyback_track_for(&row.subtitle_status, &tracks)
            .map(|track_id| PiggybackExtract { track_id }),
        Err(e) => {
            tracing::warn!(item_id, error = %e, "piggyback eligibility read failed");
            None
        }
    };
    let hls = Arc::clone(&state.hls);
    let hls_for_start = Arc::clone(&hls);
    let src = abs_path(&lib_root, &row.path);
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
            burn_in,
            keyframe_map,
            encode_plan,
            piggyback,
        )
    })
    .await
    .map_err(|e| ApiError::internal(format!("hls start task: {e}")))?;

    match started {
        Ok(session_id) => {
            let view = hls.view(&session_id).map_err(|e| {
                ApiError::internal(format!("session {session_id} view after start: {e:?}"))
            })?;
            if hls.map_fallback(&session_id) {
                request_map_rebuild(&state, &row);
            }
            log_hls_client_req(&session_id, "POST /sessions", Some(start_ms), 202, None);
            Ok((StatusCode::ACCEPTED, Json(dto_from_view(view))))
        }
        Err(StartSessionError::CapFull) => {
            log_hls_client_req("-", "POST /sessions", Some(start_ms), 503, None);
            Err(ApiError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: "all playback sessions are in use; retry shortly".into(),
            })
        }
        Err(StartSessionError::Spawn(e)) => Err(ApiError::internal(e)),
    }
}

/// Keyframe map for this session, or None when the item has no usable one.
///
/// A missing map is the ADR-0023 §8 fallback: the session starts with `-ss`
/// on the real file and a rebuild goes on the library pool. Identity is
/// re-checked against the bytes on disk at every bind, inside the session.
fn keyframe_map_for(state: &AppState, row: &MediaItemRow) -> Option<KeyframeMap> {
    let map = match state.db.keyframe_map(row.id) {
        Ok(Some(rows)) => KeyframeMap::from_db_rows(&rows),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(item_id = row.id, error = %e, "keyframe map read failed at session start");
            None
        }
    };
    if map.is_none() {
        request_map_rebuild(state, row);
    }
    map
}

/// Puts a map build at the front of the library pool's background work
/// (ADR-0023 §8). Already pending or in flight is a no-op.
pub(crate) fn request_map_rebuild(state: &AppState, row: &MediaItemRow) {
    let Ok(root) = library_root(state, row.library_id) else {
        return;
    };
    state
        .pool
        .prioritize_map_rebuild(row.id, row.library_id, abs_path(&root, &row.path));
}

/// Which audio stream this session maps (ADR-0012 / ADR-0024).
fn resolve_audio(
    state: &AppState,
    row: &MediaItemRow,
    requested: Option<&str>,
    profile: &ClientCapabilityProfile,
) -> Result<AudioSelection, ApiError> {
    let max_channels = profile.max_audio_channels.unwrap_or(u32::MAX);
    let root = library_root(state, row.library_id)?;
    let tracks = match list_audio_tracks(&abs_path(&root, &row.path)) {
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
        Some(id) => {
            let t = tracks.iter().find(|t| t.track_id() == id).ok_or_else(|| {
                ApiError::not_found(format!("audio track {id} not found for item {}", row.id))
            })?;
            tracing::info!(
                item_id = row.id,
                track_id = %id,
                reason = "client requested audioTrackId",
                "audio track selected"
            );
            Some(t)
        }
        None => {
            let candidates: Vec<TrackCandidate> = tracks
                .iter()
                .map(|t| TrackCandidate {
                    track_id: t.track_id(),
                    language: t.language.clone(),
                    title: t.title.clone(),
                    is_default: t.is_default,
                    is_forced: false,
                    is_image: false,
                    stream_index: t.stream_index,
                })
                .collect();
            let sel = select_audio_track(&candidates, Some(DEFAULT_PREFERENCE_LANGUAGE));
            tracing::info!(
                item_id = row.id,
                track_id = sel.track_id.as_deref().unwrap_or("-"),
                reason = %sel.reason,
                "audio track selected"
            );
            sel.track_id
                .as_deref()
                .and_then(|id| tracks.iter().find(|t| t.track_id() == id))
        }
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

/// Burn-in track for this session (ADR-0018). Soft track ids are rejected.
fn resolve_burn_in(
    state: &AppState,
    row: &MediaItemRow,
    requested: Option<&str>,
) -> Result<Option<BurnInSelection>, ApiError> {
    let Some(id) = requested else {
        return Ok(None);
    };
    let root = library_root(state, row.library_id)?;
    let tracks = subtitle_tracks_for(state, row, &root).map_err(ApiError::internal)?;
    let track = tracks.iter().find(|t| t.track_id == id).ok_or_else(|| {
        ApiError::not_found(format!("subtitle track {id} not found for item {}", row.id))
    })?;
    if track.render != "burnIn" {
        return Err(ApiError::not_found(format!(
            "subtitle track {id} is not a burn-in track"
        )));
    }
    let kind = burn_in_kind_for_codec(&track.codec).ok_or_else(|| {
        ApiError::not_found(format!(
            "subtitle track {id} codec {} is not burnable",
            track.codec
        ))
    })?;
    if track.source == "sidecar" {
        let sidecars = state
            .db
            .list_item_sidecars(row.id)
            .map_err(ApiError::internal)?;
        let path = sidecars
            .iter()
            .find(|s| s.track_id == id)
            .map(|s| resolve_media_path(&root, &s.path))
            .ok_or_else(|| {
                ApiError::not_found(format!("sidecar path for burn-in track {id} missing"))
            })?;
        return Ok(Some(BurnInSelection {
            track_id: id.to_string(),
            kind: BurnInKind::Ass,
            stream_index: None,
            subtitle_ordinal: None,
            sidecar_path: Some(path),
        }));
    }
    let embedded =
        list_burn_in_subtitles(&abs_path(&root, &row.path)).map_err(ApiError::internal)?;
    let stream = embedded
        .iter()
        .find(|s| s.track_id() == id)
        .ok_or_else(|| {
            ApiError::not_found(format!("embedded burn-in track {id} missing from probe"))
        })?;
    Ok(Some(BurnInSelection {
        track_id: id.to_string(),
        kind,
        stream_index: Some(stream.stream_index),
        subtitle_ordinal: Some(stream.subtitle_ordinal),
        sidecar_path: None,
    }))
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

/// ADR-0041 Decision 7: piggyback target for a session on an `eligible` item.
/// The session's ffmpeg side output (`-map 0:s?` + `-c:s webvtt`) is only
/// safe when the source holds exactly one embedded text or ASS track: the
/// HLS muxer accepts a single WebVTT subtitle stream — a second subtitle
/// output, an image stream (PGS/VobSub), or an unknown codec fails the whole
/// session. Every other `eligible` item stays `eligible` for a standalone or
/// later pass. Returns the library track id to publish on completion.
fn piggyback_track_for(status: &str, tracks: &[SubtitleTrackRow]) -> Option<String> {
    if status != "eligible" {
        return None;
    }
    if tracks.len() != 1 {
        return None;
    }
    let t = &tracks[0];
    if !matches!(t.kind.as_str(), "text" | "ass") {
        return None;
    }
    Some(format!("e{}", t.stream_index))
}

fn snapshot_hls_tracks(
    state: &AppState,
    row: &MediaItemRow,
    library_root: &str,
    tracks: &[crate::routes::items::SubtitleTrackDto],
) -> Result<Vec<HlsSubtitleTrack>, String> {
    let sidecars = state.db.list_item_sidecars(row.id)?;
    let ready: Vec<&crate::routes::items::SubtitleTrackDto> = tracks
        .iter()
        // HLS MEDIA only for fully extracted tracks. Declaring a cold
        // session-inline rendition re-demuxes the source beside the encode
        // and can block Safari start when seg000.vtt never lands (ADR-0013).
        // Pending/partial stay on play-priority extract + preparing UI;
        // captions appear on the next session once complete.
        .filter(|t| t.readiness == Some("complete") && t.url.is_some())
        .collect();
    let audio_lang = list_audio_tracks(&abs_path(library_root, &row.path))
        .ok()
        .and_then(|audio| {
            let cands: Vec<TrackCandidate> = audio
                .iter()
                .map(|t| TrackCandidate {
                    track_id: t.track_id(),
                    language: t.language.clone(),
                    title: t.title.clone(),
                    is_default: t.is_default,
                    is_forced: false,
                    is_image: false,
                    stream_index: t.stream_index,
                })
                .collect();
            let id = select_audio_track(&cands, Some(DEFAULT_PREFERENCE_LANGUAGE)).track_id?;
            audio
                .iter()
                .find(|t| t.track_id() == id)
                .and_then(|t| t.language.clone())
        });
    let sub_cands: Vec<TrackCandidate> = ready
        .iter()
        .map(|t| TrackCandidate {
            track_id: t.track_id.clone(),
            language: t.language.clone(),
            title: t.label.clone(),
            is_default: false,
            is_forced: t.forced,
            is_image: false,
            stream_index: t.stream_index.unwrap_or(u32::MAX),
        })
        .collect();
    let sub_sel = select_subtitle_track(
        &sub_cands,
        Some(DEFAULT_PREFERENCE_LANGUAGE),
        audio_lang.as_deref(),
    );
    tracing::info!(
        item_id = row.id,
        track_id = sub_sel.track_id.as_deref().unwrap_or("-"),
        reason = %sub_sel.reason,
        "subtitle track selected"
    );
    let default_id = sub_sel.track_id.as_deref();
    let mut out = Vec::new();
    for t in ready {
        let name = t
            .label
            .clone()
            .or_else(|| t.language.clone())
            .unwrap_or_else(|| t.track_id.clone());
        let is_default = default_id == Some(t.track_id.as_str());
        let (stream_index, sidecar_path) = if t.source == "sidecar" {
            let path = sidecars
                .iter()
                .find(|s| s.track_id == t.track_id)
                .map(|s| resolve_media_path(library_root, &s.path));
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

pub async fn get(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> ApiResult<Json<TranscodeSessionDto>> {
    let hls = Arc::clone(&state.hls);
    let sid = session_id.clone();
    let result = tokio::task::spawn_blocking(move || hls.view(&sid))
        .await
        .map_err(|e| ApiError::internal(format!("hls view task: {e}")))?;
    match result {
        Ok(view) => {
            log_hls_client_req(&session_id, "GET /sessions", None, 200, None);
            Ok(Json(dto_from_view(view)))
        }
        Err(PlaylistError::NotFound) => Err(ApiError::not_found(format!(
            "session {session_id} not found"
        ))),
        Err(PlaylistError::Failed(e)) => Err(ApiError::internal(e)),
        Err(other) => Err(ApiError::internal(format!("{other:?}"))),
    }
}

pub async fn seek(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<SeekQuery>,
) -> ApiResult<(StatusCode, Json<TranscodeSessionDto>)> {
    let hls = Arc::clone(&state.hls);
    let sid = session_id.clone();
    let start_ms = query.start_ms;
    let result = tokio::task::spawn_blocking(move || hls.seek(&sid, start_ms))
        .await
        .map_err(|e| ApiError::internal(format!("hls seek task: {e}")))?;
    match result {
        Ok(view) => {
            // A seek restart re-binds the virtual file, so this is where a
            // mid-session replacement shows up (ADR-0023 §4).
            if state.hls.map_fallback(&session_id)
                && let Ok(Some(row)) = state.db.get_item(view.item_id)
            {
                request_map_rebuild(&state, &row);
            }
            log_hls_client_req(&session_id, "POST /seek", Some(start_ms), 202, None);
            Ok((StatusCode::ACCEPTED, Json(dto_from_view(view))))
        }
        Err(PlaylistError::NotFound) => Err(ApiError::not_found(format!(
            "session {session_id} not found"
        ))),
        Err(PlaylistError::Failed(e)) => Err(ApiError::internal(e)),
        Err(other) => Err(ApiError::internal(format!("seek failed: {other:?}"))),
    }
}

pub async fn master(
    State(state): State<AppState>,
    Path((session_id, run_id)): Path<(String, u64)>,
) -> ApiResult<Response> {
    wait_playlist(state, session_id, run_id, PlaylistKind::Master).await
}

pub async fn playlist(
    State(state): State<AppState>,
    Path((session_id, run_id)): Path<(String, u64)>,
) -> ApiResult<Response> {
    wait_playlist(state, session_id, run_id, PlaylistKind::Media).await
}

pub async fn run_init(
    State(state): State<AppState>,
    Path((session_id, run_id)): Path<(String, u64)>,
) -> ApiResult<Response> {
    let hls = Arc::clone(&state.hls);
    let sid = session_id.clone();
    let result = tokio::task::spawn_blocking(move || hls.run_asset(&sid, run_id, "init.mp4"))
        .await
        .map_err(|e| ApiError::internal(format!("hls run init task: {e}")))?;
    match result {
        Ok(bytes) => {
            log_hls_client_req(&session_id, "init.mp4", None, 200, None);
            let mut res = Response::new(Body::from(bytes));
            *res.status_mut() = StatusCode::OK;
            res.headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("video/mp4"));
            res.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            Ok(res)
        }
        Err(e) => map_playlist_err(&session_id, e),
    }
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
            let result =
                tokio::task::spawn_blocking(move || hls.subtitle_playlist(&sid, &track_id))
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
    run_id: u64,
    kind: PlaylistKind,
) -> ApiResult<Response> {
    let hls = Arc::clone(&state.hls);
    let sid = session_id.clone();
    let result = tokio::task::spawn_blocking(move || {
        // The playlist is held back until the map has at least one segment.
        // Mid-title hardware sessions on a real library can take longer than
        // 5s to produce the first segment, especially during an audio switch
        // if the old session has only just been reaped. Match SEGMENT_WAIT in
        // hls.rs so the browser sees one long request instead of repeated 503s.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let outcome = match kind {
                PlaylistKind::Master => hls.master(&sid, run_id),
                PlaylistKind::Media => hls.playlist(&sid, run_id),
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
            log_hls_client_req(&session_id, resource, None, 200, None);
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
                PlaylistError::AbandonedHoldEnded => 204,
                PlaylistError::Failed(_) => 500,
            };
            log_hls_client_req(&session_id, resource, None, status, None);
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
        // Asset-path hold ceiling (ADR-0011 §7); playlists should not hit this.
        PlaylistError::AbandonedHoldEnded => {
            let mut res = Response::new(Body::empty());
            *res.status_mut() = StatusCode::NO_CONTENT;
            Ok(res)
        }
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
    Query(query): Query<AssetQuery>,
) -> ApiResult<Response> {
    let hls = Arc::clone(&state.hls);
    let sid = session_id.clone();
    let name = asset.clone();
    let fetcher = query.nj_fetcher.clone();
    let fetcher_for_log = fetcher.clone();
    let result = tokio::task::spawn_blocking(move || hls.asset(&sid, &name, fetcher.as_deref()))
        .await
        .map_err(|e| ApiError::internal(format!("hls asset task: {e}")))?;

    let fetcher_ref = fetcher_for_log.as_deref();
    match result {
        Ok(bytes) => {
            log_hls_client_req(&session_id, &asset, None, 200, fetcher_ref);
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
            log_hls_client_req(&session_id, &asset, None, 404, fetcher_ref);
            Err(ApiError::not_found(format!(
                "asset {asset} for session {session_id} not found"
            )))
        }
        // Not yet on disk: ask the player to retry. 404 makes hls.js / Safari
        // give up on the fragment; 503 is recoverable while FFmpeg catches up.
        Err(PlaylistError::NotReady) => {
            log_hls_client_req(&session_id, &asset, None, 503, fetcher_ref);
            Err(ApiError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: format!("asset {asset} for session {session_id} not ready yet"),
            })
        }
        // Abandoned / superseded hold ceiling: empty 204 (ADR-0011 §7).
        Err(PlaylistError::AbandonedHoldEnded) => {
            log_hls_client_req(&session_id, &asset, None, 204, fetcher_ref);
            let mut res = Response::new(Body::empty());
            *res.status_mut() = StatusCode::NO_CONTENT;
            Ok(res)
        }
        Err(PlaylistError::Failed(e)) => {
            log_hls_client_req(&session_id, &asset, None, 500, fetcher_ref);
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
        log_hls_client_req(&session_id, "DELETE /sessions", None, 204, None);
        Ok(StatusCode::NO_CONTENT)
    } else {
        // Idempotent teardown: already gone is fine for player unmount.
        log_hls_client_req(&session_id, "DELETE /sessions", None, 204, None);
        Ok(StatusCode::NO_CONTENT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(stream_index: i64, kind: &str) -> SubtitleTrackRow {
        SubtitleTrackRow {
            media_item_id: 1,
            stream_index,
            codec: "subrip".into(),
            language: None,
            title: None,
            forced: false,
            sdh: false,
            kind: kind.into(),
        }
    }

    /// ADR-0041 Decision 7 gate, table-driven: the piggyback fires only for
    /// an `eligible` item holding exactly one embedded text or ASS track.
    #[test]
    fn piggyback_track_gate() {
        let cases: &[(&str, &[SubtitleTrackRow], Option<&str>)] = &[
            // Not eligible: no piggyback, whatever the inventory says.
            ("ready", &[row(2, "text")], None),
            ("none", &[row(2, "text")], None),
            ("pending", &[row(2, "text")], None),
            // Eligible with exactly one text track → piggyback it.
            ("eligible", &[row(2, "text")], Some("e2")),
            ("eligible", &[row(7, "ass")], Some("e7")),
            // Multi-track eligible items stay on a standalone/later pass
            // (the HLS muxer accepts one WebVTT subtitle stream per session).
            ("eligible", &[row(2, "text"), row(3, "text")], None),
            ("eligible", &[row(2, "text"), row(3, "image")], None),
            ("eligible", &[row(2, "unknown")], None),
            ("eligible", &[row(2, "image")], None),
            // Eligible with no embedded rows (sidecar-only) has no side output.
            ("eligible", &[], None),
        ];
        for (status, tracks, expected) in cases {
            assert_eq!(
                piggyback_track_for(status, tracks).as_deref(),
                *expected,
                "status={status} tracks={tracks:?}"
            );
        }
    }
}
