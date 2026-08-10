mod accounts;
mod artwork;
pub mod auth;
mod browse;
pub mod items;
mod libraries;
mod metadata_fix;
pub mod sessions;
mod system;
mod track_ids;

use crate::state::AppState;
use axum::{
    Json, Router,
    routing::{delete, get, post, put},
};
use serde::Serialize;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route(
            "/api/v0/system/transcode",
            get(system::transcode_capabilities),
        )
        .route(
            "/api/v0/libraries",
            get(libraries::list).post(libraries::create),
        )
        .route(
            "/api/v0/libraries/{library_id}",
            get(libraries::get).patch(libraries::patch),
        )
        .route("/api/v0/libraries/{library_id}/scan", post(libraries::scan))
        .route("/api/v0/scan-jobs/{job_id}", get(libraries::get_scan_job))
        .route(
            "/api/v0/libraries/{library_id}/items",
            get(libraries::list_items),
        )
        .route(
            "/api/v0/libraries/{library_id}/units",
            get(browse::list_units),
        )
        .route(
            "/api/v0/libraries/{library_id}/scan-progress",
            get(libraries::scan_progress),
        )
        .route("/api/v0/series", get(browse::get))
        .route("/api/v0/system/setup", get(auth::setup_state))
        .route("/api/v0/auth/bootstrap", post(auth::bootstrap))
        .route("/api/v0/auth/login", post(auth::login))
        .route("/api/v0/auth/logout", post(auth::logout))
        .route("/api/v0/auth/logout-all", post(auth::logout_all))
        .route(
            "/api/v0/auth/session",
            get(auth::get_session)
                .post(auth::select_profile)
                .delete(auth::widen_to_account),
        )
        .route(
            "/api/v0/accounts",
            get(accounts::list).post(accounts::create),
        )
        .route("/api/v0/accounts/{account_id}", delete(accounts::delete))
        .route(
            "/api/v0/accounts/{account_id}/role",
            put(accounts::set_role),
        )
        .route(
            "/api/v0/profiles",
            get(accounts::list_profiles).post(accounts::create_profile),
        )
        .route(
            "/api/v0/profiles/{profile_ref}",
            delete(accounts::delete_profile),
        )
        .route("/api/v0/items/{item_id}", get(items::get))
        .route(
            "/api/v0/artwork/{item_key}/{kind}",
            get(artwork::get_artwork),
        )
        .route(
            "/api/v0/items/{item_id}/metadata/candidates",
            get(metadata_fix::candidates),
        )
        .route(
            "/api/v0/items/{item_id}/metadata/assign",
            post(metadata_fix::assign_match),
        )
        .route(
            "/api/v0/items/{item_id}/metadata/clear",
            post(metadata_fix::clear),
        )
        .route(
            "/api/v0/items/{item_id}/metadata/retry",
            post(metadata_fix::retry),
        )
        .route(
            "/api/v0/items/{item_id}/playback-info",
            get(items::playback_info),
        )
        .route(
            "/api/v0/items/{item_id}/subtitles/{asset}",
            get(items::subtitle_vtt),
        )
        .route("/api/v0/items/{item_id}/sessions", post(sessions::start))
        .route(
            "/api/v0/items/{item_id}/stream",
            get(crate::stream::stream_item),
        )
        .route(
            "/api/v0/sessions/{session_id}/runs/{run_id}/master.m3u8",
            get(sessions::master),
        )
        .route(
            "/api/v0/sessions/{session_id}/runs/{run_id}/index.m3u8",
            get(sessions::playlist),
        )
        .route(
            "/api/v0/sessions/{session_id}/runs/{run_id}/init.mp4",
            get(sessions::run_init),
        )
        .route(
            "/api/v0/sessions/{session_id}/subs/{*asset}",
            get(sessions::subtitle_playlist),
        )
        .route("/api/v0/sessions/{session_id}/seek", post(sessions::seek))
        .route(
            "/api/v0/sessions/{session_id}",
            get(sessions::get).delete(sessions::delete),
        )
        .route(
            "/api/v0/sessions/{session_id}/{asset}",
            get(sessions::asset),
        )
        .with_state(state)
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
    version: &'static str,
    core: &'static str,
}

async fn health() -> Json<Health> {
    Json(Health {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        core: nightjar_core::version(),
    })
}

/// What a route on the account and profile surface requires.
///
/// Three values because ADR-0040 item 1 draws three boundaries, and a uniform
/// "admin only" would be wrong for the profile routes: a member reaches the
/// profiles under their own account, since a parent who cannot manage their
/// own child's profile has no reason to hold an account.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Authority {
    /// Owner or manager, in account scope.
    AccountPowers,
    /// Owner only: one of ADR-0040 item 3's three.
    Owner,
    /// Anyone, for their own account; refused for anyone else's.
    OwnAccount,
}

/// Every route on the account and profile surface, with the authority it
/// requires. B2-1 step 5's acceptance is driven from this table.
///
/// **What this catches and what it does not.**
///
/// It catches a wrong role requirement, because each entry is driven as a real
/// request and the *refusal* is asserted rather than the gate's presence: a
/// route wired to the extractor but demanding the wrong role fails. It catches
/// a route added to this surface without an entry, because a companion test
/// checks the table against what is actually registered.
///
/// It does **not** make authentication true by construction across the whole
/// API. That is B2-2 step 1's restructure, where the route layer takes a
/// session and the *unauthenticated* set becomes the enumerated one. Until
/// then a new route outside this surface can be added ungated and nothing here
/// notices.
#[cfg(test)]
pub(crate) const GATED_ROUTES: &[(&str, &str, Authority)] = &[
    ("GET", "/api/v0/accounts", Authority::AccountPowers),
    ("POST", "/api/v0/accounts", Authority::AccountPowers),
    (
        "DELETE",
        "/api/v0/accounts/{account_id}",
        Authority::AccountPowers,
    ),
    (
        "PUT",
        "/api/v0/accounts/{account_id}/role",
        Authority::Owner,
    ),
    ("GET", "/api/v0/profiles", Authority::OwnAccount),
    ("POST", "/api/v0/profiles", Authority::OwnAccount),
    (
        "DELETE",
        "/api/v0/profiles/{profile_ref}",
        Authority::OwnAccount,
    ),
];

#[cfg(test)]
mod gated_route_tests {
    use super::*;
    use crate::state::{AppState, test_support};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use nightjar_auth::mint_session_token;
    use tower::ServiceExt;

    struct Actor {
        token: String,
        account_id: i64,
        profile_ref: String,
    }

    fn actor(state: &AppState, username: &str, role: &str) -> Actor {
        let minted = mint_session_token();
        let profile_ref = format!("{username:0<32}");
        let account_id = state
            .db
            .with_conn(|conn| {
                let hash = nightjar_auth::hash_password("x").map_err(|e| format!("hash: {e:?}"))?;
                nightjar_db::create_account_with_profile(
                    conn,
                    username,
                    &hash,
                    role,
                    "P",
                    &profile_ref,
                )?;
                let account = nightjar_db::account_by_username(conn, username)?.unwrap();
                let expires = nightjar_db::session_expiry(conn)?;
                nightjar_db::create_session(
                    conn,
                    account.id,
                    &minted.sha256_hex,
                    "test",
                    &expires,
                )?;
                Ok(account.id)
            })
            .unwrap();
        Actor {
            token: minted.plaintext,
            account_id,
            profile_ref,
        }
    }

    async fn status(
        state: AppState,
        method: &str,
        uri: &str,
        body: &str,
        token: Option<&str>,
    ) -> StatusCode {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let request = builder.body(Body::from(body.to_string())).unwrap();
        router(state).oneshot(request).await.unwrap().status()
    }

    /// A request the given actor is entitled to make.
    fn own_request(method: &str, pattern: &str, actor: &Actor) -> (String, String) {
        let uri = pattern
            .replace("{account_id}", "9999")
            .replace("{profile_ref}", &actor.profile_ref);
        let body = match (method, pattern) {
            ("POST", "/api/v0/accounts") => {
                r#"{"username":"new","password":"y","role":"member"}"#.to_string()
            }
            ("PUT", _) => r#"{"role":"manager"}"#.to_string(),
            ("POST", "/api/v0/profiles") => r#"{"name":"x"}"#.to_string(),
            _ => String::new(),
        };
        (uri, body)
    }

    /// The same route aimed at somebody else's account, for `OwnAccount`.
    fn other_account_request(method: &str, pattern: &str, other: &Actor) -> (String, String) {
        match (method, pattern) {
            ("GET", "/api/v0/profiles") => (
                format!("/api/v0/profiles?accountId={}", other.account_id),
                String::new(),
            ),
            ("POST", "/api/v0/profiles") => (
                "/api/v0/profiles".to_string(),
                format!(r#"{{"name":"x","accountId":{}}}"#, other.account_id),
            ),
            ("DELETE", "/api/v0/profiles/{profile_ref}") => (
                format!("/api/v0/profiles/{}", other.profile_ref),
                String::new(),
            ),
            _ => unreachable!("only OwnAccount routes have an other-account form"),
        }
    }

    /// B2-1 step 5. Every gated route refuses a session below the role it
    /// requires, and admits one that holds it.
    ///
    /// The refusal is asserted, not the gate's presence. A route wired to the
    /// extractor but demanding the wrong role passes a presence check and
    /// fails this: that is how the `OwnAccount` rows were found, because the
    /// first version of this table assumed the whole surface was admin-only
    /// and the profile routes are deliberately not.
    #[tokio::test]
    async fn every_gated_route_enforces_the_authority_it_declares() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let owner = actor(&state, "owner", "owner");
        let manager = actor(&state, "manager", "manager");
        let member = actor(&state, "member", "member");

        for (method, pattern, authority) in GATED_ROUTES {
            let (uri, body) = own_request(method, pattern, &owner);

            match authority {
                Authority::AccountPowers => {
                    assert_eq!(
                        status(state.clone(), method, &uri, &body, Some(&member.token)).await,
                        StatusCode::FORBIDDEN,
                        "{method} {pattern} must refuse a member"
                    );
                    assert_ne!(
                        status(state.clone(), method, &uri, &body, Some(&owner.token)).await,
                        StatusCode::FORBIDDEN,
                        "{method} {pattern} must admit an account power"
                    );
                }
                Authority::Owner => {
                    // The middle role is the one that matters here: a manager
                    // holds account powers and still may not do this.
                    assert_eq!(
                        status(state.clone(), method, &uri, &body, Some(&manager.token)).await,
                        StatusCode::FORBIDDEN,
                        "{method} {pattern} must refuse a manager"
                    );
                    assert_ne!(
                        status(state.clone(), method, &uri, &body, Some(&owner.token)).await,
                        StatusCode::FORBIDDEN,
                        "{method} {pattern} must admit the owner"
                    );
                }
                Authority::OwnAccount => {
                    let (own_uri, own_body) = own_request(method, pattern, &member);
                    assert_ne!(
                        status(
                            state.clone(),
                            method,
                            &own_uri,
                            &own_body,
                            Some(&member.token)
                        )
                        .await,
                        StatusCode::FORBIDDEN,
                        "{method} {pattern} must admit a member on their own account"
                    );

                    let (other_uri, other_body) = other_account_request(method, pattern, &owner);
                    assert_eq!(
                        status(
                            state.clone(),
                            method,
                            &other_uri,
                            &other_body,
                            Some(&member.token)
                        )
                        .await,
                        StatusCode::FORBIDDEN,
                        "{method} {pattern} must refuse a member aimed at another account"
                    );
                }
            }
        }
    }

    /// No credential is a distinct outcome from refused by role, so the two
    /// cannot be conflated in a log or by a client.
    #[tokio::test]
    async fn every_gated_route_refuses_an_absent_credential_as_unauthenticated() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let owner = actor(&state, "owner", "owner");
        for (method, pattern, _) in GATED_ROUTES {
            let (uri, body) = own_request(method, pattern, &owner);
            assert_eq!(
                status(state.clone(), method, &uri, &body, None).await,
                StatusCode::UNAUTHORIZED,
                "{method} {pattern} with no credential"
            );
        }
    }

    /// The table must cover every route registered on this surface, so adding
    /// one without an entry fails rather than going unguarded.
    ///
    /// Extraction is checked against a raw count of `.route(` calls, so a
    /// reformat that breaks the parse fails loudly rather than silently
    /// matching nothing and passing — which would be a test that could not
    /// have returned the answer you wanted.
    #[test]
    fn the_table_covers_every_registered_account_and_profile_route() {
        let source = include_str!("mod.rs");
        let registered: Vec<&str> = source
            .match_indices(".route(")
            .filter_map(|(i, _)| {
                let rest = &source[i..];
                let start = rest.find('"')? + 1;
                let end = rest[start..].find('"')? + start;
                Some(&rest[start..end])
            })
            .collect();
        assert_eq!(
            registered.len(),
            source.matches(".route(").count(),
            "route extraction missed a call; the parse is stale, not the table"
        );

        let surface: Vec<&str> = registered
            .into_iter()
            .filter(|p| p.starts_with("/api/v0/accounts") || p.starts_with("/api/v0/profiles"))
            .collect();
        assert!(!surface.is_empty(), "extraction found no surface routes");

        for path in surface {
            assert!(
                GATED_ROUTES.iter().any(|(_, gated, _)| *gated == path),
                "{path} is registered but absent from GATED_ROUTES, so nothing asserts it is gated"
            );
        }
    }
}
