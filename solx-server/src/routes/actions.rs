use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use solx_surface::entities::{Action, ActionExecResult};
use solx_surface::managers::Solx;
use solx_surface::query::{ListOptions, Page};
use solx_surface::wire::{ExecRequest, RefRequest, SaveRequest};

use crate::error::ApiError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/actions/save", post(save_action))
        .route("/actions/get", post(get_action))
        .route("/actions/delete", post(delete_action))
        .route("/actions/list", post(list_actions))
        .route("/actions/exec", post(exec_action))
}

async fn save_action(
    State(state): State<AppState>,
    Json(req): Json<SaveRequest<solx_surface::entities::ActionInput>>,
) -> Result<Json<Action>, ApiError> {
    let action = state.app.actions().save(&req.path, &req.name, req.input).await?;
    Ok(Json(action))
}

async fn get_action(
    State(state): State<AppState>,
    Json(req): Json<RefRequest>,
) -> Result<Json<Action>, ApiError> {
    let action = state.app.actions().get(&req.path, &req.name).await?;
    Ok(Json(action))
}

async fn delete_action(State(state): State<AppState>, Json(req): Json<RefRequest>) -> Result<Json<()>, ApiError> {
    state.app.actions().delete(&req.path, &req.name).await?;
    Ok(Json(()))
}

async fn list_actions(
    State(state): State<AppState>,
    Json(opts): Json<ListOptions>,
) -> Result<Json<Page<Action>>, ApiError> {
    let page = state.app.actions().list(opts).await?;
    Ok(Json(page))
}

async fn exec_action(
    State(state): State<AppState>,
    Json(req): Json<ExecRequest>,
) -> Result<Json<ActionExecResult>, ApiError> {
    let result = state.app.actions().exec(&req.path, &req.name, req.params).await?;
    Ok(Json(result))
}
