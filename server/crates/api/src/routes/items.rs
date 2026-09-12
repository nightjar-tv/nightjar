use crate::authority::Caller;
use crate::error::{ApiError, ApiResult, blocking};
use crate::state::AppState;
use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    response::Response,
};
use nightjar_core::{
    BROWSER_V0, ClientCapabilityProfile, PlaybackDecision, PlaybackMethod, TrackCandidate,
    decide_playback, known_profile, needs_standalone_subtitle_extract, resolve_profile_bag,
    title_looks_forced, title_looks_sdh,
};
use nightjar_db::{MediaItemRow, SidecarRow, resolve_media_path};
use nightjar_metadata::{ArtworkKind, ItemMetadata, item_metadata, rating_max};
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
    /// Metadata pipeline state: pending | matched | ready | unmatched (ADR-0026).
    pub metadata_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scan_error: Option<String>,
    pub playback_method: &'static str,
}

/// One item with its canonical metadata (ADR-0029 §1.2).
///
/// A superset of [`MediaItemDto`] rather than a second endpoint: the page that
/// renders a synopsis needs the file facts on the same screen, and ADR-0029's
/// link table makes it one join. The library listing keeps the bulk shape,
/// because a 23k-row response has no use for 23k synopses.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaItemDetailDto {
    #[serde(flatten)]
    pub item: MediaItemDto,
    /// Opaque item_key (ADR-0035 item 11).
    pub item_key: String,
    /// Canonical title when the item has a canonical row; `title` stays the
    /// scan-derived one so a mismatch between them is visible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_minutes: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub air_date: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ratings: Vec<RatingDto>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cast: Vec<CastMemberDto>,
    /// Only kinds this title actually has. A kind absent here does not exist
    /// for this title, so the client draws the layout without it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub artwork: Vec<ArtworkDto>,
    /// Series this episode belongs to (ADR-0039 item 2), for the link back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_title: Option<String>,
}

/// One rating, with the scale it is expressed against when that is known.
///
/// `max` is present because `value` alone is not renderable: the dogfood
/// library returns `tomatometerallcritics` on 0–100 in the same array as `imdb`
/// on 0–10, and a client given two bare numbers can only guess or mislead.
/// Absent `max` means the scale is unknown for that source — `default`, the
/// NFO's unnamed `<rating>`, is the one that reaches it — and a client must
/// then show the number without implying a denominator.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RatingDto {
    pub source: String,
    pub value: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub votes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CastMemberDto {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<i32>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtworkDto {
    /// poster | backdrop | logo.
    pub kind: &'static str,
    pub url: String,
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
    /// Why the audio track was selected (ADR-0038 item 5).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_reason: Option<String>,
    /// Why a subtitle track was selected, or why none was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle_reason: Option<String>,
}

pub async fn get(
    State(state): State<AppState>,
    Path(item_id): Path<i64>,
) -> ApiResult<Json<MediaItemDetailDto>> {
    blocking(move || {
        let row = state
            .db
            .get_item(item_id)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found(format!("item {item_id} not found")))?;
        let root = library_root(&state, row.library_id)?;
        let library_id = row.library_id;
        let relpath = row.path.clone();
        let meta = state
            .db
            .with_conn(|c| item_metadata(c, item_id, library_id, &relpath))
            .map_err(ApiError::internal)?;
        Ok(Json(to_detail_dto(row, &root, meta)))
    })
    .await
}

fn to_detail_dto(row: MediaItemRow, library_root: &str, meta: ItemMetadata) -> MediaItemDetailDto {
    MediaItemDetailDto {
        item: to_dto(row, library_root),
        item_key: meta.item_key,
        canonical_title: meta.title,
        plot: meta.plot,
        runtime_minutes: meta.runtime_minutes,
        air_date: meta.air_date,
        genres: meta.genres,
        ratings: meta
            .ratings
            .into_iter()
            .map(|r| RatingDto {
                max: rating_max(&r.source),
                source: r.source,
                value: r.value,
                votes: r.votes,
            })
            .collect(),
        cast: meta
            .cast
            .into_iter()
            .map(|c| CastMemberDto {
                name: c.name,
                role: c.role,
                order: c.order,
            })
            .collect(),
        artwork: meta
            .artwork
            .into_iter()
            .filter_map(|a| {
                let kind = artwork_kind_name(a.kind)?;
                Some(ArtworkDto {
                    kind,
                    url: format!("/api/v0/artwork/{}/{kind}", a.item_key),
                })
            })
            .collect(),
        series_key: meta.series_key,
        show_title: meta.show_title,
    }
}

/// Wire name for the kinds an item page renders. Kinds with no surface yet
/// return `None` rather than being advertised with a URL nothing asks for.
fn artwork_kind_name(kind: ArtworkKind) -> Option<&'static str> {
    match kind {
        ArtworkKind::Poster => Some("poster"),
        ArtworkKind::Backdrop => Some("backdrop"),
        ArtworkKind::Logo => Some("logo"),
        ArtworkKind::Banner | ArtworkKind::Other => None,
    }
}

pub(crate) fn library_root(state: &AppState, library_id: i64) -> ApiResult<String> {
    Ok(state
        .db
        .get_library(library_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("library {library_id} not found")))?
        .path)
}

pub(crate) fn abs_path(library_root: &str, stored: &str) -> std::path::PathBuf {
    resolve_media_path(library_root, stored)
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
    caller: Caller,
    Path(item_id): Path<i64>,
    Query(query): Query<ProfileQuery>,
) -> ApiResult<Json<PlaybackInfoDto>> {
    // Every step below blocks: two DB reads, and `subtitle_tracks_for` /
    // `audio_tracks_for` each wait on an ffprobe child reading over SMB.
    blocking(move || {
        playback_info_blocking(state, item_id, query, caller.session.active_profile_id)
    })
    .await
}

fn playback_info_blocking(
    state: AppState,
    item_id: i64,
    query: ProfileQuery,
    profile_id: Option<i64>,
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
    let root = library_root(&state, row.library_id)?;
    let abs = abs_path(&root, &row.path);
    let subtitle_tracks = subtitle_tracks_for(&state, &row, &root).unwrap_or_else(|e| {
        tracing::warn!(item_id, error = %e, "subtitle list failed");
        Vec::new()
    });
    // Listed the same for every method: the client asks for a track and never
    // reasons about delivery to find one (ADR-0012).
    let audio_tracks = audio_tracks_for(&row, &root).unwrap_or_else(|e| {
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

    // ADR-0041 Decision 5: the on-demand trigger. Standalone extraction runs
    // only for an `eligible` item on a client that cannot read embedded
    // container subtitles (Decision 3) via a method that will not produce the
    // rendition as a side output (Decision 4). Replaces ADR-0013 §11's
    // unconditional `pending` bump; remux/transcode sessions get subtitles as
    // a side output instead (ADR-0041 Decision 7, step 4).
    if row.subtitle_status == "eligible"
        && needs_standalone_subtitle_extract(query.profile_id.as_deref(), decision.method)
    {
        state
            .pool
            .prioritize_extract(row.id, row.library_id, abs.clone());
    }

    // ADR-0023 §9.1: playback info is the demand trigger for the keyframe
    // map. An item with no ready/valid map gets a priority build (index-first,
    // §2 mechanism unchanged) so the map is warm before a session exists; the
    // scan path no longer queues the whole library (§2 amendment).
    if state.db.keyframe_map(row.id).ok().flatten().is_none() {
        crate::routes::sessions::request_map_rebuild(&state, &row);
    }

    // ADR-0038 item 5: the selection reasons ride this response so the track
    // menu can explain itself. The selection runs the same precedence the
    // session does (Rule 4.11).
    let prefs = super::sessions::load_track_preferences(&state, profile_id, &row);
    let audio_candidates: Vec<TrackCandidate> = audio_tracks
        .iter()
        .map(|a| TrackCandidate {
            track_id: a.track_id.clone(),
            language: a.language.clone(),
            title: a.label.clone(),
            is_default: a.default,
            is_forced: false,
            is_image: false,
            stream_index: a.stream_index,
        })
        .collect();
    let audio_selection = super::sessions::choose_audio_track(&audio_candidates, &prefs);
    let audio_language = audio_selection
        .track_id
        .as_deref()
        .and_then(|id| audio_tracks.iter().find(|a| a.track_id == id))
        .and_then(|a| a.language.clone());
    let subtitle_candidates: Vec<TrackCandidate> = subtitle_tracks
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
    let subtitle_selection = super::sessions::choose_subtitle_track(
        &subtitle_candidates,
        &prefs,
        audio_language.as_deref(),
    );

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
        audio_reason: Some(audio_selection.reason),
        subtitle_reason: Some(subtitle_selection.reason),
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
    // The DB read and the subs-store lookups block; the body read does not.
    let (path, cache) = blocking(move || {
        let row = state
            .db
            .get_item(item_id)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found(format!("item {item_id} not found")))?;

        // ADR-0013: playback never extracts; serve a stored file or 404.
        let path = stored_webvtt(&state.subs, item_id, &track_id).map_err(ApiError::not_found)?;

        let (readiness, _) = state
            .subs
            .track_readiness(item_id, &track_id, &row.subtitle_status);
        // A growing partial must not be cached: the next GET needs the newer body.
        let cache = match readiness {
            TrackReadiness::Complete if row.subtitle_status == "ready" => "private, max-age=3600",
            _ => "private, no-cache",
        };
        Ok((path, cache))
    })
    .await?;

    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| ApiError::internal(format!("read subtitle {}: {e}", path.display())))?;
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

pub(crate) fn audio_tracks_for(
    row: &MediaItemRow,
    library_root: &str,
) -> Result<Vec<AudioTrackDto>, String> {
    let tracks = list_audio_tracks(&abs_path(library_root, &row.path))?
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
    library_root: &str,
) -> Result<Vec<SubtitleTrackDto>, String> {
    let mut tracks = Vec::new();
    let src_buf = abs_path(library_root, &row.path);
    let src = src_buf.as_path();
    for s in list_text_subtitles(src)? {
        let forced = s.is_forced || title_looks_forced(s.title.as_deref());
        let sdh = title_looks_sdh(s.title.as_deref());
        tracks.push(serveable_track_dto(
            state,
            row,
            ServeableTrack {
                track_id: s.track_id(),
                source: "embedded",
                codec: s.codec,
                language: s.language,
                label: s.title,
                forced,
                sdh,
                stream_index: Some(s.stream_index),
            },
        ));
    }
    for s in list_burn_in_subtitles(src)? {
        let forced = title_looks_forced(s.title.as_deref());
        let sdh = title_looks_sdh(s.title.as_deref());
        tracks.push(burn_in_track_dto(ServeableTrack {
            track_id: s.track_id(),
            source: "embedded",
            codec: s.codec,
            language: s.language,
            label: s.title,
            forced,
            sdh,
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

pub fn to_dto(row: MediaItemRow, library_root: &str) -> MediaItemDto {
    let decision = decide(&row, &BROWSER_V0, true);
    let path = abs_path(library_root, &row.path)
        .to_string_lossy()
        .into_owned();
    MediaItemDto {
        id: row.id,
        library_id: row.library_id,
        path,
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
        metadata_status: row.metadata_status,
        scan_error: row.scan_error,
        playback_method: decision.method.as_str(),
    }
}

/// ADR-0038 item 5, through the real router. The selection reasons ride
/// playback-info so the track menu can explain itself. Asserting the JSON
/// strings, not the DTO source, is what makes deleting either field fail.
#[cfg(test)]
mod playback_info_reason_tests {
    use crate::routes::router;
    use crate::state::{AppState, test_support};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use nightjar_auth::mint_session_token;
    use nightjar_db::{NewLibrary, UpsertItem};
    use tower::ServiceExt;

    /// One account, one profile, one profile-scoped session token.
    fn profile_token(state: &AppState) -> String {
        let minted = mint_session_token();
        state
            .db
            .with_conn(|conn| {
                let hash = nightjar_auth::hash_password("x").map_err(|e| format!("hash: {e:?}"))?;
                nightjar_db::create_account_with_profile(
                    conn,
                    "m",
                    &hash,
                    "member",
                    "P",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                )?;
                let account = nightjar_db::account_by_username(conn, "m")?.unwrap();
                let profile =
                    nightjar_db::profile_by_ref(conn, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")?.unwrap();
                let expires = nightjar_db::session_expiry(conn)?;
                let session = nightjar_db::create_session(
                    conn,
                    account.id,
                    &minted.sha256_hex,
                    "t",
                    &expires,
                )?;
                nightjar_db::set_active_profile(conn, session, Some(profile.id))?;
                Ok(())
            })
            .unwrap();
        minted.plaintext
    }

    /// A library and one indexed item. No media file is written: the track
    /// lists fail and are caught, which is exactly the no-inventory path the
    /// reasons must still cover.
    fn seed_item(state: &AppState, dir: &std::path::Path) -> i64 {
        let library = state
            .db
            .create_library(&NewLibrary {
                name: "shows".to_string(),
                path: dir.to_string_lossy().into_owned(),
                kind: "shows".to_string(),
            })
            .unwrap();
        state
            .db
            .upsert_items_indexed(
                library.id,
                &[UpsertItem {
                    path: "a/s01e01.mkv".to_string(),
                    mtime_ms: 0,
                    size_bytes: 1,
                    title: "a".to_string(),
                    kind: "episode".to_string(),
                    year: None,
                    season: Some(1),
                    episode: Some(1),
                    content_id: None,
                }],
            )
            .unwrap()[0]
    }

    /// Reads a non-empty string field, panicking when the key is absent. The
    /// absent case is the failure this test exists for.
    fn non_empty_string(body: &str, key: &str) -> String {
        let at = body
            .find(&format!("\"{key}\":\""))
            .unwrap_or_else(|| panic!("no {key} in {body}"));
        let start = at + key.len() + 4;
        let end = body[start..].find('"').unwrap() + start;
        let value = &body[start..end];
        assert!(!value.is_empty(), "{key} is empty: {body}");
        value.to_string()
    }

    #[tokio::test]
    async fn playback_info_exposes_selection_reasons() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let token = profile_token(&state);
        let item_id = seed_item(&state, dir.path());

        let request = Request::builder()
            .method("GET")
            .uri(format!("/api/v0/items/{item_id}/playback-info"))
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = router(state).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();

        let audio = non_empty_string(&body, "audioReason");
        let subtitle = non_empty_string(&body, "subtitleReason");
        assert_ne!(audio, subtitle, "each axis explains itself: {body}");
    }
}
