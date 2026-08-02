use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use nightjar_core::LibraryKind;
use nightjar_db::{NewLibrary, ScanJobRow, normalize_library_root};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryDto {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub kind: String,
    pub item_count: i64,
    pub reachable: bool,
    pub paths_unresolved: i64,
    pub skipped_outside_root: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateLibraryResponse {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub kind: String,
    pub item_count: i64,
    pub reachable: bool,
    pub paths_unresolved: i64,
    pub skipped_outside_root: i64,
    pub job_id: i64,
}

#[derive(Deserialize)]
pub struct CreateLibraryRequest {
    pub name: String,
    pub path: String,
    pub kind: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchLibraryRequest {
    pub name: Option<String>,
    pub path: Option<String>,
}

#[derive(Serialize)]
pub struct LibrariesResponse {
    pub libraries: Vec<LibraryDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemsResponse {
    pub items: Vec<super::items::MediaItemDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanJobAcceptedDto {
    pub job_id: i64,
    pub library_id: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanJobDto {
    pub id: i64,
    pub library_id: i64,
    pub state: String,
    pub added: i64,
    pub updated: i64,
    pub removed: i64,
    pub unchanged: i64,
    pub probed: i64,
    pub errors: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_path: Option<String>,
    pub skipped_outside_root: i64,
}

pub async fn list(State(state): State<AppState>) -> ApiResult<Json<LibrariesResponse>> {
    let libraries = state
        .db
        .list_libraries()
        .map_err(ApiError::internal)?
        .into_iter()
        .map(to_dto)
        .collect();
    Ok(Json(LibrariesResponse { libraries }))
}

pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<CreateLibraryRequest>,
) -> ApiResult<(StatusCode, Json<CreateLibraryResponse>)> {
    let name = body.name.trim();
    let path = body.path.trim();
    if name.is_empty() || path.is_empty() {
        return Err(ApiError::bad_request("name and path are required"));
    }
    let kind = LibraryKind::parse(&body.kind)
        .ok_or_else(|| ApiError::bad_request("kind must be movies or shows"))?;
    let path_buf = std::path::PathBuf::from(path);
    if !path_buf.is_dir() {
        return Err(ApiError::bad_request(format!(
            "path is not a directory: {path}"
        )));
    }
    let abs = std::fs::canonicalize(&path_buf)
        .map_err(|e| ApiError::bad_request(format!("resolve path {path}: {e}")))?;
    let abs = normalize_library_root(&abs.to_string_lossy());
    let row = state
        .db
        .create_library(&NewLibrary {
            name: name.to_string(),
            path: abs,
            kind: kind.as_str().to_string(),
        })
        .map_err(|e| {
            if e.contains("UNIQUE") {
                ApiError::bad_request("a library with that path already exists")
            } else {
                ApiError::internal(e)
            }
        })?;
    let db = std::sync::Arc::clone(&state.db);
    let pool = std::sync::Arc::clone(&state.pool);
    let library_id = row.id;
    let job_id =
        tokio::task::spawn_blocking(move || nightjar_scanner::request_scan(db, pool, library_id))
            .await
            .map_err(|e| ApiError::internal(format!("scan on create join: {e}")))?
            .map_err(ApiError::internal)?;
    Ok((
        StatusCode::CREATED,
        Json(CreateLibraryResponse {
            id: row.id,
            name: row.name,
            path: row.path,
            kind: row.kind,
            item_count: row.item_count,
            reachable: row.reachable,
            paths_unresolved: row.paths_unresolved,
            skipped_outside_root: row.skipped_outside_root,
            job_id,
        }),
    ))
}

pub async fn get(
    State(state): State<AppState>,
    Path(library_id): Path<i64>,
) -> ApiResult<Json<LibraryDto>> {
    let row = state
        .db
        .get_library(library_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("library {library_id} not found")))?;
    Ok(Json(to_dto(row)))
}

/// ADR-0030 §3: name-only → 200; path change → 202 + async repoint job.
pub async fn patch(
    State(state): State<AppState>,
    Path(library_id): Path<i64>,
    Json(body): Json<PatchLibraryRequest>,
) -> ApiResult<axum::response::Response> {
    use axum::response::IntoResponse;

    let row = state
        .db
        .get_library(library_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("library {library_id} not found")))?;

    let name = body.name.as_ref().map(|n| n.trim().to_string());
    if let Some(ref n) = name
        && n.is_empty()
    {
        return Err(ApiError::bad_request("name must not be empty"));
    }

    let path_change = match body.path.as_ref().map(|p| p.trim()) {
        Some("") => return Err(ApiError::bad_request("path must not be empty")),
        Some(p) => {
            let path_buf = std::path::PathBuf::from(p);
            if !path_buf.is_dir() {
                return Err(ApiError::bad_request(format!(
                    "path is not a directory: {p}"
                )));
            }
            let abs = std::fs::canonicalize(&path_buf)
                .map_err(|e| ApiError::bad_request(format!("resolve path {p}: {e}")))?;
            let abs = normalize_library_root(&abs.to_string_lossy());
            if abs != normalize_library_root(&row.path) {
                Some(abs)
            } else {
                None
            }
        }
        None => None,
    };

    if name.is_none() && path_change.is_none() && body.path.is_none() {
        return Err(ApiError::bad_request("name or path is required"));
    }

    if let Some(n) = name {
        state
            .db
            .update_library_name(library_id, &n)
            .map_err(ApiError::internal)?;
    }

    if let Some(candidate) = path_change {
        let db = std::sync::Arc::clone(&state.db);
        let pool = std::sync::Arc::clone(&state.pool);
        let job_id = tokio::task::spawn_blocking(move || {
            nightjar_scanner::request_repoint(db, pool, library_id, &candidate)
        })
        .await
        .map_err(|e| ApiError::internal(format!("repoint join: {e}")))?
        .map_err(|e| {
            if e.contains("not found") {
                ApiError::not_found(e)
            } else if e.contains("already has active job") {
                ApiError::bad_request(e)
            } else {
                ApiError::internal(e)
            }
        })?;
        return Ok((
            StatusCode::ACCEPTED,
            Json(ScanJobAcceptedDto { job_id, library_id }),
        )
            .into_response());
    }

    let updated = state
        .db
        .get_library(library_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("library {library_id} not found")))?;
    Ok((StatusCode::OK, Json(to_dto(updated))).into_response())
}

pub async fn scan(
    State(state): State<AppState>,
    Path(library_id): Path<i64>,
) -> ApiResult<(StatusCode, Json<ScanJobAcceptedDto>)> {
    let db = std::sync::Arc::clone(&state.db);
    let pool = std::sync::Arc::clone(&state.pool);
    let job_id =
        tokio::task::spawn_blocking(move || nightjar_scanner::request_scan(db, pool, library_id))
            .await
            .map_err(|e| ApiError::internal(format!("scan start join: {e}")))?
            .map_err(|e| {
                if e.contains("not found") {
                    ApiError::not_found(e)
                } else {
                    ApiError::internal(e)
                }
            })?;
    Ok((
        StatusCode::ACCEPTED,
        Json(ScanJobAcceptedDto { job_id, library_id }),
    ))
}

pub async fn get_scan_job(
    State(state): State<AppState>,
    Path(job_id): Path<i64>,
) -> ApiResult<Json<ScanJobDto>> {
    let row = state
        .db
        .get_scan_job(job_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("scan job {job_id} not found")))?;
    Ok(Json(job_to_dto(row)))
}

pub async fn list_items(
    State(state): State<AppState>,
    Path(library_id): Path<i64>,
) -> ApiResult<Json<ItemsResponse>> {
    let lib = state
        .db
        .get_library(library_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("library {library_id} not found")))?;
    let root = lib.path.clone();
    let items = state
        .db
        .list_items(library_id)
        .map_err(ApiError::internal)?
        .into_iter()
        .map(|row| super::items::to_dto(row, &root))
        .collect();
    Ok(Json(ItemsResponse { items }))
}

fn to_dto(row: nightjar_db::LibraryRow) -> LibraryDto {
    LibraryDto {
        id: row.id,
        name: row.name,
        path: row.path,
        kind: row.kind,
        item_count: row.item_count,
        reachable: row.reachable,
        paths_unresolved: row.paths_unresolved,
        skipped_outside_root: row.skipped_outside_root,
    }
}

fn job_to_dto(row: ScanJobRow) -> ScanJobDto {
    ScanJobDto {
        id: row.id,
        library_id: row.library_id,
        state: row.state,
        added: row.added,
        updated: row.updated,
        removed: row.removed,
        unchanged: row.unchanged,
        probed: row.probed,
        errors: row.errors,
        index_duration_ms: row.index_duration_ms,
        probe_duration_ms: row.probe_duration_ms,
        error: row.error_message,
        started_at: row.started_at,
        finished_at: row.finished_at,
        kind: row.kind,
        candidate_path: row.candidate_path,
        skipped_outside_root: row.skipped_outside_root,
    }
}
