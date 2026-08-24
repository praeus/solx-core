//! The MCP `ServerHandler` implementation for solx (`SolxMcpServer`), plus a
//! constructor for each transport it's served over.
//!
//! `SolxMcpServer` itself is transport-agnostic — it holds only `Arc<App>`
//! and an optional path prefix. `main.rs` (the `solx-mcp` binary) serves it
//! over stdio, for MCP clients that spawn a local subprocess. `solx-server`
//! serves the same type over Streamable HTTP via [`streamable_http_service`]
//! below, mounted alongside its other routes and sharing its own in-process
//! `Arc<App>` — no subprocess, no per-client exe lock, one shared bearer-auth
//! gate. Both paths go through the identical `server`/`tools`/`error`
//! logic, so there's one source of truth for the tool catalogue regardless of
//! how a client reaches it.

pub mod error;
pub mod server;
pub mod tools;

pub use server::SolxMcpServer;

use std::sync::Arc;

use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

use solx_manager::App;

/// Build a `tower::Service` implementing MCP's Streamable HTTP transport
/// (SEP-2567), backed by `app`, for mounting into an existing HTTP router.
///
/// `path_prefix` behaves exactly as it does for the stdio server: restricts
/// the exposed tool catalogue to that path (and everything under it).
///
/// Session state is kept in-memory (`LocalSessionManager`) and DNS-rebinding
/// protection defaults to loopback-only hosts — both match `solx-server`'s
/// existing posture (single long-lived process, bound to `127.0.0.1`).
pub fn streamable_http_service(
    app: Arc<App>,
    path_prefix: Option<String>,
) -> StreamableHttpService<SolxMcpServer, LocalSessionManager> {
    StreamableHttpService::new(
        move || Ok(SolxMcpServer::new(app.clone(), path_prefix.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    )
}
