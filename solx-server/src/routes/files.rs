use axum::extract::{DefaultBodyLimit, State};
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine as _;
use solx_surface::error::SolxError;
use solx_surface::managers::Solx;
use solx_surface::wire::{FileDeleteRequest, FileGetRequest, FileGetResponse, FileListRequest, FileListResponse, FilePutRequest, FilePutResponse};

use crate::error::ApiError;
use crate::state::AppState;

/// Base64-encoded bytes are ~33% larger than raw; raise the default 2 MiB
/// body-size limit for this router specifically.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/files/put", post(put_file))
        .route("/files/get", post(get_file))
        .route("/files/delete", post(delete_file))
        .route("/files/list", post(list_files))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
}

async fn put_file(
    State(state): State<AppState>,
    Json(req): Json<FilePutRequest>,
) -> Result<Json<FilePutResponse>, ApiError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&req.bytes_b64)
        .map_err(|e| ApiError(SolxError::Invalid(format!("invalid base64 content: {e}"))))?;
    let rel_path = state.app.files().put(&req.rel_path, bytes).await?;
    Ok(Json(FilePutResponse { rel_path }))
}

async fn get_file(
    State(state): State<AppState>,
    Json(req): Json<FileGetRequest>,
) -> Result<Json<FileGetResponse>, ApiError> {
    let bytes = state.app.files().get(&req.rel_path).await?;
    Ok(Json(FileGetResponse {
        bytes_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
    }))
}

async fn delete_file(State(state): State<AppState>, Json(req): Json<FileDeleteRequest>) -> Result<Json<()>, ApiError> {
    state.app.files().delete(&req.rel_path).await?;
    Ok(Json(()))
}

async fn list_files(
    State(state): State<AppState>,
    Json(req): Json<FileListRequest>,
) -> Result<Json<FileListResponse>, ApiError> {
    let paths = state.app.files().list(&req.prefix).await?;
    Ok(Json(FileListResponse { paths }))
}
