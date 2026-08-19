//! In-process MCP client/server integration test — connects a real rmcp
//! client to `SolxMcpServer` over an in-memory duplex pipe (no subprocess
//! spawn), against an isolated temp appdata dir.

use std::sync::{Arc, Mutex};

use rmcp::model::{CallToolRequestParams, ProgressNotificationParam};
use rmcp::service::NotificationContext;
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use solx_mcp::server;
use solx_surface::entities::{ActionInput, ActionType};
use solx_surface::managers::Solx;

#[tokio::test]
async fn tools_list_and_call_tool_round_trip() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let app = solx_manager::App::build_in(dir.path()).await?;

    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move {
        server::SolxMcpServer::new(app, None)
            .serve(server_transport)
            .await
            .expect("server serve")
            .waiting()
            .await
            .expect("server waiting");
    });

    let client = ().serve(client_transport).await?;

    let tools = client.list_tools(None).await?;
    let names: Vec<&str> = tools.tools.iter().map(|t| t.name.as_ref()).collect();
    assert!(
        names.contains(&"act__builtin__document__search_documents"),
        "expected search_documents among tools, got: {names:?}"
    );
    assert!(names.contains(&"act__builtin__file__file_put"), "expected file_put among tools, got: {names:?}");
    assert!(
        !names.iter().any(|n| !n.starts_with("act__")),
        "every tool should be a dynamic action tool (no fixed CRUD layer), got: {names:?}"
    );

    // file_put -> file_get round trip through two real tool calls.
    let put = client
        .call_tool(CallToolRequestParams::new("act__builtin__file__file_put").with_arguments(
            serde_json::json!({"rel_path": "notes/a.txt", "content": "hello from mcp"})
                .as_object()
                .unwrap()
                .clone(),
        ))
        .await?;
    assert!(put.content[0].as_text().is_some());

    let got = client
        .call_tool(CallToolRequestParams::new("act__builtin__file__file_get").with_arguments(
            serde_json::json!({"rel_path": "notes/a.txt"}).as_object().unwrap().clone(),
        ))
        .await?;
    let structured = got.structured_content.expect("structured_content present");
    assert_eq!(structured.get("content").and_then(|v| v.as_str()), Some("hello from mcp"));

    // An unparseable tool name is a hard protocol error, not an in-band one.
    let err = client.call_tool(CallToolRequestParams::new("not_a_solx_tool")).await;
    assert!(err.is_err(), "expected a protocol error for an unparseable tool name");

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A `ClientHandler` that records every `notifications/progress` it
/// receives, so the test can assert on what `call_tool` streamed back while
/// a tool was still running.
#[derive(Clone, Default)]
struct ProgressCapture {
    received: Arc<Mutex<Vec<ProgressNotificationParam>>>,
}

impl ClientHandler for ProgressCapture {
    async fn on_progress(&self, params: ProgressNotificationParam, _context: NotificationContext<RoleClient>) {
        self.received.lock().unwrap().push(params);
    }
}

/// A `call_tool` invocation that attaches a `progressToken` should stream
/// each console entry the action writes back as a `notifications/progress`
/// message, on top of (not instead of) the normal final result — this is
/// the mechanism that lets a model see log lines from a long-running action
/// while it's still in flight, instead of only after the tool call returns.
#[tokio::test]
async fn call_tool_streams_console_entries_as_progress_notifications() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let app = solx_manager::App::build_in(dir.path()).await?;

    // A `Script` action with two stages — `ActionCommandRunner::run` logs
    // one console entry per stage before running it (already-built and
    // already-tested behavior; see `solx-actions/src/script.rs`), so this
    // is the simplest real action that reliably produces more than one
    // console entry per invocation without a subprocess or a wasm fixture.
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

    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move {
        server::SolxMcpServer::new(app, None)
            .serve(server_transport)
            .await
            .expect("server serve")
            .waiting()
            .await
            .expect("server waiting");
    });

    let capture = ProgressCapture::default();
    let client = capture.clone().serve(client_transport).await?;

    // `rmcp`'s own `Peer::send_request` (which `call_tool` calls into)
    // unconditionally mints and attaches its own progress token to every
    // outgoing request — see `service.rs`'s `send_request_with_option_and_
    // subscription`, which overwrites whatever `set_progress_token` put
    // here. That's a client-crate default, not something every real MCP
    // client does, but it does mean this test can't control *which* token
    // value arrives — only that one arrives, and that it's used
    // consistently across every notification for this call.
    let result = client.call_tool(CallToolRequestParams::new("act__tools__count")).await?;
    assert!(!result.is_error.unwrap_or(false), "{result:?}");

    // `notify_progress` hands off to the transport's own send loop rather
    // than flushing synchronously, so give the in-memory pipe a moment to
    // deliver before asserting — same allowance rmcp's own progress test
    // (`tests/test_progress_subscriber.rs`) makes.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let received = capture.received.lock().unwrap().clone();
    assert!(!received.is_empty(), "expected at least one progress notification");
    let token = received[0].progress_token.clone();
    assert!(received.iter().all(|p| p.progress_token == token), "{received:?}");
    let messages: Vec<String> = received.iter().filter_map(|p| p.message.clone()).collect();
    assert!(
        messages.iter().any(|m| m.contains("exec /builtin/random_string")),
        "expected a progress message naming the first stage, got: {messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("exec /builtin/action/entity_list_actions")),
        "expected a progress message naming the second stage, got: {messages:?}"
    );
    // Progress must be non-decreasing per the MCP spec — using the
    // console's own monotonic `seq` as the progress value gets this for
    // free rather than needing a separate counter.
    let progress_values: Vec<f64> = received.iter().map(|p| p.progress).collect();
    assert!(
        progress_values.windows(2).all(|w| w[0] <= w[1]),
        "progress must be non-decreasing: {progress_values:?}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A server constructed with a `path_prefix` only exposes tools for actions
/// under that path (and everything under it) — the mechanism a client uses
/// to run several narrower `solx-mcp` instances instead of always seeing the
/// full action catalogue. See `docs/next-steps.md` §5.
#[tokio::test]
async fn list_tools_respects_path_prefix() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let app = solx_manager::App::build_in(dir.path()).await?;

    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move {
        server::SolxMcpServer::new(app, Some("/builtin/console".to_string()))
            .serve(server_transport)
            .await
            .expect("server serve")
            .waiting()
            .await
            .expect("server waiting");
    });

    let client = ().serve(client_transport).await?;
    let tools = client.list_tools(None).await?;
    let names: Vec<&str> = tools.tools.iter().map(|t| t.name.as_ref()).collect();

    assert_eq!(
        names.len(),
        5,
        "expected only the 5 /builtin/console actions, got: {names:?}"
    );
    assert!(names.iter().all(|n| n.starts_with("act__builtin__console__")), "{names:?}");
    assert!(
        !names.contains(&"act__builtin__file__file_put"),
        "a /builtin-root action should not appear when scoped to /builtin/console, got: {names:?}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}
