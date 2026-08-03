//! Manual metadata fix API (ADR-0028).
//!
//! Pre-accounts: local-trust like the rest of `/api/v0`. Block 2 must make
//! these admin-only first — assign rewrites watch state across profiles.

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::{
    Json,
    extract::{Path, Query, State},
};
use nightjar_metadata::{
    AssignRequest, NoopArtwork, Resolver, TmdbClient, assign, clear_match, get_fix_item,
    resolve_credentials_with, retry_unmatched, search_candidates,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidatesQuery {
    pub q: Option<String>,
    pub year: Option<i32>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataCandidateDto {
    pub provider: String,
    pub kind: String,
    pub id: i64,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidatesResponse {
    pub candidates: Vec<MetadataCandidateDto>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssignBody {
    /// `movie` or `tv`
    pub kind: String,
    pub id: i64,
    #[serde(default = "default_provider")]
    pub provider: String,
}

fn default_provider() -> String {
    "tmdb".into()
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssignResponse {
    pub item_key: String,
    pub media_item_id: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClearResponse {
    pub item_key: String,
    pub media_item_id: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryResponse {
    pub media_item_id: i64,
    pub status: &'static str,
}

fn tmdb_client(data_dir: &std::path::Path) -> ApiResult<TmdbClient> {
    let env_key = std::env::var("NIGHTJAR_TMDB_API_KEY").ok();
    let creds = resolve_credentials_with(
        Some(data_dir),
        env_key.as_deref(),
        nightjar_metadata::embedded_application_key(),
    )
    .map_err(|e| ApiError::internal(e.operator_reason()))?;
    Ok(TmdbClient::new(creds))
}

fn data_dir() -> std::path::PathBuf {
    std::env::var_os("NIGHTJAR_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("data"))
}

pub async fn candidates(
    State(state): State<AppState>,
    Path(item_id): Path<i64>,
    Query(q): Query<CandidatesQuery>,
) -> ApiResult<Json<CandidatesResponse>> {
    let item = state
        .db
        .with_conn(|c| get_fix_item(c, item_id))
        .map_err(ApiError::internal)?;
    let client = tmdb_client(&data_dir())?;
    // Network off the Db lock.
    let list =
        search_candidates(&client, &item, q.q.as_deref(), q.year).map_err(ApiError::internal)?;
    Ok(Json(CandidatesResponse {
        candidates: list
            .into_iter()
            .map(|c| MetadataCandidateDto {
                provider: c.provider,
                kind: c.kind,
                id: c.id,
                title: c.title,
                year: c.year,
            })
            .collect(),
    }))
}

pub async fn assign_match(
    State(state): State<AppState>,
    Path(item_id): Path<i64>,
    Json(body): Json<AssignBody>,
) -> ApiResult<Json<AssignResponse>> {
    if body.provider != "tmdb" {
        return Err(ApiError::bad_request(
            "only provider tmdb is supported in v1",
        ));
    }
    let kind = body.kind.to_ascii_lowercase();
    if kind != "movie" && kind != "tv" {
        return Err(ApiError::bad_request("kind must be movie or tv"));
    }
    // Own connection so TMDB HTTP during assign does not hold process Db mutex.
    let conn = open_db_conn(&data_dir())?;
    let client = tmdb_client(&data_dir())?;
    let resolver = Resolver { tmdb: &client };
    let result = if let Some(art) = state.artwork.as_ref() {
        assign(
            &conn,
            &resolver,
            &client,
            art.as_ref(),
            &AssignRequest {
                media_item_id: item_id,
                kind: kind.clone(),
                tmdb_id: body.id,
            },
        )
    } else {
        assign(
            &conn,
            &resolver,
            &client,
            &NoopArtwork,
            &AssignRequest {
                media_item_id: item_id,
                kind,
                tmdb_id: body.id,
            },
        )
    }
    .map_err(|e| {
        if e.contains("not found") {
            ApiError::not_found(e)
        } else {
            ApiError::internal(e)
        }
    })?;
    Ok(Json(AssignResponse {
        item_key: result.item_key,
        media_item_id: item_id,
    }))
}

pub async fn clear(
    State(state): State<AppState>,
    Path(item_id): Path<i64>,
) -> ApiResult<Json<ClearResponse>> {
    let conn = open_db_conn(&data_dir())?;
    let result = if let Some(art) = state.artwork.as_ref() {
        clear_match(&conn, art.as_ref(), item_id)
    } else {
        clear_match(&conn, &NoopArtwork, item_id)
    }
    .map_err(|e| {
        if e.contains("not found") {
            ApiError::not_found(e)
        } else {
            ApiError::internal(e)
        }
    })?;
    Ok(Json(ClearResponse {
        item_key: result.item_key,
        media_item_id: item_id,
    }))
}

pub async fn retry(
    State(_state): State<AppState>,
    Path(item_id): Path<i64>,
) -> ApiResult<Json<RetryResponse>> {
    let conn = open_db_conn(&data_dir())?;
    retry_unmatched(&conn, item_id).map_err(|e| {
        if e.contains("not found") {
            ApiError::not_found(e)
        } else {
            ApiError::internal(e)
        }
    })?;
    Ok(Json(RetryResponse {
        media_item_id: item_id,
        status: "pending",
    }))
}

fn open_db_conn(data_dir: &std::path::Path) -> ApiResult<rusqlite::Connection> {
    let path = nightjar_db::db_path(data_dir);
    let conn = rusqlite::Connection::open(&path)
        .map_err(|e| ApiError::internal(format!("open db: {e}")))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA foreign_keys=ON;
         PRAGMA busy_timeout=30000;",
    )
    .map_err(|e| ApiError::internal(format!("pragma: {e}")))?;
    Ok(conn)
}
