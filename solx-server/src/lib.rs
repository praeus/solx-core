//! HTTP server hosting the local solx managers (`solx_manager::App::build_local()`)
//! so multiple `solx-cli`/`solx-mcp` processes can share one appdata dir
//! concurrently, proxying through `solx-client` instead of each opening
//! their own exclusive local storage (in particular, the Tantivy docs
//! index, which only tolerates one writer per process — see
//! `solx-docs/src/search.rs`).

pub mod auth;
pub mod error;
pub mod routes;
pub mod state;

use axum::middleware;
use axum::routing::get;
use axum::Router;

use state::AppState;

/// Build the full router: an unauthenticated `/health`, plus every data
/// route behind the bearer-auth middleware.
pub fn build_router(state: AppState) -> Router {
    let protected = routes::router()
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::require_bearer));

    Router::new()
        .route("/health", get(|| async { "ok" }))
        .merge(protected)
        .with_state(state)
}
