use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};
use serde::Deserialize;
use solx_surface::entities::{Action, ActionExecResult, ActionInput};
use solx_surface::managers::Solx;
use solx_surface::query::{ActionSearchQuery, ListOptions, Page};

use crate::error::ApiError;
use crate::routes::refs::split_url_ref;
use crate::state::AppState;

/// `/actions` (the collection) plus `/actions/{*ref}` (one action), and the
/// top-level `/actions-search`.
///
/// Execution is `POST` on the action's own URL, with the params as the
/// body — RFC 9110's "resource-specific processing of the request
/// payload". Unlike a static `/actions/exec/...` segment, this shares the
/// exact path pattern of the CRUD routes and so can't shadow an action
/// stored at any particular reference.
///
/// Search lives at the top-level `/actions-search` rather than
/// `/actions/search`, for the same reason document search lives at `/search`
/// rather than `/docs/search` — see `super::docs::router`.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/actions", get(list_actions))
        .route(
            "/actions/*ref",
            get(get_action).put(save_action).delete(delete_action).post(exec_action),
        )
        .route("/actions-search", get(search_actions))
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

/// Split across two `Query` extractors rather than one
/// `Query<ActionSearchQuery>`: axum's `Query` (via `serde_urlencoded`) can't
/// deserialize a `#[serde(flatten)]`ed field — numeric fields like `limit`
/// come back as strings and fail with "invalid type: string, expected
/// usize". Each extractor re-parses the same query string into its own
/// flat (non-nested) struct, which `serde_urlencoded` handles fine.
async fn search_actions(
    State(state): State<AppState>,
    Query(list): Query<ListOptions>,
    Query(SearchTerm { q, exclude_hidden }): Query<SearchTerm>,
) -> Result<Json<Page<Action>>, ApiError> {
    let page = state
        .app
        .actions()
        .search(ActionSearchQuery { list, q, exclude_hidden })
        .await?;
    Ok(Json(page))
}

#[derive(Deserialize)]
struct SearchTerm {
    /// `?exclude_hidden=true` drops actions hidden from model-facing
    /// catalogues. Off by default, like every other caller of `search`.
    #[serde(default)]
    exclude_hidden: bool,
    #[serde(default)]
    q: Option<String>,
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
