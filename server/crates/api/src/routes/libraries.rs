use crate::authority::AdminCaller;
use crate::error::{ApiError, ApiResult, blocking};
use crate::state::AppState;
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use nightjar_core::{IndexPass, LibraryKind, metadata_display, probe_display, probe_total};
use nightjar_db::{NewLibrary, ScanJobRow, ScanProgressCounts, normalize_library_root};
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
    /// Present on repoint jobs: unmatched rows left in place (delete deferred).
    pub deferred_remove: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeProgressDto {
    pub display: &'static str,
    /// Probes finished by this job. Cumulative, never a queue depth.
    pub done: i64,
    /// Items waiting to be probed right now. A depth, never a total.
    pub queued: i64,
    pub errors: i64,
    /// Absent while the index pass is still discovering files, because a
    /// denominator that grows makes a percentage move backwards.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataProgressDto {
    pub display: &'static str,
    pub pending: i64,
    pub ready: i64,
    pub unmatched: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanProgressDto {
    pub library_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    pub index_pass_complete: bool,
    pub found: i64,
    pub probe: ProbeProgressDto,
    /// Its own line, never folded into the probe bar: the two finish at
    /// different times and one figure over both would report neither.
    pub metadata: MetadataProgressDto,
}

pub async fn list(State(state): State<AppState>) -> ApiResult<Json<LibrariesResponse>> {
    blocking(move || {
        let libraries = state
            .db
            .list_libraries()
            .map_err(ApiError::internal)?
            .into_iter()
            .map(to_dto)
            .collect();
        Ok(Json(LibrariesResponse { libraries }))
    })
    .await
}

pub async fn create(
    State(state): State<AppState>,
    _admin: AdminCaller,
    Json(body): Json<CreateLibraryRequest>,
) -> ApiResult<(StatusCode, Json<CreateLibraryResponse>)> {
    // `is_dir` and `canonicalize` stat a possibly-remote root, and
    // `create_library` takes the store mutex.
    let row = {
        let db = std::sync::Arc::clone(&state.db);
        blocking(move || {
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
            db.create_library(&NewLibrary {
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
            })
        })
        .await?
    };
    let db = std::sync::Arc::clone(&state.db);
    let pool = std::sync::Arc::clone(&state.pool);
    let library_id = row.id;
    let job_id = tokio::task::spawn_blocking(move || {
        nightjar_scanner::request_scan(db, pool, library_id, nightjar_scanner::ScanTrigger::Create)
    })
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
    blocking(move || {
        let row = state
            .db
            .get_library(library_id)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found(format!("library {library_id} not found")))?;
        Ok(Json(to_dto(row)))
    })
    .await
}

/// ADR-0030 §3: name-only → 200; path change → 202 + async repoint job.
pub async fn patch(
    State(state): State<AppState>,
    _admin: AdminCaller,
    Path(library_id): Path<i64>,
    Json(body): Json<PatchLibraryRequest>,
) -> ApiResult<axum::response::Response> {
    use axum::response::IntoResponse;

    // Validate, apply the name, and decide whether the path moved — all of it
    // stats the root or takes the store mutex.
    let path_change = {
        let db = std::sync::Arc::clone(&state.db);
        blocking(move || {
            let row = db
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
                db.update_library_name(library_id, &n)
                    .map_err(ApiError::internal)?;
            }
            Ok(path_change)
        })
        .await?
    };

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

    let updated = blocking(move || {
        state
            .db
            .get_library(library_id)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found(format!("library {library_id} not found")))
    })
    .await?;
    Ok((StatusCode::OK, Json(to_dto(updated))).into_response())
}

pub async fn scan(
    State(state): State<AppState>,
    _admin: AdminCaller,
    Path(library_id): Path<i64>,
) -> ApiResult<(StatusCode, Json<ScanJobAcceptedDto>)> {
    let db = std::sync::Arc::clone(&state.db);
    let pool = std::sync::Arc::clone(&state.pool);
    let job_id = tokio::task::spawn_blocking(move || {
        nightjar_scanner::request_scan(db, pool, library_id, nightjar_scanner::ScanTrigger::Manual)
    })
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
    blocking(move || {
        let row = state
            .db
            .get_scan_job(job_id)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found(format!("scan job {job_id} not found")))?;
        Ok(Json(job_to_dto(row)))
    })
    .await
}

/// Progress of the library's most recent scan job.
///
/// Poll cadence is the client's, and it is deliberately not the ~2/second
/// `/sessions` cadence: the counts here move in bursts because the index pass
/// commits 200 rows at a time, so nothing changes between most of those
/// requests. See the web client's `PROGRESS_POLL_MS`.
pub async fn scan_progress(
    State(state): State<AppState>,
    Path(library_id): Path<i64>,
) -> ApiResult<Json<ScanProgressDto>> {
    blocking(move || {
        state
            .db
            .get_library(library_id)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found(format!("library {library_id} not found")))?;
        let job = state
            .db
            .latest_scan_job(library_id)
            .map_err(ApiError::internal)?;
        let counts = state
            .db
            .scan_progress_counts(library_id)
            .map_err(ApiError::internal)?;
        Ok(Json(progress_to_dto(library_id, job, counts)))
    })
    .await
}

fn progress_to_dto(
    library_id: i64,
    job: Option<ScanJobRow>,
    counts: ScanProgressCounts,
) -> ScanProgressDto {
    let index = IndexPass::from_job_state(job.as_ref().map(|j| j.state.as_str()));
    // `probed` on the job row is cumulative for that job; `probe_queued` is a
    // live queue depth. They are reported as separate named fields and only
    // added together to form the total, once that total has stopped growing.
    let done = job.as_ref().map_or(0, |j| j.probed);
    ScanProgressDto {
        library_id,
        job_id: job.as_ref().map(|j| j.id),
        state: job.as_ref().map(|j| j.state.clone()),
        index_pass_complete: index != IndexPass::Running,
        found: counts.found,
        probe: ProbeProgressDto {
            display: probe_display(index, counts.probe_queued).as_str(),
            done,
            queued: counts.probe_queued,
            errors: counts.probe_errors,
            total: probe_total(index, done, counts.probe_queued),
        },
        metadata: MetadataProgressDto {
            display: metadata_display(counts.metadata_pending).as_str(),
            pending: counts.metadata_pending,
            ready: counts.metadata_ready,
            unmatched: counts.metadata_unmatched,
        },
    }
}

pub async fn list_items(
    State(state): State<AppState>,
    caller: crate::authority::Caller,
    Path(library_id): Path<i64>,
) -> ApiResult<Json<ItemsResponse>> {
    // The whole-library read behind the grid. On a 23k-item library this holds
    // the store mutex long enough to matter, and it ran on a Tokio worker.
    blocking(move || {
        let scope = crate::authority::viewer_scope(&state, &caller)?;
        let lib = state
            .db
            .get_library(library_id)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found(format!("library {library_id} not found")))?;
        let root = lib.path.clone();
        let rows = state
            .db
            .list_items(library_id)
            .map_err(ApiError::internal)?;
        let ids: Vec<i64> = rows.iter().map(|row| row.id).collect();
        // One cache per request, so the visibility batch is issued once even if
        // the caller makes more than one decision.
        let mut visibility = nightjar_metadata::VisibilityCache::new();
        let visible = state
            .db
            .with_conn(|conn| {
                nightjar_metadata::visible_item_ids_cached(conn, &scope, &ids, &mut visibility)
            })
            .map_err(ApiError::internal)?;
        let items = rows
            .into_iter()
            .filter(|row| visible.contains(&row.id))
            .map(|row| super::items::to_dto(row, &root))
            .collect();
        Ok(Json(ItemsResponse { items }))
    })
    .await
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
        deferred_remove: row.deferred_remove,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: i64, state: &str, probed: i64) -> ScanJobRow {
        ScanJobRow {
            id,
            library_id: 2,
            state: state.into(),
            added: 0,
            updated: 0,
            removed: 0,
            unchanged: 0,
            probed,
            errors: 0,
            index_duration_ms: None,
            probe_duration_ms: None,
            error_message: None,
            started_at: "2026-08-09T00:00:00.000Z".into(),
            finished_at: None,
            kind: "scan".into(),
            candidate_path: None,
            skipped_outside_root: 0,
            deferred_remove: 0,
        }
    }

    fn counts(found: i64, queued: i64, metadata_pending: i64) -> ScanProgressCounts {
        ScanProgressCounts {
            found,
            probe_queued: queued,
            probe_errors: 0,
            metadata_pending,
            metadata_ready: 0,
            metadata_unmatched: 0,
        }
    }

    /// The temptation this feature exists to resist: a percentage while the
    /// walk is still discovering files. There is no denominator on the wire to
    /// build one from, however deep the queue happens to be.
    #[test]
    fn no_total_reaches_the_wire_during_the_index_pass() {
        for state in ["queued", "indexing"] {
            let dto = progress_to_dto(2, Some(job(7, state, 4_000)), counts(13_945, 9_945, 0));
            assert_eq!(dto.probe.total, None, "{state}");
            assert_eq!(dto.probe.display, "count", "{state}");
            assert!(!dto.index_pass_complete, "{state}");
            assert_eq!(dto.found, 13_945, "{state}");
            // done and queued stay distinct: one is the job's cumulative
            // count, the other the live depth.
            assert_eq!(dto.probe.done, 4_000, "{state}");
            assert_eq!(dto.probe.queued, 9_945, "{state}");
        }
    }

    #[test]
    fn a_fixed_total_appears_only_after_the_index_pass() {
        let deep = progress_to_dto(2, Some(job(7, "probing", 1_794)), counts(25_038, 23_244, 0));
        assert!(deep.index_pass_complete);
        assert_eq!(deep.probe.total, Some(25_038));
        assert_eq!(deep.probe.display, "bar");

        // The keep-pace regime: the total is known and the bar is not drawn,
        // because it would appear and vanish.
        let shallow = progress_to_dto(2, Some(job(7, "probing", 24_863)), counts(25_038, 175, 0));
        assert_eq!(shallow.probe.total, Some(25_038));
        assert_eq!(shallow.probe.display, "count");
    }

    #[test]
    fn metadata_keeps_its_own_line() {
        let dto = progress_to_dto(2, Some(job(7, "completed", 25_038)), counts(25_038, 0, 200));
        assert_eq!(dto.probe.display, "none", "probe is finished");
        assert_eq!(dto.metadata.display, "count", "metadata is not");
        assert_eq!(dto.metadata.pending, 200);
    }

    #[test]
    fn a_library_that_never_scanned_shows_nothing() {
        let dto = progress_to_dto(2, None, counts(0, 0, 0));
        assert_eq!(dto.job_id, None);
        assert_eq!(dto.state, None);
        assert_eq!(dto.probe.display, "none");
        assert_eq!(dto.metadata.display, "none");
    }
}
