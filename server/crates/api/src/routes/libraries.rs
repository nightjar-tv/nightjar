use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use nightjar_core::LibraryKind;
use nightjar_db::{NewLibrary, ScanJobRow};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryDto {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub kind: String,
    pub item_count: i64,
}

#[derive(Deserialize)]
pub struct CreateLibraryRequest {
    pub name: String,
    pub path: String,
    pub kind: String,
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
) -> ApiResult<(StatusCode, Json<LibraryDto>)> {
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
    let row = state
        .db
        .create_library(&NewLibrary {
            name: name.to_string(),
            path: abs.to_string_lossy().into_owned(),
            kind: kind.as_str().to_string(),
        })
        .map_err(|e| {
            if e.contains("UNIQUE") {
                ApiError::bad_request("a library with that path already exists")
            } else {
                ApiError::internal(e)
            }
        })?;
    Ok((StatusCode::CREATED, Json(to_dto(row))))
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

pub async fn scan(
    State(state): State<AppState>,
    Path(library_id): Path<i64>,
) -> ApiResult<(StatusCode, Json<ScanJobAcceptedDto>)> {
    let db = std::sync::Arc::clone(&state.db);
    let pool = std::sync::Arc::clone(&state.pool);
    let job_id =
        tokio::task::spawn_blocking(move || nightjar_scanner::start_scan_job(db, pool, library_id))
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
    if state
        .db
        .get_library(library_id)
        .map_err(ApiError::internal)?
        .is_none()
    {
        return Err(ApiError::not_found(format!(
            "library {library_id} not found"
        )));
    }
    let items = state
        .db
        .list_items(library_id)
        .map_err(ApiError::internal)?
        .into_iter()
        .map(super::items::to_dto)
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
    }
}
