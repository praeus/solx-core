use std::hash::{Hash, Hasher};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde::Deserialize;
use solx_surface::managers::Solx;
use solx_surface::wire::{FileListResponse, FilePutResponse};

use crate::error::ApiError;
use crate::state::AppState;

/// File content is transferred as raw bytes, so this is a plain byte
/// ceiling rather than the base64-inflated one it used to be.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Deserialize)]
pub struct FileListQuery {
    /// Absent means "everything" — the store treats `""` as the root.
    #[serde(default)]
    prefix: Option<String>,
}

/// `/files` (list) plus `/files/{*rel_path}` (one file's bytes).
///
/// Unlike the entity routes, the capture here is used verbatim: a file's
/// `rel_path` is already a relative path with no path/name split, and
/// `solx-files` does its own normalization and traversal rejection.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/files", get(list_files))
        .route("/files/*rel_path", get(get_file).put(put_file).delete(delete_file))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
}

async fn put_file(
    State(state): State<AppState>,
    Path(rel_path): Path<String>,
    bytes: Bytes,
) -> Result<Json<FilePutResponse>, ApiError> {
    let rel_path = state.app.files().put(&rel_path, bytes.to_vec()).await?;
    Ok(Json(FilePutResponse { rel_path }))
}

/// The `FileStore` trait doesn't persist a content type (that lives on a
/// `FileRef` in document metadata), so the response type is guessed from
/// the extension and falls back to `application/octet-stream`.
///
/// ## Why this sends validators
///
/// The store is mutable and addressed by a stable path, so the bytes behind
/// `/files/widgets/foo.js` change every time that widget is rebuilt. This
/// response used to carry `Content-Type` and nothing else -- no
/// `Cache-Control`, no `ETag`, no `Last-Modified` -- which leaves a browser
/// free to reuse a stored copy on its own heuristics, with nothing in the
/// response that could reveal it had gone stale. The failure mode is
/// miserable to debug: you rebuild a widget, upload it, reload, and keep
/// running yesterday's code while every byte on the server is correct.
///
/// `no-cache` does not mean "don't store" -- it means "revalidate before
/// reusing", which is exactly right for a mutable store. The `ETag` is what
/// keeps that cheap: an unchanged file revalidates to a bodiless `304`
/// rather than resending itself.
///
/// The hash is `DefaultHasher`, not a digest crate: an ETag only has to
/// change when the content does, and nothing here is a security boundary.
async fn get_file(
    State(state): State<AppState>,
    Path(rel_path): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let bytes = state.app.files().get(&rel_path).await?;

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    let etag = format!("\"{:016x}\"", hasher.finish());

    // A conditional request lists one or more candidate tags, or `*`.
    let fresh = headers
        .get(IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|c| c.trim() == etag || c.trim() == "*"));
    if fresh {
        return Ok((
            StatusCode::NOT_MODIFIED,
            [(ETAG, etag), (CACHE_CONTROL, "no-cache".to_string())],
        )
            .into_response());
    }

    let content_type = mime_guess::from_path(&rel_path)
        .first_or_octet_stream()
        .to_string();
    Ok((
        [
            (CONTENT_TYPE, content_type),
            (ETAG, etag),
            (CACHE_CONTROL, "no-cache".to_string()),
        ],
        bytes,
    )
        .into_response())
}

async fn delete_file(
    State(state): State<AppState>,
    Path(rel_path): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.app.files().delete(&rel_path).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_files(
    State(state): State<AppState>,
    Query(query): Query<FileListQuery>,
) -> Result<Json<FileListResponse>, ApiError> {
    let paths = state.app.files().list(query.prefix.as_deref().unwrap_or("")).await?;
    Ok(Json(FileListResponse { paths }))
}
