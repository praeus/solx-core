//! Mounts the MCP Streamable HTTP transport at `/mcp`, sharing this
//! process's own `Arc<App>` — no HTTP round-trip through `solx-client` the
//! way a remote-mode `solx-mcp` (stdio) client would otherwise need.
//!
//! This is the same `SolxMcpServer` the `solx-mcp` binary serves over stdio
//! (`solx-mcp/src/server.rs`); only the transport differs. Gated by the same
//! bearer-auth middleware as every other route here — see `build_router` in
//! `crate::lib`.

use std::sync::Arc;

use axum::Router;
use solx_manager::App;

use crate::state::AppState;

pub fn router(app: Arc<App>) -> Router<AppState> {
    // `path_prefix: None` — exposes the full tool catalogue. Scoped routes
    // (e.g. `/mcp/builtin`) can be added later if a client needs a narrower
    // one; nothing depends on that today (see docs/next-steps.md §5).
    Router::new().route_service("/mcp", solx_mcp::streamable_http_service(app, None))
}
