//! HTTP server hosting the local solx managers (`solx_manager::App::build_local()`)
//! so multiple `solx-cli`/`solx-mcp` processes can share one appdata dir
//! concurrently, proxying through `solx-client` instead of each opening
//! their own local libsql/SQLite storage directly.
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

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

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

/// Bind `127.0.0.1:port` and start serving in the background. Returns once
/// the listener is bound; the spawned task keeps running until the process
/// exits. For a process (like `solx-cli`) that wants a real HTTP surface
/// available to child processes it spawns, without itself being
/// `solx-server` — see `solx-cli`'s `build_app` for the caller.
pub async fn spawn_embedded(state: AppState, port: u16) -> std::io::Result<SocketAddr> {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    let router = build_router(state);
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            tracing::warn!("embedded solx-server on {addr} exited: {e}");
        }
    });
    Ok(bound)
}
