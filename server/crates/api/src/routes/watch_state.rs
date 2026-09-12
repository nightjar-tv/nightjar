//! Watch state (ADR-0035 item 6 and the 2026-09-12 amendment).
//!
//! The route is profile-scoped in the path and the item key is an opaque query
//! parameter, because a path key contains slashes and spaces. The request
//! carries a position and a duration and nothing else: `played` is derived and
//! `hidden` belongs to the later rollup block.

use crate::authority::{Caller, INSUFFICIENT_ROLE};
use crate::error::{ApiError, ApiResult, TypedJson, blocking};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use nightjar_db::WatchStateRow;
use nightjar_metadata::{WatchError, WatchWriteOutcome, read_watch_state, write_watch_report};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchStateQuery {
    pub item_key: Option<String>,
}

/// The whole PUT body (ADR-0035 amendment item 1). `played`, `hidden` and any
/// client timestamp are not fields, so the server cannot be told them. A
/// prohibited or unknown field is refused with the typed 422 rather than
/// ignored: the body is exactly these two values.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WatchReportRequest {
    pub position_ms: i64,
    pub duration_ms: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchStateDto {
    pub item_key: String,
    pub position_ms: i64,
    pub duration_ms: i64,
    pub played: bool,
    pub hidden: bool,
    pub first_played_at: String,
    pub last_played_at: String,
}

impl From<WatchStateRow> for WatchStateDto {
    fn from(row: WatchStateRow) -> Self {
        Self {
            item_key: row.item_key,
            position_ms: row.position_ms,
            duration_ms: row.duration_ms,
            played: row.played,
            hidden: row.hidden,
            first_played_at: row.first_played_at,
            last_played_at: row.last_played_at,
        }
    }
}

#[derive(Serialize)]
pub struct WatchStateEnvelope {
    pub state: Option<WatchStateDto>,
}

pub async fn get_state(
    State(state): State<AppState>,
    caller: Caller,
    Path(profile_ref): Path<String>,
    Query(query): Query<WatchStateQuery>,
) -> ApiResult<Json<WatchStateEnvelope>> {
    blocking(move || {
        let item_key = required_item_key(&query)?;
        let profile_id = authorize_profile(&state, &caller, &profile_ref)?;
        let found = state
            .db
            .with_conn(|conn| Ok(read_watch_state(conn, profile_id, &item_key)))
            .map_err(ApiError::internal)?;
        match found {
            Ok(row) => Ok(Json(WatchStateEnvelope {
                state: row.map(WatchStateDto::from),
            })),
            Err(WatchError::UnresolvedKey) => Err(unresolved_item(&item_key)),
            Err(WatchError::Invalid(message)) => Err(ApiError::unprocessable(message)),
            Err(WatchError::Db(error)) => Err(ApiError::internal(error)),
        }
    })
    .await
}

pub async fn put_state(
    State(state): State<AppState>,
    caller: Caller,
    Path(profile_ref): Path<String>,
    Query(query): Query<WatchStateQuery>,
    TypedJson(body): TypedJson<WatchReportRequest>,
) -> ApiResult<Json<WatchStateEnvelope>> {
    blocking(move || {
        let item_key = required_item_key(&query)?;
        let profile_id = authorize_profile(&state, &caller, &profile_ref)?;
        let outcome = state
            .db
            .with_conn(|conn| {
                Ok(write_watch_report(
                    conn,
                    profile_id,
                    &item_key,
                    body.position_ms,
                    body.duration_ms,
                ))
            })
            .map_err(ApiError::internal)?;
        match outcome {
            Ok(WatchWriteOutcome::Cleared) => Ok(Json(WatchStateEnvelope { state: None })),
            Ok(WatchWriteOutcome::Stored(row)) => Ok(Json(WatchStateEnvelope {
                state: Some(WatchStateDto::from(row)),
            })),
            Err(WatchError::UnresolvedKey) => Err(unresolved_item(&item_key)),
            Err(WatchError::Invalid(message)) => Err(ApiError::unprocessable(message)),
            Err(WatchError::Db(error)) => Err(ApiError::internal(error)),
        }
    })
    .await
}

/// The key stays opaque here: the handler only moves the string to the
/// identity layer, which is the one place that knows its grammar.
fn required_item_key(query: &WatchStateQuery) -> ApiResult<String> {
    match query.item_key.as_deref() {
        Some(key) if !key.is_empty() => Ok(key.to_string()),
        _ => Err(ApiError::bad_request("itemKey is required")),
    }
}

/// Resolve `profile_ref` and apply ADR-0035 item 7.
///
/// A profile that does not exist and one this caller may not address get the
/// same named forbidden error, so the response cannot be used to probe for a
/// ref.
fn authorize_profile(state: &AppState, caller: &Caller, profile_ref: &str) -> ApiResult<i64> {
    let profile = state
        .db
        .with_conn(|conn| nightjar_db::profile_by_ref(conn, profile_ref))
        .map_err(ApiError::internal)?;
    match profile.filter(|p| caller.may_address_profile(p)) {
        Some(profile) => Ok(profile.id),
        None => Err(ApiError::forbidden(INSUFFICIENT_ROLE)),
    }
}

fn unresolved_item(item_key: &str) -> ApiError {
    ApiError::not_found(format!("item key {item_key} not found"))
}

#[cfg(test)]
mod tests {
    use crate::routes::router;
    use crate::state::{AppState, test_support};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use nightjar_auth::mint_session_token;
    use nightjar_db::{NewLibrary, UpsertItem};
    use tower::ServiceExt;

    struct Actor {
        account_id: i64,
        profile_id: i64,
        profile_ref: String,
    }

    fn actor(state: &AppState, username: &str, role: &str, profile_ref: &str) -> Actor {
        let (account_id, profile_id) = state
            .db
            .with_conn(|conn| {
                let hash = nightjar_auth::hash_password("x").map_err(|e| format!("hash: {e:?}"))?;
                nightjar_db::create_account_with_profile(
                    conn,
                    username,
                    &hash,
                    role,
                    "P",
                    profile_ref,
                )?;
                let account = nightjar_db::account_by_username(conn, username)?.unwrap();
                let profile = nightjar_db::profile_by_ref(conn, profile_ref)?.unwrap();
                Ok((account.id, profile.id))
            })
            .unwrap();
        Actor {
            account_id,
            profile_id,
            profile_ref: profile_ref.to_string(),
        }
    }

    /// A bearer token for `actor`, optionally narrowed to `profile_id`.
    fn token(state: &AppState, actor: &Actor, scoped: bool) -> String {
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
                if scoped {
                    nightjar_db::set_active_profile(conn, session, Some(actor.profile_id))?;
                }
                Ok(())
            })
            .unwrap();
        minted.plaintext
    }

    /// One resolvable item and its path key.
    fn seed_item(state: &AppState) -> String {
        let library = state
            .db
            .create_library(&NewLibrary {
                name: "movies".to_string(),
                path: "/media/movies".to_string(),
                kind: "movies".to_string(),
            })
            .unwrap();
        state
            .db
            .upsert_items_indexed(
                library.id,
                &[UpsertItem {
                    path: "a.mkv".to_string(),
                    mtime_ms: 0,
                    size_bytes: 1,
                    title: "A".to_string(),
                    kind: "movie".to_string(),
                    year: None,
                    season: None,
                    episode: None,
                    content_id: None,
                }],
            )
            .unwrap();
        format!("path:{}:a.mkv", library.id)
    }

    async fn send(
        state: &AppState,
        method: &str,
        uri: &str,
        body: &str,
        token: &str,
    ) -> (StatusCode, String) {
        send_with(state, method, uri, body, Some("application/json"), token).await
    }

    /// Like [`send`], but able to omit the content type, so the 415 path is
    /// exercised through the real router rather than asserted from the type.
    async fn send_with(
        state: &AppState,
        method: &str,
        uri: &str,
        body: &str,
        content_type: Option<&str>,
        token: &str,
    ) -> (StatusCode, String) {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"));
        if let Some(content_type) = content_type {
            builder = builder.header("content-type", content_type);
        }
        let request = builder.body(Body::from(body.to_string())).unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let text = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        (status, text)
    }

    /// The `code` field of a typed error body. Asserting this rather than a
    /// substring pins the machine-readable class a client branches on.
    fn error_code(body: &str) -> String {
        let value: serde_json::Value = serde_json::from_str(body)
            .unwrap_or_else(|e| panic!("error body is not JSON: {e}: {body}"));
        value
            .get("code")
            .and_then(|code| code.as_str())
            .unwrap_or_else(|| panic!("no code in {body}"))
            .to_string()
    }

    fn uri(profile_ref: &str, item_key: &str) -> String {
        format!(
            "/api/v0/profiles/{profile_ref}/watch-state?itemKey={}",
            urlencode(item_key)
        )
    }

    /// Minimal percent-encoding for the query value, so a path key with
    /// slashes and spaces travels the way a client sends it.
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

    /// The authorized write and read, plus the persistence of a reverse seek:
    /// a later lower position overwrites the earlier higher one.
    #[tokio::test]
    async fn a_profile_writes_and_reads_its_own_state() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let owner = actor(&state, "owner", "owner", "aa");
        let key = seed_item(&state);
        let token = token(&state, &owner, true);

        let (status, body) = send(
            &state,
            "PUT",
            &uri(&owner.profile_ref, &key),
            r#"{"positionMs":8000,"durationMs":10000}"#,
            &token,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "write: {body}");
        assert!(body.contains("\"played\":false"), "{body}");
        assert!(body.contains("\"positionMs\":8000"), "{body}");

        // Reverse seek: the later lower report wins (item 3).
        let (status, body) = send(
            &state,
            "PUT",
            &uri(&owner.profile_ref, &key),
            r#"{"positionMs":1000,"durationMs":10000}"#,
            &token,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "reverse seek: {body}");
        assert!(body.contains("\"positionMs\":1000"), "{body}");

        let (status, body) =
            send(&state, "GET", &uri(&owner.profile_ref, &key), "{}", &token).await;
        assert_eq!(status, StatusCode::OK, "read: {body}");
        assert!(body.contains("\"positionMs\":1000"), "{body}");
    }

    /// A client cannot set `played`; the server derives it. At 95% the row is
    /// played even though the body has no such field, and a later 30% report
    /// clears it (the rewatch case).
    #[tokio::test]
    async fn played_is_derived_and_a_rewatch_clears_it() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let owner = actor(&state, "owner", "owner", "aa");
        let key = seed_item(&state);
        let token = token(&state, &owner, true);

        let (status, body) = send(
            &state,
            "PUT",
            &uri(&owner.profile_ref, &key),
            r#"{"positionMs":9500,"durationMs":10000}"#,
            &token,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("\"played\":true"), "95% is played: {body}");

        let (status, body) = send(
            &state,
            "PUT",
            &uri(&owner.profile_ref, &key),
            r#"{"positionMs":3000,"durationMs":10000}"#,
            &token,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body.contains("\"played\":false"),
            "a rewatch clears played: {body}"
        );
    }

    /// The body is exactly `positionMs` and `durationMs`. A field that would
    /// set server-derived state, a client clock, a numeric profile id, or
    /// anything else unknown is refused with the typed 422 rather than ignored.
    #[tokio::test]
    async fn prohibited_and_unknown_fields_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let owner = actor(&state, "owner", "owner", "aa");
        let key = seed_item(&state);
        let token = token(&state, &owner, true);

        for field in [
            r#""played":true"#,
            r#""hidden":true"#,
            r#""timestamp":"2099-01-01T00:00:00.000Z""#,
            r#""profileId":1"#,
            r#""surprise":true"#,
        ] {
            let body = format!(r#"{{"positionMs":1000,"durationMs":10000,{field}}}"#);
            let (status, text) =
                send(&state, "PUT", &uri(&owner.profile_ref, &key), &body, &token).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}: {text}");
            assert_eq!(error_code(&text), "validation_error", "{body}: {text}");
            assert!(text.contains("unknown field"), "{body}: {text}");
        }

        // The refusal is about the body: none of those writes landed.
        let (status, text) =
            send(&state, "GET", &uri(&owner.profile_ref, &key), "{}", &token).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert!(text.contains("\"state\":null"), "{text}");
    }

    /// A body the server cannot read at all is 400, a body of the wrong shape
    /// is the typed 422, and a body sent with no JSON content type is 415.
    /// Each carries the typed error shape, not axum's plain-text rejection.
    #[tokio::test]
    async fn malformed_json_and_content_type_get_typed_errors() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let owner = actor(&state, "owner", "owner", "aa");
        let key = seed_item(&state);
        let token = token(&state, &owner, true);
        let uri = uri(&owner.profile_ref, &key);

        // Syntactically invalid JSON: a request the server could not read.
        let (status, text) = send(&state, "PUT", &uri, r#"{"positionMs":1000,"#, &token).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
        assert_eq!(error_code(&text), "bad_request", "{text}");

        // Valid JSON, wrong shape: a missing required field, a field of the
        // wrong type, and a non-object body are one typed 422.
        for body in [
            r#"{"positionMs":1000}"#,
            r#"{"positionMs":"soon","durationMs":10000}"#,
            r#"[1000,10000]"#,
        ] {
            let (status, text) = send(&state, "PUT", &uri, body, &token).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}: {text}");
            assert_eq!(error_code(&text), "validation_error", "{body}: {text}");
        }

        // No JSON content type at all.
        let (status, text) = send_with(
            &state,
            "PUT",
            &uri,
            r#"{"positionMs":1000,"durationMs":10000}"#,
            None,
            &token,
        )
        .await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{text}");
        assert_eq!(error_code(&text), "unsupported_media_type", "{text}");

        // Nothing landed through any of the refusals.
        let (status, text) = send(&state, "GET", &uri, "{}", &token).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert!(text.contains("\"state\":null"), "{text}");
    }

    /// Below 2% removes state, and GET then reports no state distinctly from
    /// a 404.
    #[tokio::test]
    async fn below_the_floor_removes_state_and_get_reports_null() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let owner = actor(&state, "owner", "owner", "aa");
        let key = seed_item(&state);
        let token = token(&state, &owner, true);

        let (status, body) = send(
            &state,
            "PUT",
            &uri(&owner.profile_ref, &key),
            r#"{"positionMs":199,"durationMs":10000}"#,
            &token,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("\"state\":null"), "{body}");

        let (status, body) =
            send(&state, "GET", &uri(&owner.profile_ref, &key), "{}", &token).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("\"state\":null"), "{body}");
    }

    /// A missing `itemKey` is a 400, an unresolved one is a 404, and neither
    /// writes a row.
    #[tokio::test]
    async fn missing_and_unresolved_keys_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let owner = actor(&state, "owner", "owner", "aa");
        let token = token(&state, &owner, true);

        let (status, body) = send(
            &state,
            "PUT",
            &format!("/api/v0/profiles/{}/watch-state", owner.profile_ref),
            r#"{"positionMs":1000,"durationMs":10000}"#,
            &token,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        let (status, body) = send(
            &state,
            "PUT",
            &uri(&owner.profile_ref, "path:1:missing.mkv"),
            r#"{"positionMs":1000,"durationMs":10000}"#,
            &token,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

        let (status, body) = send(
            &state,
            "GET",
            &uri(&owner.profile_ref, "tmdb:show:1396"),
            "{}",
            &token,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }

    /// Zero duration and a position past the duration are the typed validation
    /// response, not a 400.
    #[tokio::test]
    async fn invalid_values_are_422() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let owner = actor(&state, "owner", "owner", "aa");
        let key = seed_item(&state);
        let token = token(&state, &owner, true);

        for body in [
            r#"{"positionMs":1000,"durationMs":0}"#,
            r#"{"positionMs":10001,"durationMs":10000}"#,
        ] {
            let (status, text) =
                send(&state, "PUT", &uri(&owner.profile_ref, &key), body, &token).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}: {text}");
            assert!(text.contains("validation_error"), "{text}");
        }
    }

    /// The authorization rule: a profile session reaches only its active
    /// profile, a member account scope only its own account's profiles, and an
    /// owner reaches any profile. The refusal is identical for a missing ref.
    #[tokio::test]
    async fn profile_authorization_is_centralized() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let owner = actor(&state, "owner", "owner", "aa");
        let member = actor(&state, "member", "member", "bb");
        let manager = actor(&state, "manager", "manager", "cc");
        let key = seed_item(&state);

        let watcher = token(&state, &member, true);
        let member_scope = token(&state, &member, false);
        let owner_scope = token(&state, &owner, false);
        let manager_scope = token(&state, &manager, false);

        // A profile session reaches its own profile.
        let (status, body) = send(
            &state,
            "GET",
            &uri(&member.profile_ref, &key),
            "{}",
            &watcher,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        // ... and no other, whether it exists or not. Both refusals are the
        // same named forbidden shape.
        for ref_ in [&owner.profile_ref, "ffffffffffffffffffffffffffffffff"] {
            let (status, body) = send(&state, "GET", &uri(ref_, &key), "{}", &watcher).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{ref_}: {body}");
            assert!(body.contains("insufficient_role"), "{body}");
        }

        // A numeric profile id is not a ref, so it gets the same answer rather
        // than resolving to a rowid.
        let (status, _) = send(&state, "GET", &uri("1", &key), "{}", &watcher).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // A member account scope reaches its own account's profiles and not
        // another account's.
        let (status, body) = send(
            &state,
            "GET",
            &uri(&member.profile_ref, &key),
            "{}",
            &member_scope,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, _) = send(
            &state,
            "GET",
            &uri(&owner.profile_ref, &key),
            "{}",
            &member_scope,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // An owner or manager account scope reaches any profile.
        for scope in [&owner_scope, &manager_scope] {
            let (status, body) =
                send(&state, "GET", &uri(&member.profile_ref, &key), "{}", scope).await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
    }

    /// One profile's state is invisible to another profile.
    #[tokio::test]
    async fn profiles_do_not_share_state() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let owner = actor(&state, "owner", "owner", "aa");
        let other = actor(&state, "other", "member", "bb");
        let key = seed_item(&state);

        let owner_token = token(&state, &owner, true);
        let (status, body) = send(
            &state,
            "PUT",
            &uri(&owner.profile_ref, &key),
            r#"{"positionMs":5000,"durationMs":10000}"#,
            &owner_token,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        // The owner in account scope may read the other profile, which has no
        // state of its own.
        let owner_scope = token(&state, &owner, false);
        let (status, body) = send(
            &state,
            "GET",
            &uri(&other.profile_ref, &key),
            "{}",
            &owner_scope,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("\"state\":null"), "{body}");
    }
}
