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
    AccountPlaybackPolicy, BROWSER_V0, ClientCapabilityProfile, PlaybackCeilings, PlaybackDecision,
    PlaybackMethod, TrackCandidate, classify_client_origin, compose_playback_ceilings,
    decide_playback, known_profile, needs_standalone_subtitle_extract, resolve_profile_bag,
    title_looks_forced, title_looks_sdh,
};
use nightjar_db::{
    CertifiedSubtitleSource, MediaItemRow, SidecarRow, SubtitleArtifactState,
    SubtitleListingSource, is_valid_generation_token, resolve_media_path,
};
use nightjar_metadata::{ArtworkKind, ItemMetadata, item_metadata, rating_max};
use nightjar_transcode::{
    TrackReadiness, burn_in_kind_for_codec, is_burn_in_sidecar_format, is_serveable_sidecar_format,
    list_audio_tracks, stored_webvtt,
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
    /// The account policy bitrate ceiling, surfaced whether or not applied
    /// (ADR-0022 §5 as amended 2026-09-12). Absent means no account ceiling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_max_bitrate_bps: Option<i64>,
    /// The account policy height ceiling, surfaced whether or not applied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_max_height: Option<i64>,
}

pub async fn get(
    State(state): State<AppState>,
    caller: crate::authority::Caller,
    Path(item_id): Path<i64>,
) -> ApiResult<Json<MediaItemDetailDto>> {
    blocking(move || {
        crate::authority::require_item_visible(&state, &caller, item_id)?;
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

/// Load the account's playback-policy ceilings (ADR-0022 §5 as amended
/// 2026-09-12, ADR-0034 item 8). Null columns become `None`.
pub(crate) fn account_playback_policy(
    state: &AppState,
    account_id: i64,
) -> ApiResult<AccountPlaybackPolicy> {
    let account = state
        .db
        .with_conn(|conn| nightjar_db::account_by_id(conn, account_id))
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::internal(format!("account {account_id} not found")))?;
    Ok(AccountPlaybackPolicy {
        max_concurrent_sessions: account
            .max_concurrent_sessions
            .and_then(|v| u32::try_from(v).ok()),
        max_bitrate_bps: account.max_bitrate_bps.and_then(|v| u64::try_from(v).ok()),
        max_height: account.max_height.and_then(|v| u32::try_from(v).ok()),
    })
}

/// The one capability/policy composition, applied to a capability profile
/// (ADR-0022 §5 as amended 2026-09-12). Shared by playback-info, `/stream` and
/// session start, so all three agree on the effective ceiling.
///
/// The origin comes from the single unknown-as-local classifier, so the policy
/// half is surfaced and not applied, and no request header can change that. A
/// wider capability query cannot widen the applied ceiling either, because the
/// composition only ever narrows.
pub(crate) fn apply_playback_policy(
    capability: ClientCapabilityProfile,
    policy: AccountPlaybackPolicy,
) -> (ClientCapabilityProfile, PlaybackCeilings) {
    let ceilings = compose_playback_ceilings(&capability, policy, classify_client_origin());
    let effective = ClientCapabilityProfile {
        max_bitrate_bps: ceilings.max_bitrate_bps,
        max_height: ceilings.max_height,
        ..capability
    };
    (effective, ceilings)
}

pub async fn playback_info(
    State(state): State<AppState>,
    caller: Caller,
    Path(item_id): Path<i64>,
    Query(query): Query<ProfileQuery>,
) -> ApiResult<Json<PlaybackInfoDto>> {
    // Every step below blocks: DB reads, and `audio_tracks_for` waits on an
    // ffprobe child reading over SMB. `subtitle_tracks_for` reads the ADR-0058
    // coherent stored inventory and does not probe (D2B.2 acceptance 6).
    blocking(move || {
        crate::authority::require_item_visible(&state, &caller, item_id)?;
        playback_info_blocking(
            state,
            item_id,
            query,
            caller.session.active_profile_id,
            caller.session.account_id,
        )
    })
    .await
}

fn playback_info_blocking(
    state: AppState,
    item_id: i64,
    query: ProfileQuery,
    profile_id: Option<i64>,
    account_id: i64,
) -> ApiResult<Json<PlaybackInfoDto>> {
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
    let policy = account_playback_policy(&state, account_id)?;
    let (profile, ceilings) = apply_playback_policy(capability, policy);
    let decision = decide(&row, &profile, state.tonemap_available);
    let root = library_root(&state, row.library_id)?;
    let abs = abs_path(&root, &row.path);
    // One coherent read obtains the certification, the durable sidecar
    // membership, the committed per-track publications and the coarse
    // lifecycle fields (ADR-0013 §13.4). Listing and demand both read it, so
    // neither can combine an observation of one generation with another's
    // publication state.
    let subtitle_source = state
        .db
        .subtitle_listing_source(row.id)
        .unwrap_or_else(|e| {
            tracing::warn!(item_id, error = %e, "subtitle source read failed");
            None
        });
    let subtitle_tracks = subtitle_tracks_for(&row, subtitle_source.as_ref());
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
    // only on a client that cannot read embedded container subtitles
    // (Decision 3) via a method that will not produce the rendition as a side
    // output (Decision 4). Replaces ADR-0013 §11's unconditional `pending`
    // bump; remux/transcode sessions get subtitles as a side output instead
    // (ADR-0041 Decision 7, step 4).
    //
    // ADR-0013 §13.4: the demand comes from the per-track publications, not the
    // coarse lifecycle field. A formerly `ready` item whose sidecar was edited
    // and a formerly `none` item that gained its first sidecar both still have a
    // member without a complete committed publication, and both must repair.
    if subtitle_source
        .as_ref()
        .is_some_and(|source| source.needs_publication() && subtitle_extract_allowed(source))
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
        policy_max_bitrate_bps: ceilings.policy_max_bitrate_bps.map(|v| v as i64),
        policy_max_height: ceilings.policy_max_height.map(|v| v as i64),
    }))
}

/// Generation query for a subtitle artifact URL (D2B.2). Clients receive the
/// URL from `playbackInfo`; they never construct it.
#[derive(Deserialize)]
pub struct SubtitleAssetQuery {
    #[serde(default)]
    pub g: Option<String>,
}

pub async fn subtitle_vtt(
    State(state): State<AppState>,
    caller: Caller,
    Path((item_id, asset)): Path<(i64, String)>,
    Query(query): Query<SubtitleAssetQuery>,
) -> ApiResult<Response> {
    let track_id = asset
        .strip_suffix(".vtt")
        .filter(|id| super::track_ids::is_valid_track_id(id))
        .ok_or_else(|| ApiError::not_found(format!("subtitle asset {asset} not found")))?
        .to_string();
    let token = query
        .g
        .filter(|g| is_valid_generation_token(g))
        .ok_or_else(|| ApiError::not_found("subtitle URL has no current generation"))?;
    // The DB read and the subs-store lookups block; the body read does not.
    let (path, cache) = blocking(move || {
        crate::authority::require_item_visible(&state, &caller, item_id)?;

        // One coherent DB read obtains the certification, membership, token,
        // per-track publication state and `subtitle_content_id` (ADR-0013
        // §13.4). Serving never combines an older item-level observation with a
        // newer certified source.
        let source = state
            .db
            .certified_subtitle_source(item_id)
            .map_err(ApiError::internal)?
            .ok_or_else(|| {
                ApiError::not_found(format!(
                    "item {item_id} not found or its subtitle source is not certified"
                ))
            })?;
        if source.token_for_track(&track_id).as_deref() != Some(token.as_str()) {
            return Err(ApiError::not_found(
                "subtitle generation is stale or the track is not a current member",
            ));
        }

        // A track is served only through its committed per-track publication
        // reference. A file on disk is never enough: an artifact renamed but
        // not committed is an orphan, and a stale URL is rejected even when
        // cleanup has not run (ADR-0013 §13.4/§13.7).
        let Some(artifact) = source.artifact_for(&track_id) else {
            return Err(ApiError::not_found("subtitle artifact is not published"));
        };
        let readiness = match artifact.state {
            SubtitleArtifactState::Complete => TrackReadiness::Complete,
            SubtitleArtifactState::Partial => TrackReadiness::Partial,
        };

        // ADR-0013 §13.2: the filename comes exclusively from the committed
        // publication row's artifact revision, never from the request.
        // Playback never extracts; serve the stored file or 404.
        let path = stored_webvtt(
            &state.subs,
            item_id,
            &token,
            &track_id,
            artifact.artifact_revision,
        )
        .map_err(ApiError::not_found)?;

        // A growing partial must not be cached: the next GET needs the newer body.
        let cache = match readiness {
            TrackReadiness::Complete => "private, max-age=3600",
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

/// ADR-0013 §13.4: whether a missing per-track publication may start a
/// standalone extract. The coarse lifecycle and its backoff gate new work, but
/// they never decide whether there is work to do.
///
/// `error` is a permanent failure until the source row is re-upserted, and
/// `unavailable` is refused until the scan-time requeue moves it back to
/// `pending` past its retry deadline. Every other state — including a stale
/// `ready` or `none` — still demands the publications its members are missing.
fn subtitle_extract_allowed(source: &SubtitleListingSource) -> bool {
    !matches!(source.subtitle_status.as_str(), "error" | "unavailable")
}

/// List an item's subtitle tracks from one coherent subtitle-source read
/// (ADR-0013 §13.4).
///
/// D2B.2 acceptance 6: the embedded inventory comes from the ADR-0058 coherent
/// stored snapshot, never a playback-time ffprobe. An item whose snapshot is not
/// certified lists no embedded tracks; its durable sidecar rows still come from
/// the same read and are listed without delivery. The sidecar set is never
/// reloaded separately, so a listing cannot mix two generations.
pub(crate) fn subtitle_tracks_for(
    row: &MediaItemRow,
    source: Option<&SubtitleListingSource>,
) -> Vec<SubtitleTrackDto> {
    let mut tracks = Vec::new();
    let certified = source.and_then(|source| source.certified());
    if let Some(snapshot) = source.and_then(|source| source.snapshot.as_ref()) {
        for t in &snapshot.snapshot.subtitle_tracks {
            if t.kind != "text" {
                continue;
            }
            let Some(stream_index) = u32::try_from(t.stream_index).ok() else {
                continue;
            };
            tracks.push(serveable_track_dto(
                row,
                certified.as_ref(),
                ServeableTrack {
                    track_id: format!("e{stream_index}"),
                    source: "embedded",
                    codec: t.codec.clone(),
                    language: t.language.clone(),
                    label: t.title.clone(),
                    forced: t.forced || title_looks_forced(t.title.as_deref()),
                    sdh: t.sdh || title_looks_sdh(t.title.as_deref()),
                    stream_index: Some(stream_index),
                },
            ));
        }
        for t in &snapshot.snapshot.subtitle_tracks {
            if t.kind == "text" || burn_in_kind_for_codec(&t.codec).is_none() {
                continue;
            }
            let Some(stream_index) = u32::try_from(t.stream_index).ok() else {
                continue;
            };
            tracks.push(burn_in_track_dto(ServeableTrack {
                track_id: format!("e{stream_index}"),
                source: "embedded",
                codec: t.codec.clone(),
                language: t.language.clone(),
                label: t.title.clone(),
                forced: t.forced || title_looks_forced(t.title.as_deref()),
                sdh: t.sdh || title_looks_sdh(t.title.as_deref()),
                stream_index: Some(stream_index),
            }));
        }
    }
    if let Some(source) = source {
        for s in &source.sidecars {
            tracks.push(sidecar_to_dto(row, certified.as_ref(), s));
        }
    }
    tracks
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
    row: &MediaItemRow,
    certified: Option<&CertifiedSubtitleSource>,
    t: ServeableTrack,
) -> SubtitleTrackDto {
    // No certified source, or a track the current source does not mint a token
    // for, gets no URL: the server never hands out a URL a client could
    // construct or that names a non-generation artifact (D2B.2 acceptance 4).
    let Some(token) = certified.and_then(|source| source.token_for_track(&t.track_id)) else {
        return SubtitleTrackDto {
            url: None,
            readiness: Some(TrackReadiness::Preparing.as_str()),
            revision: Some(0),
            track_id: t.track_id,
            source: t.source,
            codec: t.codec,
            language: t.language,
            label: t.label,
            forced: t.forced,
            sdh: t.sdh,
            stream_index: t.stream_index,
            render: "soft",
        };
    };
    let (readiness, revision) = match certified.and_then(|source| source.artifact_for(&t.track_id))
    {
        Some(artifact) => (
            match artifact.state {
                SubtitleArtifactState::Complete => TrackReadiness::Complete,
                SubtitleArtifactState::Partial => TrackReadiness::Partial,
            },
            artifact.revision,
        ),
        None => (TrackReadiness::Preparing, 0),
    };
    let url = match readiness {
        TrackReadiness::Preparing => None,
        TrackReadiness::Partial | TrackReadiness::Complete => Some(format!(
            "/api/v0/items/{}/subtitles/{}.vtt?g={}",
            row.id, t.track_id, token
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

fn sidecar_to_dto(
    row: &MediaItemRow,
    certified: Option<&CertifiedSubtitleSource>,
    s: &SidecarRow,
) -> SubtitleTrackDto {
    if is_serveable_sidecar_format(&s.format) {
        return serveable_track_dto(
            row,
            certified,
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

/// The one capability/policy composition through the real router (ADR-0022 §5
/// as amended 2026-09-12). The account policy is surfaced and not applied,
/// because the single classifier is unknown-as-local; a capability query still
/// narrows the applied ceiling, and forwarding headers change neither.
#[cfg(test)]
mod policy_composition_router_tests {
    use crate::routes::router;
    use crate::state::{AppState, test_support};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use nightjar_auth::mint_session_token;
    use nightjar_db::{NewLibrary, ProbeUpdate, UpsertItem};
    use tower::ServiceExt;

    /// One account with one profile-scoped session. `policy` sets the account
    /// bitrate/height ceilings, or leaves all three null.
    fn profile_token(state: &AppState, username: &str, policy: Option<(i64, i64)>) -> String {
        let minted = mint_session_token();
        let profile_ref = format!("{username:0<32}");
        state
            .db
            .with_conn(|conn| {
                let hash = nightjar_auth::hash_password("x").map_err(|e| format!("hash: {e:?}"))?;
                let (account_id, profile_id) = nightjar_db::create_account_with_profile(
                    conn,
                    username,
                    &hash,
                    "member",
                    "P",
                    &profile_ref,
                )?;
                if let Some((bitrate, height)) = policy {
                    nightjar_db::update_account_playback_policy(
                        conn,
                        account_id,
                        None,
                        Some(bitrate),
                        Some(height),
                    )?;
                }
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

    /// A library, a probed h264/aac/mp4 item that direct-plays, and the bytes
    /// `/stream` serves.
    fn seed_direct_play_item(state: &AppState, dir: &std::path::Path) -> i64 {
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
                    path: "movie.mp4".to_string(),
                    mtime_ms: 0,
                    size_bytes: 1,
                    title: "movie".to_string(),
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
                container: Some("mp4".to_string()),
                video_codec: Some("h264".to_string()),
                audio_codec: Some("aac".to_string()),
                audio_channels: Some(2),
                width: Some(1920),
                height: Some(1080),
                video_bitrate_bps: Some(40_000_000),
                video_frame_rate_num: Some(24),
                video_frame_rate_den: Some(1),
                hdr: None,
                probe_status: "probed".to_string(),
                scan_error: None,
            })
            .unwrap();
        std::fs::write(dir.join("movie.mp4"), b"enough bytes to serve a range").unwrap();
        item_id
    }

    async fn call(
        state: &AppState,
        method: &str,
        uri: &str,
        token: &str,
        headers: &[(&str, &str)],
    ) -> (StatusCode, String) {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"));
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let response = router(state.clone())
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let text = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        (status, text)
    }

    #[tokio::test]
    async fn playback_info_surfaces_the_policy_and_stays_advisory() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let item_id = seed_direct_play_item(&state, dir.path());
        let capped = profile_token(&state, "capped", Some((8_000_000, 480)));
        let uncapped = profile_token(&state, "uncapped", None);

        let (status, body) = call(
            &state,
            "GET",
            &format!("/api/v0/items/{item_id}/playback-info"),
            &capped,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("\"policyMaxBitrateBps\":8000000"), "{body}");
        assert!(body.contains("\"policyMaxHeight\":480"), "{body}");
        // Advisory: the account cap did not force a session.
        assert!(body.contains("\"playbackMethod\":\"directPlay\""), "{body}");

        // Null ceilings are absent, the same shape as the account response.
        let (status, body) = call(
            &state,
            "GET",
            &format!("/api/v0/items/{item_id}/playback-info"),
            &uncapped,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!body.contains("policyMaxBitrateBps"), "{body}");
        assert!(!body.contains("policyMaxHeight"), "{body}");
    }

    #[tokio::test]
    async fn stream_is_advisory_for_policy_and_enforces_capability() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let item_id = seed_direct_play_item(&state, dir.path());
        let token = profile_token(&state, "capped", Some((8_000_000, 480)));
        let stream = format!("/api/v0/items/{item_id}/stream");

        // The account cap does not stop a direct byte serve: advisory.
        let (status, body) = call(&state, "GET", &stream, &token, &[]).await;
        assert_eq!(status, StatusCode::OK, "{body}");

        // A capability query ceiling below the source height still forces the
        // existing typed 415 session-required response.
        let capped_stream = format!("{stream}?maxHeight=480");
        let (status, body) = call(&state, "GET", &capped_stream, &token, &[]).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{body}");

        // A forwarding header cannot activate the remote policy half, so the
        // uncapped byte serve is unchanged.
        let (status, body) = call(
            &state,
            "GET",
            &stream,
            &token,
            &[
                ("x-forwarded-for", "203.0.113.7"),
                ("forwarded", "for=203.0.113.7"),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[tokio::test]
    async fn session_start_is_advisory_for_policy() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let item_id = seed_direct_play_item(&state, dir.path());
        let token = profile_token(&state, "capped", Some((8_000_000, 480)));

        // The account cap did not turn direct play into a session, so the
        // route answers the existing "does not need a session" 415. A remote
        // classifier would have started a session here.
        let (status, body) = call(
            &state,
            "POST",
            &format!("/api/v0/items/{item_id}/sessions"),
            &token,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{body}");
        assert!(body.contains("does not need a session"), "{body}");
    }
}

/// Direct item, byte, and session-creation routes behind the kids filter
/// (ADR-0037 item 7). A denied item answers the same 404 a missing one does,
/// so the response cannot be used to probe for titles a profile may not see.
#[cfg(test)]
mod kids_scope_router_tests {
    use crate::routes::router;
    use crate::state::{AppState, test_support};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use nightjar_auth::mint_session_token;
    use nightjar_db::{NewLibrary, UpsertItem};
    use tower::ServiceExt;

    /// One account with one capped, profile-scoped session. The region is
    /// selected first because a cap cannot be set without one.
    fn capped_token(state: &AppState, cap: &str) -> String {
        let minted = mint_session_token();
        state
            .db
            .with_conn(|conn| {
                let hash = nightjar_auth::hash_password("x").map_err(|e| format!("hash: {e:?}"))?;
                let (account_id, profile_id) = nightjar_db::create_account_with_profile(
                    conn, "kid", &hash, "member", "P", "kidref",
                )?;
                nightjar_db::select_classification_region(conn, "US")?;
                conn.execute(
                    "UPDATE profiles SET classification_cap = ?1 WHERE id = ?2",
                    rusqlite::params![cap, profile_id],
                )
                .map_err(|e| e.to_string())?;
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

    /// A library with one direct-play movie. `certification` is written to the
    /// canonical row when given; the item is `ready` either way.
    fn seed_movie(
        state: &AppState,
        dir: &std::path::Path,
        library_id: i64,
        provider_id: &str,
        certification: Option<&str>,
    ) -> i64 {
        let item_id = state
            .db
            .upsert_items_indexed(
                library_id,
                &[UpsertItem {
                    path: format!("{provider_id}.mp4"),
                    mtime_ms: 0,
                    size_bytes: 1,
                    title: format!("movie {provider_id}"),
                    kind: "movie".to_string(),
                    year: None,
                    season: None,
                    episode: None,
                    content_id: None,
                }],
            )
            .unwrap()[0];
        state
            .db
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO media_item_links (media_item_id, item_key, manually_matched)
                     VALUES (?1, ?2, 0)",
                    rusqlite::params![item_id, format!("tmdb:movie:{provider_id}")],
                )
                .map_err(|e| e.to_string())?;
                conn.execute(
                    "INSERT INTO metadata_canonical
                        (provider, entity_kind, provider_id, title, ids_json, projected_at,
                         certifications_json)
                     VALUES ('tmdb', 'movie', ?1, 'M', '{}', 'now', ?2)",
                    rusqlite::params![provider_id, certification],
                )
                .map_err(|e| e.to_string())?;
                conn.execute(
                    "UPDATE media_items SET metadata_status = 'ready' WHERE id = ?1",
                    rusqlite::params![item_id],
                )
                .map_err(|e| e.to_string())?;
                Ok(())
            })
            .unwrap();
        std::fs::write(dir.join(format!("{provider_id}.mp4")), b"bytes to serve").unwrap();
        item_id
    }

    async fn call(state: &AppState, uri: &str, token: &str) -> StatusCode {
        let request = Request::builder()
            .method("GET")
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        router(state.clone())
            .oneshot(request)
            .await
            .unwrap()
            .status()
    }

    /// Start a playback session for `item_id` through the real router, and
    /// return the status and error body.
    async fn start_session(state: &AppState, item_id: i64, token: &str) -> (StatusCode, String) {
        let request = Request::builder()
            .method("POST")
            .uri(format!("/api/v0/items/{item_id}/sessions"))
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        (status, body)
    }

    /// The `error` sentence an `ApiError` body carries.
    fn error_message(body: &str) -> &str {
        let at = body
            .find("\"error\":\"")
            .unwrap_or_else(|| panic!("no error field in {body}"))
            + "\"error\":\"".len();
        let end = body[at..].find('"').unwrap() + at;
        &body[at..end]
    }

    #[tokio::test]
    async fn direct_item_and_bytes_deny_over_cap_and_uncertified() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let token = capped_token(&state, "little_kid");
        let library = state
            .db
            .create_library(&NewLibrary {
                name: "movies".to_string(),
                path: dir.path().to_string_lossy().into_owned(),
                kind: "movies".to_string(),
            })
            .unwrap()
            .id;
        let in_cap = seed_movie(&state, dir.path(), library, "1", Some("{\"US\":\"G\"}"));
        let over_cap = seed_movie(&state, dir.path(), library, "2", Some("{\"US\":\"R\"}"));
        let uncertified = seed_movie(&state, dir.path(), library, "3", None);

        // Positive control: the at-cap title is served.
        assert_eq!(
            call(&state, &format!("/api/v0/items/{in_cap}"), &token).await,
            StatusCode::OK
        );
        // Over-cap and uncertified deny on the detail route...
        assert_eq!(
            call(&state, &format!("/api/v0/items/{over_cap}"), &token).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(&state, &format!("/api/v0/items/{uncertified}"), &token).await,
            StatusCode::NOT_FOUND
        );
        // ...and on the bytes route, before any file is opened.
        assert_eq!(
            call(&state, &format!("/api/v0/items/{over_cap}/stream"), &token).await,
            StatusCode::NOT_FOUND
        );
        // The at-cap title clears the visibility gate, so the bytes route
        // reaches its own method decision (415 here: the fixture is not
        // probed). It is not the 404 a denied item gets.
        assert_ne!(
            call(&state, &format!("/api/v0/items/{in_cap}/stream"), &token).await,
            StatusCode::NOT_FOUND
        );
    }

    /// An account-scope session is unrestricted (ADR-0037 item 7), so the same
    /// over-cap item is served. Without this the filter could be hiding
    /// everything and the test above would still pass.
    #[tokio::test]
    async fn account_scope_is_unrestricted() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let library = state
            .db
            .create_library(&NewLibrary {
                name: "movies".to_string(),
                path: dir.path().to_string_lossy().into_owned(),
                kind: "movies".to_string(),
            })
            .unwrap()
            .id;
        let over_cap = seed_movie(&state, dir.path(), library, "9", Some("{\"US\":\"R\"}"));
        let token = {
            let minted = mint_session_token();
            state
                .db
                .with_conn(|conn| {
                    let hash =
                        nightjar_auth::hash_password("x").map_err(|e| format!("hash: {e:?}"))?;
                    let (account_id, _) = nightjar_db::create_account_with_profile(
                        conn, "adult", &hash, "owner", "P", "ownerref",
                    )?;
                    let expires = nightjar_db::session_expiry(conn)?;
                    nightjar_db::create_session(
                        conn,
                        account_id,
                        &minted.sha256_hex,
                        "t",
                        &expires,
                    )?;
                    Ok(())
                })
                .unwrap();
            minted.plaintext
        };
        assert_eq!(
            call(&state, &format!("/api/v0/items/{over_cap}"), &token).await,
            StatusCode::OK
        );
    }

    /// Session creation is item-returning in effect and is not exempt
    /// (ADR-0037 item 7).
    ///
    /// The negative uses a known, over-cap item that is not probed. Without the
    /// gate the handler reaches its readiness check and answers 415; with the
    /// gate the answer is the missing-item 404, which the handler returns before
    /// it reads the row, probes a track, or calls `hls.start`, so no session and
    /// no encoder is created. The at-cap item is the positive control: it clears
    /// the gate and reaches that readiness check.
    #[tokio::test]
    async fn session_start_denies_over_cap_and_uncertified() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let token = capped_token(&state, "little_kid");
        let library = state
            .db
            .create_library(&NewLibrary {
                name: "movies".to_string(),
                path: dir.path().to_string_lossy().into_owned(),
                kind: "movies".to_string(),
            })
            .unwrap()
            .id;
        let in_cap = seed_movie(&state, dir.path(), library, "1", Some("{\"US\":\"G\"}"));
        let over_cap = seed_movie(&state, dir.path(), library, "2", Some("{\"US\":\"R\"}"));
        let uncertified = seed_movie(&state, dir.path(), library, "3", None);

        // Positive control: the at-cap title clears the visibility gate, so
        // the handler answers its own readiness refusal (415: not probed), not
        // the gate's 404.
        let (status, body) = start_session(&state, in_cap, &token).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{body}");

        // Over-cap and uncertified get the same 404 a missing item gets.
        for denied in [over_cap, uncertified] {
            let (status, body) = start_session(&state, denied, &token).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
            assert_eq!(
                error_message(&body),
                format!("item {denied} not found"),
                "{body}"
            );
        }
    }
}
