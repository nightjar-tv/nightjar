mod error;
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
    let db = nightjar_db::open(&data_dir).unwrap_or_else(|e| panic!("database: {e}"));
    let remux = nightjar_transcode::RemuxRegistry::new(
        data_dir.join("cache").join("remux"),
        remux_cache_cap_bytes(),
    )
    .unwrap_or_else(|e| panic!("remux cache: {e}"));
    let hls = nightjar_transcode::HlsSessionRegistry::new(data_dir.join("cache").join("hls"))
        .unwrap_or_else(|e| panic!("hls cache: {e}"));
    let state = AppState::new(db, remux, hls);
    nightjar_scanner::spawn_library_watcher(std::sync::Arc::clone(&state.db));

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
            axum::response::Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime.as_ref())
                .body(Body::from(file.data.into_owned()))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        None => match Assets::get("index.html") {
            Some(file) => axum::response::Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
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

fn remux_cache_cap_bytes() -> u64 {
    const DEFAULT: u64 = 10 * 1024 * 1024 * 1024;
    std::env::var("NIGHTJAR_REMUX_CACHE_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT)
}

fn listen_addr() -> SocketAddr {
    let port = std::env::var("NIGHTJAR_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8096);
    SocketAddr::from(([0, 0, 0, 0], port))
}
