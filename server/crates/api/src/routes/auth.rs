//! Bootstrap, login, session scope and sign-out (ADR-0034 items 3, 5, 10, 11).

use crate::authority::{Caller, SESSION_COOKIE};
use crate::error::{ApiError, ApiResult, blocking};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use nightjar_auth::{
    VerifyOutcome, hash_password, mint_profile_ref, mint_session_token, verify_login,
};
use nightjar_core::Role;
use nightjar_db::{AccountRow, Db, ProfileRow};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupStateDto {
    pub admin_exists: bool,
    pub library_exists: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountDto {
    pub id: i64,
    pub username: String,
    pub role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_concurrent_sessions: Option<i64>,
    /// Account policy bitrate ceiling; null/absent means no ceiling
    /// (ADR-0022 §5 as amended 2026-09-12).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bitrate_bps: Option<i64>,
    /// Account policy height ceiling; null/absent means no ceiling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_height: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileDto {
    pub profile_ref: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classification_cap: Option<String>,
    pub simple_interface: bool,
    /// ISO-639-1-shaped lowercase code, or null for no preference
    /// (ADR-0038 amendment §2).
    pub preferred_language: Option<String>,
    /// `auto` | `off`.
    pub subtitle_default: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginResponseDto {
    pub token: String,
    pub expires_at: String,
    pub account: AccountDto,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionViewDto {
    pub account: AccountDto,
    pub scope: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_profile: Option<ProfileDto>,
    pub client_label: String,
    pub expires_at: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapRequest {
    pub username: String,
    pub password: String,
    pub client_label: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
    pub client_label: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectProfileRequest {
    pub profile_ref: String,
}

#[derive(Deserialize)]
pub struct WidenRequest {
    pub password: String,
}

pub(crate) fn account_dto(row: &AccountRow) -> ApiResult<AccountDto> {
    let role = Role::parse(&row.role)
        .ok_or_else(|| ApiError::internal(format!("account {} has role {}", row.id, row.role)))?;
    Ok(AccountDto {
        id: row.id,
        username: row.username.clone(),
        role: role.as_str(),
        max_concurrent_sessions: row.max_concurrent_sessions,
        max_bitrate_bps: row.max_bitrate_bps,
        max_height: row.max_height,
    })
}

pub(crate) fn profile_dto(row: &ProfileRow) -> ProfileDto {
    ProfileDto {
        profile_ref: row.profile_ref.clone(),
        name: row.name.clone(),
        classification_cap: row.classification_cap.clone(),
        simple_interface: row.simple_interface,
        preferred_language: row.preferred_language.clone(),
        subtitle_default: row.subtitle_default.clone(),
    }
}

pub async fn setup_state(State(state): State<AppState>) -> ApiResult<Json<SetupStateDto>> {
    blocking(move || {
        state
            .db
            .with_conn(|conn| {
                Ok(SetupStateDto {
                    admin_exists: nightjar_db::admin_exists(conn)?,
                    library_exists: nightjar_db::library_exists(conn)?,
                })
            })
            .map(Json)
            .map_err(ApiError::internal)
    })
    .await
}

/// Issue a session and return the token exactly once, also setting the cookie.
fn issue_session(
    db: &Db,
    account: &AccountRow,
    client_label: &str,
    secure: bool,
) -> ApiResult<Response> {
    let minted = mint_session_token();
    let expires_at = db
        .with_conn(|conn| {
            let expires_at = nightjar_db::session_expiry(conn)?;
            nightjar_db::create_session(
                conn,
                account.id,
                &minted.sha256_hex,
                client_label,
                &expires_at,
            )?;
            Ok(expires_at)
        })
        .map_err(ApiError::internal)?;

    let body = LoginResponseDto {
        token: minted.plaintext.clone(),
        expires_at,
        account: account_dto(account)?,
    };
    let mut response = (StatusCode::OK, Json(body)).into_response();
    // One credential in two envelopes (ADR-0034 item 9). Scoped to /api/v0 so
    // it is not offered to the static asset routes, HttpOnly so script cannot
    // read it, Lax so a cross-site form cannot carry it.
    let mut cookie = format!(
        "{SESSION_COOKIE}={}; Path=/api/v0; HttpOnly; SameSite=Lax",
        minted.plaintext
    );
    if secure {
        cookie.push_str("; Secure");
    }
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    Ok(response)
}

pub async fn bootstrap(
    State(state): State<AppState>,
    Json(body): Json<BootstrapRequest>,
) -> ApiResult<Response> {
    blocking(move || {
        let username = body.username.trim();
        if username.is_empty() || body.password.is_empty() {
            return Err(ApiError::bad_request("username and password are required"));
        }
        // The one unauthenticated write, gated on exactly one condition.
        let already = state
            .db
            .with_conn(nightjar_db::account_exists)
            .map_err(ApiError::internal)?;
        if already {
            return Err(ApiError::conflict(
                "bootstrap_already_complete: an account already exists",
            ));
        }
        let hash = hash_password(&body.password)
            .map_err(|e| ApiError::internal(format!("hash password: {e:?}")))?;
        let account = state
            .db
            .with_conn(|conn| {
                nightjar_db::create_account_with_profile(
                    conn,
                    username,
                    &hash,
                    Role::Owner.as_str(),
                    username,
                    &mint_profile_ref(),
                )?;
                nightjar_db::account_by_username(conn, username)?
                    .ok_or_else(|| "account vanished after creation".to_string())
            })
            .map_err(ApiError::internal)?;

        let label = body.client_label.as_deref().unwrap_or("first run");
        let mut response = issue_session(&state.db, &account, label, false)?;
        *response.status_mut() = StatusCode::CREATED;
        Ok(response)
    })
    .await
}

/// Verify a login and upgrade the stored hash if it is below the current
/// parameters. Takes `&Db` rather than `AppState` so it is testable without
/// spawning worker threads and probing ffmpeg.
pub(crate) fn authenticate(db: &Db, username: &str, password: &str) -> ApiResult<AccountRow> {
    let stored = db
        .with_conn(|conn| nightjar_db::account_by_username(conn, username.trim()))
        .map_err(ApiError::internal)?;

    // One call whether or not the account exists: the Option is the whole
    // mitigation, because a branch here would be the enumeration oracle.
    let outcome = verify_login(password, stored.as_ref().map(|a| a.password_hash.as_str()))
        .map_err(|e| ApiError::internal(format!("verify: {e:?}")))?;

    match (outcome, stored) {
        (VerifyOutcome::Wrong, _) | (_, None) => Err(ApiError::unauthorized(
            "invalid_credentials: username or password is wrong",
        )),
        (VerifyOutcome::Correct, Some(account)) => Ok(account),
        (VerifyOutcome::CorrectNeedsRehash, Some(account)) => {
            // ADR-0034 item 4: a hash below the current constants verifies
            // against its own parameters and is rehashed on this login.
            //
            // A failure here must not cost the user their session. The old
            // hash still verifies, so refusing the login would be strictly
            // worse than not upgrading; log and proceed.
            match hash_password(password) {
                Ok(fresh) => {
                    if let Err(e) = db
                        .with_conn(|conn| nightjar_db::set_password_hash(conn, account.id, &fresh))
                    {
                        tracing::warn!(account_id = account.id, error = %e, "rehash store failed");
                    } else {
                        tracing::info!(account_id = account.id, "password hash upgraded");
                    }
                }
                Err(e) => tracing::warn!(account_id = account.id, error = ?e, "rehash failed"),
            }
            Ok(account)
        }
    }
}

pub async fn login(
    State(state): State<AppState>,
    Json(body): Json<LoginRequest>,
) -> ApiResult<Response> {
    blocking(move || {
        let account = authenticate(&state.db, &body.username, &body.password)?;
        let label = body.client_label.as_deref().unwrap_or("unnamed device");
        issue_session(&state.db, &account, label, false)
    })
    .await
}

pub async fn logout(State(state): State<AppState>, caller: Caller) -> ApiResult<StatusCode> {
    blocking(move || {
        state
            .db
            .with_conn(|conn| {
                let now = nightjar_db::now_iso(conn)?;
                nightjar_db::revoke_session(conn, caller.session.id, &now)
            })
            .map_err(ApiError::internal)?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

pub async fn logout_all(State(state): State<AppState>, caller: Caller) -> ApiResult<StatusCode> {
    blocking(move || {
        state
            .db
            .with_conn(|conn| {
                let now = nightjar_db::now_iso(conn)?;
                nightjar_db::revoke_all_for_account(conn, caller.session.account_id, &now)?;
                Ok(())
            })
            .map_err(ApiError::internal)?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

pub async fn get_session(
    State(state): State<AppState>,
    caller: Caller,
) -> ApiResult<Json<SessionViewDto>> {
    blocking(move || {
        state
            .db
            .with_conn(|conn| {
                let account = nightjar_db::account_by_id(conn, caller.session.account_id)?
                    .ok_or_else(|| "session account missing".to_string())?;
                let active = match caller.session.active_profile_id {
                    Some(id) => nightjar_db::profiles_for_account(conn, account.id)?
                        .into_iter()
                        .find(|p| p.id == id),
                    None => None,
                };
                Ok((account, active))
            })
            .map_err(ApiError::internal)
            .and_then(|(account, active)| {
                Ok(Json(SessionViewDto {
                    account: account_dto(&account)?,
                    scope: if active.is_some() {
                        "profile"
                    } else {
                        "account"
                    },
                    active_profile: active.as_ref().map(profile_dto),
                    client_label: caller.session.client_label.clone(),
                    expires_at: caller.session.expires_at.clone(),
                }))
            })
    })
    .await
}

pub async fn select_profile(
    State(state): State<AppState>,
    caller: Caller,
    Json(body): Json<SelectProfileRequest>,
) -> ApiResult<StatusCode> {
    blocking(move || {
        let profile = state
            .db
            .with_conn(|conn| nightjar_db::profile_by_ref(conn, &body.profile_ref))
            .map_err(ApiError::internal)?;
        // Narrowing is free, but only within your own account, and a profile
        // on someone else's account is refused identically to one that does
        // not exist so the answer cannot be used to probe.
        let Some(profile) = profile.filter(|p| p.account_id == caller.session.account_id) else {
            return Err(ApiError::forbidden(
                "profile_not_on_account: no such profile on this account",
            ));
        };
        state
            .db
            .with_conn(|conn| {
                nightjar_db::set_active_profile(conn, caller.session.id, Some(profile.id))
            })
            .map_err(ApiError::internal)?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

pub async fn widen_to_account(
    State(state): State<AppState>,
    caller: Caller,
    Json(body): Json<WidenRequest>,
) -> ApiResult<StatusCode> {
    blocking(move || {
        let account = state
            .db
            .with_conn(|conn| nightjar_db::account_by_id(conn, caller.session.account_id))
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::internal("session account missing"))?;

        // Widening restores reach, so it re-authenticates. Without this a
        // capped viewer leaves the cap by navigating (ADR-0034 item 3).
        let outcome = verify_login(&body.password, Some(&account.password_hash))
            .map_err(|e| ApiError::internal(format!("verify: {e:?}")))?;
        if outcome == VerifyOutcome::Wrong {
            return Err(ApiError::unauthorized(
                "invalid_credentials: the session stays narrowed",
            ));
        }
        state
            .db
            .with_conn(|conn| nightjar_db::set_active_profile(conn, caller.session.id, None))
            .map_err(ApiError::internal)?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use argon2::password_hash::{PasswordHasher, SaltString, rand_core::OsRng};
    use argon2::{Algorithm, Argon2, Params, Version};
    use nightjar_auth::{
        ARGON2_MEMORY_KIB, ARGON2_OUTPUT_LEN, ARGON2_PARALLELISM, ARGON2_TIME_COST,
    };

    fn db() -> Db {
        Db::open(std::path::Path::new(":memory:")).unwrap()
    }

    fn stored_hash(db: &Db, username: &str) -> String {
        db.with_conn(|conn| {
            Ok(nightjar_db::account_by_username(conn, username)?
                .unwrap()
                .password_hash)
        })
        .unwrap()
    }

    /// A hash written under weaker parameters than today's.
    fn weak_hash(password: &str) -> String {
        let params = Params::new(8, 1, 1, Some(ARGON2_OUTPUT_LEN)).unwrap();
        let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let salt = SaltString::generate(&mut OsRng);
        argon
            .hash_password(password.as_bytes(), &salt)
            .unwrap()
            .to_string()
    }

    /// The slice-completion condition: `CorrectNeedsRehash` is *consumed*.
    ///
    /// Asserting only that the login succeeded would pass whether or not the
    /// rehash landed, so this reads the stored hash back and asserts it moved
    /// to the current parameters.
    #[test]
    fn a_weak_hash_is_upgraded_on_successful_login() {
        let db = db();
        let before = weak_hash("hunter2");
        db.with_conn(|conn| {
            nightjar_db::create_account_with_profile(conn, "a", &before, "owner", "P", "r0")?;
            Ok(())
        })
        .unwrap();

        authenticate(&db, "a", "hunter2").expect("weak hash must still verify");

        let after = stored_hash(&db, "a");
        assert_ne!(after, before, "the stored hash must have been replaced");

        let parsed = argon2::password_hash::PasswordHash::new(&after).unwrap();
        let params = Params::try_from(&parsed).unwrap();
        assert_eq!(params.m_cost(), ARGON2_MEMORY_KIB);
        assert_eq!(params.t_cost(), ARGON2_TIME_COST);
        assert_eq!(params.p_cost(), ARGON2_PARALLELISM);

        // And the upgrade did not lock the user out of their own password.
        authenticate(&db, "a", "hunter2").expect("the upgraded hash must verify");
    }

    /// The converse, so the test above is not passing because every login
    /// rewrites the hash.
    #[test]
    fn a_current_hash_is_left_alone_on_login() {
        let db = db();
        let hash = nightjar_auth::hash_password("hunter2").unwrap();
        db.with_conn(|conn| {
            nightjar_db::create_account_with_profile(conn, "a", &hash, "owner", "P", "r0")?;
            Ok(())
        })
        .unwrap();

        authenticate(&db, "a", "hunter2").unwrap();
        assert_eq!(
            stored_hash(&db, "a"),
            hash,
            "a hash already at current parameters must not be rewritten"
        );
    }

    /// ADR-0034 item 10: bootstrap is the **one unauthenticated write in the
    /// server**, gated on exactly one condition. Both halves are tested here
    /// against the real handler, because the gate is the whole control.
    ///
    /// That it is *unauthenticated* is enforced above the test, by the
    /// handler's signature: `bootstrap` takes no `Caller`, so it cannot
    /// require a credential. Adding one would be a deliberate edit the
    /// compiler forces, which is stronger than an assertion.
    #[tokio::test]
    async fn bootstrap_creates_the_first_owner_then_refuses_a_second() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::state::test_support::state(dir.path());

        let first = bootstrap(
            State(state.clone()),
            Json(BootstrapRequest {
                username: "founder".into(),
                password: "correct horse".into(),
                client_label: Some("first run".into()),
            }),
        )
        .await
        .expect("a server with no accounts must admit bootstrap");
        assert_eq!(first.status(), StatusCode::CREATED);

        // The first account is the owner, and it has a profile to write with.
        let (role, profiles) = state
            .db
            .with_conn(|conn| {
                let account = nightjar_db::account_by_username(conn, "founder")?.unwrap();
                let profiles = nightjar_db::profiles_for_account(conn, account.id)?;
                Ok((account.role, profiles.len()))
            })
            .unwrap();
        assert_eq!(role, "owner");
        assert_eq!(
            profiles, 1,
            "an account with no profile cannot write watch state"
        );

        let second = bootstrap(
            State(state.clone()),
            Json(BootstrapRequest {
                username: "usurper".into(),
                password: "any".into(),
                client_label: None,
            }),
        )
        .await
        .expect_err("a second bootstrap must be refused");
        assert_eq!(second.status, StatusCode::CONFLICT);
        assert!(
            second.message.starts_with("bootstrap_already_complete:"),
            "named, not a generic 403: {second:?}"
        );

        // And the refusal did not half-create anything.
        let accounts = state.db.with_conn(nightjar_db::list_accounts).unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].username, "founder");
    }

    /// The gate is "no account exists", not "no admin exists". A member-only
    /// server is already bootstrapped, and reopening it would let anyone mint
    /// an owner.
    #[tokio::test]
    async fn bootstrap_is_closed_by_any_account_not_only_an_admin() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::state::test_support::state(dir.path());
        let hash = nightjar_auth::hash_password("x").unwrap();
        state
            .db
            .with_conn(|conn| {
                nightjar_db::create_account_with_profile(conn, "m", &hash, "member", "P", "r0")?;
                Ok(())
            })
            .unwrap();

        let refused = bootstrap(
            State(state.clone()),
            Json(BootstrapRequest {
                username: "usurper".into(),
                password: "any".into(),
                client_label: None,
            }),
        )
        .await
        .expect_err("an existing member account closes bootstrap");
        assert!(refused.message.starts_with("bootstrap_already_complete:"));
    }

    /// The setup readout is two independent facts, and all four states are
    /// reachable. `F,T` is the dogfood install: libraries and no account.
    #[tokio::test]
    async fn setup_state_reports_all_four_combinations() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::state::test_support::state(dir.path());

        let fresh = setup_state(State(state.clone())).await.unwrap().0;
        assert!(!fresh.admin_exists && !fresh.library_exists);

        state
            .db
            .with_conn(|conn| {
                conn.execute_batch(
                    "INSERT INTO libraries (name, path, kind) VALUES ('t', '/t', 'movies');",
                )
                .map_err(|e| e.to_string())
            })
            .unwrap();
        let libraries_only = setup_state(State(state.clone())).await.unwrap().0;
        assert!(
            !libraries_only.admin_exists && libraries_only.library_exists,
            "the dogfood state: a wizard trusting one flag would offer to add a library twice"
        );

        let hash = nightjar_auth::hash_password("x").unwrap();
        state
            .db
            .with_conn(|conn| {
                nightjar_db::create_account_with_profile(conn, "o", &hash, "owner", "P", "r0")?;
                Ok(())
            })
            .unwrap();
        let complete = setup_state(State(state.clone())).await.unwrap().0;
        assert!(complete.admin_exists && complete.library_exists);
    }

    #[test]
    fn a_wrong_password_and_an_unknown_user_are_the_same_answer() {
        let db = db();
        let hash = nightjar_auth::hash_password("hunter2").unwrap();
        db.with_conn(|conn| {
            nightjar_db::create_account_with_profile(conn, "a", &hash, "owner", "P", "r0")?;
            Ok(())
        })
        .unwrap();

        let wrong = authenticate(&db, "a", "nope").unwrap_err();
        let absent = authenticate(&db, "nobody", "nope").unwrap_err();
        assert_eq!(wrong.status, absent.status);
        assert_eq!(
            wrong.message, absent.message,
            "the two must be indistinguishable in the response as well as in timing"
        );
    }
}
