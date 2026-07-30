//! In-process MCP client/server integration test — connects a real rmcp
//! client to `SolxMcpServer` over an in-memory duplex pipe (no subprocess
//! spawn), against an isolated temp appdata dir.

#[path = "../src/error.rs"]
mod error;
#[path = "../src/schema.rs"]
mod schema;
#[path = "../src/server.rs"]
mod server;
#[path = "../src/tools.rs"]
mod tools;

use rmcp::model::CallToolRequestParams;
use rmcp::ServiceExt;

#[tokio::test]
async fn tools_list_and_call_tool_round_trip() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let app = solx_manager::App::build_in(dir.path()).await?;

    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move {
        server::SolxMcpServer::new(app)
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
        names.contains(&"act__builtin__search_documents"),
        "expected search_documents among tools, got: {names:?}"
    );
    assert!(names.contains(&"act__builtin__file_put"), "expected file_put among tools, got: {names:?}");
    assert!(
        !names.iter().any(|n| !n.starts_with("act__")),
        "every tool should be a dynamic action tool (no fixed CRUD layer), got: {names:?}"
    );

    // file_put -> file_get round trip through two real tool calls.
    let put = client
        .call_tool(CallToolRequestParams::new("act__builtin__file_put").with_arguments(
            serde_json::json!({"rel_path": "notes/a.txt", "content": "hello from mcp"})
                .as_object()
                .unwrap()
                .clone(),
        ))
        .await?;
    assert!(put.content[0].as_text().is_some());

    let got = client
        .call_tool(CallToolRequestParams::new("act__builtin__file_get").with_arguments(
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
