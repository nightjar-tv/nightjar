pub mod items;
mod libraries;
pub mod sessions;
mod system;

use crate::state::AppState;
use axum::{
    Json, Router,
    routing::{delete, get, post},
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
        .route("/api/v0/libraries/{library_id}", get(libraries::get))
        .route("/api/v0/libraries/{library_id}/scan", post(libraries::scan))
        .route("/api/v0/scan-jobs/{job_id}", get(libraries::get_scan_job))
        .route(
            "/api/v0/libraries/{library_id}/items",
            get(libraries::list_items),
        )
        .route("/api/v0/items/{item_id}", get(items::get))
        .route(
            "/api/v0/items/{item_id}/playback-info",
            get(items::playback_info),
        )
        .route(
            "/api/v0/items/{item_id}/subtitles/{stream_index}.vtt",
            get(items::subtitle_vtt),
        )
        .route("/api/v0/items/{item_id}/remux", post(items::start_remux))
        .route("/api/v0/items/{item_id}/sessions", post(sessions::start))
        .route(
            "/api/v0/items/{item_id}/stream",
            get(crate::stream::stream_item),
        )
        .route(
            "/api/v0/sessions/{session_id}/index.m3u8",
            get(sessions::playlist),
        )
        .route(
            "/api/v0/sessions/{session_id}/{asset}",
            get(sessions::asset),
        )
        .route("/api/v0/sessions/{session_id}", delete(sessions::delete))
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
