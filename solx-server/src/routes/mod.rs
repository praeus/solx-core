pub mod actions;
pub mod docs;
pub mod files;
pub mod types;

use axum::Router;

use crate::state::AppState;

/// All data routes, merged into one router (auth middleware is layered on
/// by the caller — see `main.rs`).
pub fn router() -> Router<AppState> {
    Router::new()
        .merge(types::router())
        .merge(docs::router())
        .merge(actions::router())
        .merge(files::router())
}
