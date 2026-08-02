//! Drives every manager trait method through `solx-client` against a real
//! `solx-server` router (bound to an ephemeral local port, in-process — no
//! subprocess spawn), asserting parity with what the underlying local `App`
//! provides directly.

use std::sync::Arc;

use serde_json::json;
use solx_client::{RemoteActionManager, RemoteDocManager, RemoteFileStore, RemoteTypeManager};
use solx_surface::entities::{ActionInput, ActionType, DocumentInput, TypeInput};
use solx_surface::error::SolxError;
use solx_surface::managers::{ActionManager, DocManager, FileStore, TypeManager};
use solx_surface::query::{ListOptions, SearchQuery};
use solx_server::state::AppState;

async fn spawn_server() -> (tempfile::TempDir, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let app = solx_manager::App::build_local_in(dir.path()).await.unwrap();
    let token = app.config.ensure_server_token().unwrap();

    let state = AppState { app, token: Arc::from(token.as_str()) };
    let router = solx_server::build_router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    (dir, format!("http://{addr}"), token)
}

#[tokio::test]
async fn types_docs_actions_files_round_trip_over_http() {
    let (_dir, base_url, token) = spawn_server().await;

    let types = RemoteTypeManager::new(base_url.clone(), token.clone());
    let docs = RemoteDocManager::new(base_url.clone(), token.clone());
    let actions = RemoteActionManager::new(base_url.clone(), token.clone());
    let files = RemoteFileStore::new(base_url.clone(), token.clone());

    // Types.
    let ty = types
        .post(
            "/types/custom",
            "Person",
            TypeInput {
                schema: Some(json!({"type": "object", "required": ["name"], "properties": {"name": {"type": "string"}}})),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(ty.path, "/types/custom");
    let fetched_ty = types.get("/types/custom", "Person").await.unwrap();
    assert_eq!(fetched_ty.name, "Person");
    let ty_page = types.list(ListOptions { path_prefix: Some("/types/custom".into()), ..Default::default() }).await.unwrap();
    assert_eq!(ty_page.total, 1);
    let resolved = types.resolve("/types/custom/Person").await.unwrap();
    assert_eq!(resolved.id, ty.id);
    types.validate(&json!({"name": "Ada"}), "/types/custom/Person").await.unwrap();
    let bad = types.validate(&json!({}), "/types/custom/Person").await.unwrap_err();
    assert!(matches!(bad, SolxError::Validation(_)));

    // Docs.
    let doc = docs
        .post(
            "/research/ai",
            "note",
            DocumentInput {
                type_ref: Some("/types/custom/Person".into()),
                contents: json!({"name": "Ada"}),
                title: Some("AI note".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(doc.path, "/research/ai");
    let fetched_doc = docs.get("/research/ai", "note").await.unwrap();
    assert_eq!(fetched_doc.contents["name"], "Ada");
    let search_results = docs.search(SearchQuery { q: Some("Ada".into()), ..Default::default() }).await.unwrap();
    assert!(search_results.total >= 1);
    let missing = docs.get("/nope", "nope").await.unwrap_err();
    assert!(matches!(missing, SolxError::NotFound(_)));
    docs.delete("/research/ai", "note").await.unwrap();
    assert!(docs.get("/research/ai", "note").await.is_err());

    // Actions (Command — exercises exec() end to end, server-side).
    // The config carries a secret key, so this also covers redaction across
    // the wire: masking happens in the server's LocalActionManager, so the
    // key must never appear in an HTTP response, while exec — which runs
    // server-side against the unmasked row — still works.
    let real_key = "c3VwZXItc2VjcmV0LWtleS1oZXJlLXBhZGRpbmc=";
    actions
        .post(
            "/tools",
            "echo",
            ActionInput {
                action_type: Some(ActionType::Command),
                fn_name: Some("echo 42".into()),
                action_config: Some(json!({ "cwd": ".", "secrets": { "API_TOKEN": real_key } })),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let result = actions.exec("/tools", "echo", json!({})).await.unwrap();
    assert!(result.success);
    assert_eq!(result.result, json!(42));

    let fetched = actions.get("/tools", "echo").await.unwrap();
    let cfg = fetched.action_config.as_ref().unwrap();
    assert_eq!(cfg["secrets"]["API_TOKEN"], json!("***"), "secret key crossed the wire");
    assert_eq!(cfg["cwd"], json!("."), "non-secret config should stay readable");

    // Round-tripping the redacted config back must not destroy the key.
    let mut edited = cfg.clone();
    edited["cwd"] = json!("..");
    actions
        .post("/tools", "echo", ActionInput { action_config: Some(edited), ..Default::default() })
        .await
        .unwrap();
    assert!(
        actions.exec("/tools", "echo", json!({})).await.unwrap().success,
        "action should still execute after a redacted round trip"
    );

    let action_page = actions.list(ListOptions { path_prefix: Some("/tools".into()), ..Default::default() }).await.unwrap();
    assert_eq!(action_page.total, 1);
    assert_eq!(
        action_page.items[0].action_config.as_ref().unwrap()["secrets"]["API_TOKEN"],
        json!("***"),
        "list must redact too"
    );
    actions.delete("/tools", "echo").await.unwrap();

    // Files.
    let stored = files.put("notes/a.txt", b"hello over http".to_vec()).await.unwrap();
    assert_eq!(stored, "notes/a.txt");
    let bytes = files.get("notes/a.txt").await.unwrap();
    assert_eq!(bytes, b"hello over http");
    let listed = files.list("notes").await.unwrap();
    assert_eq!(listed, vec!["notes/a.txt".to_string()]);
    files.delete("notes/a.txt").await.unwrap();
    assert!(files.get("notes/a.txt").await.is_err());
}

#[tokio::test]
async fn wrong_token_is_rejected() {
    let (_dir, base_url, _token) = spawn_server().await;
    let types = RemoteTypeManager::new(base_url, "wrong-token");
    let err = types.list(ListOptions::default()).await.unwrap_err();
    // A 401 with no SolxError body falls back to SolxError::Other — still
    // a hard failure, which is what matters here.
    assert!(matches!(err, SolxError::Other(_)));
}
