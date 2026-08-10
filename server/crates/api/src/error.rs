use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;

#[derive(Serialize)]
pub struct ErrorBody {
    pub error: String,
}

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
        }
    }

    /// No usable credential. Distinct from [`Self::forbidden`]: this says the
    /// caller is nobody, that one says the caller is somebody without reach.
    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: msg.into(),
        }
    }

    /// Authenticated and refused. Never a 404 for an authorisation failure: a
    /// 404 leaks whether the thing exists (ADR-0035 item 7).
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: msg.into(),
        }
    }

    pub fn conflict(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: msg.into(),
        }
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: msg.into(),
        }
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: msg.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

/// Run handler work that blocks — SQLite, the filesystem, or an ffprobe child
/// — off the async runtime.
///
/// The database is one `Connection` behind a `Mutex`, so a scanner index batch
/// can hold it for the length of a 200-item transaction. Taking that mutex on
/// a Tokio worker thread parks the worker, which degrades routes that never
/// touch the database at all; `list_audio_tracks` and `list_text_subtitles`
/// are worse still, since they wait on a child process reading over SMB.
///
/// Handlers that touch any of the three run their whole body in here.
///
/// This is measured, not predicted. On the 2026-08-07 cold scan, with this in
/// place, API latency held at **max 18.97 ms across 7,140 samples** — zero
/// non-200, zero over 25 ms — with an independent server-side cross-check
/// agreeing at 19 ms. The route behind the worst of that is
/// `/api/v0/libraries/{id}/items`, which is exactly the one contending with the
/// scanner's index batch. Do not re-argue this from the mechanism; the number
/// exists.
pub async fn blocking<T, F>(f: F) -> ApiResult<T>
where
    F: FnOnce() -> ApiResult<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::internal(format!("blocking handler task: {e}")))?
}
