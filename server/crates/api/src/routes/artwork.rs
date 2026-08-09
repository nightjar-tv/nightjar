//! Serve cached artwork (ADR-0027).

use crate::error::{ApiError, ApiResult, blocking};
use crate::state::AppState;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::Response,
};
use nightjar_metadata::{ArtworkStore, resolve_artwork_key};
use serde::Deserialize;
use std::fs;

#[derive(Debug, Deserialize)]
pub struct ArtworkQuery {
    /// Optional derived width (342 or 780).
    pub w: Option<u32>,
}

pub async fn get_artwork(
    State(state): State<AppState>,
    Path((item_key, kind_s)): Path<(String, String)>,
    Query(q): Query<ArtworkQuery>,
) -> ApiResult<Response> {
    // A DB read plus a cache-file read, on every poster in the grid.
    blocking(move || get_artwork_blocking(state, item_key, kind_s, q)).await
}

fn get_artwork_blocking(
    state: AppState,
    item_key: String,
    kind_s: String,
    q: ArtworkQuery,
) -> ApiResult<Response> {
    let kind = ArtworkStore::parse_kind(&kind_s)
        .ok_or_else(|| ApiError::bad_request(format!("unknown artwork kind {kind_s}")))?;
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
