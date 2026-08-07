mod error;
mod metadata_drain;
mod routes;
mod state;
mod stream;

use rust_embed::Embed;
use state::AppState;
use std::net::SocketAddr;
use std::path::PathBuf;
use tower_http::trace::TraceLayer;

#[derive(Embed)]
#[folder = "../../../web/build"]
struct Assets;

#[tokio::main]
async fn main() {
    let started = std::time::Instant::now();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nightjar=info,tower_http=info".into()),
        )
        .init();

    let data_dir = data_dir();
    let db_raw = nightjar_db::open(&data_dir).unwrap_or_else(|e| panic!("database: {e}"));
    match db_raw.fail_stale_scan_jobs() {
        Ok(n) if n > 0 => {
            tracing::info!(count = n, "cleared scan jobs left active by a prior exit")
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "clear stale scan jobs failed"),
    }
    let subs = std::sync::Arc::new(
        nightjar_transcode::SubsStore::new(data_dir.join("subs"))
            .unwrap_or_else(|e| panic!("subtitle store: {e}")),
    );
    let db = std::sync::Arc::new(db_raw);
    // ADR-0009: verify encoders once at startup; sessions reuse this Arc.
    let transcode_caps = nightjar_transcode::probe_h264_encoders_arc(&data_dir.join("cache"));
    let hls = nightjar_transcode::HlsSessionRegistry::with_cap(
        data_dir.join("cache").join("hls"),
        hls_max_sessions(),
        transcode_caps.preferred_encode_leg.clone(),
        Some(subs.clone()),
        Some(db.clone()),
    )
    .unwrap_or_else(|e| panic!("hls cache: {e}"));
    let pool = nightjar_scanner::LibraryPool::spawn(
        std::sync::Arc::clone(&db),
        std::sync::Arc::clone(&subs),
    );
    // ADR-0023 §9.3: a seek that arrives before the keyframe map is ready
    // waits, bounded, for a build the consumer already triggered; the pool
    // answers whether one is queued or in flight.
    hls.set_map_build_in_flight(Some(std::sync::Arc::new({
        let pool = std::sync::Arc::clone(&pool);
        move |item_id: i64| pool.map_build_pending(item_id)
    })));
    match pool.drain_pending_probes() {
        Ok(n) if n > 0 => tracing::info!(count = n, "resumed indexed items awaiting probe"),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "enqueue pending probes failed"),
    }
    // No startup subtitle-extract drain: ADR-0041 Decision 9 — a first-start
    // backfill is exactly the scan-time surprise this slice removes; the probe
    // drain above reclassifies `pending` items, and extraction is on-demand.
    // No startup keyframe-map drain either (ADR-0023 §2/§9): a map builds
    // when a consumer asks — playbackInfo, session create, or a seek's
    // bounded wait — never as a whole-library consequence of starting up.
    if let Err(e) = pool.cleanup_orphan_subtitles() {
        tracing::warn!(error = %e, "subtitle orphan cleanup failed");
    }
    let tonemap_available = nightjar_transcode::host_tonemap_available();
    if !tonemap_available {
        tracing::warn!(
            "FFmpeg lacks zscale (libzimg); HDR→SDR sessions will be refused (ADR-0022)"
        );
    }
    let artwork = nightjar_metadata::ArtworkStore::new(&data_dir)
        .map(std::sync::Arc::new)
        .map_err(|e| {
            tracing::warn!(error = %e, "artwork store init failed");
            e
        })
        .ok();
    let state = AppState {
        db,
        hls,
        transcode_caps,
        tonemap_available,
        subs,
        pool: std::sync::Arc::clone(&pool),
        artwork,
    };
    nightjar_scanner::spawn_library_watcher(std::sync::Arc::clone(&state.db), pool);
    // ADR-0026: metadata is async; scan never waits. Own SQLite connection.
    metadata_drain::spawn_metadata_drain(data_dir.clone());

    let app = routes::router(state)
        .fallback(static_handler)
        .layer(TraceLayer::new_for_http());

    let addr = listen_addr();
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));
    // startup_ms excludes exec/dyld/Gatekeeper time before main; the gate script
    // measures the full spawn-to-health number and compares against this.
    tracing::info!(
        %addr,
        data_dir = %data_dir.display(),
        startup_ms = started.elapsed().as_millis() as u64,
        "nightjar listening"
    );
    axum::serve(listener, app)
        .await
        .unwrap_or_else(|e| panic!("server error: {e}"));
}

async fn static_handler(req: axum::extract::Request) -> axum::response::Response {
    use axum::{
        body::Body,
        http::{StatusCode, header},
        response::IntoResponse,
    };

    let path = req.uri().path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match Assets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            // Hashed `/_app/immutable/*` can be cached forever; index.html must
            // not be, or Chrome keeps a stale shell that points at deleted
            // chunks (and our SPA fallback used to 200 HTML for those misses).
            let cache = if path.starts_with("_app/immutable/") {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            };
            axum::response::Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime.as_ref())
                .header(header::CACHE_CONTROL, cache)
                .body(Body::from(file.data.into_owned()))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        None if path.starts_with("_app/") => {
            // Never SPA-fallback hashed assets: a 200 HTML body for a .js URL
            // leaves Chrome running a broken or stale module graph.
            (StatusCode::NOT_FOUND, "not found").into_response()
        }
        None => match Assets::get("index.html") {
            Some(file) => axum::response::Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .header(header::CACHE_CONTROL, "no-cache")
                .body(Body::from(file.data.into_owned()))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
            None => (StatusCode::NOT_FOUND, "not found").into_response(),
        },
    }
}

fn data_dir() -> PathBuf {
    std::env::var_os("NIGHTJAR_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("data"))
}

fn hls_max_sessions() -> usize {
    const DEFAULT: usize = 3;
    std::env::var("NIGHTJAR_HLS_MAX_SESSIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(DEFAULT)
}

fn listen_addr() -> SocketAddr {
    let port = std::env::var("NIGHTJAR_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8096);
    SocketAddr::from(([0, 0, 0, 0], port))
}
