//! Who is asking, and may they (ADR-0034 items 3 and 9, ADR-0040 item 1).
//!
//! One extractor, used by every route that needs one. It reads the session's
//! role rather than a flag, and it answers the questions ADR-0040 item 1 draws
//! boundaries for: the whole server, this account's own people, or refuse.

use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::{FromRequestParts, OptionalFromRequestParts};
use axum::http::{Method, request::Parts};
use nightjar_auth::token_sha256_hex;
use nightjar_core::Role;
use nightjar_db::{SessionRejection, SessionRow};

/// The routes that accept the login cookie, enumerated because ADR-0034 item 9
/// enumerates them.
///
/// **Matched against the resolved route pattern, never a path prefix.** A
/// prefix such as `/api/v0/items/` would admit the whole item surface,
/// including the metadata-fix endpoints B2-2 makes admin-only, and it would do
/// it on the day someone adds a route rather than the day someone changes the
/// rule.
///
/// Anything not in this list is refused a cookie by default, so a new route
/// cannot gain cookie acceptance by being added. What this list cannot guard
/// is the catch-all at the end growing new behaviour inside its handler, which
/// is issue #96.
pub const COOKIE_ACCEPTED_ROUTES: [&str; 8] = [
    "/api/v0/artwork/{item_key}/{kind}",
    "/api/v0/items/{item_id}/stream",
    "/api/v0/items/{item_id}/subtitles/{asset}",
    "/api/v0/sessions/{session_id}/runs/{run_id}/master.m3u8",
    "/api/v0/sessions/{session_id}/runs/{run_id}/index.m3u8",
    "/api/v0/sessions/{session_id}/runs/{run_id}/init.mp4",
    "/api/v0/sessions/{session_id}/subs/{*asset}",
    "/api/v0/sessions/{session_id}/{asset}",
];

/// An authenticated caller. Account scope until it narrows to a profile.
#[derive(Debug, Clone)]
pub struct Caller {
    pub session: SessionRow,
    pub role: Role,
}

impl Caller {
    /// ADR-0034 item 3: a profile session never administers the server,
    /// including the account holder's own profile and an adult-flagged one.
    pub fn is_account_scope(&self) -> bool {
        self.session.active_profile_id.is_none()
    }

    /// Full account powers across the server (ADR-0040 item 1), which also
    /// requires account scope.
    pub fn has_account_powers(&self) -> bool {
        self.is_account_scope() && self.role.has_account_powers()
    }

    pub fn is_owner(&self) -> bool {
        self.is_account_scope() && self.role.is_owner()
    }

    /// Whether this caller may act on `account_id`. A member reaches their own
    /// account and no other; the refusal is identical whether or not the other
    /// account exists, so the answer cannot be used to probe for one.
    pub fn may_act_on_account(&self, account_id: i64) -> bool {
        self.session.account_id == account_id || self.has_account_powers()
    }
}

fn rejection_error(rejection: SessionRejection) -> ApiError {
    // Three distinct named errors, not one bool: an operator reading a log
    // needs to tell a stale bookmark from a revoked device.
    let (code, detail) = match rejection {
        SessionRejection::Unknown => ("session_unknown", "no such session"),
        SessionRejection::Expired => ("session_expired", "session has expired"),
        SessionRejection::Revoked => ("session_revoked", "session was revoked"),
    };
    ApiError::unauthorized(format!("{code}: {detail}"))
}

/// Read the presented credential, honouring item 9's two transports.
fn presented_token(parts: &Parts) -> Option<String> {
    if let Some(value) = parts.headers.get(axum::http::header::AUTHORIZATION)
        && let Ok(text) = value.to_str()
        && let Some(token) = text.strip_prefix("Bearer ")
    {
        let token = token.trim();
        if !token.is_empty() {
            return Some(token.to_string());
        }
    }
    if !cookie_permitted(parts) {
        return None;
    }
    cookie_token(parts)
}

/// The cookie is accepted only on GET and HEAD of an enumerated route, matched
/// after routing has resolved a handler.
fn cookie_permitted(parts: &Parts) -> bool {
    if parts.method != Method::GET && parts.method != Method::HEAD {
        return false;
    }
    let Some(matched) = parts.extensions.get::<axum::extract::MatchedPath>() else {
        // No resolved pattern means no way to check the list, so refuse. The
        // default is deny in every branch, which is what makes adding a route
        // safe by omission.
        return false;
    };
    COOKIE_ACCEPTED_ROUTES.contains(&matched.as_str())
}

fn cookie_token(parts: &Parts) -> Option<String> {
    let raw = parts
        .headers
        .get(axum::http::header::COOKIE)?
        .to_str()
        .ok()?;
    for pair in raw.split(';') {
        let (name, value) = pair.split_once('=')?;
        if name.trim() == SESSION_COOKIE {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

pub const SESSION_COOKIE: &str = "nj_session";

impl FromRequestParts<AppState> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let Some(token) = presented_token(parts) else {
            return Err(ApiError::unauthorized(
                "no_credential: no session presented",
            ));
        };
        let digest = token_sha256_hex(&token);
        let db = std::sync::Arc::clone(&state.db);
        let resolved = tokio::task::spawn_blocking(move || {
            db.with_conn(|conn| {
                let now = nightjar_db::now_iso(conn)?;
                let found = nightjar_db::session_for_token(conn, &digest, &now)?;
                if let Ok(row) = &found {
                    nightjar_db::touch_last_seen(conn, row.id, &now)?;
                }
                Ok(found)
            })
        })
        .await
        .map_err(|e| ApiError::internal(format!("session lookup task: {e}")))?
        .map_err(ApiError::internal)?;

        let session = resolved.map_err(rejection_error)?;
        let role = Role::parse(&session.role).ok_or_else(|| {
            ApiError::internal(format!(
                "account {} has role {}",
                session.account_id, session.role
            ))
        })?;
        Ok(Caller { session, role })
    }
}

/// `Option<Caller>` for the one route that must work both ways: bootstrap is
/// unauthenticated, and setup state is read before anyone can log in.
impl OptionalFromRequestParts<AppState> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Option<Self>, Self::Rejection> {
        match <Caller as FromRequestParts<AppState>>::from_request_parts(parts, state).await {
            Ok(caller) => Ok(Some(caller)),
            Err(_) => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0034 item 9 enumerates the cookie-accepted routes, so the test
    /// enumerates them too and compares.
    ///
    /// **Fails in both directions, which is the point.** Adding a route to
    /// `COOKIE_ACCEPTED_ROUTES` without editing this list fails; removing one
    /// without editing this list fails. Neither can happen as a side effect of
    /// touching the router, so widening the cookie surface is always a
    /// deliberate edit in two places.
    #[test]
    fn the_cookie_accepted_set_is_exactly_these_eight() {
        let expected = [
            "/api/v0/artwork/{item_key}/{kind}",
            "/api/v0/items/{item_id}/stream",
            "/api/v0/items/{item_id}/subtitles/{asset}",
            "/api/v0/sessions/{session_id}/runs/{run_id}/master.m3u8",
            "/api/v0/sessions/{session_id}/runs/{run_id}/index.m3u8",
            "/api/v0/sessions/{session_id}/runs/{run_id}/init.mp4",
            "/api/v0/sessions/{session_id}/subs/{*asset}",
            "/api/v0/sessions/{session_id}/{asset}",
        ];
        assert_eq!(
            COOKIE_ACCEPTED_ROUTES.len(),
            8,
            "item 9 names eight; changing the count is an ADR amendment"
        );
        assert_eq!(COOKIE_ACCEPTED_ROUTES, expected);
    }

    /// Every route that writes must be absent from the list. This is the
    /// property that makes CSRF structurally absent rather than defended: a
    /// cross-site form or image cannot reach anything that changes state.
    #[test]
    fn no_write_shaped_route_accepts_a_cookie() {
        for route in COOKIE_ACCEPTED_ROUTES {
            assert!(
                route.starts_with("/api/v0/"),
                "{route} is not under the API"
            );
            for writing in [
                "/api/v0/auth/",
                "/metadata/",
                "/scan",
                "/sessions/{session_id}/seek",
                "/accounts",
                "/profiles",
            ] {
                assert!(
                    !route.contains(writing),
                    "{route} overlaps the write surface at {writing}"
                );
            }
        }
    }

    /// Prefix matching is the failure mode item 9 names, so the check is
    /// membership of the resolved pattern and never a `starts_with`. A route
    /// sharing a prefix with an accepted one must not be accepted.
    #[test]
    fn a_prefix_of_an_accepted_route_is_not_accepted() {
        for near_miss in [
            "/api/v0/items/{item_id}",
            "/api/v0/items/{item_id}/metadata/assign",
            "/api/v0/items/{item_id}/playback-info",
            "/api/v0/sessions/{session_id}/seek",
            "/api/v0/artwork/{item_key}",
        ] {
            assert!(
                !COOKIE_ACCEPTED_ROUTES.contains(&near_miss),
                "{near_miss} must not be cookie-accepted"
            );
        }
    }
}

/// End-to-end cookie acceptance, through real axum routing.
///
/// Separate from the list tests above because it exercises a different thing:
/// those assert what is *in* the allow-list, this asserts that the extractor
/// honours it once `MatchedPath` is populated. `MatchedPath` has no public
/// constructor and only exists during routing, so a real `Router` is the only
/// way to reach this path.
#[cfg(test)]
mod routing_tests {
    use super::*;
    use crate::state::{AppState, test_support};
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::{get, post};
    use nightjar_auth::mint_session_token;
    use tower::ServiceExt;

    /// Requires a `Caller`, so a 200 means the extractor accepted the
    /// credential and anything else means it refused.
    async fn probe(_caller: Caller) -> StatusCode {
        StatusCode::OK
    }

    /// The eight accepted patterns, plus two that are not on the list: one
    /// sharing a prefix with an accepted route, and one write.
    fn router(state: AppState) -> Router {
        const STREAM: &str = "/api/v0/items/{item_id}/stream";
        let mut router = Router::new();
        for route in COOKIE_ACCEPTED_ROUTES {
            // The stream route also takes POST, so the method gate can be
            // tested on a path the list *does* accept. Registering it twice
            // would overlap, so it is built once with both methods.
            router = if route == STREAM {
                router.route(route, get(probe).post(probe))
            } else {
                router.route(route, get(probe))
            };
        }
        router
            .route("/api/v0/items/{item_id}/metadata/assign", post(probe))
            .route("/api/v0/items/{item_id}/playback-info", get(probe))
            .with_state(state)
    }

    /// A live session, and the plaintext token for it.
    fn session(state: &AppState) -> String {
        let minted = mint_session_token();
        state
            .db
            .with_conn(|conn| {
                let hash = nightjar_auth::hash_password("x").map_err(|e| format!("hash: {e:?}"))?;
                nightjar_db::create_account_with_profile(conn, "a", &hash, "owner", "P", "r0")?;
                let account = nightjar_db::account_by_username(conn, "a")?.unwrap();
                let expires = nightjar_db::session_expiry(conn)?;
                nightjar_db::create_session(conn, account.id, &minted.sha256_hex, "tv", &expires)?;
                Ok(())
            })
            .unwrap();
        minted.plaintext
    }

    async fn call(router: Router, method: &str, uri: &str, header: (&str, String)) -> StatusCode {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header.0, header.1)
            .body(Body::empty())
            .unwrap();
        router.oneshot(request).await.unwrap().status()
    }

    #[tokio::test]
    async fn a_cookie_is_accepted_on_a_listed_route_and_refused_everywhere_else() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let token = session(&state);
        let cookie = || ("cookie", format!("{SESSION_COOKIE}={token}"));

        // Listed, GET: accepted.
        assert_eq!(
            call(
                router(state.clone()),
                "GET",
                "/api/v0/items/7/stream",
                cookie()
            )
            .await,
            StatusCode::OK
        );
        assert_eq!(
            call(
                router(state.clone()),
                "GET",
                "/api/v0/sessions/s1/runs/0/index.m3u8",
                cookie()
            )
            .await,
            StatusCode::OK
        );

        // Not listed, though it shares a prefix with a listed route. This is
        // the endpoint B2-2 makes admin-only, so a prefix match here would be
        // a live hole rather than a hypothetical one.
        assert_eq!(
            call(
                router(state.clone()),
                "GET",
                "/api/v0/items/7/playback-info",
                cookie()
            )
            .await,
            StatusCode::UNAUTHORIZED
        );

        // A write, on a listed path, by a method the list does not cover.
        assert_eq!(
            call(
                router(state.clone()),
                "POST",
                "/api/v0/items/7/stream",
                cookie()
            )
            .await,
            StatusCode::UNAUTHORIZED,
            "the cookie is GET and HEAD only, however listed the path is"
        );
        assert_eq!(
            call(
                router(state.clone()),
                "POST",
                "/api/v0/items/7/metadata/assign",
                cookie()
            )
            .await,
            StatusCode::UNAUTHORIZED
        );
    }

    /// The bearer token is not restricted to the eight: it is the ordinary
    /// credential, and the list exists only because an HTML element cannot set
    /// a header.
    #[tokio::test]
    async fn a_bearer_token_works_on_a_route_the_cookie_cannot_reach() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let token = session(&state);
        let bearer = || ("authorization", format!("Bearer {token}"));

        assert_eq!(
            call(
                router(state.clone()),
                "GET",
                "/api/v0/items/7/playback-info",
                bearer()
            )
            .await,
            StatusCode::OK
        );
        assert_eq!(
            call(
                router(state.clone()),
                "POST",
                "/api/v0/items/7/metadata/assign",
                bearer()
            )
            .await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn an_unknown_cookie_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let _ = session(&state);
        assert_eq!(
            call(
                router(state.clone()),
                "GET",
                "/api/v0/items/7/stream",
                ("cookie", format!("{SESSION_COOKIE}=deadbeef"))
            )
            .await,
            StatusCode::UNAUTHORIZED
        );
        // A cookie by another name is not a credential at all.
        assert_eq!(
            call(
                router(state.clone()),
                "GET",
                "/api/v0/items/7/stream",
                ("cookie", "other=whatever".to_string())
            )
            .await,
            StatusCode::UNAUTHORIZED
        );
    }
}
