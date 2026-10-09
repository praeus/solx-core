use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use solx_server::state::AppState;

#[derive(Parser)]
#[command(name = "solx-server", about = "HTTP server hosting the local solx managers", version)]
struct Cli {
    /// Address to bind on. Defaults to config's `server_host`, else
    /// `127.0.0.1`. Use `0.0.0.0` (or a specific interface IP) to serve
    /// other machines.
    #[arg(long)]
    host: Option<IpAddr>,
    /// Port to bind on. Defaults to config's `server_port`, else
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

    // A tokio::spawn'd task (what `action-start` uses) isn't cancelled when
    // an axum handler future drops, so detachment survives client
    // disconnect here — unlike solx-cli, which exits the instant `exec`
    // returns and would kill a just-started invocation before it could be
    // polled. See `docs/async-actions-plan.md` §6.
    solx_actions::set_long_lived_host(true);

    // Always local — must never proxy to itself, even if this appdata
    // dir's config happens to contain a stray `server_url`.
    let app = solx_manager::App::build_local().await.context("build local App")?;

    let token = app.config.ensure_server_token().context("ensure server token")?;

    let snap = app.config.snapshot();
    let host = match cli.host {
        Some(host) => host,
        None => match snap.server_host.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) => s
                .parse::<IpAddr>()
                .with_context(|| format!("config server_host {s:?} is not an IP address"))?,
            None => IpAddr::V4(Ipv4Addr::LOCALHOST),
        },
    };
    let port = cli.port.or(snap.server_port).unwrap_or(solx_config::DEFAULT_SERVER_PORT);
    let addr = SocketAddr::new(host, port);

    let cors = solx_server::cors_layer(host, snap.server_cors_origins.as_deref().unwrap_or_default());
    let state = AppState { app, token: Arc::from(token.as_str()) };
    let router = solx_server::build_router_with_cors(state, cors);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    tracing::info!("solx-server listening on http://{addr}");
    println!("solx-server listening on http://{addr}");
    if !host.is_loopback() {
        // Off loopback, anyone who can reach the port can attempt auth, and
        // the bearer token travels in cleartext.
        let warning = "solx-server is reachable from the network: the bearer token is the \
                       only gate and traffic is unencrypted HTTP. Use a private network \
                       (e.g. Tailscale/WireGuard) or a TLS reverse proxy.";
        tracing::warn!("{warning}");
        eprintln!("WARNING: {warning}");
    }

    axum::serve(listener, router).await.context("serve")?;
    Ok(())
}
