use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use solx_surface::entities::Document;
use solx_surface::managers::Solx;
use solx_surface::query::{ListOptions, Page, SearchQuery, SearchResults};
use solx_surface::wire::{PostRequest, RefRequest};

use crate::error::ApiError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/docs/post", post(post_doc))
        .route("/docs/get", post(get_doc))
        .route("/docs/delete", post(delete_doc))
        .route("/docs/list", post(list_docs))
        .route("/docs/search", post(search_docs))
}

async fn post_doc(
    State(state): State<AppState>,
    Json(req): Json<PostRequest<solx_surface::entities::DocumentInput>>,
) -> Result<Json<Document>, ApiError> {
    let doc = state.app.docs().post(&req.path, &req.name, req.input).await?;
    Ok(Json(doc))
}

async fn get_doc(
    State(state): State<AppState>,
    Json(req): Json<RefRequest>,
) -> Result<Json<Document>, ApiError> {
    let doc = state.app.docs().get(&req.path, &req.name).await?;
    Ok(Json(doc))
}

async fn delete_doc(State(state): State<AppState>, Json(req): Json<RefRequest>) -> Result<Json<()>, ApiError> {
    state.app.docs().delete(&req.path, &req.name).await?;
    Ok(Json(()))
}

async fn list_docs(
    State(state): State<AppState>,
    Json(opts): Json<ListOptions>,
) -> Result<Json<Page<Document>>, ApiError> {
    let page = state.app.docs().list(opts).await?;
    Ok(Json(page))
}

async fn search_docs(
    State(state): State<AppState>,
    Json(query): Json<SearchQuery>,
) -> Result<Json<SearchResults>, ApiError> {
    let results = state.app.docs().search(query).await?;
    Ok(Json(results))
}
