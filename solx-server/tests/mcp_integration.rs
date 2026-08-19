//! MCP-over-HTTP integration test — connects a real `rmcp` Streamable HTTP
//! client to solx-server's `/mcp` route (bound to an ephemeral local port,
//! in-process — no subprocess spawn), exercising the same tool round-trip
//! and progress-notification behavior `solx-mcp`'s own stdio integration
//! test (`solx-mcp/tests/mcp_integration.rs`) covers for the stdio
//! transport, proving `SolxMcpServer` behaves identically either way — and
//! that `/mcp` is gated by the same bearer-auth middleware as every other
//! route.

use std::sync::{Arc, Mutex};

use rmcp::model::{CallToolRequestParams, ProgressNotificationParam};
use rmcp::service::NotificationContext;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use solx_server::state::AppState;
use solx_surface::entities::{ActionInput, ActionType};
use solx_surface::managers::Solx;

async fn spawn_server() -> (tempfile::TempDir, Arc<solx_manager::App>, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let app = solx_manager::App::build_local_in(dir.path()).await.unwrap();
    let token = app.config.ensure_server_token().unwrap();

    let state = AppState { app: app.clone(), token: Arc::from(token.as_str()) };
    let router = solx_server::build_router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    (dir, app, format!("http://{addr}/mcp"), token)
}

// Returns a config, not a built transport: `rmcp`'s reqwest transport pins
// its own `reqwest` dependency version, separate from the workspace's, so
// `StreamableHttpClientTransport<reqwest::Client>` isn't a type this crate
// can name. `StreamableHttpClientTransport::from_config(..)` builds rmcp's
// internal client itself — call it inline at each `.serve(..)` call site so
// type inference resolves it there instead.
fn mcp_client_config(uri: String, auth_header: Option<String>) -> StreamableHttpClientTransportConfig {
    let mut config = StreamableHttpClientTransportConfig::with_uri(uri);
    config.auth_header = auth_header;
    config
}

#[tokio::test]
async fn tools_list_and_call_tool_round_trip_over_http() -> anyhow::Result<()> {
    let (_dir, _app, mcp_url, token) = spawn_server().await;

    let client = ().serve(StreamableHttpClientTransport::from_config(mcp_client_config(mcp_url, Some(token)))).await?;

    let tools = client.list_tools(None).await?;
    let names: Vec<&str> = tools.tools.iter().map(|t| t.name.as_ref()).collect();
    assert!(
        names.iter().any(|n| n.starts_with("act__builtin")),
        "expected builtin action tools among the MCP catalogue, got: {names:?}"
    );

    // Owned (not `&str` borrowed from `tools`): `CallToolRequestParams::new`
    // takes `impl Into<Cow<'static, str>>`, which a borrow scoped to this
    // function can't satisfy.
    let put_name = names.iter().find(|n| n.contains("file_put")).expect("a file_put tool should exist").to_string();
    let get_name = names.iter().find(|n| n.contains("file_get")).expect("a file_get tool should exist").to_string();

    let put = client
        .call_tool(CallToolRequestParams::new(put_name).with_arguments(
            serde_json::json!({"rel_path": "notes/a.txt", "content": "hello over mcp http"})
                .as_object()
                .unwrap()
                .clone(),
        ))
        .await?;
    assert!(put.content[0].as_text().is_some());

    let got = client
        .call_tool(CallToolRequestParams::new(get_name).with_arguments(
            serde_json::json!({"rel_path": "notes/a.txt"}).as_object().unwrap().clone(),
        ))
        .await?;
    let structured = got.structured_content.expect("structured_content present");
    assert_eq!(structured.get("content").and_then(|v| v.as_str()), Some("hello over mcp http"));

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn mcp_route_rejects_missing_or_wrong_token() -> anyhow::Result<()> {
    let (_dir, _app, mcp_url, _token) = spawn_server().await;

    let err = ().serve(StreamableHttpClientTransport::from_config(mcp_client_config(mcp_url, Some("wrong-token".to_string())))).await;
    assert!(err.is_err(), "expected initialize over /mcp to fail with a wrong bearer token");
    Ok(())
}

/// A `ClientHandler` that records every `notifications/progress` it
/// receives, mirroring `solx-mcp/tests/mcp_integration.rs`'s `ProgressCapture`.
#[derive(Clone, Default)]
struct ProgressCapture {
    received: Arc<Mutex<Vec<ProgressNotificationParam>>>,
}

impl ClientHandler for ProgressCapture {
    async fn on_progress(&self, params: ProgressNotificationParam, _context: NotificationContext<RoleClient>) {
        self.received.lock().unwrap().push(params);
    }
}

/// Same case as `solx-mcp`'s stdio integration test — a `call_tool` with a
/// `progressToken` streams console entries back as `notifications/progress`
/// while the action is still running — proving the SSE half of the
/// Streamable HTTP transport carries server-initiated notifications, not
/// just request/response.
#[tokio::test]
async fn call_tool_streams_console_entries_as_progress_notifications_over_http() -> anyhow::Result<()> {
    let (_dir, app, mcp_url, token) = spawn_server().await;

    app.files()
        .put(
            &solx_files::shared_action_file_path("count.solx"),
            b"exec /builtin/random_string; exec /builtin/action/entity_list_actions".to_vec(),
        )
        .await?;
    app.actions()
        .save(
            "/tools",
            "count",
            ActionInput {
                action_type: Some(ActionType::Script),
                bin_name: Some("count.solx".into()),
                ..Default::default()
            },
        )
        .await?;

    let capture = ProgressCapture::default();
    let client = capture.clone().serve(StreamableHttpClientTransport::from_config(mcp_client_config(mcp_url, Some(token)))).await?;

    let result = client.call_tool(CallToolRequestParams::new("act__tools__count")).await?;
    assert!(!result.is_error.unwrap_or(false), "{result:?}");

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let received = capture.received.lock().unwrap().clone();
    assert!(!received.is_empty(), "expected at least one progress notification over HTTP/SSE");
    let messages: Vec<String> = received.iter().filter_map(|p| p.message.clone()).collect();
    assert!(
        messages.iter().any(|m| m.contains("exec /builtin/random_string")),
        "expected a progress message naming the first stage, got: {messages:?}"
    );

    client.cancel().await?;
    Ok(())
}
