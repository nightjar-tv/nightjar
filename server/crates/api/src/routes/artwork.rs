//! Serve cached artwork (ADR-0027).

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::Response,
};
use nightjar_metadata::{ArtworkKind, ArtworkStore, poster_path_for_item_key};
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
    let kind = ArtworkStore::parse_kind(&kind_s)
        .ok_or_else(|| ApiError::bad_request(format!("unknown artwork kind {kind_s}")))?;
    let store = state
        .artwork
        .as_ref()
        .ok_or_else(|| ApiError::internal("artwork store not configured"))?;

    let tmdb_path = state
        .db
        .with_conn(|c| Ok(poster_path_for_item_key(c, &item_key)))
        .map_err(ApiError::internal)?;

    // Prefer matching kind from path helper only for poster; backdrop later.
    let path_hint = if matches!(kind, ArtworkKind::Poster) {
        tmdb_path.as_deref()
    } else {
        None
    };

    let file = store
        .resolve_file(&item_key, kind, q.w, path_hint)
        .map_err(|e| {
            if e.contains("not cached") {
                ApiError::not_found(e)
            } else {
                ApiError::internal(e)
            }
        })?;

    let bytes = fs::read(&file).map_err(|e| ApiError::internal(format!("read art: {e}")))?;
    let ctype = if file.extension().and_then(|e| e.to_str()) == Some("jpg") {
        "image/jpeg"
    } else {
        "application/octet-stream"
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, ctype)
        .header(header::CACHE_CONTROL, "public, max-age=86400")
        .body(Body::from(bytes))
        .map_err(|e| ApiError::internal(e.to_string()))
}
