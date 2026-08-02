use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use solx_surface::entities::TypeEntity;
use solx_surface::managers::Solx;
use solx_surface::query::{ListOptions, Page};
use solx_surface::wire::{PostRequest, RefRequest, ResolveRequest, ValidateRequest};

use crate::error::ApiError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/types/post", post(post_type))
        .route("/types/get", post(get_type))
        .route("/types/delete", post(delete_type))
        .route("/types/list", post(list_types))
        .route("/types/resolve", post(resolve_type))
        .route("/types/validate", post(validate_type))
}

async fn post_type(
    State(state): State<AppState>,
    Json(req): Json<PostRequest<solx_surface::entities::TypeInput>>,
) -> Result<Json<TypeEntity>, ApiError> {
    let entity = state.app.types().post(&req.path, &req.name, req.input).await?;
    Ok(Json(entity))
}

async fn get_type(
    State(state): State<AppState>,
    Json(req): Json<RefRequest>,
) -> Result<Json<TypeEntity>, ApiError> {
    let entity = state.app.types().get(&req.path, &req.name).await?;
    Ok(Json(entity))
}

async fn delete_type(State(state): State<AppState>, Json(req): Json<RefRequest>) -> Result<Json<()>, ApiError> {
    state.app.types().delete(&req.path, &req.name).await?;
    Ok(Json(()))
}

async fn list_types(
    State(state): State<AppState>,
    Json(opts): Json<ListOptions>,
) -> Result<Json<Page<TypeEntity>>, ApiError> {
    let page = state.app.types().list(opts).await?;
    Ok(Json(page))
}

async fn resolve_type(
    State(state): State<AppState>,
    Json(req): Json<ResolveRequest>,
) -> Result<Json<TypeEntity>, ApiError> {
    let entity = state.app.types().resolve(&req.type_ref).await?;
    Ok(Json(entity))
}

async fn validate_type(
    State(state): State<AppState>,
    Json(req): Json<ValidateRequest>,
) -> Result<Json<()>, ApiError> {
    state.app.types().validate(&req.value, &req.type_ref).await?;
    Ok(Json(()))
}
