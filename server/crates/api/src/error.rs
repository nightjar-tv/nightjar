use axum::{
    Json,
    extract::{FromRequest, Request, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Serialize, de::DeserializeOwned};

#[derive(Serialize)]
pub struct ErrorBody {
    pub error: String,
    /// Stable machine-readable class of the error. A client branches on this,
    /// never on the human sentence in `error` (that sentence is a person's
    /// copy, not an API).
    pub code: &'static str,
}

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
    pub code: &'static str,
}

impl ApiError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
            code: "bad_request",
        }
    }

    /// No usable credential. Distinct from [`Self::forbidden`]: this says the
    /// caller is nobody, that one says the caller is somebody without reach.
    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: msg.into(),
            code: "unauthorized",
        }
    }

    /// Authenticated and refused. Never a 404 for an authorisation failure: a
    /// 404 leaks whether the thing exists (ADR-0035 item 7).
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: msg.into(),
            code: "forbidden",
        }
    }

    pub fn conflict(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: msg.into(),
            code: "conflict",
        }
    }

    /// The request parsed and its values are not usable (ADR-0035 amendment
    /// item 4). Distinct from [`Self::bad_request`], which is a request the
    /// server could not read at all.
    pub fn unprocessable(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: msg.into(),
            code: "validation_error",
        }
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: msg.into(),
            code: "not_found",
        }
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: msg.into(),
            code: "internal",
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: self.message,
                code: self.code,
            }),
        )
            .into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

/// A JSON request body whose refusals answer in the repository's error shape.
///
/// Axum's [`Json`] rejection writes a plain-text body, so a client that
/// branches on `code` (Rule 4.11) cannot tell a malformed body from a missing
/// content type. This keeps axum's status for each case — 400 for a syntax
/// error, 415 for a missing JSON content type, 422 for a body that parsed but
/// does not match the target shape — and maps the message to [`ErrorBody`].
pub struct TypedJson<T>(pub T);

impl<T, S> FromRequest<S> for TypedJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let Json(value) = Json::<serde_json::Value>::from_request(req, state)
            .await
            .map_err(typed_json_rejection)?;
        // serde accepts a struct as a JSON array in field order, which is a
        // shape this API never means. A body is an object or it is refused.
        if !value.is_object() {
            return Err(ApiError::unprocessable(
                "request body must be a JSON object",
            ));
        }
        let body = serde_json::from_value(value).map_err(|error| {
            ApiError::unprocessable(format!(
                "Failed to deserialize the JSON body into the target type: {error}"
            ))
        })?;
        Ok(TypedJson(body))
    }
}

/// Map a [`JsonRejection`] to the typed error body, keeping its status.
fn typed_json_rejection(rejection: JsonRejection) -> ApiError {
    match &rejection {
        JsonRejection::MissingJsonContentType(_) => ApiError {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            message: rejection.body_text(),
            code: "unsupported_media_type",
        },
        JsonRejection::JsonDataError(_) => ApiError::unprocessable(rejection.body_text()),
        JsonRejection::JsonSyntaxError(_) => ApiError::bad_request(rejection.body_text()),
        // A body that could not be read at all: keep axum's status and call it
        // a bad request, because the client can only fix the bytes it sent.
        _ => ApiError {
            status: rejection.status(),
            message: rejection.body_text(),
            code: "bad_request",
        },
    }
}

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
