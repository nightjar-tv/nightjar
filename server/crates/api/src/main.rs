use axum::{
    Router,
    body::Body,
    extract::Request,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use rust_embed::Embed;
use std::net::SocketAddr;
use tower_http::trace::TraceLayer;

#[derive(Embed)]
#[folder = "../../../web/build"]
struct Assets;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nightjar=info,tower_http=info".into()),
        )
        .init();

    let app = Router::new()
        .route("/api/health", get(health))
        .fallback(static_handler)
        .layer(TraceLayer::new_for_http());

    let addr = listen_addr();
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));
    tracing::info!("nightjar listening on http://{addr}");
    axum::serve(listener, app)
        .await
        .unwrap_or_else(|e| panic!("server error: {e}"));
}

async fn health() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/json")],
        format!(
            r#"{{"status":"ok","version":"{}","core":"{}"}}"#,
            env!("CARGO_PKG_VERSION"),
            nightjar_core::version()
        ),
    )
}

async fn static_handler(req: Request) -> Response {
    let path = req.uri().path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match Assets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime.as_ref())
                .body(Body::from(file.data.into_owned()))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        None => {
            // SPA fallback: serve index.html for unknown paths.
            match Assets::get("index.html") {
                Some(file) => Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                    .body(Body::from(file.data.into_owned()))
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
                None => (StatusCode::NOT_FOUND, "not found").into_response(),
            }
        }
    }
}

fn listen_addr() -> SocketAddr {
    let port = std::env::var("NIGHTJAR_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8096);
    SocketAddr::from(([0, 0, 0, 0], port))
}
