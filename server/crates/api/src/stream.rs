use crate::error::{ApiError, ApiResult};
use crate::routes::items::decide;
use crate::state::AppState;
use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::Response,
};
use nightjar_core::{PlaybackMethod, mime_for_path};
use std::io::SeekFrom;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

pub async fn stream_item(
    State(state): State<AppState>,
    Path(item_id): Path<i64>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let row = state
        .db
        .get_item(item_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("item {item_id} not found")))?;
    let decision = decide(&row);

    match decision.method {
        PlaybackMethod::DirectPlay => {
            let path = std::path::PathBuf::from(&row.path);
            let mime = mime_for_path(&row.path);
            serve_file(path, mime, &headers).await
        }
        PlaybackMethod::Remux | PlaybackMethod::Transcode => Err(ApiError {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            message: format!(
                "item {item_id} needs an HLS session; POST /api/v0/items/{item_id}/sessions: {}",
                decision.reason
            ),
        }),
    }
}

async fn serve_file(
    path: std::path::PathBuf,
    mime: String,
    headers: &HeaderMap,
) -> ApiResult<Response> {
    let meta = tokio::fs::metadata(&path)
        .await
        .map_err(|e| ApiError::internal(format!("stat {}: {e}", path.display())))?;
    let file_size = meta.len();

    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_byte_range);

    match range {
        Some((start, end_opt)) if start < file_size => {
            let end = end_opt.unwrap_or(file_size - 1).min(file_size - 1);
            if start > end {
                return Err(ApiError {
                    status: StatusCode::RANGE_NOT_SATISFIABLE,
                    message: "invalid range".into(),
                });
            }
            let len = end - start + 1;
            let mut file = File::open(&path)
                .await
                .map_err(|e| ApiError::internal(format!("open {}: {e}", path.display())))?;
            file.seek(SeekFrom::Start(start))
                .await
                .map_err(|e| ApiError::internal(format!("seek {}: {e}", path.display())))?;
            // Stream the range. Never buffer the whole span into RAM.
            // Browsers send `bytes=0-` for large files; that used to hang here.
            let stream = ReaderStream::new(file.take(len));
            let mut res = Response::new(Body::from_stream(stream));
            *res.status_mut() = StatusCode::PARTIAL_CONTENT;
            let headers = res.headers_mut();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(&mime).unwrap());
            headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            headers.insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&len.to_string()).unwrap(),
            );
            headers.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes {start}-{end}/{file_size}")).unwrap(),
            );
            Ok(res)
        }
        Some(_) => Err(ApiError {
            status: StatusCode::RANGE_NOT_SATISFIABLE,
            message: format!("range not satisfiable; size {file_size}"),
        }),
        None => {
            let file = File::open(&path)
                .await
                .map_err(|e| ApiError::internal(format!("open {}: {e}", path.display())))?;
            let stream = ReaderStream::new(file);
            let mut res = Response::new(Body::from_stream(stream));
            *res.status_mut() = StatusCode::OK;
            let headers = res.headers_mut();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(&mime).unwrap());
            headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            headers.insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&file_size.to_string()).unwrap(),
            );
            Ok(res)
        }
    }
}

/// Parse `bytes=START-` or `bytes=START-END`. Only single ranges.
fn parse_byte_range(value: &str) -> Option<(u64, Option<u64>)> {
    let value = value.strip_prefix("bytes=")?;
    if value.contains(',') {
        return None;
    }
    let (start, end) = value.split_once('-')?;
    if start.is_empty() {
        return None;
    }
    let start: u64 = start.parse().ok()?;
    let end = if end.is_empty() {
        None
    } else {
        Some(end.parse().ok()?)
    };
    Some((start, end))
}

#[cfg(test)]
mod tests {
    use super::parse_byte_range;

    #[test]
    fn parses_open_ended_range() {
        assert_eq!(parse_byte_range("bytes=0-"), Some((0, None)));
        assert_eq!(parse_byte_range("bytes=1024-"), Some((1024, None)));
    }

    #[test]
    fn parses_closed_range() {
        assert_eq!(parse_byte_range("bytes=0-1023"), Some((0, Some(1023))));
    }
}
