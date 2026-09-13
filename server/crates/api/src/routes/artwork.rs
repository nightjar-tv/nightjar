//! Serve cached artwork (ADR-0027).

use crate::authority::Caller;
use crate::error::{ApiError, ApiResult, blocking};
use crate::state::AppState;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::Response,
};
use nightjar_metadata::{ArtworkStore, artwork_key_is_visible, resolve_artwork_key};
use serde::Deserialize;
use std::fs;

#[derive(Debug, Deserialize)]
pub struct ArtworkQuery {
    /// Optional derived width (342 or 780).
    pub w: Option<u32>,
}

pub async fn get_artwork(
    State(state): State<AppState>,
    caller: Caller,
    Path((item_key, kind_s)): Path<(String, String)>,
    Query(q): Query<ArtworkQuery>,
) -> ApiResult<Response> {
    // A DB read plus a cache-file read, on every poster in the grid.
    blocking(move || get_artwork_blocking(state, caller, item_key, kind_s, q)).await
}

fn get_artwork_blocking(
    state: AppState,
    caller: Caller,
    item_key: String,
    kind_s: String,
    q: ArtworkQuery,
) -> ApiResult<Response> {
    let kind = ArtworkStore::parse_kind(&kind_s)
        .ok_or_else(|| ApiError::bad_request(format!("unknown artwork kind {kind_s}")))?;

    // Gate before any store read or provider fetch (ADR-0037 item 7). The
    // request key is resolved through the authoritative media/series identity,
    // and the same viewer scope that filters items decides. A hidden key gets
    // the same not-found a missing image gets, so a capped profile cannot probe
    // for titles by guessing a provider key.
    let scope = crate::authority::viewer_scope(&state, &caller)?;
    let visible = state
        .db
        .with_conn(|c| artwork_key_is_visible(c, &scope, &item_key))
        .map_err(ApiError::internal)?;
    if !visible {
        return Err(ApiError::not_found(format!("artwork {item_key} not found")));
    }

    let store = state
        .artwork
        .as_ref()
        .ok_or_else(|| ApiError::internal("artwork store not configured"))?;

    // Resolve the request key to the key the store warmed/serves under: a
    // provider key straight up, or the provisional `tmdb:show:` / `tmdb:movie:`
    // link for an effective path key (R2 — matched TV warms under show keys).
    // The kind goes in, so every kind can find its own source and fetch on
    // demand (ADR-0027 §5); before this only posters could.
    let (serve_key, source) = state
        .db
        .with_conn(|c| resolve_artwork_key(c, &item_key, kind))
        .map_err(ApiError::internal)?;

    let file = store
        .resolve_file(&serve_key, kind, q.w, source.as_deref())
        .map_err(|e| {
            if e.contains("not cached") {
                ApiError::not_found(e)
            } else {
                ApiError::internal(e)
            }
        })?;

    let bytes = fs::read(&file).map_err(|e| ApiError::internal(format!("read art: {e}")))?;
    let ctype = image_content_type(&bytes);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, ctype)
        .header(header::CACHE_CONTROL, "public, max-age=86400")
        .body(Body::from(bytes))
        .map_err(|e| ApiError::internal(e.to_string()))
}

/// Content type from the bytes, not the filename: originals are stored as
/// `.orig` whatever they are, so extension-sniffing labelled every one of them
/// `application/octet-stream` and left the browser to guess.
fn image_content_type(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else {
        "application/octet-stream"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_stored_originals() {
        assert_eq!(image_content_type(&[0xFF, 0xD8, 0xFF, 0xE0]), "image/jpeg");
        assert_eq!(image_content_type(b"\x89PNG\r\n\x1a\nrest"), "image/png");
        assert_eq!(image_content_type(b"RIFF\0\0\0\0WEBPVP8 "), "image/webp");
        assert_eq!(
            image_content_type(b"not an image"),
            "application/octet-stream"
        );
        assert_eq!(image_content_type(&[]), "application/octet-stream");
    }
}

/// The artwork route is gated by the same viewer scope as every item-returning
/// surface (ADR-0037 item 7). A capped profile cannot fetch a hidden title's
/// cached poster by guessing its provider key, and hidden answers exactly the
/// not-found a missing key answers.
#[cfg(test)]
mod kids_scope_router_tests {
    use crate::routes::router;
    use crate::state::{AppState, test_support};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use nightjar_auth::mint_session_token;
    use nightjar_db::UpsertItem;
    use nightjar_metadata::{ArtworkKind, ArtworkStore};
    use std::sync::Arc;
    use tower::ServiceExt;

    /// A capped, profile-scoped session with the US region selected.
    fn capped_token(state: &AppState) -> String {
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
                    "UPDATE profiles SET classification_cap = 'little_kid' WHERE id = ?1",
                    rusqlite::params![profile_id],
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

    /// An owner account-scope session.
    fn account_token(state: &AppState) -> String {
        let minted = mint_session_token();
        state
            .db
            .with_conn(|conn| {
                let hash = nightjar_auth::hash_password("x").map_err(|e| format!("hash: {e:?}"))?;
                let (account_id, _) = nightjar_db::create_account_with_profile(
                    conn, "owner", &hash, "owner", "P", "ownerref",
                )?;
                let expires = nightjar_db::session_expiry(conn)?;
                nightjar_db::create_session(conn, account_id, &minted.sha256_hex, "t", &expires)?;
                Ok(())
            })
            .unwrap();
        minted.plaintext
    }

    /// A movie item linked to `tmdb:movie:{provider_id}` with the given
    /// certification, ready, plus a warmed poster file under its serve key.
    fn seed_movie(state: &AppState, dir: &std::path::Path, provider_id: &str, cert: Option<&str>) {
        let library = state
            .db
            .create_library(&nightjar_db::NewLibrary {
                name: format!("movies{provider_id}"),
                path: dir.join(provider_id).to_string_lossy().into_owned(),
                kind: "movies".to_string(),
            })
            .unwrap();
        let item_id = state
            .db
            .upsert_items_indexed(
                library.id,
                &[UpsertItem {
                    path: format!("{provider_id}.mkv"),
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
                    rusqlite::params![provider_id, cert],
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
    }

    fn warm(store: &ArtworkStore, provider_id: &str) {
        let path = store.original_path(&format!("tmdb:movie:{provider_id}"), ArtworkKind::Poster);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"\x89PNG\r\n\x1a\nposter").unwrap();
    }

    async fn art(state: &AppState, provider_id: &str, token: &str) -> StatusCode {
        let request = Request::builder()
            .method("GET")
            .uri(format!("/api/v0/artwork/tmdb:movie:{provider_id}/poster"))
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        router(state.clone())
            .oneshot(request)
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn a_capped_profile_cannot_fetch_hidden_artwork() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = test_support::state(dir.path());
        let store = ArtworkStore::new(dir.path()).unwrap();
        warm(&store, "1");
        warm(&store, "2");
        state.artwork = Some(Arc::new(store));
        seed_movie(&state, dir.path(), "1", Some("{\"US\":\"G\"}"));
        seed_movie(&state, dir.path(), "2", Some("{\"US\":\"R\"}"));
        let token = capped_token(&state);

        // Positive control: the at-cap title's warmed poster is served.
        assert_eq!(art(&state, "1", &token).await, StatusCode::OK);
        // The over-cap title's poster is cached but hidden, and answers the
        // same not-found a missing key answers.
        assert_eq!(art(&state, "2", &token).await, StatusCode::NOT_FOUND);
        assert_eq!(art(&state, "999", &token).await, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn account_scope_artwork_is_unrestricted() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = test_support::state(dir.path());
        let store = ArtworkStore::new(dir.path()).unwrap();
        warm(&store, "2");
        state.artwork = Some(Arc::new(store));
        seed_movie(&state, dir.path(), "2", Some("{\"US\":\"R\"}"));
        let token = account_token(&state);

        // The same over-cap title is served to account scope, so the filter is
        // scoping and not an unconditional refusal.
        assert_eq!(art(&state, "2", &token).await, StatusCode::OK);
    }
}
