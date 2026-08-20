//! HTTP server hosting the local solx managers (`solx_manager::App::build_local()`)
//! so multiple `solx-cli`/`solx-mcp` processes can share one appdata dir
//! concurrently, proxying through `solx-client` instead of each opening
//! their own exclusive local storage (in particular, the Tantivy docs
//! index, which only tolerates one writer per process — see
//! `solx-docs/src/search.rs`).
//!
//! The surface is REST: an entity's `path`/`name` are the URL
//! (`GET /docs/research/ai/note`), `list`/`search` options are the query
//! string, and the method carries the verb. `docs/http-api.md` documents it
//! for anyone writing a client; `routes/refs.rs` explains how a URL is read
//! back into a `(path, name)` pair, and each route module's `router()`
//! records why its non-CRUD operations sit where they do.
//!
//! Also mounts `/mcp` (`routes::mcp`), the MCP Streamable HTTP transport —
//! sharing this same in-process `Arc<App>` and bearer-auth gate, so an MCP
//! client can reach solx over HTTP instead of spawning `solx-mcp` as a
//! stdio subprocess (which otherwise keeps the exe locked for the session's
//! lifetime). `solx-mcp` still exists and works over stdio for clients that
//! need a subprocess transport; this is an additional way in, not a
//! replacement.

pub mod auth;
pub mod error;
pub mod routes;
pub mod state;

use axum::middleware;
use axum::routing::get;
use axum::Router;
use tower_http::cors::CorsLayer;

use state::AppState;

/// Build the full router: an unauthenticated `/health`, plus every data
/// route behind the bearer-auth middleware.
///
/// CORS is wide open (`CorsLayer::permissive()`) so browser-based clients
/// (e.g. `solx-web`'s frontend, via `@solx/http`) can call this server
/// directly. This is a deliberate widening of the trust boundary — until
/// now only Rust/Node clients could reach `solx-server` — but it's
/// consistent with the existing model: bind is `127.0.0.1`-only (see
/// `main.rs`) and the bearer token is the only real gate, same posture
/// `solx-web`'s Bun backend already used (CORS `*`) for its own callers.
/// Layered outermost (after `.merge`) so a preflight `OPTIONS` is answered
/// before it ever reaches `require_bearer` — which now matters for ordinary
/// calls too, since `PUT`/`DELETE` are always preflighted by browsers.
pub fn build_router(state: AppState) -> Router {
    let protected = routes::router(&state)
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::require_bearer));

    Router::new()
        .route("/health", get(|| async { "ok" }))
        .merge(protected)
        .layer(CorsLayer::permissive())
        .with_state(state)
}
