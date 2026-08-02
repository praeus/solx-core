use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use solx_server::state::AppState;

#[derive(Parser)]
#[command(name = "solx-server", about = "HTTP server hosting the local solx managers", version)]
struct Cli {
    /// Port to bind on 127.0.0.1. Defaults to config's `server_port`, else
    /// `solx_config::DEFAULT_SERVER_PORT`.
    #[arg(long)]
    port: Option<u16>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    // Always local — must never proxy to itself, even if this appdata
    // dir's config happens to contain a stray `server_url`.
    let app = solx_manager::App::build_local().await.context("build local App")?;

    let token = app.config.ensure_server_token().context("ensure server token")?;

    let port = cli
        .port
        .or(app.config.snapshot().server_port)
        .unwrap_or(solx_config::DEFAULT_SERVER_PORT);
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);

    let state = AppState { app, token: Arc::from(token.as_str()) };
    let router = solx_server::build_router(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    tracing::info!("solx-server listening on http://{addr}");
    println!("solx-server listening on http://{addr}");

    axum::serve(listener, router).await.context("serve")?;
    Ok(())
}
