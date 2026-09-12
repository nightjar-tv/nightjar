//! Who is asking, and may they (ADR-0034 items 3 and 9, ADR-0040 item 1).
//!
//! One extractor, used by every route that needs one. It reads the session's
//! role rather than a flag, and it answers the questions ADR-0040 item 1 draws
//! boundaries for: the whole server, this account's own people, or refuse.

use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::{FromRequestParts, OptionalFromRequestParts, RawPathParams, Request, State};
use axum::http::{Method, request::Parts};
use axum::middleware::Next;
use axum::response::Response;
use nightjar_auth::token_sha256_hex;
use nightjar_core::Role;
use nightjar_db::{SessionRejection, SessionRow};
use nightjar_transcode::SessionOwner;

/// The whole unauthenticated surface of the API (ADR-0034 item 11).
///
/// **This is an exception list, not a gate list.** `require_session` wraps
/// every route in one place, so a route added tomorrow is authenticated
/// because nobody did anything, and making it anonymous means editing this
/// array. That is the direction the default has to point: the failure mode
/// worth designing against is the route nobody thought about, not the route
/// somebody thought about wrongly.
///
/// Four entries, and each one has to be reachable before a credential can
/// exist. Health is what a container orchestrator polls; setup state is read
/// by a client deciding whether to show bootstrap or login; bootstrap creates
/// the first account; login is how every other credential is obtained.
pub const UNAUTHENTICATED_ROUTES: [(&str, &str); 4] = [
    ("GET", "/api/health"),
    ("GET", "/api/v0/system/setup"),
    ("POST", "/api/v0/auth/bootstrap"),
    ("POST", "/api/v0/auth/login"),
];

fn is_unauthenticated(method: &Method, matched: &str) -> bool {
    UNAUTHENTICATED_ROUTES
        .iter()
        .any(|(m, path)| method == m && *path == matched)
}

/// Require a session on every route but the four, and resolve it once.
///
/// Applied with `route_layer`, so it runs only after routing has resolved a
/// handler. Two things follow from that and both are wanted: `MatchedPath` is
/// always populated, so the exception check compares route patterns rather
/// than raw paths; and a request that matches nothing passes through to the
/// SPA fallback untouched, which keeps the static assets public without
/// naming them here.
///
/// A presented credential is resolved even on the four, so `Option<Caller>`
/// means "is somebody logged in" rather than "was this route gated". The
/// resolution happens once and travels in the request extensions, so a handler
/// taking `Caller` costs no second lookup.
pub async fn require_session(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let (mut parts, body) = request.into_parts();
    let matched = parts
        .extensions
        .get::<axum::extract::MatchedPath>()
        .map(|m| m.as_str().to_string());
    let exempt = matched
        .as_deref()
        .is_some_and(|matched| is_unauthenticated(&parts.method, matched));

    match presented_token(&parts) {
        Some(token) => match resolve_caller(&state, &token).await {
            Ok(caller) => {
                parts.extensions.insert(caller);
            }
            // A bad credential on an anonymous route is not an error: the
            // client is simply not logged in, which is the state those four
            // exist to serve.
            Err(error) => {
                if !exempt {
                    return Err(error);
                }
            }
        },
        None => {
            if !exempt {
                return Err(ApiError::unauthorized(
                    "no_credential: no session presented",
                ));
            }
        }
    }

    request = Request::from_parts(parts, body);
    Ok(next.run(request).await)
}

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
/// cannot gain cookie acceptance by being added.
///
/// **Issue #96, narrowed on 2026-08-11 and not closed.** A session's top-level
/// asset capture serves `init.mp4` and `seg_<start_ms>.m4s`, closed by
/// `hls::is_safe_asset`. ADR-0051's rung capture is narrower: it serves only
/// the time-keyed segment shape, closed by `parse_time_keyed_segment_name`.
/// Axum cannot express a segment that mixes static text with a parameter, so
/// both captures remain enumerated here and guarded in their handlers.
pub const COOKIE_ACCEPTED_ROUTES: [&str; 11] = [
    "/api/v0/artwork/{item_key}/{kind}",
    "/api/v0/items/{item_id}/stream",
    "/api/v0/items/{item_id}/subtitles/{asset}",
    "/api/v0/sessions/{session_id}/master.m3u8",
    "/api/v0/sessions/{session_id}/index.m3u8",
    "/api/v0/sessions/{session_id}/v/{rung}/index.m3u8",
    "/api/v0/sessions/{session_id}/v/{rung}/{asset}",
    "/api/v0/sessions/{session_id}/runs/{run_id}/init.mp4",
    "/api/v0/sessions/{session_id}/subs/{*asset}",
    "/api/v0/sessions/{session_id}/init.mp4",
    "/api/v0/sessions/{session_id}/{asset}",
];

/// Refused for want of authority, as distinct from refused for want of a
/// credential. A client that sees this must not retry by logging in again.
/// Refused because the session has no profile, which is neither a missing
/// credential nor a missing role. The client's move is to select a profile,
/// and naming it separately is what tells them so.
pub const ACCOUNT_SCOPE_CANNOT_PLAY: &str = "profile_required: select a profile before playing";

pub const INSUFFICIENT_ROLE: &str = "insufficient_role: this account may not perform that action";

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

    /// The account-powers gate, named once and used by every route that
    /// needs it (Rule 4.11). One string, so an operator greps one code and a
    /// client matches one prefix; and one place to change if the boundary
    /// ever moves.
    pub fn require_account_powers(&self) -> Result<(), ApiError> {
        if self.has_account_powers() {
            Ok(())
        } else {
            Err(ApiError::forbidden(INSUFFICIENT_ROLE))
        }
    }

    /// ADR-0034 item 3, the other direction: the byte routes need to know
    /// *who is watching*, and an account-scope session does not say. Refusing
    /// is not a formality — B2-6's kids filter has nothing to filter against
    /// without a profile, so a session that plays without one is a session
    /// that plays around the filter.
    pub fn require_profile_scope(&self) -> Result<(), ApiError> {
        if self.is_account_scope() {
            Err(ApiError::forbidden(ACCOUNT_SCOPE_CANNOT_PLAY))
        } else {
            Ok(())
        }
    }

    /// Whether this caller may act on `account_id`. A member reaches their own
    /// account and no other; the refusal is identical whether or not the other
    /// account exists, so the answer cannot be used to probe for one.
    pub fn may_act_on_account(&self, account_id: i64) -> bool {
        self.session.account_id == account_id || self.has_account_powers()
    }

    /// The playback-session ownership identity (ADR-0034 item 8): the account
    /// plus the selected profile, and nothing else.
    ///
    /// Account scope has no profile, so its key can never equal a session's:
    /// a session is always created from a profile-scope caller. The key is
    /// built from the resolved session row, never from a query field, a role,
    /// or the credential text.
    pub fn session_owner(&self) -> SessionOwner {
        SessionOwner::new(format!(
            "{}:{}",
            self.session.account_id,
            self.session.active_profile_id.unwrap_or_default()
        ))
    }
}

/// The one ownership boundary for playback sessions (ADR-0034 item 8).
///
/// Every session-scoped route carries a `{session_id}` path parameter, so this
/// layer runs on all routes and enforces only where that parameter exists.
/// That is deliberate: a route added under the session namespace is covered
/// because nobody did anything, which is the direction the default has to
/// point (the same reason [`require_session`] wraps the whole router).
///
/// A caller who is not the owner gets exactly the missing-session 404, before
/// the handler can touch last-access, readiness, or session state. The owner
/// keeps the handler's current behaviour.
///
/// Applied *inside* [`require_session`] so the resolved caller is already in
/// the request extensions.
pub async fn require_session_owner(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let (mut parts, body) = request.into_parts();
    // `RawPathParams` is populated by routing for every matched route. An
    // error here means the route has no parameter set to read, so it is not a
    // session resource and this layer has nothing to decide.
    let session_id = RawPathParams::from_request_parts(&mut parts, &state)
        .await
        .ok()
        .and_then(|params| {
            params
                .iter()
                .find(|(key, _)| *key == "session_id")
                .map(|(_, value)| value.to_string())
        });
    let Some(session_id) = session_id else {
        return Ok(next.run(Request::from_parts(parts, body)).await);
    };
    let Some(caller) = parts.extensions.get::<Caller>() else {
        // `require_session` runs first and resolves the caller. Arriving here
        // without one is a wiring bug, and it fails closed rather than
        // serving a session to nobody.
        return Err(ApiError::unauthorized(
            "no_credential: no session presented",
        ));
    };
    if !state.hls.is_owned_by(&session_id, &caller.session_owner()) {
        return Err(ApiError::not_found(format!(
            "session {session_id} not found"
        )));
    }
    Ok(next.run(Request::from_parts(parts, body)).await)
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

/// A caller with account powers, refused at extraction time.
///
/// Not a second mechanism: it reads exactly what `require_session` resolved,
/// the same as `Caller`. What it changes is *when* the refusal happens. A gate
/// written as the first line of a handler runs after every extractor, so a
/// malformed body answers 422 and the authority check never runs — a member
/// posting nonsense to an admin route learns the body was wrong before
/// learning they were never allowed to ask. Extracting the requirement moves
/// it in front of the body, and puts it in the signature where it is
/// greppable and hard to drop by accident.
///
/// It carries nothing. A handler that also needs to know *who* the admin is
/// takes `Caller` as well, which costs nothing — both read the same resolved
/// value out of the request extensions. Giving this a payload no handler reads
/// would be a field that exists to look thorough.
pub struct AdminCaller;

/// A caller who has selected a profile (ADR-0034 item 3), same reasoning.
///
/// It carries the ownership identity resolved at extraction. That is the
/// "watching identity" the profile-scope gate used to discard: a session is
/// bound to it at create (ADR-0034 item 8), so the value has to survive the
/// extraction rather than be looked up a second time.
#[derive(Debug, Clone)]
pub struct WatchingCaller {
    owner: SessionOwner,
}

impl WatchingCaller {
    pub fn owner(&self) -> &SessionOwner {
        &self.owner
    }
}

impl FromRequestParts<AppState> for AdminCaller {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let caller =
            <Caller as FromRequestParts<AppState>>::from_request_parts(parts, state).await?;
        caller.require_account_powers()?;
        Ok(Self)
    }
}

impl FromRequestParts<AppState> for WatchingCaller {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let caller =
            <Caller as FromRequestParts<AppState>>::from_request_parts(parts, state).await?;
        caller.require_profile_scope()?;
        Ok(Self {
            owner: caller.session_owner(),
        })
    }
}

/// Resolve a presented token to a caller, touching `last_seen_at` as it goes.
async fn resolve_caller(state: &AppState, token: &str) -> Result<Caller, ApiError> {
    let digest = token_sha256_hex(token);
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

/// Reads what `require_session` resolved. The extractor does no lookup of its
/// own, so a handler cannot be authenticated by taking `Caller` while the
/// layer is missing — the only way to be authenticated is to be behind the
/// layer, which is what makes the guarantee structural rather than habitual.
impl FromRequestParts<AppState> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        parts.extensions.get::<Caller>().cloned().ok_or_else(|| {
            // Unreachable behind the layer, and it fails closed rather than
            // 500ing, because the one way to arrive here is a route that
            // escaped `route_layer` and that must not be a route that works.
            ApiError::unauthorized("no_credential: no session presented")
        })
    }
}

/// `Option<Caller>` for a route in the unauthenticated set that still wants to
/// know whether somebody is logged in. `None` means no valid credential was
/// presented, not that the route is anonymous.
impl OptionalFromRequestParts<AppState> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &AppState,
    ) -> Result<Option<Self>, Self::Rejection> {
        Ok(parts.extensions.get::<Caller>().cloned())
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
    fn the_cookie_accepted_set_is_exactly_these_eleven() {
        let expected = [
            "/api/v0/artwork/{item_key}/{kind}",
            "/api/v0/items/{item_id}/stream",
            "/api/v0/items/{item_id}/subtitles/{asset}",
            "/api/v0/sessions/{session_id}/master.m3u8",
            "/api/v0/sessions/{session_id}/index.m3u8",
            "/api/v0/sessions/{session_id}/v/{rung}/index.m3u8",
            "/api/v0/sessions/{session_id}/v/{rung}/{asset}",
            "/api/v0/sessions/{session_id}/runs/{run_id}/init.mp4",
            "/api/v0/sessions/{session_id}/subs/{*asset}",
            "/api/v0/sessions/{session_id}/init.mp4",
            "/api/v0/sessions/{session_id}/{asset}",
        ];
        assert_eq!(
            COOKIE_ACCEPTED_ROUTES.len(),
            11,
            "item 9 plus ADR-0051's 2026-09-04 amendment name eleven; changing the count needs an ADR amendment"
        );
        assert_eq!(COOKIE_ACCEPTED_ROUTES, expected);
    }

    /// How much of the cookie surface is still an open set, pinned by name.
    ///
    /// An entry ending in a capture accepts whatever its handler chooses to
    /// serve, so its cookie acceptance covers a set the router cannot see the
    /// edges of (issue #96). Five such entries exist, each closed by a parser
    /// in its own handler and none by the router, and this test exists so a
    /// sixth is a deliberate edit rather than a side effect. Shortening this
    /// list is progress; lengthening it needs a reason in the same commit.
    #[test]
    fn the_open_captures_are_these_five_and_no_others() {
        let open: Vec<&str> = COOKIE_ACCEPTED_ROUTES
            .iter()
            .copied()
            .filter(|route| {
                let last = route.rsplit('/').next().unwrap_or_default();
                last.starts_with('{') && last.ends_with('}')
            })
            .collect();
        assert_eq!(
            open,
            [
                // `{kind}`: `ArtworkStore::parse_kind`.
                "/api/v0/artwork/{item_key}/{kind}",
                // `{asset}`: `is_valid_track_id` plus a `.vtt` suffix.
                "/api/v0/items/{item_id}/subtitles/{asset}",
                // `{rung}`: `VideoRung`; trailing `{asset}`:
                // `parse_time_keyed_segment_name`, so `init.mp4` cannot enter.
                "/api/v0/sessions/{session_id}/v/{rung}/{asset}",
                // `{*asset}`: a sidecar tree the router would otherwise have
                // to encode the layout of.
                "/api/v0/sessions/{session_id}/subs/{*asset}",
                // `{asset}`: `hls::is_safe_asset`, two names. Would be two
                // static routes if axum could route `seg_{start_ms}.m4s`.
                "/api/v0/sessions/{session_id}/{asset}",
            ],
            "the cookie surface gained or lost an open capture; say which in \
             the commit that did it"
        );
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

    /// The eleven accepted patterns, plus two that are not on the list: one
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
            // The same layer the real router applies. Without it every probe
            // answers 401, because `Caller` reads what the layer resolved and
            // does no lookup of its own — which is the property, not a
            // nuisance: a handler cannot authenticate itself into existence.
            .route_layer(axum::middleware::from_fn_with_state(
                state.clone(),
                require_session,
            ))
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
                "/api/v0/sessions/s1/index.m3u8",
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
