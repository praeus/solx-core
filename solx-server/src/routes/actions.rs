use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};
use solx_surface::entities::{Action, ActionExecResult, ActionInput};
use solx_surface::managers::Solx;
use solx_surface::query::{ListOptions, Page};

use crate::error::ApiError;
use crate::routes::refs::split_url_ref;
use crate::state::AppState;

/// `/actions` (the collection) plus `/actions/{*ref}` (one action).
///
/// Execution is `POST` on the action's own URL, with the params as the
/// body — RFC 9110's "resource-specific processing of the request
/// payload". Unlike a static `/actions/exec/...` segment, this shares the
/// exact path pattern of the CRUD routes and so can't shadow an action
/// stored at any particular reference.
pub fn router() -> Router<AppState> {
    Router::new().route("/actions", get(list_actions)).route(
        "/actions/*ref",
        get(get_action).put(save_action).delete(delete_action).post(exec_action),
    )
}

async fn save_action(
    State(state): State<AppState>,
    Path(reference): Path<String>,
    Json(input): Json<ActionInput>,
) -> Result<Json<Action>, ApiError> {
    let (path, name) = split_url_ref(&reference)?;
    let action = state.app.actions().save(&path, &name, input).await?;
    Ok(Json(action))
}

async fn get_action(
    State(state): State<AppState>,
    Path(reference): Path<String>,
) -> Result<Json<Action>, ApiError> {
    let (path, name) = split_url_ref(&reference)?;
    let action = state.app.actions().get(&path, &name).await?;
    Ok(Json(action))
}

async fn delete_action(
    State(state): State<AppState>,
    Path(reference): Path<String>,
) -> Result<StatusCode, ApiError> {
    let (path, name) = split_url_ref(&reference)?;
    state.app.actions().delete(&path, &name).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_actions(
    State(state): State<AppState>,
    Query(opts): Query<ListOptions>,
) -> Result<Json<Page<Action>>, ApiError> {
    let page = state.app.actions().list(opts).await?;
    Ok(Json(page))
}

/// Execute the action. The body is optional so a parameterless action can
/// be invoked with an empty `POST` (no `Content-Type` needed) rather than
/// forcing callers to send a literal `{}`.
async fn exec_action(
    State(state): State<AppState>,
    Path(reference): Path<String>,
    body: Option<Json<Value>>,
) -> Result<Json<ActionExecResult>, ApiError> {
    let (path, name) = split_url_ref(&reference)?;
    let params = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let result = state.app.actions().exec(&path, &name, params).await?;
    Ok(Json(result))
}
