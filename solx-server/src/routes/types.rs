use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use solx_surface::entities::{TypeEntity, TypeInput};
use solx_surface::managers::Solx;
use solx_surface::query::{ListOptions, Page};
use solx_surface::wire::ValidateRequest;

use crate::error::ApiError;
use crate::routes::refs::split_url_ref;
use crate::state::AppState;

/// `/types` (the collection) plus `/types/{*ref}` (one type), and the
/// top-level `/validate`.
///
/// There is no `resolve` route: `TypeManager::resolve` is defined as
/// `split_ref` + `get` (solx-types/src/lib.rs), which is exactly what
/// `GET /types/{*ref}` does. `RemoteTypeManager` implements the trait
/// method client-side against this same route.
///
/// Validation is a top-level `/validate` for the same reason search is —
/// see [`super::docs::router`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/types", get(list_types))
        .route("/types/*ref", get(get_type).put(save_type).delete(delete_type))
        .route("/validate", post(validate_type))
}

async fn save_type(
    State(state): State<AppState>,
    Path(reference): Path<String>,
    Json(input): Json<TypeInput>,
) -> Result<Json<TypeEntity>, ApiError> {
    let (path, name) = split_url_ref(&reference)?;
    let entity = state.app.types().save(&path, &name, input).await?;
    Ok(Json(entity))
}

async fn get_type(
    State(state): State<AppState>,
    Path(reference): Path<String>,
) -> Result<Json<TypeEntity>, ApiError> {
    let (path, name) = split_url_ref(&reference)?;
    let entity = state.app.types().get(&path, &name).await?;
    Ok(Json(entity))
}

async fn delete_type(
    State(state): State<AppState>,
    Path(reference): Path<String>,
) -> Result<StatusCode, ApiError> {
    let (path, name) = split_url_ref(&reference)?;
    state.app.types().delete(&path, &name).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_types(
    State(state): State<AppState>,
    Query(opts): Query<ListOptions>,
) -> Result<Json<Page<TypeEntity>>, ApiError> {
    let page = state.app.types().list(opts).await?;
    Ok(Json(page))
}

async fn validate_type(
    State(state): State<AppState>,
    Json(req): Json<ValidateRequest>,
) -> Result<StatusCode, ApiError> {
    state.app.types().validate(&req.value, &req.type_ref).await?;
    Ok(StatusCode::NO_CONTENT)
}
