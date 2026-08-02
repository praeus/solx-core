use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

use crate::state::AppState;

/// Bearer-token middleware: checks `Authorization: Bearer <token>` against
/// `state.token`. `127.0.0.1`-only binding (enforced in `main.rs`) plus this
/// shared secret is the entire auth model for this first pass — matching
/// the trust model everywhere else in solx today (no multi-user/permissions
/// system exists elsewhere either).
pub async fn require_bearer(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let provided = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match provided {
        Some(token) if token == &*state.token => Ok(next.run(request).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}
