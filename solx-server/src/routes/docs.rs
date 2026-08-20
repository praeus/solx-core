use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use solx_surface::entities::{Document, DocumentInput};
use solx_surface::managers::Solx;
use solx_surface::query::{ListOptions, Page, SearchQuery, SearchResults};

use crate::error::ApiError;
use crate::routes::refs::split_url_ref;
use crate::state::AppState;

/// `/docs` (the collection) plus `/docs/{*ref}` (one document), and the
/// top-level `/search`.
///
/// Search lives at `/search` rather than `/docs/search` because a static
/// segment beside a catch-all wins the match for *every* method it is
/// registered under and 405s the rest — so `/docs/search` would make a
/// document named `search` at the root unreachable.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/docs", get(list_docs))
        .route("/docs/*ref", get(get_doc).put(save_doc).delete(delete_doc))
        .route("/search", get(search_docs))
}

async fn save_doc(
    State(state): State<AppState>,
    Path(reference): Path<String>,
    Json(input): Json<DocumentInput>,
) -> Result<Json<Document>, ApiError> {
    let (path, name) = split_url_ref(&reference)?;
    let doc = state.app.docs().save(&path, &name, input).await?;
    Ok(Json(doc))
}

async fn get_doc(
    State(state): State<AppState>,
    Path(reference): Path<String>,
) -> Result<Json<Document>, ApiError> {
    let (path, name) = split_url_ref(&reference)?;
    let doc = state.app.docs().get(&path, &name).await?;
    Ok(Json(doc))
}

async fn delete_doc(
    State(state): State<AppState>,
    Path(reference): Path<String>,
) -> Result<StatusCode, ApiError> {
    let (path, name) = split_url_ref(&reference)?;
    state.app.docs().delete(&path, &name).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_docs(
    State(state): State<AppState>,
    Query(opts): Query<ListOptions>,
) -> Result<Json<Page<Document>>, ApiError> {
    let page = state.app.docs().list(opts).await?;
    Ok(Json(page))
}

async fn search_docs(
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<SearchResults>, ApiError> {
    let results = state.app.docs().search(query).await?;
    Ok(Json(results))
}
