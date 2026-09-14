use crate::authority::{WatchingCaller, require_item_visible_for_profile};
use crate::error::{ApiError, ApiResult, blocking};
use crate::routes::items::{
    abs_path, account_playback_policy, apply_playback_policy, decide, library_root,
    profile_from_query, subtitle_tracks_for,
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
    ClientCapabilityProfile, PlaybackMethod, TrackCandidate, TrackSelection,
    select_audio_for_description, select_audio_track, select_subtitle_for_description,
    select_subtitle_track, video_encode_plan,
};
use nightjar_db::MediaItemRow;
use nightjar_db::SubtitleTrackRow;
use nightjar_db::{SubtitleChoiceRow, TrackDescription, resolve_media_path};
use nightjar_transcode::{
    AudioSelection, BurnInKind, BurnInSelection, HlsSubtitleTrack, KeyframeMap, PiggybackExtract,
    PlaylistError, SessionMode, SessionOwner, StartSessionError, VideoRung, burn_in_kind_for_codec,
    list_audio_tracks, list_burn_in_subtitles, parse_time_keyed_segment_name,
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

/// Wire code carried by the 503 admission-refusal body, for the client's
/// session-create retry decision. The web watch page matches this code, not
/// the sentence below it, and `web/tests/sessionRetry.test.ts` pins this
/// declaration to the client's constant so changing one side without the
/// other goes red (Rule 4.11).
pub const ADMISSION_REFUSED_CODE: &str = "admission_refused";

/// Wire code carried by the 503 account-ceiling-refusal body (ADR-0034
/// item 8). Distinct from [`ADMISSION_REFUSED_CODE`]: this names a per-account
/// policy limit, not measured host capacity. A client retries an admission
/// refusal but must not retry this one until a session ends or the cap moves.
pub const ACCOUNT_CEILING_REFUSED_CODE: &str = "account_ceiling_refused";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscodeSessionDto {
    pub session_id: String,
    pub item_id: i64,
    pub playlist_url: String,
    pub video_encoder: String,
    pub encoder_kind: &'static str,
    pub landed_ms: u64,
    /// Title time that element `currentTime` 0 means. A full-title listing
    /// zeroes at 0; a per-run listing zeroes at its first segment. Distinct
    /// from `landed_ms`, which is where the producer started.
    pub media_origin_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usable_extent_ms: Option<u64>,
    /// Why this session's audio track was selected (ADR-0038 item 5). Server
    /// time only, and the string the track menu shows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_reason: Option<String>,
    /// Why this session's subtitle track was selected, or why none was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle_reason: Option<String>,
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
    /// Explicit predecessor for a replacement start (ADR-0034 item 8). The
    /// predecessor must be a live session of the authenticated account and
    /// profile on the same item. Without it, a start at the account ceiling is
    /// refused rather than treated as a replacement.
    pub replaces_session_id: Option<String>,
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
    /// The flat `/index.m3u8` route: the single production rung.
    Media,
    /// The rung-scoped `/v/{rung}/index.m3u8` route, with the parsed rung.
    RungMedia(VideoRung),
}

fn dto_from_view(view: nightjar_transcode::SessionView) -> TranscodeSessionDto {
    TranscodeSessionDto {
        session_id: view.session_id,
        item_id: view.item_id,
        playlist_url: view.playlist_url,
        video_encoder: view.video_encoder,
        encoder_kind: view.encoder_kind.as_str(),
        landed_ms: view.landed_ms,
        media_origin_ms: view.media_origin_ms,
        usable_extent_ms: view.usable_extent_ms,
        audio_reason: None,
        subtitle_reason: None,
    }
}

pub async fn start(
    State(state): State<AppState>,
    watching: WatchingCaller,
    Path(item_id): Path<i64>,
    Query(query): Query<StartQuery>,
) -> ApiResult<(StatusCode, Json<TranscodeSessionDto>)> {
    // The whole body blocks: DB reads, two ffprobe children over SMB in
    // `subtitle_tracks_for` and `resolve_audio`, then the session spawn. It
    // used to run on a Tokio worker with only `hls.start` moved off, so the
    // most expensive part of the most latency-sensitive route was the part
    // still parking the runtime.
    let owner = watching.owner().clone();
    let profile_id = watching.profile_id();
    let account_id = watching.account_id();
    blocking(move || start_blocking(state, item_id, query, owner, profile_id, account_id)).await
}

fn start_blocking(
    state: AppState,
    item_id: i64,
    query: StartQuery,
    owner: SessionOwner,
    profile_id: i64,
    account_id: i64,
) -> ApiResult<(StatusCode, Json<TranscodeSessionDto>)> {
    // A playback session is item-returning in effect and is not exempt
    // (ADR-0037 item 7). The gate runs first, so a capped profile denied by
    // certification gets the missing-item 404 before the row is read, before
    // any probe, and before `hls.start` creates a session or an encoder.
    require_item_visible_for_profile(&state, profile_id, item_id)?;
    let row = state
        .db
        .get_item(item_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("item {item_id} not found")))?;
    let capability = profile_from_query(
        query.profile_id.as_deref(),
        query.max_bitrate_bps,
        query.max_height,
        query.hdr.as_deref(),
    );
    // The one composition, shared with playback-info and `/stream`
    // (ADR-0022 §5 as amended). Unknown-as-local, so the policy half is
    // surfaced and not applied.
    let policy = account_playback_policy(&state, account_id)?;
    let account_max_concurrent_sessions = policy.max_concurrent_sessions;
    let (profile, _ceilings) = apply_playback_policy(capability, policy);
    let decision = decide(&row, &profile, state.tonemap_available);
    if row.probe_status != "probed" {
        return Err(ApiError {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            message: format!("item {item_id} is not ready to play: {}", decision.reason),
            code: "unsupported_media_type",
        });
    }

    let Some(duration_ms) = row.duration_ms.filter(|d| *d > 0) else {
        return Err(ApiError {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            message: format!(
                "item {item_id} has no probed duration; cannot build a session playlist"
            ),
            code: "unsupported_media_type",
        });
    };

    let prefs = load_track_preferences(&state, Some(profile_id), &row);
    let audio = resolve_audio(
        &state,
        &row,
        query.audio_track_id.as_deref(),
        &profile,
        &prefs,
    )?;
    let burn_in = resolve_burn_in(&state, &row, query.subtitle_track_id.as_deref())?;

    // DirectPlay is allowed when a track selection requires encode work the
    // progressive path cannot do (ADR-0012 hybrid / ADR-0018 burn-in).
    let needs_encode_selection = burn_in.is_some() || audio.selection.needs_downmix();
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
                code: "unsupported_media_type",
            });
        }
    };
    if burn_in.is_some() {
        mode = SessionMode::Transcode;
    }

    let lib_root = library_root(&state, row.library_id)?;
    let (subtitle_tracks, subtitle_reason) = match subtitle_tracks_for(&state, &row, &lib_root) {
        Ok(tracks) => match snapshot_hls_tracks(
            &state,
            &row,
            &lib_root,
            &tracks,
            &prefs,
            audio.language.as_deref(),
        ) {
            Ok((snap, reason)) => (snap, reason),
            Err(e) => {
                tracing::warn!(item_id, error = %e, "subtitle snapshot failed at session start");
                (Vec::new(), "subtitle snapshot failed".to_string())
            }
        },
        Err(e) => {
            tracing::warn!(item_id, error = %e, "subtitle list failed at session start");
            (Vec::new(), "subtitle list failed".to_string())
        }
    };
    // An explicit burn-in track is the selection, whatever the soft default
    // would have been.
    let subtitle_reason = if burn_in.is_some() {
        "client requested subtitleTrackId".to_string()
    } else {
        subtitle_reason
    };

    let start_ms = query.start_ms.unwrap_or(0);
    let keyframe_map = keyframe_map_for(&state, &row);
    // ADR-0052: a transcode session needs the source frame rate to derive its
    // IDR interval. Items probed before that migration have none, so resolve
    // it here and write it back rather than encoding without it (Rule 4.13).
    let frame_rate = match (row.video_frame_rate_num, row.video_frame_rate_den) {
        (Some(n), Some(d)) if n > 0 && d > 0 => Some((n as u32, d as u32)),
        _ if mode == SessionMode::Transcode => resolve_frame_rate(&state, &row),
        _ => None,
    };
    let encode_plan = video_encode_plan(
        row.height.and_then(|h| u32::try_from(h).ok()),
        row.video_bitrate_bps.and_then(|b| u64::try_from(b).ok()),
        row.hdr.as_deref(),
        frame_rate,
        &profile,
    );
    // Profile 5: no tonemap attempt (decide already names the refuse reason).
    if mode == SessionMode::Transcode && nightjar_core::is_dolby_vision_profile5(row.hdr.as_deref())
    {
        return Err(ApiError {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            message: decision.reason.clone(),
            code: "unsupported_media_type",
        });
    }
    if encode_plan.tone_map && !state.tonemap_available {
        return Err(ApiError {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            message: decision.reason.clone(),
            code: "unsupported_media_type",
        });
    }
    let piggyback = match state.db.list_item_subtitle_tracks(row.id) {
        Ok(tracks) => piggyback_track_for(&row.subtitle_status, &tracks),
        Err(e) => {
            tracing::warn!(item_id, error = %e, "piggyback eligibility read failed");
            None
        }
    };
    let hls = Arc::clone(&state.hls);
    let src = abs_path(&lib_root, &row.path);
    let started = hls.start(
        owner,
        account_id,
        account_max_concurrent_sessions,
        query.replaces_session_id.clone(),
        item_id,
        &src,
        start_ms,
        duration_ms as u64,
        mode,
        audio.selection,
        subtitle_tracks,
        burn_in,
        keyframe_map,
        encode_plan,
        piggyback,
    );

    match started {
        Ok(session_id) => {
            let view = hls.view(&session_id).map_err(|e| {
                ApiError::internal(format!("session {session_id} view after start: {e:?}"))
            })?;
            if hls.map_fallback(&session_id) {
                request_map_rebuild(&state, &row);
            }
            log_hls_client_req(&session_id, "POST /sessions", Some(start_ms), 202, None);
            let mut dto = dto_from_view(view);
            dto.audio_reason = Some(audio.reason);
            dto.subtitle_reason = Some(subtitle_reason);
            Ok((StatusCode::ACCEPTED, Json(dto)))
        }
        Err(StartSessionError::AdmissionRefused) => {
            log_hls_client_req("-", "POST /sessions", Some(start_ms), 503, None);
            Err(ApiError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                // The sentence is for a person reading a log or a raw response.
                // The client retries on ADMISSION_REFUSED_CODE, never on this
                // wording (a user-facing sentence is not an API).
                message: "playback capacity is temporarily unavailable; retry shortly".into(),
                code: ADMISSION_REFUSED_CODE,
            })
        }
        Err(StartSessionError::AccountCeilingRefused) => {
            log_hls_client_req("-", "POST /sessions", Some(start_ms), 503, None);
            Err(ApiError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: "this account has reached its concurrent playback limit; \
                          stop a playback session or raise the account limit"
                    .into(),
                code: ACCOUNT_CEILING_REFUSED_CODE,
            })
        }
        // A missing predecessor, another account's or profile's predecessor,
        // and an already-retired one are one answer, so the caller cannot use
        // the response to probe other sessions.
        Err(StartSessionError::PredecessorNotFound) => {
            log_hls_client_req("-", "POST /sessions", Some(start_ms), 404, None);
            Err(ApiError::not_found(
                "replacesSessionId names no playable session of this account and profile",
            ))
        }
        Err(StartSessionError::PredecessorItemMismatch) => {
            log_hls_client_req("-", "POST /sessions", Some(start_ms), 422, None);
            Err(ApiError::unprocessable(
                "replacesSessionId must name a session on the same item",
            ))
        }
        Err(StartSessionError::ReplacementAlreadyPending) => {
            log_hls_client_req("-", "POST /sessions", Some(start_ms), 409, None);
            Err(ApiError::conflict(
                "replacesSessionId already has an in-flight replacement",
            ))
        }
        Err(StartSessionError::Spawn(e)) => Err(ApiError::internal(e)),
    }
}

/// Keyframe map for this session, or None when the item has no usable one.
///
/// A missing map is the ADR-0023 §8 fallback: the session starts with `-ss`
/// on the real file and a rebuild goes on the library pool. Identity is
/// re-checked against the bytes on disk at every bind, inside the session.
/// Read the source frame rate for an item the probe never recorded one for
/// (ADR-0052 decision 4). One `ffprobe` on the video stream, written back so
/// the next session reads it from the row. Returns `None` when the file
/// cannot be probed; `spawn_ffmpeg` logs that the grid may not hold.
fn resolve_frame_rate(state: &AppState, row: &MediaItemRow) -> Option<(u32, u32)> {
    let root = library_root(state, row.library_id).ok()?;
    let abs = abs_path(&root, &row.path);
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=avg_frame_rate",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(&abs)
        .output()
        .ok()?;
    if !out.status.success() {
        tracing::warn!(item_id = row.id, "frame-rate probe failed at session start");
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let (num, den) = text.trim().split_once('/')?;
    let num: u32 = num.trim().parse().ok()?;
    let den: u32 = den.trim().parse().ok()?;
    if num == 0 || den == 0 {
        return None;
    }
    if let Err(e) = state.db.set_item_frame_rate(row.id, num as i64, den as i64) {
        // Not fatal: the session can encode on the value we just read.
        tracing::warn!(item_id = row.id, error = %e, "frame-rate write-back failed");
    }
    tracing::info!(
        item_id = row.id,
        frame_rate = %format!("{num}/{den}"),
        "resolved source frame rate at session start"
    );
    Some((num, den))
}

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

/// The profile defaults and per-series override that feed selection
/// (ADR-0038 items 1, 3 and 6).
pub(crate) struct TrackPreferences {
    pub preferred_language: Option<String>,
    /// `auto` | `off`.
    pub subtitle_default: String,
    pub audio_description: Option<TrackDescription>,
    pub subtitle_choice: SubtitleChoiceRow,
}

impl TrackPreferences {
    /// No profile or no stored preference: ADR-0024's no-preference case and
    /// `auto`, which is what a profile nobody configured gets.
    fn none() -> Self {
        Self {
            preferred_language: None,
            subtitle_default: "auto".to_string(),
            audio_description: None,
            subtitle_choice: SubtitleChoiceRow::Unset,
        }
    }
}

/// Load the profile defaults and the series override for one item.
///
/// `profile_id` is `None` for an account-scope caller (playback-info can be
/// read there), which is the no-preference case. Every read failure degrades to
/// the no-preference case with a log rather than failing playback: a preference
/// is an input to selection, not a precondition for it.
pub(crate) fn load_track_preferences(
    state: &AppState,
    profile_id: Option<i64>,
    row: &MediaItemRow,
) -> TrackPreferences {
    let Some(profile_id) = profile_id else {
        return TrackPreferences::none();
    };
    let root = match library_root(state, row.library_id) {
        Ok(root) => root,
        Err(e) => {
            tracing::warn!(item_id = row.id, error = ?e, "library root read failed");
            return TrackPreferences::none();
        }
    };
    let loaded = state.db.with_conn(|conn| {
        let profile = nightjar_db::profile_by_id(conn, profile_id)?;
        // The series key is derived through the one resolver (ADR-0039 item 5).
        let series_key = nightjar_metadata::series_key_for_item(
            conn,
            row.id,
            row.library_id,
            &row.path,
            &root,
            &row.kind,
        )?;
        let choice = nightjar_db::load_track_choice(conn, profile_id, &series_key)?;
        Ok((profile, choice))
    });
    match loaded {
        Ok((Some(profile), choice)) => TrackPreferences {
            preferred_language: profile.preferred_language,
            subtitle_default: profile.subtitle_default,
            audio_description: choice.as_ref().and_then(|c| c.audio.clone()),
            subtitle_choice: choice
                .map(|c| c.subtitle)
                .unwrap_or(SubtitleChoiceRow::Unset),
        },
        Ok((None, _)) => TrackPreferences::none(),
        Err(e) => {
            tracing::warn!(item_id = row.id, error = %e, "track preference read failed");
            TrackPreferences::none()
        }
    }
}

/// Audio precedence (ADR-0038 item 6): stored description, then profile
/// default, then the ADR-0024 rank rule, then the audio last resort. A stored
/// description that matches nothing falls through with the ranker's reason.
pub(crate) fn choose_audio_track(
    candidates: &[TrackCandidate],
    prefs: &TrackPreferences,
) -> TrackSelection {
    if let Some(desc) = prefs.audio_description.as_ref()
        && let Some(selection) = select_audio_for_description(
            candidates,
            desc.language.as_deref(),
            &desc.kind,
            desc.sdh,
            desc.forced,
        )
    {
        return selection;
    }
    select_audio_track(candidates, prefs.preferred_language.as_deref())
}

/// Subtitle precedence (ADR-0038 item 6). `off` selects none; `track` restricts
/// candidates and ranks, falling through to the profile default when nothing
/// matches; `unset` uses the profile default directly. The profile default
/// `off` selects none, and `auto` runs the ADR-0024 rule.
pub(crate) fn choose_subtitle_track(
    candidates: &[TrackCandidate],
    prefs: &TrackPreferences,
    audio_language: Option<&str>,
) -> TrackSelection {
    match &prefs.subtitle_choice {
        SubtitleChoiceRow::Off => TrackSelection {
            track_id: None,
            reason: "subtitles off for this series".to_string(),
        },
        SubtitleChoiceRow::Track(desc) => {
            if let Some(selection) = select_subtitle_for_description(
                candidates,
                desc.language.as_deref(),
                &desc.kind,
                desc.sdh,
                desc.forced,
            ) {
                return selection;
            }
            profile_subtitle_default(candidates, prefs, audio_language)
        }
        SubtitleChoiceRow::Unset => profile_subtitle_default(candidates, prefs, audio_language),
    }
}

fn profile_subtitle_default(
    candidates: &[TrackCandidate],
    prefs: &TrackPreferences,
    audio_language: Option<&str>,
) -> TrackSelection {
    if prefs.subtitle_default == "off" {
        return TrackSelection {
            track_id: None,
            reason: "subtitles off by profile default".to_string(),
        };
    }
    select_subtitle_track(
        candidates,
        prefs.preferred_language.as_deref(),
        audio_language,
    )
}

/// A resolved audio stream plus the reason and language selection produced.
pub(crate) struct ResolvedAudio {
    pub selection: AudioSelection,
    /// The selected track's language, for the subtitle forced rule.
    pub language: Option<String>,
    pub reason: String,
}

/// Which audio stream this session maps (ADR-0012 / ADR-0024 / ADR-0038).
fn resolve_audio(
    state: &AppState,
    row: &MediaItemRow,
    requested: Option<&str>,
    profile: &ClientCapabilityProfile,
    prefs: &TrackPreferences,
) -> Result<ResolvedAudio, ApiError> {
    let max_channels = profile.max_audio_channels.unwrap_or(u32::MAX);
    let root = library_root(state, row.library_id)?;
    let tracks = match list_audio_tracks(&abs_path(&root, &row.path)) {
        Ok(tracks) => tracks,
        // Without a requested track the stored first-audio count still
        // applies the ceiling, so a failed inventory need not fail playback.
        Err(e) if requested.is_none() => {
            tracing::warn!(item_id = row.id, error = %e, "audio track list failed at session start");
            return Ok(ResolvedAudio {
                selection: AudioSelection {
                    stream_index: None,
                    channels: stored_channels(row),
                    channel_layout: None,
                    max_channels,
                },
                language: None,
                reason: "audio track list unavailable".to_string(),
            });
        }
        Err(e) => return Err(ApiError::internal(e)),
    };

    let (track, language, reason) = match requested {
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
            (
                Some(t),
                t.language.clone(),
                "client requested audioTrackId".to_string(),
            )
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
            let sel = choose_audio_track(&candidates, prefs);
            tracing::info!(
                item_id = row.id,
                track_id = sel.track_id.as_deref().unwrap_or("-"),
                reason = %sel.reason,
                "audio track selected"
            );
            let t = sel
                .track_id
                .as_deref()
                .and_then(|id| tracks.iter().find(|t| t.track_id() == id));
            let language = t.and_then(|t| t.language.clone());
            (t, language, sel.reason)
        }
    };
    let selection = match track {
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
    };
    Ok(ResolvedAudio {
        selection,
        language,
        reason,
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
/// The session's ffmpeg side output maps exactly one absolute input stream
/// (`-map 0:{stream_index}` + `-c:s webvtt`): a broad `0:s?` selector, an
/// image or unknown stream, or a second text stream would fail the whole
/// session. Select the single embedded text or ASS track and fail closed when
/// the inventory holds none, holds more than one text/ASS track, or its stream
/// index is not a nonnegative absolute index. Every other `eligible` item
/// stays `eligible` for a standalone or later pass. Returns the target to
/// publish on completion.
fn piggyback_track_for(status: &str, tracks: &[SubtitleTrackRow]) -> Option<PiggybackExtract> {
    if status != "eligible" {
        return None;
    }
    let mut texts = tracks
        .iter()
        .filter(|t| matches!(t.kind.as_str(), "text" | "ass"));
    let t = texts.next()?;
    if texts.next().is_some() {
        return None;
    }
    let stream_index = u32::try_from(t.stream_index).ok()?;
    Some(PiggybackExtract {
        track_id: format!("e{stream_index}"),
        stream_index,
    })
}

fn snapshot_hls_tracks(
    state: &AppState,
    row: &MediaItemRow,
    library_root: &str,
    tracks: &[crate::routes::items::SubtitleTrackDto],
    prefs: &TrackPreferences,
    audio_language: Option<&str>,
) -> Result<(Vec<HlsSubtitleTrack>, String), String> {
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
    // The selected audio track's language decides the forced rule, so it is the
    // language the session actually chose, not a second guess (ADR-0024 §2.3).
    let sub_sel = choose_subtitle_track(&sub_cands, prefs, audio_language);
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
    Ok((out, sub_sel.reason))
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
    // The map-fallback branch reads the item and pushes a priority rebuild, so
    // the whole handler blocks, not just `hls.seek`.
    blocking(move || seek_blocking(state, session_id, query)).await
}

fn seek_blocking(
    state: AppState,
    session_id: String,
    query: SeekQuery,
) -> ApiResult<(StatusCode, Json<TranscodeSessionDto>)> {
    let start_ms = query.start_ms;
    let result = state.hls.seek(&session_id, start_ms);
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

/// The session's master playlist (ADR-0054 decision 5).
///
/// One URI for the life of the session. A far seek mints a new run and the
/// client re-attaches to this same URI, where it re-reads the media playlist and
/// the `EXT-X-MAP` that names the new run's init.
pub async fn master(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> ApiResult<Response> {
    wait_playlist(state, session_id, PlaylistKind::Master).await
}

/// The session's media playlist for the single production rung
/// (ADR-0054 decision 5).
///
/// `EXT-X-MAP` inside it is per run, because the init carries the land in its
/// `elst` empty edit (decision 4, overturned 2026-08-31). That is the one thing
/// in this response that changes between two fetches of the same URI.
///
/// This flat route stays served so a client that attached before the master
/// advertised the rung namespace keeps playing; it resolves to the single
/// rung, byte-identical to `/v/single/index.m3u8`.
pub async fn playlist(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> ApiResult<Response> {
    wait_playlist(state, session_id, PlaylistKind::Media).await
}

/// The session's media playlist for a named video rung (ADR-0051 amendment 1).
///
/// The parsed rung is used, not just validated: the bytes come from that
/// rung's segment map and name that rung's segment URIs, so a rung-scoped URL
/// cannot silently resolve to the single rendition. Validation happens first:
/// an unknown name must never select a rendition implicitly.
pub async fn rung_playlist(
    State(state): State<AppState>,
    Path((session_id, rung_name)): Path<(String, String)>,
) -> ApiResult<Response> {
    let rung = require_known_rung(&rung_name)?;
    wait_playlist(state, session_id, PlaylistKind::RungMedia(rung)).await
}

pub async fn run_init(
    State(state): State<AppState>,
    Path((session_id, run_id)): Path<(String, u64)>,
) -> ApiResult<Response> {
    let hls = Arc::clone(&state.hls);
    let sid = session_id.clone();
    // The flat `runs/{run_id}/init.mp4` URI carries no rung, and the run ids
    // it names are the sole production rung's per-rung counters. `run_asset`
    // needs the rung explicitly because two rungs each have a run 3.
    let result = tokio::task::spawn_blocking(move || {
        hls.run_asset(&sid, VideoRung::SingleVideo, run_id, "init.mp4")
    })
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
                    code: "not_ready",
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
                    code: "not_ready",
                }),
                Err(other) => map_playlist_err(&session_id, other),
            }
        }
    }
}

async fn wait_playlist(
    state: AppState,
    session_id: String,
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
                PlaylistKind::Master => hls.master(&sid),
                PlaylistKind::Media => hls.playlist(&sid),
                PlaylistKind::RungMedia(rung) => hls.rung_playlist(&sid, rung),
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
                PlaylistKind::Master => "master.m3u8".to_string(),
                PlaylistKind::Media => "index.m3u8".to_string(),
                PlaylistKind::RungMedia(rung) => {
                    format!("v/{}/index.m3u8", rung.as_str())
                }
            };
            log_hls_client_req(&session_id, &resource, None, 200, None);
            m3u8_ok(bytes)
        }
        Err(e) => {
            let resource = match kind {
                PlaylistKind::Master => "master.m3u8".to_string(),
                PlaylistKind::Media => "index.m3u8".to_string(),
                PlaylistKind::RungMedia(rung) => {
                    format!("v/{}/index.m3u8", rung.as_str())
                }
            };
            let status = match &e {
                PlaylistError::NotFound => 404,
                PlaylistError::NotReady => 503,
                PlaylistError::AbandonedHoldEnded => 204,
                PlaylistError::Failed(_) => 500,
            };
            log_hls_client_req(&session_id, &resource, None, status, None);
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
            code: "not_ready",
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

/// Response for one session asset, with the caching each kind can justify.
///
/// **The route set `Content-Type` and nothing else until 2026-08-31.** A
/// response with no validator and no cache directive is subject to heuristic
/// freshness: a browser or a proxy may keep it as long as it likes and reuse
/// it without asking. The playlist route has said `no-cache` since it was
/// written (`m3u8_ok`); the assets said nothing.
///
/// The route serves exactly two shapes, and `hls::is_safe_asset` is what
/// closes that set: `init.mp4`, and `seg_<ms:011>.m4s`. There is no `.ts` —
/// every mode muxes fMP4 (`-hls_segment_type fmp4`), copy included.
///
/// **Both get `no-cache`, and the reasons are different.** They are written
/// out because "one header for the route" is the answer that would be reached
/// by not asking, and one of these two reasons is about to change.
///
/// - **`init.mp4`.** Under today's per-run URI it is immutable, and the URI
///   changing per run is the only thing that has been preventing a stale init
///   being served. **ADR-0054 decision 5 makes the map session-scoped**, at
///   which point the URI stops changing and the bytes it names become
///   whatever the current run wrote. The header has to be right before that
///   protection's job is removed, not after. **A wrong init is a decode
///   failure, not a stale listing.**
///
/// - **A segment.** Its bytes for a given URI are **not** immutable, which is
///   the surprise here. The session map is keyed on title-absolute start and
///   `SegmentMap::insert` replaces: *"a newer run that produces a different
///   packing at the same start replaces the prior entry"*. So one
///   `seg_<ms>.m4s` can resolve to a different run's file within one session.
///   Whether a cached older copy still decodes depends on init being
///   interchangeable across runs, which is **measured for QSV transcode only**
///   (ADR-0054 decision 4), recorded as false for VideoToolbox, and unmeasured
///   for copy. `no-cache` is the choice that does not rest on that.
///
/// `no-cache` permits storing and requires revalidation. With no validator on
/// this route a revalidation is a full re-fetch, which costs nothing a player
/// was not already doing: players do not re-request what they have buffered.
/// **An `ETag` would make revalidation cheap and is the obvious next step**;
/// it is not this change, because it is an addition rather than a correction.
fn asset_ok(asset: &str, bytes: Vec<u8>) -> Response {
    let mime = if asset.ends_with(".mp4") {
        "video/mp4"
    } else {
        "video/iso.segment"
    };
    let mut res = Response::new(Body::from(bytes));
    *res.status_mut() = StatusCode::OK;
    res.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    res
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

/// The session-scoped `init.mp4`.
///
/// The playlist points at the run-scoped `runs/{run_id}/init.mp4` instead, so
/// this is the shape a client that kept an older URI asks for. It stays served
/// and it stays named, because the point of issue #96 is that the router says
/// which shapes exist rather than a handler deciding after the fact.
pub async fn session_init(
    state: State<AppState>,
    Path(session_id): Path<String>,
    query: Query<AssetQuery>,
) -> ApiResult<Response> {
    asset(state, session_id, "init.mp4".to_string(), query).await
}

/// One media segment, `seg_<start_ms>.m4s` (ADR-0020 time-keyed names).
///
/// Still a capture rather than the shape it serves, because axum cannot route
/// `seg_{start_ms}.m4s` — a path segment is static or a parameter, not both.
/// `hls::is_safe_asset` is therefore the thing that closes the set, and it has
/// a test pinning it to exactly these two names so a third cannot arrive
/// unremarked (issue #96).
pub async fn segment(
    state: State<AppState>,
    Path((session_id, asset_name)): Path<(String, String)>,
    query: Query<AssetQuery>,
) -> ApiResult<Response> {
    asset(state, session_id, asset_name, query).await
}

/// One media segment for a named video rung (ADR-0051 amendment 1).
///
/// The parsed rung is used, not just validated: the bytes are resolved against
/// that rung's segment map, so a rung-scoped segment URL cannot silently serve
/// the single rendition's file at the same title time. This capture is
/// narrower than the top-level session capture: the rung URI grammar contains
/// only `seg_<ms:011>.m4s`, not `init.mp4`. Axum cannot mix the static segment
/// spelling with a path parameter, so the handler closes the capture with the
/// segment-name parser before serving.
pub async fn rung_segment(
    state: State<AppState>,
    Path((session_id, rung_name, asset_name)): Path<(String, String, String)>,
    query: Query<AssetQuery>,
) -> ApiResult<Response> {
    let rung = require_known_rung(&rung_name)?;
    if parse_time_keyed_segment_name(&asset_name).is_none() {
        return Err(ApiError::not_found(format!(
            "asset {asset_name} for rung {rung_name} not found"
        )));
    }
    rung_asset(state, session_id, rung, asset_name, query).await
}

fn require_known_rung(rung_name: &str) -> ApiResult<VideoRung> {
    rung_name
        .parse()
        .map_err(|()| ApiError::not_found(format!("rung {rung_name} not found")))
}

async fn asset(
    State(state): State<AppState>,
    session_id: String,
    asset: String,
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
    map_asset_outcome(&session_id, &asset, fetcher_for_log.as_deref(), result)
}

/// The rung-scoped twin of [`asset`]: resolves against the named rung's map
/// instead of the single production rung's. Error handling is shared with the
/// flat path — one response policy for both namespaces (Rule 4.11).
async fn rung_asset(
    state: State<AppState>,
    session_id: String,
    rung: VideoRung,
    asset: String,
    query: Query<AssetQuery>,
) -> ApiResult<Response> {
    let hls = Arc::clone(&state.hls);
    let sid = session_id.clone();
    let name = asset.clone();
    let fetcher = query.nj_fetcher.clone();
    let fetcher_for_log = fetcher.clone();
    let result =
        tokio::task::spawn_blocking(move || hls.rung_asset(&sid, rung, &name, fetcher.as_deref()))
            .await
            .map_err(|e| ApiError::internal(format!("hls rung asset task: {e}")))?;
    map_asset_outcome(&session_id, &asset, fetcher_for_log.as_deref(), result)
}

/// Maps a transcode asset outcome to an HTTP response and logs it. Shared by
/// the flat and rung-scoped asset routes so the two namespaces cannot drift.
fn map_asset_outcome(
    session_id: &str,
    asset: &str,
    fetcher_ref: Option<&str>,
    result: Result<Vec<u8>, PlaylistError>,
) -> ApiResult<Response> {
    match result {
        Ok(bytes) => {
            log_hls_client_req(session_id, asset, None, 200, fetcher_ref);
            Ok(asset_ok(asset, bytes))
        }
        Err(PlaylistError::NotFound) => {
            log_hls_client_req(session_id, asset, None, 404, fetcher_ref);
            Err(ApiError::not_found(format!(
                "asset {asset} for session {session_id} not found"
            )))
        }
        // Not yet on disk: ask the player to retry. 404 makes hls.js / Safari
        // give up on the fragment; 503 is recoverable while FFmpeg catches up.
        Err(PlaylistError::NotReady) => {
            log_hls_client_req(session_id, asset, None, 503, fetcher_ref);
            Err(ApiError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: format!("asset {asset} for session {session_id} not ready yet"),
                code: "not_ready",
            })
        }
        // Abandoned / superseded hold ceiling: empty 204 (ADR-0011 §7).
        Err(PlaylistError::AbandonedHoldEnded) => {
            log_hls_client_req(session_id, asset, None, 204, fetcher_ref);
            let mut res = Response::new(Body::empty());
            *res.status_mut() = StatusCode::NO_CONTENT;
            Ok(res)
        }
        Err(PlaylistError::Failed(e)) => {
            log_hls_client_req(session_id, asset, None, 500, fetcher_ref);
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

    fn response_error(result: ApiResult<Response>) -> (StatusCode, String) {
        match result {
            Ok(response) => panic!("expected an error, got {}", response.status()),
            Err(error) => (error.status, error.message),
        }
    }

    /// The single rung's playlist route and the flat route answer a missing
    /// session identically, because both resolve to the same rung's map.
    ///
    /// This is the compat half of the rung namespace (the old URLs keep
    /// working). The half that proves the parsed rung is *used* — a rung-scoped
    /// request resolving against that rung's map rather than rung one's — needs
    /// a live two-rung session and lives in the transcode crate, where the
    /// existing `#[cfg(test)]` `SecondVideo` can build one without widening
    /// production visibility (plan 2026-09-05-the-ladder-s8, answer B).
    #[tokio::test]
    async fn single_rung_and_flat_playlist_answer_a_missing_session_identically() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::state::test_support::state(dir.path());
        let session_id = "missing".to_string();

        let top_level =
            response_error(playlist(State(state.clone()), Path(session_id.clone())).await);
        let rung = response_error(
            rung_playlist(
                State(state),
                Path((session_id, VideoRung::SingleVideo.as_str().to_string())),
            )
            .await,
        );

        assert_eq!(rung, top_level);
    }

    /// The single rung's segment route and the flat route answer a missing
    /// session identically. The rung-resolution behaviour itself is pinned in
    /// the transcode crate, against a live two-rung session.
    #[tokio::test]
    async fn single_rung_and_flat_segment_answer_a_missing_session_identically() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::state::test_support::state(dir.path());
        let session_id = "missing".to_string();
        let asset_name = "seg_00000002000.m4s".to_string();
        let query = || {
            Query(AssetQuery {
                nj_fetcher: Some("rung-test".to_string()),
            })
        };

        let top_level = response_error(
            segment(
                State(state.clone()),
                Path((session_id.clone(), asset_name.clone())),
                query(),
            )
            .await,
        );
        let rung = response_error(
            rung_segment(
                State(state),
                Path((
                    session_id,
                    VideoRung::SingleVideo.as_str().to_string(),
                    asset_name,
                )),
                query(),
            )
            .await,
        );

        assert_eq!(rung, top_level);
    }

    #[tokio::test]
    async fn unknown_rung_never_resolves_to_the_single_rendition() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::state::test_support::state(dir.path());
        let unknown = "future-rung".to_string();

        let playlist_error = response_error(
            rung_playlist(
                State(state.clone()),
                Path(("missing".to_string(), unknown.clone())),
            )
            .await,
        );
        let segment_error = response_error(
            rung_segment(
                State(state),
                Path((
                    "missing".to_string(),
                    unknown.clone(),
                    "seg_00000002000.m4s".to_string(),
                )),
                Query(AssetQuery { nj_fetcher: None }),
            )
            .await,
        );

        for error in [playlist_error, segment_error] {
            assert_eq!(error.0, StatusCode::NOT_FOUND);
            assert_eq!(error.1, format!("rung {unknown} not found"));
        }
    }

    #[tokio::test]
    async fn rung_segment_rejects_everything_but_a_time_keyed_segment() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::state::test_support::state(dir.path());

        for asset_name in ["init.mp4", "seg_2000.m4s", "anything"] {
            let error = response_error(
                rung_segment(
                    State(state.clone()),
                    Path((
                        "missing".to_string(),
                        VideoRung::SingleVideo.as_str().to_string(),
                        asset_name.to_string(),
                    )),
                    Query(AssetQuery { nj_fetcher: None }),
                )
                .await,
            );
            assert_eq!(error.0, StatusCode::NOT_FOUND, "{asset_name}");
        }
    }

    /// The header is read off the response the route builds, not matched in
    /// the source. A test that greps for a string proves nothing about what
    /// a client receives.
    fn cache_control_of(asset: &str) -> String {
        let res = asset_ok(asset, vec![0u8; 8]);
        res.headers()
            .get(header::CACHE_CONTROL)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default()
    }

    /// `init.mp4` must revalidate.
    ///
    /// Its URI changes per run today, and that is the only thing stopping a
    /// stale init being served. ADR-0054 decision 5 removes that by making the
    /// map session-scoped, so the header has to be right before the accidental
    /// protection's job goes. A wrong init is a decode failure.
    #[test]
    fn init_is_not_cacheable_without_revalidation() {
        assert_eq!(cache_control_of("init.mp4"), "no-cache");
        let res = asset_ok("init.mp4", vec![0u8; 8]);
        assert_eq!(
            res.headers().get(header::CONTENT_TYPE).unwrap(),
            "video/mp4",
            "the kind is still distinguished; only the caching was missing"
        );
    }

    /// A segment must revalidate too, and for a different reason: its bytes
    /// for a given URI are not immutable. See
    /// `a_segment_uri_is_not_immutable_within_a_session` in the transcode
    /// crate, which pins the property this rests on.
    #[test]
    fn a_segment_is_not_cacheable_without_revalidation() {
        assert_eq!(cache_control_of("seg_00000042000.m4s"), "no-cache");
        let res = asset_ok("seg_00000042000.m4s", vec![0u8; 8]);
        assert_eq!(
            res.headers().get(header::CONTENT_TYPE).unwrap(),
            "video/iso.segment"
        );
    }

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
    /// Image and unknown streams do not block the selection; they are simply
    /// never mapped. More than one text/ASS track (or a negative index) fails
    /// closed rather than falling back to a broad subtitle map.
    #[test]
    fn piggyback_track_gate() {
        type GateCase<'a> = (&'a str, &'a [SubtitleTrackRow], Option<(&'a str, u32)>);
        let cases: &[GateCase] = &[
            // Not eligible: no piggyback, whatever the inventory says.
            ("ready", &[row(2, "text")], None),
            ("none", &[row(2, "text")], None),
            ("pending", &[row(2, "text")], None),
            // Eligible with exactly one text track → piggyback it.
            ("eligible", &[row(2, "text")], Some(("e2", 2))),
            ("eligible", &[row(7, "ass")], Some(("e7", 7))),
            // Image and unknown streams never enter the map, and they do not
            // hide the one text track beside them.
            (
                "eligible",
                &[row(2, "text"), row(3, "image")],
                Some(("e2", 2)),
            ),
            (
                "eligible",
                &[
                    row(2, "text"),
                    row(3, "image"),
                    row(4, "image"),
                    row(5, "image"),
                ],
                Some(("e2", 2)),
            ),
            // Two text tracks are ambiguous: fail closed, never map both.
            ("eligible", &[row(2, "text"), row(3, "text")], None),
            (
                "eligible",
                &[row(2, "text"), row(3, "image"), row(4, "ass")],
                None,
            ),
            ("eligible", &[row(2, "unknown")], None),
            ("eligible", &[row(2, "image")], None),
            // A negative stream index cannot name an absolute input stream.
            ("eligible", &[row(-1, "text")], None),
            // Eligible with no embedded rows (sidecar-only) has no side output.
            ("eligible", &[], None),
        ];
        for (status, tracks, expected) in cases {
            let got = piggyback_track_for(status, tracks).map(|p| (p.track_id, p.stream_index));
            assert_eq!(
                got.as_ref().map(|(id, i)| (id.as_str(), *i)),
                *expected,
                "status={status} tracks={tracks:?}"
            );
        }
    }
}

/// ADR-0034 item 8, end to end through the real router.
///
/// Two distinct accounts, each with its own profile, drive real requests. The
/// first creates a real session; the second must be answered exactly as if the
/// session did not exist. The owner controls prove the refusal is ownership
/// and not a handler that happens to be broken for everyone.
#[cfg(test)]
mod ownership_tests {
    use crate::authority::SESSION_COOKIE;
    use crate::routes::router;
    use crate::routes::sessions::{ACCOUNT_CEILING_REFUSED_CODE, ADMISSION_REFUSED_CODE};
    use crate::state::{AppState, test_support};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use nightjar_auth::{mint_profile_ref, mint_session_token};
    use nightjar_db::{NewLibrary, ObservedSidecar, ProbeUpdate, UpsertItem};
    use tower::ServiceExt;

    struct Actor {
        account_id: i64,
        profile_id: i64,
    }

    fn actor(state: &AppState, username: &str) -> Actor {
        let profile_ref = mint_profile_ref();
        state
            .db
            .with_conn(|conn| {
                let hash = nightjar_auth::hash_password("x").map_err(|e| format!("hash: {e:?}"))?;
                nightjar_db::create_account_with_profile(
                    conn,
                    username,
                    &hash,
                    "member",
                    "P",
                    &profile_ref,
                )?;
                let account = nightjar_db::account_by_username(conn, username)?.unwrap();
                let profile = nightjar_db::profile_by_ref(conn, &profile_ref)?.unwrap();
                Ok((account.id, profile.id))
            })
            .map(|(account_id, profile_id)| Actor {
                account_id,
                profile_id,
            })
            .unwrap()
    }

    /// A profile-scoped session token for this actor.
    fn token_for(state: &AppState, actor: &Actor) -> String {
        let minted = mint_session_token();
        state
            .db
            .with_conn(|conn| {
                let expires = nightjar_db::session_expiry(conn)?;
                let session = nightjar_db::create_session(
                    conn,
                    actor.account_id,
                    &minted.sha256_hex,
                    "t",
                    &expires,
                )?;
                nightjar_db::set_active_profile(conn, session, Some(actor.profile_id))?;
                Ok(())
            })
            .unwrap();
        minted.plaintext
    }

    /// A second profile on `account_id`, plus a profile-scoped token for it.
    ///
    /// `actor` mints an account and its first profile together. This adds a
    /// sibling profile to an account that already exists: the account is the
    /// same, the watching identity is not (ADR-0034 item 8).
    fn second_profile_token(state: &AppState, account_id: i64) -> String {
        let profile_ref = mint_profile_ref();
        let minted = mint_session_token();
        state
            .db
            .with_conn(|conn| {
                let profile_id = nightjar_db::create_profile(
                    conn,
                    account_id,
                    &profile_ref,
                    "second",
                    None,
                    false,
                    None,
                    "auto",
                )?;
                let expires = nightjar_db::session_expiry(conn)?;
                let session = nightjar_db::create_session(
                    conn,
                    account_id,
                    &minted.sha256_hex,
                    "t",
                    &expires,
                )?;
                nightjar_db::set_active_profile(conn, session, Some(profile_id))?;
                Ok(())
            })
            .unwrap();
        minted.plaintext
    }

    fn ffmpeg_available() -> bool {
        let ok = std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success());
        if !ok && std::env::var_os("NIGHTJAR_TEST_REQUIRE_FFMPEG").is_some() {
            panic!("NIGHTJAR_TEST_REQUIRE_FFMPEG is set but ffmpeg is not on PATH");
        }
        ok
    }

    /// A tiny real H.264/AAC Matroska, so the session path spawns a real
    /// FFmpeg rather than a mock that could hide an ownership gap.
    fn write_fixture(dir: &std::path::Path) -> bool {
        let out = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=160x120:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440",
                "-t",
                "4",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-shortest",
            ])
            .arg("-y")
            .arg(dir.join("in.mkv"))
            .output();
        out.is_ok_and(|o| o.status.success())
    }

    /// A library, a probed item, and a complete sidecar subtitle track. The
    /// container forces a stream-copy session (BROWSER_V0 cannot read mkv),
    /// which is enough to prove the ownership boundary for every session
    /// resource.
    fn seed_item(state: &AppState, dir: &std::path::Path) -> i64 {
        let library = state
            .db
            .create_library(&NewLibrary {
                name: "movies".to_string(),
                path: dir.to_string_lossy().into_owned(),
                kind: "movies".to_string(),
            })
            .unwrap();
        let ids = state
            .db
            .upsert_items_indexed(
                library.id,
                &[UpsertItem {
                    path: "in.mkv".to_string(),
                    mtime_ms: 0,
                    size_bytes: 1,
                    title: "in".to_string(),
                    kind: "movie".to_string(),
                    year: None,
                    season: None,
                    episode: None,
                    content_id: None,
                }],
            )
            .unwrap();
        let item_id = ids[0];
        state
            .db
            .apply_probe_update(&ProbeUpdate {
                item_id,
                duration_ms: Some(4_000),
                container: Some("mkv".to_string()),
                video_codec: Some("h264".to_string()),
                audio_codec: Some("aac".to_string()),
                audio_channels: Some(2),
                width: Some(160),
                height: Some(120),
                video_bitrate_bps: Some(200_000),
                video_frame_rate_num: Some(10),
                video_frame_rate_den: Some(1),
                hdr: None,
                probe_status: "probed".to_string(),
                scan_error: None,
            })
            .unwrap();
        state.db.set_subtitle_status(item_id, "ready").unwrap();
        state
            .db
            .reconcile_item_sidecars(
                item_id,
                &[ObservedSidecar {
                    track_id: "s-en".to_string(),
                    path: "subs/s-en.vtt".to_string(),
                    mtime_ms: 0,
                    size_bytes: 1,
                    format: "vtt".to_string(),
                    language: None,
                    forced: false,
                    sdh: false,
                    content_id: "1-first-last".to_string(),
                }],
            )
            .unwrap();
        state
            .subs
            .publish_item_vtt(
                item_id,
                "s-en",
                "WEBVTT\n\n00:00:00.000 --> 00:00:02.000\nhi\n",
            )
            .unwrap();
        item_id
    }

    async fn send(
        state: AppState,
        method: &str,
        uri: &str,
        credential: Option<(&str, &str)>,
        body: &str,
    ) -> (StatusCode, String) {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        if let Some((name, value)) = credential {
            builder = builder.header(name, value);
        }
        let response = router(state)
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let text = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        (status, text)
    }

    fn bearer(token: &str) -> (String, String) {
        ("authorization".to_string(), format!("Bearer {token}"))
    }

    fn cookie(token: &str) -> (String, String) {
        ("cookie".to_string(), format!("{SESSION_COOKIE}={token}"))
    }

    /// The segment basename the playlist actually names. Guessing the
    /// producer's first land would pin the test to an encoder detail; the
    /// playlist is the contract a client reads.
    fn first_segment_basename(playlist: &str) -> Option<String> {
        playlist
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with('#'))
            .find_map(|line| {
                let name = line.rsplit('/').next().unwrap_or(line);
                name.ends_with(".m4s").then(|| name.to_string())
            })
    }

    fn field<'a>(json: &'a str, key: &str) -> &'a str {
        let at = json
            .find(&format!("\"{key}\":\""))
            .unwrap_or_else(|| panic!("no {key} in {json}"))
            + key.len()
            + 4;
        let end = json[at..].find('"').unwrap() + at;
        &json[at..end]
    }

    /// Creates a session as `owner` and returns its id.
    async fn start_session(state: &AppState, owner_token: &str, item_id: i64) -> String {
        let (bearer_name, bearer_value) = bearer(owner_token);
        let (status, body) = send(
            state.clone(),
            "POST",
            &format!("/api/v0/items/{item_id}/sessions"),
            Some((&bearer_name, &bearer_value)),
            "",
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "session start: {body}");
        field(&body, "sessionId").to_string()
    }

    /// ADR-0034 item 8 through the real router: the account ceiling counts
    /// across sibling profiles, and the refusal is a typed 503 whose code is
    /// distinct from measured-admission refusal.
    #[tokio::test]
    async fn the_account_ceiling_is_a_typed_503_distinct_from_admission() {
        let dir = tempfile::tempdir().unwrap();
        // Operator admission is above the account cap, so the account ceiling
        // is the binding limit and its typed code is what a client sees.
        let state = test_support::state_with_encoder_cap(dir.path(), 4);
        if !ffmpeg_available() || !write_fixture(dir.path()) {
            eprintln!("skipping: ffmpeg unavailable");
            return;
        }
        let owner = actor(&state, "capped");
        state
            .db
            .with_conn(|conn| {
                nightjar_db::update_account_playback_policy(
                    conn,
                    owner.account_id,
                    Some(1),
                    None,
                    None,
                )
            })
            .unwrap();
        let owner_token = token_for(&state, &owner);
        let sibling_token = second_profile_token(&state, owner.account_id);
        let item_id = seed_item(&state, dir.path());

        let _first = start_session(&state, &owner_token, item_id).await;

        // The sibling profile shares the account's cap and is refused.
        let (name, value) = bearer(&sibling_token);
        let (status, body) = send(
            state.clone(),
            "POST",
            &format!("/api/v0/items/{item_id}/sessions"),
            Some((&name, &value)),
            "",
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert!(
            body.contains(ACCOUNT_CEILING_REFUSED_CODE),
            "the refusal names the account ceiling: {body}"
        );
        assert!(
            !body.contains(ADMISSION_REFUSED_CODE),
            "the account ceiling is not measured admission: {body}"
        );
    }

    /// ADR-0034 item 8 through the real router: an explicit `replacesSessionId`
    /// lets a successor start at the account cap, retires the predecessor's
    /// playback authority, and refuses an unknown or another profile's
    /// predecessor with the same missing-session 404.
    #[tokio::test]
    async fn explicit_replaces_session_id_retires_the_predecessor() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state_with_encoder_cap(dir.path(), 4);
        if !ffmpeg_available() || !write_fixture(dir.path()) {
            eprintln!("skipping: ffmpeg unavailable");
            return;
        }
        let owner = actor(&state, "owner");
        state
            .db
            .with_conn(|conn| {
                nightjar_db::update_account_playback_policy(
                    conn,
                    owner.account_id,
                    Some(1),
                    None,
                    None,
                )
            })
            .unwrap();
        let owner_token = token_for(&state, &owner);
        let sibling_token = second_profile_token(&state, owner.account_id);
        let item_id = seed_item(&state, dir.path());
        let first = start_session(&state, &owner_token, item_id).await;
        let (name, value) = bearer(&owner_token);
        let start_uri = format!("/api/v0/items/{item_id}/sessions");

        // No reference at the cap: a same-identity POST is a new playback and
        // refuses. There is no implicit replacement.
        let (status, body) =
            send(state.clone(), "POST", &start_uri, Some((&name, &value)), "").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert!(body.contains(ACCOUNT_CEILING_REFUSED_CODE), "{body}");

        // An unknown reference is the missing-session 404, not a ceiling
        // refusal.
        let unknown = format!("{start_uri}?replacesSessionId=nope");
        let (status, body) = send(state.clone(), "POST", &unknown, Some((&name, &value)), "").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

        // A sibling profile cannot name another profile's session.
        let (sibling_name, sibling_value) = bearer(&sibling_token);
        let named = format!("{start_uri}?replacesSessionId={first}");
        let (status, body) = send(
            state.clone(),
            "POST",
            &named,
            Some((&sibling_name, &sibling_value)),
            "",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

        // The owner's explicit replacement starts at the cap.
        let (status, body) = send(state.clone(), "POST", &named, Some((&name, &value)), "").await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let second = field(&body, "sessionId").to_string();
        assert_ne!(second, first);

        // The predecessor lost playback authority: its playlist is the same
        // 404 as a missing session.
        let (status, _) = send(
            state.clone(),
            "GET",
            &format!("/api/v0/sessions/{first}/master.m3u8"),
            Some((&name, &value)),
            "",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// The full session surface, in the order the router serves it. Every
    /// entry is a GET so the cookie transport can be exercised on each.
    fn get_routes(session_id: &str) -> Vec<String> {
        vec![
            format!("/api/v0/sessions/{session_id}"),
            format!("/api/v0/sessions/{session_id}/master.m3u8"),
            format!("/api/v0/sessions/{session_id}/index.m3u8"),
            format!("/api/v0/sessions/{session_id}/v/single/index.m3u8"),
            format!("/api/v0/sessions/{session_id}/runs/0/init.mp4"),
            format!("/api/v0/sessions/{session_id}/init.mp4"),
            format!("/api/v0/sessions/{session_id}/subs/s-en.m3u8"),
        ]
    }

    /// The second account cannot inspect, seek, or read any session resource,
    /// and the owner is not blocked by the boundary that refuses the second.
    #[tokio::test]
    async fn a_second_profile_cannot_reach_another_profiles_session() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        if !ffmpeg_available() || !write_fixture(dir.path()) {
            eprintln!("skipping: ffmpeg unavailable");
            return;
        }
        let owner = actor(&state, "owner-a");
        let other = actor(&state, "owner-b");
        let owner_token = token_for(&state, &owner);
        let other_token = token_for(&state, &other);
        let item_id = seed_item(&state, dir.path());
        let session_id = start_session(&state, &owner_token, item_id).await;

        // The playlist names the segment. Read it once and cover both the
        // flat and the rung-scoped segment route with that name.
        let index = format!("/api/v0/sessions/{session_id}/index.m3u8");
        let (on, ov) = bearer(&owner_token);
        let (_, playlist_body) = send(state.clone(), "GET", &index, Some((&on, &ov)), "").await;
        let segment = first_segment_basename(&playlist_body)
            .unwrap_or_else(|| panic!("playlist names no segment: {playlist_body}"));
        let mut routes = get_routes(&session_id);
        routes.push(format!("/api/v0/sessions/{session_id}/{segment}"));
        routes.push(format!("/api/v0/sessions/{session_id}/v/single/{segment}"));

        for uri in routes {
            let (owner_name, owner_value) = bearer(&owner_token);
            let (owner_status, owner_body) = send(
                state.clone(),
                "GET",
                &uri,
                Some((&owner_name, &owner_value)),
                "",
            )
            .await;
            assert_eq!(
                owner_status,
                StatusCode::OK,
                "the owner must read {uri}: {owner_body}"
            );

            let (other_name, other_value) = bearer(&other_token);
            let (other_status, _) = send(
                state.clone(),
                "GET",
                &uri,
                Some((&other_name, &other_value)),
                "",
            )
            .await;
            assert_eq!(
                other_status,
                StatusCode::NOT_FOUND,
                "a second profile must not read {uri}"
            );
        }

        // The subtitle segment shares the `subs/{*asset}` route with the
        // playlist. The refusal must come from the ownership boundary, not
        // from the asset parser: the handler's own 404 says `subtitle asset
        // … for session …`, so only the exact missing-session message proves
        // the ownership layer ran first.
        let sub_segment = format!("/api/v0/sessions/{session_id}/subs/s-en/seg000.vtt");
        let (other_name, other_value) = bearer(&other_token);
        let (sub_status, sub_body) = send(
            state.clone(),
            "GET",
            &sub_segment,
            Some((&other_name, &other_value)),
            "",
        )
        .await;
        assert_eq!(
            sub_status,
            StatusCode::NOT_FOUND,
            "a second profile must not read a subtitle segment"
        );
        assert_eq!(
            field(&sub_body, "error"),
            format!("session {session_id} not found"),
            "the refusal must come from the ownership boundary, not the \
             subtitle asset parser: {sub_body}"
        );

        // Seek: the owner's restart applies, the second profile's is a 404.
        let seek = format!("/api/v0/sessions/{session_id}/seek?startMs=1000");
        let (owner_name, owner_value) = bearer(&owner_token);
        let (owner_status, _) = send(
            state.clone(),
            "POST",
            &seek,
            Some((&owner_name, &owner_value)),
            "",
        )
        .await;
        assert_eq!(owner_status, StatusCode::ACCEPTED, "the owner may seek");
        let (other_name, other_value) = bearer(&other_token);
        let (other_status, _) = send(
            state.clone(),
            "POST",
            &seek,
            Some((&other_name, &other_value)),
            "",
        )
        .await;
        assert_eq!(
            other_status,
            StatusCode::NOT_FOUND,
            "a second profile must not seek"
        );
    }

    /// A sibling profile under one account never reaches the session its
    /// account-mate created (ADR-0034 item 8).
    ///
    /// This is the same-account half of the boundary, and the half the
    /// two-account tests cannot see: `session_owner` keys on account *and*
    /// profile, so an account-only key leaves every two-account test green.
    /// Here the account is identical, so an account-only key makes the
    /// sibling's key equal the creator's and turns the sibling's 404 into a
    /// 200 on both routes.
    #[tokio::test]
    async fn a_sibling_profile_cannot_reach_the_sessions_session() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        if !ffmpeg_available() || !write_fixture(dir.path()) {
            eprintln!("skipping: ffmpeg unavailable");
            return;
        }
        let owner = actor(&state, "owner-a");
        let owner_token = token_for(&state, &owner);
        let sibling_token = second_profile_token(&state, owner.account_id);
        let item_id = seed_item(&state, dir.path());
        let session_id = start_session(&state, &owner_token, item_id).await;

        let routes = [
            format!("/api/v0/sessions/{session_id}"),
            format!("/api/v0/sessions/{session_id}/index.m3u8"),
        ];
        for uri in routes {
            let (sibling_name, sibling_value) = bearer(&sibling_token);
            let (refused, refused_body) = send(
                state.clone(),
                "GET",
                &uri,
                Some((&sibling_name, &sibling_value)),
                "",
            )
            .await;
            assert_eq!(
                refused,
                StatusCode::NOT_FOUND,
                "a sibling profile must not read {uri}: {refused_body}"
            );

            let (owner_name, owner_value) = bearer(&owner_token);
            let (admitted, admitted_body) = send(
                state.clone(),
                "GET",
                &uri,
                Some((&owner_name, &owner_value)),
                "",
            )
            .await;
            assert_eq!(
                admitted,
                StatusCode::OK,
                "the creating profile must read {uri}: {admitted_body}"
            );
        }
    }

    /// An unauthorized DELETE is refused and leaves the session playing for
    /// its owner. The refusal must not stop, reap, or refresh the session.
    #[tokio::test]
    async fn an_unauthorized_delete_does_not_stop_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        if !ffmpeg_available() || !write_fixture(dir.path()) {
            eprintln!("skipping: ffmpeg unavailable");
            return;
        }
        let owner = actor(&state, "owner-a");
        let other = actor(&state, "owner-b");
        let owner_token = token_for(&state, &owner);
        let other_token = token_for(&state, &other);
        let item_id = seed_item(&state, dir.path());
        let session_id = start_session(&state, &owner_token, item_id).await;
        let view = format!("/api/v0/sessions/{session_id}");

        let (other_name, other_value) = bearer(&other_token);
        let (refused, _) = send(
            state.clone(),
            "DELETE",
            &view,
            Some((&other_name, &other_value)),
            "",
        )
        .await;
        assert_eq!(
            refused,
            StatusCode::NOT_FOUND,
            "a second profile must not stop the session"
        );

        let (owner_name, owner_value) = bearer(&owner_token);
        let (alive, _) = send(
            state.clone(),
            "GET",
            &view,
            Some((&owner_name, &owner_value)),
            "",
        )
        .await;
        assert_eq!(
            alive,
            StatusCode::OK,
            "the refused DELETE must leave the session for its owner"
        );

        let (stopped, _) = send(
            state.clone(),
            "DELETE",
            &view,
            Some((&owner_name, &owner_value)),
            "",
        )
        .await;
        assert_eq!(stopped, StatusCode::NO_CONTENT, "the owner may stop it");
    }

    /// The same boundary holds on the cookie transport the browser uses for
    /// byte routes: a listed GET is accepted for the owner and 404s for
    /// anyone else, before readiness or session state is read.
    #[tokio::test]
    async fn a_cookie_authenticated_byte_route_enforces_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        if !ffmpeg_available() || !write_fixture(dir.path()) {
            eprintln!("skipping: ffmpeg unavailable");
            return;
        }
        let owner = actor(&state, "owner-a");
        let other = actor(&state, "owner-b");
        let owner_token = token_for(&state, &owner);
        let other_token = token_for(&state, &other);
        let item_id = seed_item(&state, dir.path());
        let session_id = start_session(&state, &owner_token, item_id).await;
        let uri = format!("/api/v0/sessions/{session_id}/index.m3u8");

        let (owner_name, owner_value) = cookie(&owner_token);
        let (owner_status, _) = send(
            state.clone(),
            "GET",
            &uri,
            Some((&owner_name, &owner_value)),
            "",
        )
        .await;
        assert_eq!(owner_status, StatusCode::OK, "the owner's cookie may read");

        let (other_name, other_value) = cookie(&other_token);
        let (other_status, _) = send(
            state.clone(),
            "GET",
            &uri,
            Some((&other_name, &other_value)),
            "",
        )
        .await;
        assert_eq!(
            other_status,
            StatusCode::NOT_FOUND,
            "another cookie must not read the session"
        );
    }
}

/// ADR-0038 item 6 selection precedence, exercised through the same functions
/// the session and playback-info routes call.
#[cfg(test)]
mod track_preference_tests {
    use super::*;

    fn cand(id: &str, lang: &str, title: &str, index: u32, forced: bool) -> TrackCandidate {
        TrackCandidate {
            track_id: id.into(),
            language: Some(lang.into()),
            title: Some(title.into()),
            is_default: false,
            is_forced: forced,
            is_image: false,
            stream_index: index,
        }
    }

    fn prefs(
        language: Option<&str>,
        subtitle_default: &str,
        audio: Option<TrackDescription>,
        subtitle: SubtitleChoiceRow,
    ) -> TrackPreferences {
        TrackPreferences {
            preferred_language: language.map(str::to_string),
            subtitle_default: subtitle_default.to_string(),
            audio_description: audio,
            subtitle_choice: subtitle,
        }
    }

    fn desc(language: Option<&str>, kind: &str, sdh: bool, forced: bool) -> TrackDescription {
        TrackDescription {
            language: language.map(str::to_string),
            kind: kind.to_string(),
            sdh,
            forced,
        }
    }

    /// Audio precedence: the stored description wins over the profile default,
    /// and a description that matches nothing falls through to the profile
    /// default ranker with the ranker's reason.
    #[test]
    fn stored_audio_description_wins_then_falls_through() {
        let tracks = vec![
            cand("e1", "en", "Commentary", 1, false),
            cand("e2", "en", "Main", 2, false),
            cand("e3", "ja", "Japanese", 3, false),
        ];
        // Stored commentary wins over profile `en` main.
        let p = prefs(
            Some("en"),
            "auto",
            Some(desc(Some("en"), "commentary", false, false)),
            SubtitleChoiceRow::Unset,
        );
        let sel = choose_audio_track(&tracks, &p);
        assert_eq!(sel.track_id.as_deref(), Some("e1"), "{sel:?}");
        assert!(sel.reason.contains("stored choice"), "{}", sel.reason);

        // A stored `ja` description that no longer exists falls through to the
        // profile default, and the reason is the ranker's existing vocabulary.
        let p = prefs(
            Some("en"),
            "auto",
            Some(desc(Some("de"), "main", false, false)),
            SubtitleChoiceRow::Unset,
        );
        let sel = choose_audio_track(&tracks, &p);
        assert_eq!(sel.track_id.as_deref(), Some("e2"), "{sel:?}");
        assert!(
            sel.reason.contains("matched your preference"),
            "{}",
            sel.reason
        );
    }

    /// Subtitle precedence: stored `off` selects none even though the profile
    /// default would select one; stored `track` resolves by description.
    #[test]
    fn stored_subtitle_off_selects_none_and_track_resolves() {
        let tracks = vec![
            cand("e3", "en", "English", 3, false),
            cand("e4", "en", "English [SDH]", 4, false),
        ];
        let off = prefs(Some("en"), "auto", None, SubtitleChoiceRow::Off);
        let sel = choose_subtitle_track(&tracks, &off, Some("en"));
        assert_eq!(sel.track_id, None, "{sel:?}");
        assert!(sel.reason.contains("off"), "{}", sel.reason);

        let track = prefs(
            Some("en"),
            "auto",
            None,
            SubtitleChoiceRow::Track(desc(Some("en"), "main", true, false)),
        );
        let sel = choose_subtitle_track(&tracks, &track, Some("en"));
        assert_eq!(sel.track_id.as_deref(), Some("e4"), "{sel:?}");
        assert!(sel.reason.contains("stored choice"), "{}", sel.reason);
    }

    /// `unset` uses the profile default; a profile default of `off` selects
    /// none even when a preference language would match.
    #[test]
    fn unset_uses_the_profile_default_and_off_is_off() {
        let tracks = vec![cand("e3", "en", "English", 3, false)];
        let auto = prefs(Some("en"), "auto", None, SubtitleChoiceRow::Unset);
        let sel = choose_subtitle_track(&tracks, &auto, Some("en"));
        assert_eq!(sel.track_id.as_deref(), Some("e3"), "{sel:?}");

        let off = prefs(Some("en"), "off", None, SubtitleChoiceRow::Unset);
        let sel = choose_subtitle_track(&tracks, &off, Some("en"));
        assert_eq!(sel.track_id, None, "{sel:?}");
        assert!(sel.reason.contains("profile default"), "{}", sel.reason);
    }

    /// A profile with no language is ADR-0024's no-preference case: audio
    /// still resolves (last resort), subtitles select nothing.
    #[test]
    fn a_profile_with_no_language_is_the_no_preference_case() {
        let audio = vec![cand("e2", "en", "Main", 2, false)];
        let p = prefs(None, "auto", None, SubtitleChoiceRow::Unset);
        let sel = choose_audio_track(&audio, &p);
        assert_eq!(sel.track_id.as_deref(), Some("e2"), "{sel:?}");
        assert!(sel.reason.contains("first eligible"), "{}", sel.reason);

        let subs = vec![cand("e3", "en", "English", 3, false)];
        let sel = choose_subtitle_track(&subs, &p, Some("en"));
        assert_eq!(sel.track_id, None, "{sel:?}");
    }

    /// The production read path, end to end without ffprobe: a profile's
    /// defaults and its per-series override are loaded from the database and
    /// fed to the same selection functions the session uses. The stored audio
    /// description outranks the profile language, and the stored subtitle
    /// `off` outranks the profile default.
    #[test]
    fn loaded_preferences_carry_the_series_override_into_selection() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::state::test_support::state(dir.path());
        let library = state
            .db
            .create_library(&nightjar_db::NewLibrary {
                name: "shows".to_string(),
                path: "/media/shows".to_string(),
                kind: "shows".to_string(),
            })
            .unwrap();
        state
            .db
            .upsert_items_indexed(
                library.id,
                &[nightjar_db::UpsertItem {
                    path: "Alpha/Season 1/Alpha.S01E01.mkv".to_string(),
                    mtime_ms: 0,
                    size_bytes: 1,
                    title: "Alpha".to_string(),
                    kind: "episode".to_string(),
                    year: None,
                    season: Some(1),
                    episode: Some(1),
                    content_id: None,
                }],
            )
            .unwrap();
        let (profile_id, item_id) = state
            .db
            .with_conn(|conn| {
                let hash = nightjar_auth::hash_password("x").map_err(|e| format!("hash: {e:?}"))?;
                nightjar_db::create_account_with_profile(conn, "a", &hash, "owner", "P", "r0")?;
                let profile = nightjar_db::profile_by_ref(conn, "r0")?.unwrap();
                nightjar_db::update_profile_preferences(conn, profile.id, Some("ja"), "off")?;
                conn.execute(
                    "INSERT INTO series (library_id, relpath, tmdb_show_id)
                     VALUES (?1, 'Alpha', NULL)",
                    [library.id],
                )
                .map_err(|e| e.to_string())?;
                let series_key = format!("folder:{}:Alpha", library.id);
                nightjar_db::upsert_track_choice(
                    conn,
                    profile.id,
                    &series_key,
                    Some(&TrackDescription {
                        language: Some("en".to_string()),
                        kind: "main".to_string(),
                        sdh: false,
                        forced: false,
                    }),
                    &SubtitleChoiceRow::Off,
                    "2026-09-12T00:00:00.000Z",
                )?;
                let item_id: i64 = conn
                    .query_row("SELECT id FROM media_items LIMIT 1", [], |r| r.get(0))
                    .map_err(|e| e.to_string())?;
                Ok((profile.id, item_id))
            })
            .unwrap();

        let row = state.db.get_item(item_id).unwrap().unwrap();
        let loaded = load_track_preferences(&state, Some(profile_id), &row);
        assert_eq!(loaded.preferred_language.as_deref(), Some("ja"));
        assert_eq!(loaded.subtitle_default, "off");
        assert_eq!(
            loaded
                .audio_description
                .as_ref()
                .unwrap()
                .language
                .as_deref(),
            Some("en")
        );

        let tracks = vec![
            cand("e1", "ja", "Japanese", 1, false),
            cand("e2", "en", "English", 2, false),
        ];
        // The stored description wins over the profile default `ja`.
        let audio = choose_audio_track(&tracks, &loaded);
        assert_eq!(audio.track_id.as_deref(), Some("e2"), "{audio:?}");
        assert!(audio.reason.contains("stored choice"), "{}", audio.reason);

        // The stored `off` wins over the profile default `off` too, and says so.
        let subs = vec![cand("e3", "en", "English", 3, false)];
        let subtitle = choose_subtitle_track(&subs, &loaded, Some("en"));
        assert_eq!(subtitle.track_id, None, "{subtitle:?}");
        assert!(subtitle.reason.contains("off"), "{}", subtitle.reason);
    }
}
