//! Profile track choices (ADR-0038 item 7 and its 2026-09-12 amendment).
//!
//! `PUT /api/v0/profiles/{profileRef}/track-choice?seriesKey=…` is a full
//! replacement. The `seriesKey` is an opaque query parameter because a
//! `folder:`-keyed series key contains slashes (ADR-0035 item 6), and it must
//! resolve through the effective series identity layer or the write is a typed
//! 404. Only the profile-scope session for that exact profile may write.

use crate::authority::{Caller, INSUFFICIENT_ROLE};
use crate::error::{ApiError, ApiResult, TypedJson, blocking};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use nightjar_db::{SubtitleChoiceRow, TrackChoiceRow, TrackDescription};
use nightjar_metadata::{TrackChoiceError, write_track_choice};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackChoiceQuery {
    pub series_key: Option<String>,
}

/// `TrackDescription` is exactly these four fields (ADR-0038 amendment §2).
#[derive(Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrackDescriptionDto {
    #[serde(default)]
    pub language: Option<String>,
    pub kind: String,
    pub sdh: bool,
    pub forced: bool,
}

/// The tagged subtitle choice as a struct with an explicit `mode`, so an
/// unknown field is refused with the typed 422. An internally tagged serde enum
/// cannot deny unknown fields, and the ADR requires the refusal.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubtitleChoiceRequest {
    pub mode: String,
    #[serde(default)]
    pub track: Option<TrackDescriptionDto>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrackChoiceRequest {
    #[serde(default)]
    pub audio: Option<TrackDescriptionDto>,
    pub subtitle: SubtitleChoiceRequest,
}

#[derive(Serialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum SubtitleChoiceDto {
    Unset,
    Off,
    Track { track: TrackDescriptionDto },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackChoiceDto {
    pub series_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<TrackDescriptionDto>,
    pub subtitle: SubtitleChoiceDto,
    pub updated_at: String,
}

#[derive(Serialize)]
pub struct TrackChoiceEnvelope {
    pub choice: TrackChoiceDto,
}

pub async fn put_choice(
    State(state): State<AppState>,
    caller: Caller,
    Path(profile_ref): Path<String>,
    Query(query): Query<TrackChoiceQuery>,
    TypedJson(body): TypedJson<TrackChoiceRequest>,
) -> ApiResult<Json<TrackChoiceEnvelope>> {
    blocking(move || {
        let series_key = query
            .series_key
            .filter(|key| !key.is_empty())
            .ok_or_else(|| ApiError::bad_request("seriesKey is required"))?;
        let profile_id = authorize_active_profile(&state, &caller, &profile_ref)?;
        let audio = body.audio.as_ref().map(to_description).transpose()?;
        let subtitle = to_subtitle(&body.subtitle)?;
        let outcome = state
            .db
            .with_conn(|conn| {
                Ok(write_track_choice(
                    conn,
                    profile_id,
                    &series_key,
                    audio.as_ref(),
                    &subtitle,
                ))
            })
            .map_err(ApiError::internal)?;
        match outcome {
            Ok(row) => Ok(Json(TrackChoiceEnvelope {
                choice: choice_dto(row),
            })),
            Err(TrackChoiceError::UnresolvedSeriesKey) => Err(ApiError::not_found(format!(
                "series key {series_key} not found"
            ))),
            Err(TrackChoiceError::Db(error)) => Err(ApiError::internal(error)),
        }
    })
    .await
}

/// Only the profile-scope session for this exact profile may write
/// (ADR-0038 item 7). An account-scope token, another profile, and a ref that
/// does not exist all get the same non-leaking forbidden response.
fn authorize_active_profile(
    state: &AppState,
    caller: &Caller,
    profile_ref: &str,
) -> ApiResult<i64> {
    let profile = state
        .db
        .with_conn(|conn| nightjar_db::profile_by_ref(conn, profile_ref))
        .map_err(ApiError::internal)?;
    match profile.filter(|p| caller.session.active_profile_id == Some(p.id)) {
        Some(profile) => Ok(profile.id),
        None => Err(ApiError::forbidden(INSUFFICIENT_ROLE)),
    }
}

fn to_description(dto: &TrackDescriptionDto) -> ApiResult<TrackDescription> {
    if !matches!(dto.kind.as_str(), "main" | "commentary" | "signs") {
        return Err(ApiError::unprocessable(
            "kind must be \"main\", \"commentary\" or \"signs\"",
        ));
    }
    Ok(TrackDescription {
        language: dto.language.clone(),
        kind: dto.kind.clone(),
        sdh: dto.sdh,
        forced: dto.forced,
    })
}

fn to_subtitle(request: &SubtitleChoiceRequest) -> ApiResult<SubtitleChoiceRow> {
    match request.mode.as_str() {
        "unset" => {
            if request.track.is_some() {
                return Err(ApiError::unprocessable(
                    "mode \"unset\" must not carry a track",
                ));
            }
            Ok(SubtitleChoiceRow::Unset)
        }
        "off" => {
            if request.track.is_some() {
                return Err(ApiError::unprocessable(
                    "mode \"off\" must not carry a track",
                ));
            }
            Ok(SubtitleChoiceRow::Off)
        }
        "track" => {
            let track = request
                .track
                .as_ref()
                .ok_or_else(|| ApiError::unprocessable("mode \"track\" requires a track"))?;
            Ok(SubtitleChoiceRow::Track(to_description(track)?))
        }
        _ => Err(ApiError::unprocessable(
            "mode must be \"unset\", \"off\" or \"track\"",
        )),
    }
}

fn choice_dto(row: TrackChoiceRow) -> TrackChoiceDto {
    TrackChoiceDto {
        series_key: row.series_key,
        audio: row.audio.map(description_dto),
        subtitle: match row.subtitle {
            SubtitleChoiceRow::Unset => SubtitleChoiceDto::Unset,
            SubtitleChoiceRow::Off => SubtitleChoiceDto::Off,
            SubtitleChoiceRow::Track(track) => SubtitleChoiceDto::Track {
                track: description_dto(track),
            },
        },
        updated_at: row.updated_at,
    }
}

fn description_dto(description: TrackDescription) -> TrackDescriptionDto {
    TrackDescriptionDto {
        language: description.language,
        kind: description.kind,
        sdh: description.sdh,
        forced: description.forced,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn description_kind_is_a_closed_set() {
        for bad in ["dub", "Main", "", "forced"] {
            let dto = TrackDescriptionDto {
                language: Some("en".into()),
                kind: bad.into(),
                sdh: false,
                forced: false,
            };
            let err = to_description(&dto).unwrap_err();
            assert_eq!(err.status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
        }
        for good in ["main", "commentary", "signs"] {
            let dto = TrackDescriptionDto {
                language: None,
                kind: good.into(),
                sdh: true,
                forced: false,
            };
            assert!(to_description(&dto).is_ok(), "{good}");
        }
    }

    #[test]
    fn subtitle_modes_carry_exactly_their_shape() {
        let unset = to_subtitle(&SubtitleChoiceRequest {
            mode: "unset".into(),
            track: None,
        })
        .unwrap();
        assert_eq!(unset, SubtitleChoiceRow::Unset);

        let off = to_subtitle(&SubtitleChoiceRequest {
            mode: "off".into(),
            track: None,
        })
        .unwrap();
        assert_eq!(off, SubtitleChoiceRow::Off);

        // A track mode with a track is fine; with none it is 422.
        assert!(
            to_subtitle(&SubtitleChoiceRequest {
                mode: "track".into(),
                track: Some(TrackDescriptionDto {
                    language: Some("en".into()),
                    kind: "main".into(),
                    sdh: false,
                    forced: false,
                }),
            })
            .is_ok()
        );
        assert!(
            to_subtitle(&SubtitleChoiceRequest {
                mode: "track".into(),
                track: None,
            })
            .is_err()
        );
        // A mode that carries a stray track is refused.
        assert!(
            to_subtitle(&SubtitleChoiceRequest {
                mode: "off".into(),
                track: Some(TrackDescriptionDto {
                    language: None,
                    kind: "main".into(),
                    sdh: false,
                    forced: false,
                }),
            })
            .is_err()
        );
        assert!(
            to_subtitle(&SubtitleChoiceRequest {
                mode: "maybe".into(),
                track: None,
            })
            .is_err()
        );
    }
}

#[cfg(test)]
mod routing_tests {
    use crate::routes::router;
    use crate::state::{AppState, test_support};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use nightjar_auth::mint_session_token;
    use nightjar_db::SubtitleChoiceRow;
    use tower::ServiceExt;

    struct Actor {
        profile_id: i64,
        profile_ref: String,
    }

    fn actor(state: &AppState, username: &str, profile_ref: &str) -> Actor {
        let profile_id = state
            .db
            .with_conn(|conn| {
                let hash = nightjar_auth::hash_password("x").map_err(|e| format!("hash: {e:?}"))?;
                nightjar_db::create_account_with_profile(
                    conn,
                    username,
                    &hash,
                    "member",
                    "P",
                    profile_ref,
                )?;
                let profile = nightjar_db::profile_by_ref(conn, profile_ref)?.unwrap();
                Ok(profile.id)
            })
            .unwrap();
        Actor {
            profile_id,
            profile_ref: profile_ref.to_string(),
        }
    }

    fn token(state: &AppState, actor: &Actor, scoped: bool) -> String {
        let minted = mint_session_token();
        state
            .db
            .with_conn(|conn| {
                let profile = nightjar_db::profile_by_id(conn, actor.profile_id)?.unwrap();
                let expires = nightjar_db::session_expiry(conn)?;
                let session = nightjar_db::create_session(
                    conn,
                    profile.account_id,
                    &minted.sha256_hex,
                    "t",
                    &expires,
                )?;
                if scoped {
                    nightjar_db::set_active_profile(conn, session, Some(actor.profile_id))?;
                }
                Ok(())
            })
            .unwrap();
        minted.plaintext
    }

    /// A show library with one unmatched folder whose relpath contains a
    /// slash. `folder:` keys are the population ADR-0038 wrote the query
    /// parameter for, so the slash is the point of the test.
    fn seed_folder(state: &AppState) -> (i64, String) {
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
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO series (library_id, relpath, tmdb_show_id)
                     VALUES (?1, 'Nested/Alpha', NULL)",
                    [library.id],
                )
                .map_err(|e| e.to_string())?;
                Ok(())
            })
            .unwrap();
        (library.id, "Nested/Alpha".to_string())
    }

    async fn send(
        state: &AppState,
        profile_ref: &str,
        series_key: &str,
        body: &str,
        token: &str,
    ) -> (StatusCode, String) {
        let uri = format!(
            "/api/v0/profiles/{profile_ref}/track-choice?seriesKey={}",
            urlencode(series_key)
        );
        let request = Request::builder()
            .method("PUT")
            .uri(uri)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let text = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        (status, text)
    }

    fn urlencode(value: &str) -> String {
        value
            .bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    }

    fn error_code(body: &str) -> String {
        let value: serde_json::Value = serde_json::from_str(body)
            .unwrap_or_else(|e| panic!("error body is not JSON: {e}: {body}"));
        value
            .get("code")
            .and_then(|code| code.as_str())
            .unwrap_or_else(|| panic!("no code in {body}"))
            .to_string()
    }

    /// The authorized write through the real router, with a `folder:` key that
    /// contains a slash. The stored row is asserted, not just the response.
    #[tokio::test]
    async fn a_profile_writes_a_choice_against_a_slashed_folder_key() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let member = actor(&state, "m", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let (library_id, relpath) = seed_folder(&state);
        let series_key = format!("folder:{library_id}:{relpath}");
        let token = token(&state, &member, true);

        let (status, body) = send(
            &state,
            &member.profile_ref,
            &series_key,
            r#"{"audio":{"language":"ja","kind":"main","sdh":false,"forced":false},
                "subtitle":{"mode":"off"}}"#,
            &token,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body.contains(&format!("\"seriesKey\":\"{series_key}\"")),
            "{body}"
        );
        assert!(body.contains("\"mode\":\"off\""), "{body}");

        let stored = state
            .db
            .with_conn(|conn| nightjar_db::load_track_choice(conn, member.profile_id, &series_key))
            .unwrap()
            .expect("the row is stored");
        assert_eq!(stored.subtitle, SubtitleChoiceRow::Off);
        assert_eq!(stored.audio.unwrap().language.as_deref(), Some("ja"));
    }

    /// Only the profile-scope session for that exact profile may write. An
    /// account-scope token and another profile get the same named refusal.
    #[tokio::test]
    async fn only_the_active_profile_may_write() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let member = actor(&state, "m", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let other = actor(&state, "o", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let (library_id, relpath) = seed_folder(&state);
        let series_key = format!("folder:{library_id}:{relpath}");
        let body = r#"{"audio":null,"subtitle":{"mode":"unset"}}"#;
        let watcher = token(&state, &member, true);
        let account_scope = token(&state, &member, false);

        for (token, label) in [(&account_scope, "account scope"), (&watcher, "other")] {
            let (status, text) = send(&state, &other.profile_ref, &series_key, body, token).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{label}: {text}");
            assert!(text.contains("insufficient_role"), "{label}: {text}");
        }
        let (status, text) = send(
            &state,
            &member.profile_ref,
            &series_key,
            body,
            &account_scope,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "account scope: {text}");
    }

    /// The body is exactly the tagged shape. Unknown fields, unknown kinds,
    /// and an unknown mode are the typed 422 rather than an ignored field or a
    /// database error.
    #[tokio::test]
    async fn invalid_bodies_are_typed_422() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let member = actor(&state, "m", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let (library_id, relpath) = seed_folder(&state);
        let series_key = format!("folder:{library_id}:{relpath}");
        let token = token(&state, &member, true);

        for body in [
            r#"{"audio":null,"subtitle":{"mode":"unset"},"surprise":true}"#,
            r#"{"audio":{"language":"en","kind":"dub","sdh":false,"forced":false},"subtitle":{"mode":"unset"}}"#,
            r#"{"audio":null,"subtitle":{"mode":"maybe"}}"#,
            r#"{"audio":null,"subtitle":{"mode":"track"}}"#,
            r#"{"audio":{"language":"en","kind":"main","sdh":false,"forced":false,"extra":1},"subtitle":{"mode":"unset"}}"#,
        ] {
            let (status, text) = send(&state, &member.profile_ref, &series_key, body, &token).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}: {text}");
            assert_eq!(error_code(&text), "validation_error", "{body}: {text}");
        }

        // Nothing landed.
        let count = state
            .db
            .with_conn(|conn| {
                conn.query_row("SELECT COUNT(*) FROM profile_track_choice", [], |r| {
                    r.get::<_, i64>(0)
                })
                .map_err(|e| e.to_string())
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    /// A key that names no series is a 404 and writes nothing; a missing
    /// `seriesKey` is a 400.
    #[tokio::test]
    async fn unresolved_and_missing_keys_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let member = actor(&state, "m", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let token = token(&state, &member, true);
        let body = r#"{"audio":null,"subtitle":{"mode":"unset"}}"#;

        let (status, text) = send(
            &state,
            &member.profile_ref,
            "tmdb:show:999999",
            body,
            &token,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{text}");

        let request = Request::builder()
            .method("PUT")
            .uri(format!(
                "/api/v0/profiles/{}/track-choice",
                member.profile_ref
            ))
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
