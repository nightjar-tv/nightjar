mod artwork;
pub mod items;
mod libraries;
mod metadata_fix;
pub mod sessions;
mod system;
mod track_ids;

use crate::state::AppState;
use axum::{
    Json, Router,
    routing::{get, post},
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
        .route(
            "/api/v0/sessions/{session_id}/runs/{run_id}/master.m3u8",
            get(sessions::master),
        )
        .route(
            "/api/v0/sessions/{session_id}/runs/{run_id}/index.m3u8",
            get(sessions::playlist),
        )
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
        .route(
            "/api/v0/sessions/{session_id}/{asset}",
            get(sessions::asset),
        )
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
