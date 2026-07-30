//! `solx-mcp` — an MCP server exposing solx documents, types, files, and
//! actions to LLM clients over stdio.
//!
//! stdout is the JSON-RPC channel; nothing but protocol frames may be
//! written to it. All logging goes to stderr, and the subscriber is
//! installed before anything else runs (including `App::build()`), so even
//! startup wiring errors are safely off stdout.

mod error;
mod schema;
mod server;
mod tools;

use rmcp::transport::stdio;
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let app = solx_manager::App::build().await?;
    let service = server::SolxMcpServer::new(app)
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!("serve error: {e:?}"))?;
    service.waiting().await?;
    Ok(())
}
