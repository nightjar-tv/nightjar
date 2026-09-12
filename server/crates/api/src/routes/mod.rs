mod accounts;
mod artwork;
pub mod auth;
mod browse;
pub mod items;
mod libraries;
mod metadata_fix;
pub mod sessions;
mod system;
mod track_choice;
mod track_ids;
mod watch_state;

use crate::state::AppState;
use axum::{
    Json, Router, middleware,
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
            delete(accounts::delete_profile).patch(accounts::update_profile),
        )
        .route(
            "/api/v0/profiles/{profile_ref}/watch-state",
            get(watch_state::get_state).put(watch_state::put_state),
        )
        .route(
            "/api/v0/profiles/{profile_ref}/track-choice",
            put(track_choice::put_choice),
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
        // ADR-0054 decision 5: the playlists are the session's, the init is
        // still the run's. Static segments, so they resolve ahead of the
        // `/{asset}` capture below the same way `session_init` already does.
        .route(
            "/api/v0/sessions/{session_id}/master.m3u8",
            get(sessions::master),
        )
        .route(
            "/api/v0/sessions/{session_id}/index.m3u8",
            get(sessions::playlist),
        )
        // ADR-0051 amendment 1. The static playlist and closed asset capture
        // are adjacent because together they are one rung namespace.
        .route(
            "/api/v0/sessions/{session_id}/v/{rung}/index.m3u8",
            get(sessions::rung_playlist),
        )
        .route(
            "/api/v0/sessions/{session_id}/v/{rung}/{asset}",
            get(sessions::rung_segment),
        )
        // Stays run-scoped: `EXT-X-MAP` names this, and two runs at different
        // lands cannot share an init (decision 4, overturned 2026-08-31).
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
        // Issue #96, as far as the router can take it. A session serves exactly
        // two shapes — `init.mp4` and `seg_<start_ms>.m4s` — and the first is
        // now a static route rather than one value of a capture. The second
        // cannot be expressed: axum rejects a segment that mixes static text
        // with a parameter ("Only one parameter is allowed per path segment"),
        // so `seg_{start_ms}.m4s` is not a pattern this router can hold. What
        // closes the remaining capture is a test over the served set
        // (`hls::is_safe_asset`), which fails if that handler grows a third
        // shape — see the issue for the wire-change option and why it waits.
        .route(
            "/api/v0/sessions/{session_id}/init.mp4",
            get(sessions::session_init),
        )
        .route(
            "/api/v0/sessions/{session_id}/{asset}",
            get(sessions::segment),
        )
        // ADR-0034 item 8. The playback-session ownership boundary, applied
        // before every session operation because every session route carries
        // `{session_id}`. Added *before* `require_session` so that layer runs
        // first and resolves the caller this one reads. One boundary for
        // view, seek, stop, playlists, init, segments and subtitles.
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            crate::authority::require_session_owner,
        ))
        // ADR-0034 item 11. `route_layer`, not `layer`: it runs only once
        // routing has resolved a handler, so every route above is behind it
        // and anything matching nothing falls through to the SPA static
        // handler `main` installs afterwards.
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            crate::authority::require_session,
        ))
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
/// What a route requires of the caller behind it.
///
/// Seven values because the boundaries this server actually draws are seven,
/// and collapsing any two would make the table lie somewhere. `AnySession` is
/// not a filler value: it is the positive statement that a route is
/// deliberately open to every logged-in caller, and having to write it is what
/// stops a new route being classified by nobody.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Authority {
    /// Owner or manager, in account scope (ADR-0040 item 1).
    AccountPowers,
    /// Owner only: one of ADR-0040 item 3's three.
    Owner,
    /// Anyone, for their own account; refused for anyone else's.
    OwnAccount,
    /// Any authenticated caller; which `{profile_ref}` in the path is
    /// reachable is ADR-0035 item 7's rule, enforced inside the handler.
    ProfileRef,
    /// A profile must be selected (ADR-0034 item 3). The byte routes.
    ProfileScope,
    /// The caller's active profile must be exactly the `{profile_ref}` in the
    /// path (ADR-0038 item 7): stricter than [`Authority::ProfileRef`], which
    /// also admits account scope. An account-scope token is refused.
    ActiveProfileOnly,
    /// The caller created the session (ADR-0034 item 8). A non-owner gets the
    /// missing-session 404, so this is narrower than `AnySession`.
    SessionOwner,
    /// Any authenticated caller, said out loud rather than by omission.
    AnySession,
}

/// Every route the router registers that is not in the anonymous four, with
/// the authority it requires. B2-1 step 5 and B2-2 steps 2 and 3 are all
/// driven from this one table.
///
/// **What this catches.** A wrong requirement, because each row is driven as a
/// real request and the *refusal* is asserted rather than the gate's presence:
/// a route wired to the extractor but demanding the wrong thing fails. And a
/// route added with no decision at all, because a companion test requires
/// every registered route to appear either here or in
/// `UNAUTHENTICATED_ROUTES`, and neither list has a default.
///
/// Authentication itself is not this table's job — `require_session` wraps the
/// router and makes it structural. This table is about *what else* is needed
/// once a caller is known.
#[cfg(test)]
pub(crate) const ROUTE_AUTHORITY: &[(&str, &str, Authority)] = &[
    ("GET", "/api/v0/system/transcode", Authority::AccountPowers),
    ("GET", "/api/v0/libraries", Authority::AnySession),
    ("POST", "/api/v0/libraries", Authority::AccountPowers),
    (
        "GET",
        "/api/v0/libraries/{library_id}",
        Authority::AnySession,
    ),
    (
        "PATCH",
        "/api/v0/libraries/{library_id}",
        Authority::AccountPowers,
    ),
    (
        "POST",
        "/api/v0/libraries/{library_id}/scan",
        Authority::AccountPowers,
    ),
    ("GET", "/api/v0/scan-jobs/{job_id}", Authority::AnySession),
    (
        "GET",
        "/api/v0/libraries/{library_id}/items",
        Authority::AnySession,
    ),
    (
        "GET",
        "/api/v0/libraries/{library_id}/units",
        Authority::AnySession,
    ),
    (
        "GET",
        "/api/v0/libraries/{library_id}/scan-progress",
        Authority::AnySession,
    ),
    ("GET", "/api/v0/series", Authority::AnySession),
    ("POST", "/api/v0/auth/logout", Authority::AnySession),
    ("POST", "/api/v0/auth/logout-all", Authority::AnySession),
    ("GET", "/api/v0/auth/session", Authority::AnySession),
    ("POST", "/api/v0/auth/session", Authority::AnySession),
    ("DELETE", "/api/v0/auth/session", Authority::AnySession),
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
    (
        "PATCH",
        "/api/v0/profiles/{profile_ref}",
        Authority::OwnAccount,
    ),
    (
        "PUT",
        "/api/v0/profiles/{profile_ref}/track-choice",
        Authority::ActiveProfileOnly,
    ),
    (
        "GET",
        "/api/v0/profiles/{profile_ref}/watch-state",
        Authority::ProfileRef,
    ),
    (
        "PUT",
        "/api/v0/profiles/{profile_ref}/watch-state",
        Authority::ProfileRef,
    ),
    ("GET", "/api/v0/items/{item_id}", Authority::AnySession),
    (
        "GET",
        "/api/v0/artwork/{item_key}/{kind}",
        Authority::AnySession,
    ),
    (
        "GET",
        "/api/v0/items/{item_id}/metadata/candidates",
        Authority::AccountPowers,
    ),
    (
        "POST",
        "/api/v0/items/{item_id}/metadata/assign",
        Authority::AccountPowers,
    ),
    (
        "POST",
        "/api/v0/items/{item_id}/metadata/clear",
        Authority::AccountPowers,
    ),
    (
        "POST",
        "/api/v0/items/{item_id}/metadata/retry",
        Authority::AccountPowers,
    ),
    (
        "GET",
        "/api/v0/items/{item_id}/playback-info",
        Authority::AnySession,
    ),
    (
        "GET",
        "/api/v0/items/{item_id}/subtitles/{asset}",
        Authority::AnySession,
    ),
    (
        "POST",
        "/api/v0/items/{item_id}/sessions",
        Authority::ProfileScope,
    ),
    (
        "GET",
        "/api/v0/items/{item_id}/stream",
        Authority::ProfileScope,
    ),
    (
        "GET",
        "/api/v0/sessions/{session_id}/master.m3u8",
        Authority::SessionOwner,
    ),
    (
        "GET",
        "/api/v0/sessions/{session_id}/index.m3u8",
        Authority::SessionOwner,
    ),
    (
        "GET",
        "/api/v0/sessions/{session_id}/v/{rung}/index.m3u8",
        Authority::SessionOwner,
    ),
    (
        "GET",
        "/api/v0/sessions/{session_id}/v/{rung}/{asset}",
        Authority::SessionOwner,
    ),
    (
        "GET",
        "/api/v0/sessions/{session_id}/runs/{run_id}/init.mp4",
        Authority::SessionOwner,
    ),
    (
        "GET",
        "/api/v0/sessions/{session_id}/subs/{*asset}",
        Authority::SessionOwner,
    ),
    (
        "POST",
        "/api/v0/sessions/{session_id}/seek",
        Authority::SessionOwner,
    ),
    (
        "GET",
        "/api/v0/sessions/{session_id}",
        Authority::SessionOwner,
    ),
    (
        "DELETE",
        "/api/v0/sessions/{session_id}",
        Authority::SessionOwner,
    ),
    (
        "GET",
        "/api/v0/sessions/{session_id}/init.mp4",
        Authority::SessionOwner,
    ),
    (
        "GET",
        "/api/v0/sessions/{session_id}/{asset}",
        Authority::SessionOwner,
    ),
];

/// The routes this file registers, read out of this file.
///
/// Shared by the enumeration tests below, because all of them need the same
/// fact: what is actually wired, rather than what somebody remembered to list.
#[cfg(test)]
mod route_source {
    const METHOD_FNS: [&str; 5] = ["get(", "post(", "put(", "patch(", "delete("];

    /// Just the router function body, so prose elsewhere in this file that
    /// happens to spell `.route(` is not mistaken for a registration.
    fn router_body() -> &'static str {
        let file = include_str!("mod.rs");
        let start = file
            .find("pub fn router(")
            .expect("router function not found; the parse is stale");
        let end = start
            + file[start..]
                .find(".with_state(state)")
                .expect("router body end not found; the parse is stale");
        &file[start..end]
    }

    /// Every registered `(method, pattern)` pair.
    ///
    /// The extraction guards itself twice. The pattern count is checked
    /// against a raw count of `.route(` calls, and every pattern must yield at
    /// least one method. Either check failing means the parse has gone stale,
    /// and it says so — because the alternative is returning an empty or short
    /// list that every "for each registered route" assertion below would pass
    /// vacuously. A test that cannot fail is worse than no test, and this
    /// shape is the one that invites it.
    pub(super) fn registered_routes() -> Vec<(&'static str, &'static str)> {
        let body = router_body();
        let chunks: Vec<&str> = body.split(".route(").skip(1).collect();
        assert_eq!(
            chunks.len(),
            body.matches(".route(").count(),
            "route extraction missed a call; the parse is stale, not the router"
        );
        assert!(!chunks.is_empty(), "extraction found no routes at all");

        let mut routes = Vec::new();
        for chunk in chunks {
            let open = chunk.find('"').expect("a route call with no pattern");
            let close = chunk[open + 1..].find('"').expect("unterminated pattern") + open + 1;
            let pattern = &chunk[open + 1..close];
            let before = routes.len();
            for (name, method) in METHOD_FNS
                .iter()
                .zip(["GET", "POST", "PUT", "PATCH", "DELETE"])
            {
                if chunk[close..].contains(name) {
                    routes.push((method, pattern));
                }
            }
            assert!(
                routes.len() > before,
                "{pattern} is registered with no method the parse recognises"
            );
        }
        routes
    }

    /// A pattern with its parameters filled in, so it can actually be sent.
    pub(super) fn concrete(pattern: &str) -> String {
        pattern
            .split('/')
            .map(|segment| {
                if segment.starts_with('{') {
                    "1"
                } else {
                    segment
                }
            })
            .collect::<Vec<_>>()
            .join("/")
    }
}

/// B2-2 step 1. Authentication is the default and anonymity is the exception.
#[cfg(test)]
mod unauthenticated_set_tests {
    use super::*;
    use crate::authority::UNAUTHENTICATED_ROUTES;
    use crate::state::test_support;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn anonymous(state: AppState, method: &str, uri: &str) -> (StatusCode, String) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let response = router(state).oneshot(request).await.unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        (status, body)
    }

    /// Enumerated against the *router*, not against a list of routes somebody
    /// remembered to gate. A route added without a decision lands
    /// authenticated, and if it ever does not, this is what says so.
    #[tokio::test]
    async fn every_registered_route_refuses_an_anonymous_request_except_the_four() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());

        for (method, pattern) in route_source::registered_routes() {
            let uri = route_source::concrete(pattern);
            let (status, body) = anonymous(state.clone(), method, &uri).await;

            if UNAUTHENTICATED_ROUTES.contains(&(method, pattern)) {
                assert_ne!(
                    status,
                    StatusCode::UNAUTHORIZED,
                    "{method} {pattern} is in the unauthenticated set and must be reachable"
                );
                continue;
            }
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{method} {pattern} answered an anonymous request"
            );
            // Refused for want of a credential, distinctly from refused by
            // role, so a client can tell "log in" from "you may not".
            assert!(
                body.contains("no_credential"),
                "{method} {pattern} refused anonymously but not by that name: {body}"
            );
        }
    }

    /// Bidirectional, like the cookie list: adding an entry without editing
    /// this test fails, and removing one fails. Widening the anonymous surface
    /// is always a deliberate edit in two places.
    #[test]
    fn the_unauthenticated_set_is_exactly_these_four() {
        let expected = [
            ("GET", "/api/health"),
            ("GET", "/api/v0/system/setup"),
            ("POST", "/api/v0/auth/bootstrap"),
            ("POST", "/api/v0/auth/login"),
        ];
        assert_eq!(UNAUTHENTICATED_ROUTES, expected);
    }
}

/// B2-1 step 5, extended by B2-2 steps 2 and 3: what each route requires of a
/// caller who is already authenticated.
#[cfg(test)]
mod route_authority_tests {
    use super::*;
    use crate::state::{AppState, test_support};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use nightjar_auth::{mint_profile_ref, mint_session_token};
    use tower::ServiceExt;

    struct Actor {
        account_id: i64,
        profile_id: i64,
        profile_ref: String,
    }

    /// An account with one profile. Created once and reused, because the
    /// Argon2 hash is deliberately expensive and the accounts are not what
    /// varies between rows.
    fn actor(state: &AppState, username: &str, role: &str) -> Actor {
        let profile_ref = format!("{username:0<32}");
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
                    &profile_ref,
                )?;
                let account = nightjar_db::account_by_username(conn, username)?.unwrap();
                let profile = nightjar_db::profile_by_ref(conn, &profile_ref)?.unwrap();
                Ok((account.id, profile.id))
            })
            .unwrap();
        Actor {
            account_id,
            profile_id,
            profile_ref,
        }
    }

    /// A fresh session per row, and that is not tidiness.
    ///
    /// `POST /api/v0/auth/logout` revokes the session it is called with, and
    /// `DELETE /api/v0/auth/session` widens it back to account scope. Both are
    /// rows in this table, so a token shared across rows is a token that stops
    /// working part-way through the loop — which is how this was found, as
    /// every row after `logout` failing with 401. Sharing accounts is safe;
    /// sharing sessions is not, because these routes act on the session.
    ///
    /// `scoped` selects the profile, which is the difference between a caller
    /// who can administer and one who can watch (ADR-0034 item 3).
    fn token_for(state: &AppState, actor: &Actor, scoped: bool) -> String {
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

    /// A disposable profile on this actor's account.
    ///
    /// `DELETE /api/v0/profiles/{profile_ref}` is a row in the table and it
    /// does what it says, so pointing it at the actor's only profile destroys
    /// the thing later rows need — which surfaced as a foreign key failure
    /// three rows later rather than as a message about this one. The admitted
    /// case gets something it is allowed to lose.
    fn spare_profile_ref(state: &AppState, actor: &Actor) -> String {
        let profile_ref = mint_profile_ref();
        state
            .db
            .with_conn(|conn| {
                nightjar_db::create_profile(
                    conn,
                    actor.account_id,
                    &profile_ref,
                    "spare",
                    None,
                    false,
                    None,
                    "auto",
                )?;
                Ok(())
            })
            .unwrap();
        profile_ref
    }

    async fn call(
        state: AppState,
        method: &str,
        uri: &str,
        body: &str,
        token: &str,
    ) -> (StatusCode, String) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = router(state).oneshot(request).await.unwrap();
        let status = response.status();
        let text = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        (status, text)
    }

    /// A request the actor is entitled to make.
    ///
    /// The bodies are valid rather than merely present, and finding out why
    /// changed the product code. With the gate written as the first line of a
    /// handler, `POST /libraries` answered a member 422 instead of 403,
    /// because `Json` is extracted before the handler body runs: the caller
    /// learned their body was wrong before learning they were never allowed to
    /// ask. That is why the gate is an extractor now (`AdminCaller`), so the
    /// refusal cannot be preempted by anything the caller sends.
    ///
    /// The bodies stay valid anyway. A test that passed only because the
    /// requests were malformed would be asserting the wrong thing.
    fn own_request(method: &str, pattern: &str, actor: &Actor) -> (String, String) {
        let uri = route_source::concrete(pattern)
            .replace("/profiles/1", &format!("/profiles/{}", actor.profile_ref))
            // An account that does not exist, so the admitted case lands on a
            // 404 rather than on `owner_is_unremovable` — which is a 403, and
            // would have read as the gate refusing when it was the owner
            // being protected from deletion.
            .replace("/api/v0/accounts/1", "/api/v0/accounts/9999");
        let body = match (method, pattern) {
            ("POST", "/api/v0/accounts") => {
                r#"{"username":"new","password":"y","role":"member"}"#.to_string()
            }
            ("PUT", "/api/v0/profiles/{profile_ref}/track-choice") => {
                r#"{"audio":null,"subtitle":{"mode":"unset"}}"#.to_string()
            }
            ("PUT", _) => r#"{"role":"manager"}"#.to_string(),
            ("POST", "/api/v0/profiles") => r#"{"name":"x"}"#.to_string(),
            ("POST", "/api/v0/libraries") => {
                r#"{"name":"x","path":"/nonexistent","kind":"movies"}"#.to_string()
            }
            _ => "{}".to_string(),
        };
        let uri = if pattern == "/api/v0/profiles/{profile_ref}/track-choice" {
            format!("{uri}?seriesKey=path:1:none")
        } else {
            uri
        };
        (uri, body)
    }

    /// The same route aimed at somebody else's account, for `OwnAccount`.
    fn other_account_request(method: &str, pattern: &str, other: &Actor) -> (String, String) {
        match (method, pattern) {
            ("GET", "/api/v0/profiles") => (
                format!("/api/v0/profiles?accountId={}", other.account_id),
                "{}".to_string(),
            ),
            ("POST", "/api/v0/profiles") => (
                "/api/v0/profiles".to_string(),
                format!(r#"{{"name":"x","accountId":{}}}"#, other.account_id),
            ),
            ("DELETE", "/api/v0/profiles/{profile_ref}") => (
                format!("/api/v0/profiles/{}", other.profile_ref),
                "{}".to_string(),
            ),
            ("PATCH", "/api/v0/profiles/{profile_ref}") => (
                format!("/api/v0/profiles/{}", other.profile_ref),
                r#"{"subtitleDefault":"off"}"#.to_string(),
            ),
            _ => unreachable!("only OwnAccount routes have an other-account form"),
        }
    }

    /// A watch-state request aimed at one actor's profile, for `ProfileRef`.
    fn profile_ref_request(method: &str, pattern: &str, actor: &Actor) -> (String, String) {
        let uri = route_source::concrete(pattern)
            .replace("/profiles/1/", &format!("/profiles/{}/", actor.profile_ref))
            + "?itemKey=path:1:none";
        let body = match method {
            "PUT" => r#"{"positionMs":1000,"durationMs":10000}"#.to_string(),
            _ => "{}".to_string(),
        };
        (uri, body)
    }

    /// The refusal is asserted, not the gate's presence. A route wired to the
    /// extractor but demanding the wrong thing passes a presence check and
    /// fails this — which is how the `OwnAccount` rows were found, because the
    /// first version of this table assumed a uniform admin gate and the
    /// profile routes are deliberately not that.
    #[tokio::test]
    async fn every_route_enforces_the_authority_it_declares() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let owner = actor(&state, "owner", "owner");
        let manager = actor(&state, "manager", "manager");
        let member = actor(&state, "member", "member");

        for (method, pattern, authority) in ROUTE_AUTHORITY {
            let (uri, body) = own_request(method, pattern, &owner);
            let owner_token = token_for(&state, &owner, false);
            let manager_token = token_for(&state, &manager, false);
            let member_token = token_for(&state, &member, false);
            let watcher_token = token_for(&state, &member, true);

            match authority {
                Authority::AccountPowers => {
                    let (refused, text) =
                        call(state.clone(), method, &uri, &body, &member_token).await;
                    assert_eq!(
                        refused,
                        StatusCode::FORBIDDEN,
                        "{method} {pattern} must refuse a member"
                    );
                    assert!(
                        text.contains("insufficient_role"),
                        "{method} {pattern} refused a member but not by that name: {text}"
                    );
                    let (admitted, _) =
                        call(state.clone(), method, &uri, &body, &owner_token).await;
                    assert_ne!(
                        admitted,
                        StatusCode::FORBIDDEN,
                        "{method} {pattern} must admit an account power"
                    );
                }
                Authority::Owner => {
                    // The middle role is the one that matters: a manager holds
                    // account powers and still may not do this. A member would
                    // be refused by the weaker gate too, so a member would not
                    // tell the two apart.
                    let (refused, _) =
                        call(state.clone(), method, &uri, &body, &manager_token).await;
                    assert_eq!(
                        refused,
                        StatusCode::FORBIDDEN,
                        "{method} {pattern} must refuse a manager"
                    );
                    let (admitted, _) =
                        call(state.clone(), method, &uri, &body, &owner_token).await;
                    assert_ne!(
                        admitted,
                        StatusCode::FORBIDDEN,
                        "{method} {pattern} must admit the owner"
                    );
                }
                Authority::OwnAccount => {
                    let disposable = Actor {
                        account_id: member.account_id,
                        profile_id: member.profile_id,
                        profile_ref: spare_profile_ref(&state, &member),
                    };
                    let (own_uri, own_body) = own_request(method, pattern, &disposable);
                    let (admitted, _) =
                        call(state.clone(), method, &own_uri, &own_body, &member_token).await;
                    assert_ne!(
                        admitted,
                        StatusCode::FORBIDDEN,
                        "{method} {pattern} must admit a member on their own account"
                    );

                    let (other_uri, other_body) = other_account_request(method, pattern, &owner);
                    let (refused, _) = call(
                        state.clone(),
                        method,
                        &other_uri,
                        &other_body,
                        &member_token,
                    )
                    .await;
                    assert_eq!(
                        refused,
                        StatusCode::FORBIDDEN,
                        "{method} {pattern} must refuse a member aimed at another account"
                    );
                }
                Authority::ProfileScope => {
                    let (refused, text) =
                        call(state.clone(), method, &uri, &body, &owner_token).await;
                    assert_eq!(
                        refused,
                        StatusCode::FORBIDDEN,
                        "{method} {pattern} must refuse an account-scope session"
                    );
                    // Three outcomes, not two: no credential, no profile, and
                    // no authority are different things to be told.
                    assert!(
                        text.contains("profile_required"),
                        "{method} {pattern} refused account scope but not by that name: {text}"
                    );
                    let (admitted, _) =
                        call(state.clone(), method, &uri, &body, &watcher_token).await;
                    assert_ne!(
                        admitted,
                        StatusCode::FORBIDDEN,
                        "{method} {pattern} must admit a profile-scope session"
                    );
                }
                Authority::ProfileRef => {
                    // A profile session reaches its own active profile...
                    let (own_uri, own_body) = profile_ref_request(method, pattern, &member);
                    let (admitted, text) =
                        call(state.clone(), method, &own_uri, &own_body, &watcher_token).await;
                    assert_ne!(
                        admitted,
                        StatusCode::FORBIDDEN,
                        "{method} {pattern} must admit the active profile: {text}"
                    );

                    // ...and no other profile, whether it exists or not. The
                    // refusal is the same named forbidden shape either way.
                    let (other_uri, other_body) = profile_ref_request(method, pattern, &owner);
                    let (refused, text) = call(
                        state.clone(),
                        method,
                        &other_uri,
                        &other_body,
                        &watcher_token,
                    )
                    .await;
                    assert_eq!(
                        refused,
                        StatusCode::FORBIDDEN,
                        "{method} {pattern} must refuse another profile"
                    );
                    assert!(
                        text.contains("insufficient_role"),
                        "{method} {pattern} refused another profile but not by that name: {text}"
                    );

                    // A member in account scope reaches its own account's
                    // profile and not another account's.
                    let (member_uri, member_body) = profile_ref_request(method, pattern, &member);
                    let (member_admitted, _) = call(
                        state.clone(),
                        method,
                        &member_uri,
                        &member_body,
                        &member_token,
                    )
                    .await;
                    assert_ne!(
                        member_admitted,
                        StatusCode::FORBIDDEN,
                        "{method} {pattern} must admit a member's own account"
                    );
                    let (owner_uri, owner_body) = profile_ref_request(method, pattern, &owner);
                    let (member_refused, _) = call(
                        state.clone(),
                        method,
                        &owner_uri,
                        &owner_body,
                        &member_token,
                    )
                    .await;
                    assert_eq!(
                        member_refused,
                        StatusCode::FORBIDDEN,
                        "{method} {pattern} must refuse a member aimed at another account"
                    );

                    // An owner in account scope reaches any profile.
                    let (owner_uri, owner_body) = profile_ref_request(method, pattern, &member);
                    let (owner_admitted, _) =
                        call(state.clone(), method, &owner_uri, &owner_body, &owner_token).await;
                    assert_ne!(
                        owner_admitted,
                        StatusCode::FORBIDDEN,
                        "{method} {pattern} must admit an account power"
                    );
                }
                Authority::ActiveProfileOnly => {
                    // The active profile's own session is admitted (a 404 for an
                    // unresolved series key is fine; what is asserted is that it
                    // is not the authority refusal).
                    let (own_uri, own_body) = own_request(method, pattern, &member);
                    let (admitted, text) =
                        call(state.clone(), method, &own_uri, &own_body, &watcher_token).await;
                    assert_ne!(
                        admitted,
                        StatusCode::FORBIDDEN,
                        "{method} {pattern} must admit the active profile: {text}"
                    );

                    // An account-scope token is refused, even an owner's: a
                    // track choice with no playback is not a thing a user can
                    // mean (ADR-0038 item 7).
                    for scope in [&owner_token, &member_token] {
                        let (refused, text) =
                            call(state.clone(), method, &own_uri, &own_body, scope).await;
                        assert_eq!(
                            refused,
                            StatusCode::FORBIDDEN,
                            "{method} {pattern} must refuse account scope"
                        );
                        assert!(
                            text.contains("insufficient_role"),
                            "{method} {pattern} refused but not by name: {text}"
                        );
                    }

                    // ...and another profile's session is refused.
                    let (other_uri, other_body) = own_request(method, pattern, &owner);
                    let (refused, text) = call(
                        state.clone(),
                        method,
                        &other_uri,
                        &other_body,
                        &watcher_token,
                    )
                    .await;
                    assert_eq!(
                        refused,
                        StatusCode::FORBIDDEN,
                        "{method} {pattern} must refuse another profile"
                    );
                    assert!(text.contains("insufficient_role"), "{text}");
                }
                Authority::SessionOwner => {
                    // The concrete session id does not exist, so this asserts
                    // only the non-owner refusal: the ownership boundary
                    // answers the same missing-session 404 as the handler. The
                    // positive and live halves — the owner is admitted, and a
                    // sibling profile under the same account is not — need a
                    // real session and live in
                    // `routes::sessions::ownership_tests`.
                    let (refused, text) =
                        call(state.clone(), method, &uri, &body, &watcher_token).await;
                    assert_eq!(
                        refused,
                        StatusCode::NOT_FOUND,
                        "{method} {pattern} must refuse a non-owner"
                    );
                    assert!(
                        text.contains("not found"),
                        "{method} {pattern} refused a non-owner but not by that name: {text}"
                    );
                }
                Authority::AnySession => {
                    let (status, _) =
                        call(state.clone(), method, &uri, &body, &watcher_token).await;
                    assert_ne!(
                        status,
                        StatusCode::UNAUTHORIZED,
                        "{method} {pattern} is open to any session but refused one"
                    );
                    assert_ne!(
                        status,
                        StatusCode::FORBIDDEN,
                        "{method} {pattern} is open to any session but refused one"
                    );
                }
            }
        }
    }

    /// Every registered route is classified, one way or the other. Neither
    /// list has a default, so a route added without a decision fails here
    /// rather than shipping with whatever the surrounding code happened to do.
    #[test]
    fn every_registered_route_is_classified() {
        for (method, pattern) in route_source::registered_routes() {
            let gated = ROUTE_AUTHORITY
                .iter()
                .any(|(m, p, _)| *m == method && *p == pattern);
            let anonymous = crate::authority::UNAUTHENTICATED_ROUTES.contains(&(method, pattern));
            assert!(
                gated ^ anonymous,
                "{method} {pattern} is registered but appears in neither ROUTE_AUTHORITY nor \
                 UNAUTHENTICATED_ROUTES (or in both)"
            );
        }
    }

    /// And nothing in the table names a route that does not exist, which would
    /// be dead text reading as a decision.
    #[test]
    fn every_table_row_names_a_registered_route() {
        let registered = route_source::registered_routes();
        for (method, pattern, _) in ROUTE_AUTHORITY {
            assert!(
                registered.contains(&(method, pattern)),
                "{method} {pattern} is in ROUTE_AUTHORITY but is not registered"
            );
        }
    }
}

/// B2-2 step 4. The cookie reaches exactly eleven routes on the real router.
#[cfg(test)]
mod cookie_surface_tests {
    use super::*;
    use crate::authority::{COOKIE_ACCEPTED_ROUTES, SESSION_COOKIE};
    use crate::state::test_support;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use nightjar_auth::mint_session_token;
    use tower::ServiceExt;

    /// The mini-router test in `authority` proves the matching logic. This
    /// proves it against the router the server actually serves, which is where
    /// a route added to the wrong list would show up — including the metadata
    /// fix routes, the specific case an `/api/v0/items/` prefix would have
    /// admitted silently.
    #[tokio::test]
    async fn only_the_eleven_accept_a_cookie() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());

        let minted = mint_session_token();
        state
            .db
            .with_conn(|conn| {
                let hash = nightjar_auth::hash_password("x").map_err(|e| format!("hash: {e:?}"))?;
                nightjar_db::create_account_with_profile(conn, "v", &hash, "owner", "P", "r0")?;
                let account = nightjar_db::account_by_username(conn, "v")?.unwrap();
                let profile = nightjar_db::profile_by_ref(conn, "r0")?.unwrap();
                let expires = nightjar_db::session_expiry(conn)?;
                let session = nightjar_db::create_session(
                    conn,
                    account.id,
                    &minted.sha256_hex,
                    "t",
                    &expires,
                )?;
                // Profile scope, so an accepted byte route is not refused for
                // the other reason and the assertion stays about the cookie.
                nightjar_db::set_active_profile(conn, session, Some(profile.id))?;
                Ok(())
            })
            .unwrap();

        for (method, pattern) in route_source::registered_routes() {
            let request = Request::builder()
                .method(method)
                .uri(route_source::concrete(pattern))
                .header("content-type", "application/json")
                .header("cookie", format!("{SESSION_COOKIE}={}", minted.plaintext))
                .body(Body::from("{}"))
                .unwrap();
            let status = router(state.clone())
                .oneshot(request)
                .await
                .unwrap()
                .status();

            let accepted = method == "GET" && COOKIE_ACCEPTED_ROUTES.contains(&pattern);
            if accepted {
                assert_ne!(
                    status,
                    StatusCode::UNAUTHORIZED,
                    "{method} {pattern} is on the cookie list and refused a cookie"
                );
            } else if !crate::authority::UNAUTHENTICATED_ROUTES.contains(&(method, pattern)) {
                assert_eq!(
                    status,
                    StatusCode::UNAUTHORIZED,
                    "{method} {pattern} accepted a cookie and is not on the list"
                );
            }
        }
    }
}

/// B2-2 step 5. What the spec promises is what the router enforces.
#[cfg(test)]
mod openapi_security_tests {
    use super::*;
    use crate::authority::{COOKIE_ACCEPTED_ROUTES, UNAUTHENTICATED_ROUTES};

    /// Path parameters and the literal suffixes attached to them, flattened,
    /// so `{trackId}.vtt` and `{asset}` compare equal to the router's
    /// `{asset}`. The two spell the subtitle route differently and both are
    /// right in their own idiom; what matters is that they are the same route.
    fn normalize(path: &str) -> String {
        path.split('/')
            .map(|s| if s.contains('{') { "{}" } else { s })
            .collect::<Vec<_>>()
            .join("/")
    }

    struct Documented {
        method: String,
        path: String,
        security: Option<String>,
    }

    /// The operations in `api/openapi.yaml`, with whatever `security:` each
    /// one declares. Indent-driven rather than a YAML parse, because the api
    /// crate has no YAML dependency and adding one to read a file in a test
    /// is not a dependency this earns (Rule 4.4).
    fn documented() -> Vec<Documented> {
        const SPEC: &str = include_str!("../../../../../api/openapi.yaml");
        let body = SPEC
            .split_once("\npaths:\n")
            .expect("no paths section; the parse is stale")
            .1;

        let mut out: Vec<Documented> = Vec::new();
        let mut path: Option<String> = None;
        for line in body.lines() {
            if !line.starts_with(' ') && !line.trim().is_empty() {
                break; // out of `paths:` and into the next top-level key
            }
            if let Some(rest) = line.strip_prefix("  /")
                && !line.starts_with("   ")
                && let Some(name) = rest.strip_suffix(':')
            {
                path = Some(format!("/{name}"));
                continue;
            }
            if let Some(rest) = line.strip_prefix("    ")
                && !line.starts_with("     ")
                && let Some(method) = rest.strip_suffix(':')
                && matches!(method, "get" | "post" | "put" | "patch" | "delete")
            {
                out.push(Documented {
                    method: method.to_uppercase(),
                    path: path.clone().expect("an operation before any path"),
                    security: None,
                });
                continue;
            }
            if let Some(rest) = line.strip_prefix("      security:")
                && let Some(last) = out.last_mut()
            {
                last.security = Some(rest.trim().to_string());
            }
        }
        assert!(!out.is_empty(), "no operations parsed; the parse is stale");
        out
    }

    /// Documented and registered are the same set. A route in one and not the
    /// other is drift, and it is drift in whichever direction it happens:
    /// an undocumented route ships unexplained, a documented one that does not
    /// exist is a promise to a client that will 404.
    #[test]
    fn the_spec_documents_exactly_the_routes_the_router_registers() {
        let mut documented: Vec<(String, String)> = documented()
            .into_iter()
            .map(|d| (d.method, normalize(&d.path)))
            .collect();
        let mut registered: Vec<(String, String)> = route_source::registered_routes()
            .into_iter()
            .map(|(m, p)| (m.to_string(), normalize(p)))
            .collect();
        documented.sort();
        registered.sort();
        assert_eq!(documented, registered);
    }

    /// `security: []` in the spec means anonymous, and the four that carry it
    /// are the four the router lets through. Reviewed once, asserted after.
    #[test]
    fn the_spec_marks_anonymous_exactly_where_the_router_allows_it() {
        let anonymous: Vec<(String, String)> = UNAUTHENTICATED_ROUTES
            .iter()
            .map(|(m, p)| (m.to_string(), normalize(p)))
            .collect();
        for op in documented() {
            let declares_anonymous = op.security.as_deref() == Some("[]");
            let is_anonymous = anonymous.contains(&(op.method.clone(), normalize(&op.path)));
            assert_eq!(
                declares_anonymous, is_anonymous,
                "{} {} declares security: {:?} but the router says anonymous = {is_anonymous}",
                op.method, op.path, op.security
            );
        }
    }

    /// And `cookieAuth` appears on exactly the eleven the extractor accepts, so
    /// a client reading the spec cannot conclude the cookie works on a route
    /// where it will be refused.
    #[test]
    fn the_spec_offers_the_cookie_exactly_where_it_is_accepted() {
        let cookie: Vec<String> = COOKIE_ACCEPTED_ROUTES
            .iter()
            .map(|p| normalize(p))
            .collect();
        for op in documented() {
            let offers_cookie = op
                .security
                .as_deref()
                .is_some_and(|s| s.contains("cookieAuth"));
            let accepted = op.method == "GET" && cookie.contains(&normalize(&op.path));
            assert_eq!(
                offers_cookie, accepted,
                "{} {} offers cookieAuth = {offers_cookie} but the extractor accepts it = {accepted}",
                op.method, op.path
            );
        }
    }
}

/// B2-2 step 6. The install that already exists survives the change.
#[cfg(test)]
mod dogfood_sequence_tests {
    use super::*;
    use crate::state::test_support;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use nightjar_db::{NewLibrary, UpsertItem};
    use tower::ServiceExt;

    async fn send(
        state: &AppState,
        method: &str,
        uri: &str,
        body: &str,
        token: Option<&str>,
    ) -> (StatusCode, String) {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let response = router(state.clone())
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let text = axum::body::to_bytes(response.into_body(), 256 * 1024)
            .await
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        (status, text)
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

    /// The N150 shape, which is the one that matters: a database with a
    /// library and items and **no account at all**. A fresh database would
    /// pass a weaker version of this test, because the first thing a fresh
    /// database does is bootstrap, and the question here is whether an install
    /// that predates accounts can still be reached.
    #[tokio::test]
    async fn a_database_with_items_and_no_account_reaches_a_browsing_session() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let library = state
            .db
            .create_library(&NewLibrary {
                name: "shows".to_string(),
                path: "/media/shows".to_string(),
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
            .unwrap();

        // 1. Anonymous, and it must be: this is what a client reads before it
        //    knows whether to show login or first-run.
        let (status, body) = send(&state, "GET", "/api/v0/system/setup", "", None).await;
        assert_eq!(status, StatusCode::OK);
        // Two independent facts, not one `setupComplete` — this database is
        // the state that only exists because the install predates accounts.
        assert!(
            body.contains("\"adminExists\":false"),
            "a database with no account has no admin: {body}"
        );
        assert!(
            body.contains("\"libraryExists\":true"),
            "the libraries are still there: {body}"
        );

        // 2. Bootstrap the first owner. Anonymous by construction — the
        //    handler takes no caller, so it cannot require one.
        let (status, body) = send(
            &state,
            "POST",
            "/api/v0/auth/bootstrap",
            r#"{"username":"g","password":"correct horse","clientLabel":"tv"}"#,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "bootstrap: {body}");
        let token = field(&body, "token").to_string();

        // 3. Account scope: the owner can administer, and can list.
        let (status, body) = send(&state, "GET", "/api/v0/libraries", "", Some(&token)).await;
        assert_eq!(status, StatusCode::OK, "account scope may browse: {body}");

        // 4. But it may not play. This is the step that would have been missed
        //    without it: an owner who never selects a profile has a session
        //    that administers and does not watch (ADR-0034 item 3).
        let (status, body) = send(
            &state,
            "POST",
            &format!("/api/v0/items/{}/sessions", 1),
            "{}",
            Some(&token),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "account scope must not play");
        assert!(body.contains("profile_required"), "{body}");

        // 5. Bootstrap made a profile with the account; narrow to it.
        let (status, body) = send(&state, "GET", "/api/v0/profiles", "", Some(&token)).await;
        assert_eq!(status, StatusCode::OK, "profiles: {body}");
        let profile_ref = field(&body, "profileRef").to_string();
        let (status, body) = send(
            &state,
            "POST",
            "/api/v0/auth/session",
            &format!(r#"{{"profileRef":"{profile_ref}"}}"#),
            Some(&token),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "select profile: {body}");

        // 6. The items that were there before any of this existed are
        //    reachable from the profile session.
        let (status, body) = send(
            &state,
            "GET",
            &format!("/api/v0/libraries/{}/items", library.id),
            "",
            Some(&token),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "browse: {body}");
        assert!(
            body.contains("s01e01.mkv"),
            "the pre-existing item is reachable: {body}"
        );
    }
}
