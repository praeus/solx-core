use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::StatusCode;
use axum::routing::get;
use axum::response::IntoResponse;
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
async fn get_file(
    State(state): State<AppState>,
    Path(rel_path): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let bytes = state.app.files().get(&rel_path).await?;
    let content_type = mime_guess::from_path(&rel_path)
        .first_or_octet_stream()
        .to_string();
    Ok(([(CONTENT_TYPE, content_type)], bytes))
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
