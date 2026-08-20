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

async fn spawn_server() -> (tempfile::TempDir, Arc<solx_config::ConfigService>, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let app = solx_manager::App::build_local_in(dir.path()).await.unwrap();
    let cfg = app.config.clone();
    let token = cfg.ensure_server_token().unwrap();

    let state = AppState { app, token: Arc::from(token.as_str()) };
    let router = solx_server::build_router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    (dir, cfg, format!("http://{addr}"), token)
}

#[tokio::test]
async fn types_docs_actions_files_round_trip_over_http() {
    let (_dir, cfg, base_url, token) = spawn_server().await;
    cfg.register_command(
        "echo-42",
        solx_config::CommandDef { command: "echo 42".into(), description: None, cwd: None },
    )
    .unwrap();

    let types = RemoteTypeManager::new(base_url.clone(), token.clone());
    let docs = RemoteDocManager::new(base_url.clone(), token.clone());
    let actions = RemoteActionManager::new(base_url.clone(), token.clone());
    let files = RemoteFileStore::new(base_url.clone(), token.clone());

    // Types.
    let ty = types
        .save(
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
        .save(
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
        .save(
            "/tools",
            "echo",
            ActionInput {
                action_type: Some(ActionType::Command),
                fn_name: Some("echo-42".into()),
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
        .save("/tools", "echo", ActionInput { action_config: Some(edited), ..Default::default() })
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

/// Exercises the REST surface directly, covering what a typed
/// `Remote*Manager` call can't express: how a reference is laid out in the
/// URL, query-string list options, `204` on delete, and the `Content-Type`
/// on a raw file download. This is the contract a hand-written client sees.
#[tokio::test]
async fn rest_surface_over_raw_http() {
    /// Seeded permissive document type — `type_ref` is required on create.
    const DOC_TYPE: &str = "/types/docs/Document";

    let (_dir, _cfg, base_url, token) = spawn_server().await;
    let http = reqwest::Client::new();
    let auth = |rb: reqwest::RequestBuilder| rb.bearer_auth(&token);

    // PUT to a nested reference, then GET the same URL back.
    let created = auth(http.put(format!("{base_url}/docs/research/ai/note")))
        .json(&json!({ "type_ref": DOC_TYPE, "contents": { "k": "v" }, "title": "Nested" }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 200);
    let body: serde_json::Value = created.json().await.unwrap();
    assert_eq!(body["path"], "/research/ai");
    assert_eq!(body["name"], "note");

    let fetched = auth(http.get(format!("{base_url}/docs/research/ai/note"))).send().await.unwrap();
    assert_eq!(fetched.status(), 200);
    assert_eq!(fetched.json::<serde_json::Value>().await.unwrap()["title"], "Nested");

    // A root-level entity is just one segment: `/docs/{name}`.
    auth(http.put(format!("{base_url}/docs/rootnote")))
        .json(&json!({ "type_ref": DOC_TYPE, "contents": {} }))
        .send()
        .await
        .unwrap();
    let root: serde_json::Value =
        auth(http.get(format!("{base_url}/docs/rootnote"))).send().await.unwrap().json().await.unwrap();
    assert_eq!(root["path"], "/", "a single segment means the root path");
    assert_eq!(root["name"], "rootnote");

    // A name needing percent-encoding survives the round trip intact.
    auth(http.put(format!("{base_url}/docs/notes/100%25%20%231")))
        .json(&json!({ "type_ref": DOC_TYPE, "contents": {} }))
        .send()
        .await
        .unwrap();
    let encoded: serde_json::Value = auth(http.get(format!("{base_url}/docs/notes/100%25%20%231")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(encoded["name"], "100% #1");

    // List options ride in the query string.
    let listed: serde_json::Value = auth(http.get(format!("{base_url}/docs")))
        .query(&[("path_prefix", "/research"), ("limit", "10")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["total"], 1, "path_prefix should exclude the root-level docs");

    // Search is top-level, so a doc named `search` at the root stays reachable.
    auth(http.put(format!("{base_url}/docs/search")))
        .json(&json!({ "type_ref": DOC_TYPE, "contents": {}, "title": "Not the search route" }))
        .send()
        .await
        .unwrap();
    let shadowed = auth(http.get(format!("{base_url}/docs/search"))).send().await.unwrap();
    assert_eq!(shadowed.status(), 200);
    assert_eq!(
        shadowed.json::<serde_json::Value>().await.unwrap()["title"],
        "Not the search route"
    );
    let hits: serde_json::Value = auth(http.get(format!("{base_url}/search")))
        .query(&[("q", "Nested")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(hits["total"].as_u64().unwrap() >= 1);

    // Delete answers 204 with an empty body.
    let deleted = auth(http.delete(format!("{base_url}/docs/rootnote"))).send().await.unwrap();
    assert_eq!(deleted.status(), 204);
    assert!(deleted.bytes().await.unwrap().is_empty());

    // A missing entity still returns a body that deserializes as SolxError.
    let missing = auth(http.get(format!("{base_url}/docs/research/ai/gone"))).send().await.unwrap();
    assert_eq!(missing.status(), 404);
    assert!(matches!(missing.json::<SolxError>().await.unwrap(), SolxError::NotFound(_)));

    // A malformed reference is a 400, not a 500. `:` is one of the characters
    // `solx_surface::path` forbids in a segment. (Traversal is not testable
    // from here — any conformant URL parser strips `..` segments, encoded or
    // not, long before the request is sent; `refs::tests` covers the server
    // side of that directly.)
    let bad = auth(http.get(format!("{base_url}/docs/a%3Ab"))).send().await.unwrap();
    assert_eq!(bad.status(), 400);
    assert!(matches!(bad.json::<SolxError>().await.unwrap(), SolxError::Invalid(_)));

    // Files: raw bytes in, raw bytes out, with a guessed Content-Type.
    let png = b"\x89PNG\r\n\x1a\nnot-really".to_vec();
    let put = auth(http.put(format!("{base_url}/files/media/pic.png"))).body(png.clone()).send().await.unwrap();
    assert_eq!(put.status(), 200);
    let got = auth(http.get(format!("{base_url}/files/media/pic.png"))).send().await.unwrap();
    assert_eq!(got.headers()["content-type"], "image/png");
    assert_eq!(got.bytes().await.unwrap().to_vec(), png, "bytes must survive unencoded");

    let files_listed: serde_json::Value = auth(http.get(format!("{base_url}/files")))
        .query(&[("prefix", "media")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(files_listed["paths"][0], "media/pic.png");

    // A parameterless action can be POSTed with no body at all — no
    // `Content-Type`, no literal `{}`.
    _cfg.register_command(
        "echo-42",
        solx_config::CommandDef { command: "echo 42".into(), description: None, cwd: None },
    )
    .unwrap();
    auth(http.put(format!("{base_url}/actions/tools/echo")))
        .json(&json!({ "action_type": "command", "fn_name": "echo-42" }))
        .send()
        .await
        .unwrap();
    let execed = auth(http.post(format!("{base_url}/actions/tools/echo"))).send().await.unwrap();
    assert_eq!(execed.status(), 200);
    let exec_body: serde_json::Value = execed.json().await.unwrap();
    assert_eq!(exec_body["success"], true);
    assert_eq!(exec_body["result"], 42);
}

#[tokio::test]
async fn wrong_token_is_rejected() {
    let (_dir, _cfg, base_url, _token) = spawn_server().await;
    let types = RemoteTypeManager::new(base_url, "wrong-token");
    let err = types.list(ListOptions::default()).await.unwrap_err();
    // A 401 with no SolxError body falls back to SolxError::Other — still
    // a hard failure, which is what matters here.
    assert!(matches!(err, SolxError::Other(_)));
}
