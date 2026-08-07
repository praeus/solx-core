//! Dispatcher-level native handler for internal actions.
//!
//! Internal actions are dispatched by `fn_name` — no WASM, no shell, no HTTP.
//! This is the umbrella for the built-in catalogue's entity CRUD, search,
//! file-store, document-field, environment-store, HTTP fetch, and secrets
//! operations — all of it used to be a WASM component (`solx-builtin-actions`,
//! now removed) executed under wasmtime's trusted `backend-action` world;
//! native dispatch is strictly simpler (no sync/async host-function bridging,
//! no separate guest build/packaging step) and was already how everything
//! else in this module worked. WASM now exists solely for third-party
//! *custom* actions (`crate::wasm_host`), which reach every one of these
//! same operations recursively via `action-exec` — no separate WASM ABI
//! needed for them.
//!
//! The actual handlers are split across submodules by concern:
//!
//! * [`entity`] — CRUD and search over documents, types, and actions
//!   (including the executable-action guard)
//! * [`file`] — file-store and recursive directory operations
//! * [`doc_fields`] — flat (`get_field`/`set_field`) and path-style
//!   (`get_field_at_path`/`set_field_at_path`) document field ops
//! * [`http`] — generic HTTP request (`http_request`)
//! * [`utils`] — small stateless helpers (`now`, `uuid`, `random_int`,
//!   `random_string`, `get_env`/`set_env`)
//! * [`secrets`] — the per-caller `get_secret`/`set_secret` and the
//!   `init_env_mappings` startup hook
//! * [`oauth`] — the OAuth 2.0 authorization-code loopback controllers
//!   and their registry/inbox state
//!
//! All of these are reached through the single [`run_internal`] dispatch
//! table below; there is no public per-submodule API beyond what
//! non-`internal` modules of this crate need (the OAuth registry is
//! referenced by `crate::oauth_loopback`, and `init_env_mappings` is
//! called once at startup by `solx-manager`).

use std::sync::Arc;

use serde_json::Value;

use solx_surface::entities::{ActionType, Document, DocumentInput};
use solx_surface::managers::{ActionManager, DocManager, FileStore, TypeManager};

use crate::caller::Caller;

pub mod doc_fields;
pub mod entity;
pub mod file;
pub mod http;
pub mod oauth;
pub mod open_url;
pub mod secrets;
pub mod utils;

// Re-export `init_env_mappings` so the existing call site
// `solx_actions::internal::init_env_mappings` (used by `solx-manager`
// at startup) keeps working without modification. The function lives
// in `utils.rs` next to the env store it manages.
pub use utils::init_env_mappings;

// ── Context ──────────────────────────────────────────────────────────────────

/// The manager handles internal actions need — unlike OAuth (pure
/// process-local state), entity CRUD/search/file-store actions have to call
/// straight through to the same managers `solx-cli` uses, so `path` (and
/// every other field) is honored exactly, with no guest-side marshaling in
/// between.
pub struct InternalCtx {
    pub docs: Arc<dyn DocManager>,
    pub types: Arc<dyn TypeManager>,
    pub actions: Arc<dyn ActionManager>,
    pub files: Arc<dyn FileStore>,
    /// The *executing* action's own `action_config` — i.e. the row whose
    /// `fn_name` dispatched to this handler, which for a built-in is the
    /// `/builtin/...` row itself.
    pub action_config: Option<Value>,
    /// The action that *invoked* this one, when there is one. Set only on
    /// the recursive hop from a WASM guest; `None` for the CLI, MCP, and
    /// HTTP, none of which are actions. `get_secret`/`set_secret` scope to
    /// this — see [`crate::caller`] for why it can't be spoofed.
    ///
    /// **Invariant:** read-only input to key resolution. No handler may
    /// return it, or any key inside it, in its result value.
    pub caller: Option<Caller>,
}

// ── Public entry point ───────────────────────────────────────────────────────

/// Dispatch an internal action by `fn_name`. Returns the JSON result value
/// (the caller wraps it in `ActionExecResult`).
pub async fn run_internal(fn_name: &str, params: &Value, ctx: &InternalCtx) -> Result<Value, String> {
    match fn_name {
        // ── OAuth 2.0 authorization-code loopback ────────────────────────
        "oauth_start" => oauth::oauth_start(params).await,
        "oauth_await" => oauth::oauth_await(params).await,
        "oauth_stop" => oauth::oauth_stop(params).await,

        // ── entity CRUD ──────────────────────────────────────────────────
        "entity_post_document" => entity::doc_post(params, &ctx.docs).await,
        "entity_get_document" => entity::doc_get(params, &ctx.docs).await,
        "entity_delete_document" => entity::doc_delete(params, &ctx.docs).await,
        "entity_list_documents" => entity::doc_list(params, &ctx.docs).await,

        "entity_post_type" => entity::type_post(params, &ctx.types).await,
        "entity_get_type" => entity::type_get(params, &ctx.types).await,
        "entity_delete_type" => entity::type_delete(params, &ctx.types).await,
        "entity_list_types" => entity::type_list(params, &ctx.types).await,

        "entity_post_action" => entity::action_post(params, &ctx.actions).await,
        "entity_get_action" => entity::action_get(params, &ctx.actions).await,
        "entity_delete_action" => entity::action_delete(params, &ctx.actions).await,
        "entity_list_actions" => entity::action_list(params, &ctx.actions).await,

        // ── search ───────────────────────────────────────────────────────
        "search_documents" => entity::search_documents(params, &ctx.docs).await,
        "search_actions" => entity::search_actions(params, &ctx.actions).await,

        // ── file store ───────────────────────────────────────────────────
        "file_put" => file::file_put(params, &ctx.files).await,
        "file_get" => file::file_get(params, &ctx.files).await,
        "file_delete" => file::file_delete(params, &ctx.files).await,
        "file_list" => file::file_list(params, &ctx.files).await,
        "file_copy" => file::file_copy(params, &ctx.files).await,
        "dir_copy" => file::dir_copy(params, &ctx.files).await,
        "dir_delete" => file::dir_delete(params, &ctx.files).await,

        // ── document field ops (flat and path-style) ─────────────────────
        "get_field" => doc_fields::get_field(params, &ctx.docs).await,
        "set_field" => doc_fields::set_field(params, &ctx.docs).await,
        "get_field_at_path" => doc_fields::get_field_at_path(params, &ctx.docs).await,
        "set_field_at_path" => doc_fields::set_field_at_path(params, &ctx.docs).await,

        // ── environment store ────────────────────────────────────────────
        "get_env" => Ok(utils::get_env(params)),
        "set_env" => Ok(utils::set_env(params)),

        // ── HTTP ─────────────────────────────────────────────────────────
        "http_request" => http::http_request(params).await,

        // ── small stateless utilities ────────────────────────────────────
        "now" => Ok(utils::now_value()),
        "uuid" => Ok(utils::uuid_value()),
        "random_int" => utils::random_int_value(params),
        "random_string" => utils::random_string_value(params),

        // ── system integration ────────────────────────────────────────────
        "open_url" => open_url::open_url(params).await,

        // ── secrets (per-caller scoped) ──────────────────────────────────
        "get_secret" => secrets::get_secret(params, ctx.caller.as_ref()).await,
        "set_secret" => secrets::set_secret(params, ctx.caller.as_ref()).await,

        other => Err(format!("unknown internal fn_name '{other}'")),
    }
}

// ── Shared helpers (pub(super) for submodules) ───────────────────────────────
//
// These intentionally live next to the dispatcher rather than in a separate
// `helpers.rs` sibling: every submodule needs `require_str` / `path_or_root`
// / `parse_input` / `to_value`, and a `pub(super)` flat collection keeps the
// import line on each submodule to a single `use super::*`.

pub(super) fn require_str<'a>(params: &'a Value, field: &str) -> Result<&'a str, String> {
    params
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing required param: {field}"))
}

pub(super) fn path_or_root(params: &Value) -> &str {
    params.get("path").and_then(Value::as_str).unwrap_or("/")
}

/// The path-style field ops use `path` for the JSON path *inside* the
/// document, and `doc_path` for the entity-store path. This helper picks
/// up the latter, defaulting to root when absent.
pub(super) fn doc_path_or_root(params: &Value) -> &str {
    params.get("doc_path").and_then(Value::as_str).unwrap_or("/")
}

pub(super) fn parse_input<T: serde::de::DeserializeOwned>(params: &Value) -> Result<T, String> {
    serde_json::from_value(params.clone()).map_err(|e| format!("invalid params: {e}"))
}

pub(super) fn to_value<T: serde::Serialize>(v: &T) -> Result<Value, String> {
    serde_json::to_value(v).map_err(|e| format!("failed to serialize result: {e}"))
}

/// Used by `entity::action_post` / `entity::action_delete` to apply the
/// "no executable actions from an action, MCP tool call, or script" guard
/// without duplicating the existing-row lookup.
pub(super) fn guard_executable_action(
    existing: Option<ActionType>,
    incoming: Option<ActionType>,
    path: &str,
    name: &str,
    verb: &str,
) -> Result<(), String> {
    for ty in [incoming, existing].into_iter().flatten() {
        let label = match ty {
            ActionType::Command => "command",
            ActionType::Webhook => "webhook",
            _ => continue,
        };
        return Err(format!(
            "cannot {verb} '{label}' actions from an action, MCP tool call, or script \
             (attempted on {path}/{name}); use the solx CLI"
        ));
    }
    Ok(())
}

/// Used by `doc_fields` to clone a `Document` into the upsert input shape.
pub(super) fn document_input_from(doc: &Document) -> DocumentInput {
    DocumentInput {
        title: doc.title.clone(),
        summary: doc.summary.clone(),
        type_ref: Some(doc.type_ref.clone()),
        contents: doc.contents.clone(),
        author: doc.author.clone(),
        pub_date: doc.pub_date.clone(),
        confidence: doc.confidence,
        links: doc.links.clone(),
        files: doc.files.clone(),
    }
}

pub(super) async fn load_document(
    docs: &Arc<dyn DocManager>,
    path: &str,
    name: &str,
) -> Result<Document, String> {
    docs.get(path, name).await.map_err(|e| e.to_string())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    // The whole test module lives here — not in each submodule — because
    // every test needs `test_ctx` and several need `fake_secrets` /
    // `install_test_callback`, and scattering them would force the
    // helpers to be `pub(super)` (or duplicated). Keeping one test
    // module here keeps the harness small and the assertions colocated
    // with the dispatch table they exercise.

    use super::*;
    use crate::caller::Caller;

    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use base64::Engine as _;
    use serde_json::json;
    use tokio::sync::oneshot;

    use crate::oauth_loopback::LoopbackResult;

    /// A valid 32-byte AES key, base64-encoded — `crate::secrets` rejects
    /// anything else.
    fn key_b64() -> String {
        base64::engine::general_purpose::STANDARD.encode([7u8; 32])
    }

    /// In-memory stand-in for the OS credential manager. `fn` pointers
    /// can't capture state, so the map lives in a static.
    static FAKE_SECRETS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

    fn fake_secrets() -> &'static Mutex<HashMap<String, String>> {
        FAKE_SECRETS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn fake_secret_backend() -> crate::secrets::SecretBackendOverride {
        fake_secrets().lock().unwrap().clear();
        crate::secrets::SecretBackendOverride {
            get: |name| Ok(fake_secrets().lock().unwrap().get(name).cloned()),
            set: |name, blob| {
                fake_secrets().lock().unwrap().insert(name.to_string(), blob.to_string());
                Ok(())
            },
            delete: |name| {
                fake_secrets().lock().unwrap().remove(name);
                Ok(())
            },
        }
    }

    /// Drive a manual callback for `state_value` without spinning up a
    /// real axum server — directly installs a `oneshot::Receiver` in the
    /// `INBOX` that resolves to `result` when awaited.
    fn install_test_callback(state_value: &str, result: LoopbackResult) {
        let (tx, rx) = oneshot::channel();
        oauth::test_inbox_put(state_value.into(), rx);
        let _ = tx.send(result);
    }

    /// Build a fully-wired `InternalCtx` over a temp appdata dir, mirroring
    /// `LocalActionManager`'s own test `setup()` in `lib.rs`. `action_config`
    /// stands in for the calling action's own config (secrets tests only).
    async fn test_ctx(action_config: Option<Value>) -> (tempfile::TempDir, InternalCtx) {
        let dir = tempfile::tempdir().unwrap();
        let types: Arc<dyn TypeManager> = Arc::new(
            solx_types::LocalTypeManager::open(&dir.path().join("types.db"))
                .await
                .unwrap(),
        );
        let docs: Arc<dyn DocManager> = Arc::new(
            solx_docs::LocalDocManager::open(
                &dir.path().join("docs.db"),
                &dir.path().join("idx"),
                types.clone(),
            )
            .await
            .unwrap(),
        );
        let files: Arc<dyn FileStore> = Arc::new(solx_files::LocalFileStore::new(dir.path().join("files")));
        let cfg = Arc::new(solx_config::ConfigService::open_in(dir.path()).unwrap());
        let actions_concrete = Arc::new(
            crate::LocalActionManager::open(
                &dir.path().join("actions.db"),
                cfg,
                types.clone(),
                docs.clone(),
                files.clone(),
            )
            .await
            .unwrap(),
        );
        actions_concrete.set_self_ref(Arc::downgrade(&actions_concrete));
        let actions: Arc<dyn ActionManager> = actions_concrete;
        (
            dir,
            InternalCtx { docs, types, actions, files, action_config, caller: None },
        )
    }

    #[tokio::test]
    async fn unknown_fn_name_errors() {
        let (_d, ctx) = test_ctx(None).await;
        let err = run_internal("bogus", &json!({}), &ctx).await.unwrap_err();
        assert!(err.contains("unknown internal fn_name 'bogus'"), "{err}");
    }

    #[tokio::test]
    async fn oauth_await_missing_state_value_errors() {
        let (_d, ctx) = test_ctx(None).await;
        let err = run_internal("oauth_await", &json!({}), &ctx).await.unwrap_err();
        assert!(err.contains("missing required param: state_value"), "{err}");
    }

    #[tokio::test]
    async fn oauth_await_unknown_state_value_errors() {
        let (_d, ctx) = test_ctx(None).await;
        let err = run_internal("oauth_await", &json!({"state_value": "nope"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.contains("no loopback registered"), "{err}");
    }

    #[tokio::test]
    async fn oauth_await_captures_success() {
        let (_d, ctx) = test_ctx(None).await;
        install_test_callback(
            "test-state",
            LoopbackResult {
                code: Some("the-code".into()),
                state: Some("test-state".into()),
                error: None,
                error_description: None,
            },
        );

        let v = run_internal(
            "oauth_await",
            &json!({"state_value": "test-state"}),
            &ctx,
        )
        .await
        .unwrap();

        assert_eq!(v.get("code").and_then(Value::as_str), Some("the-code"));
        assert_eq!(
            v.get("state_value").and_then(Value::as_str),
            Some("test-state")
        );
        assert_eq!(v.get("succeeded").and_then(Value::as_bool), Some(true));
    }

    #[tokio::test]
    async fn oauth_await_captures_error() {
        let (_d, ctx) = test_ctx(None).await;
        install_test_callback(
            "denied",
            LoopbackResult {
                code: None,
                state: Some("denied".into()),
                error: Some("access_denied".into()),
                error_description: Some("user said no".into()),
            },
        );

        let v = run_internal("oauth_await", &json!({"state_value": "denied"}), &ctx)
            .await
            .unwrap();

        assert_eq!(
            v.get("error").and_then(Value::as_str),
            Some("access_denied")
        );
        assert_eq!(
            v.get("error_description").and_then(Value::as_str),
            Some("user said no")
        );
        assert_eq!(v.get("succeeded").and_then(Value::as_bool), Some(false));
    }

    #[tokio::test]
    async fn oauth_stop_missing_state_value_errors() {
        let (_d, ctx) = test_ctx(None).await;
        let err = run_internal("oauth_stop", &json!({}), &ctx).await.unwrap_err();
        assert!(err.contains("missing required param: state_value"), "{err}");
    }

    #[tokio::test]
    async fn oauth_stop_unknown_state_value_succeeds_with_message() {
        let (_d, ctx) = test_ctx(None).await;
        let v = run_internal("oauth_stop", &json!({"state_value": "never-registered"}), &ctx)
            .await
            .unwrap();
        assert_eq!(v.get("stopped").and_then(Value::as_bool), Some(false));
    }

    #[tokio::test]
    async fn entity_document_crud_honors_path() {
        let (_d, ctx) = test_ctx(None).await;
        let created = run_internal(
            "entity_post_document",
            &json!({"path": "/research/ai", "name": "note", "type_ref": "/types/core/Object", "contents": {"a": 1}}),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(created.get("path").and_then(Value::as_str), Some("/research/ai"));

        // Wrong path must not find it — this is exactly the bug being fixed.
        let missing = run_internal(
            "entity_get_document",
            &json!({"path": "/wrong", "name": "note"}),
            &ctx,
        )
        .await;
        assert!(missing.is_err(), "expected not-found for the wrong path");

        let fetched = run_internal(
            "entity_get_document",
            &json!({"path": "/research/ai", "name": "note"}),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(fetched.get("name").and_then(Value::as_str), Some("note"));

        let deleted = run_internal(
            "entity_delete_document",
            &json!({"path": "/research/ai", "name": "note"}),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(deleted.get("deleted").and_then(Value::as_bool), Some(true));
    }

    #[tokio::test]
    async fn search_documents_and_search_actions_work() {
        let (_d, ctx) = test_ctx(None).await;
        run_internal(
            "entity_post_document",
            &json!({"path": "/notes", "name": "a", "type_ref": "/types/core/Object", "contents": {}, "title": "Hello world"}),
            &ctx,
        )
        .await
        .unwrap();
        let results = run_internal("search_documents", &json!({"q": "Hello"}), &ctx)
            .await
            .unwrap();
        assert!(results.get("total").and_then(Value::as_u64).unwrap_or(0) >= 1);

        let actions = run_internal("search_actions", &json!({"path_prefix": "/"}), &ctx)
            .await
            .unwrap();
        assert!(actions.get("total").and_then(Value::as_u64).unwrap_or(0) >= 1);
    }

    #[tokio::test]
    async fn file_put_get_list_delete_roundtrip() {
        let (_d, ctx) = test_ctx(None).await;
        run_internal(
            "file_put",
            &json!({"rel_path": "notes/a.txt", "content": "hello"}),
            &ctx,
        )
        .await
        .unwrap();

        let got = run_internal("file_get", &json!({"rel_path": "notes/a.txt"}), &ctx)
            .await
            .unwrap();
        assert_eq!(got.get("content").and_then(Value::as_str), Some("hello"));

        let listed = run_internal("file_list", &json!({"prefix": "notes"}), &ctx)
            .await
            .unwrap();
        assert!(listed.get("files").and_then(Value::as_array).map(|a| !a.is_empty()).unwrap_or(false));

        run_internal("file_delete", &json!({"rel_path": "notes/a.txt"}), &ctx)
            .await
            .unwrap();
        assert!(run_internal("file_get", &json!({"rel_path": "notes/a.txt"}), &ctx).await.is_err());
    }

    #[tokio::test]
    async fn file_copy_and_dir_copy_work() {
        let (_d, ctx) = test_ctx(None).await;
        run_internal("file_put", &json!({"rel_path": "src/a.txt", "content": "a"}), &ctx).await.unwrap();
        run_internal("file_put", &json!({"rel_path": "src/sub/b.txt", "content": "b"}), &ctx).await.unwrap();

        run_internal("file_copy", &json!({"source": "src/a.txt", "dest": "dst/a.txt"}), &ctx)
            .await
            .unwrap();
        let copied = run_internal("file_get", &json!({"rel_path": "dst/a.txt"}), &ctx).await.unwrap();
        assert_eq!(copied.get("content").and_then(Value::as_str), Some("a"));

        run_internal("dir_copy", &json!({"source": "src", "dest": "dst2"}), &ctx).await.unwrap();
        let copied_nested = run_internal("file_get", &json!({"rel_path": "dst2/sub/b.txt"}), &ctx)
            .await
            .unwrap();
        assert_eq!(copied_nested.get("content").and_then(Value::as_str), Some("b"));
    }

    #[tokio::test]
    async fn get_field_and_set_field_round_trip() {
        let (_d, ctx) = test_ctx(None).await;
        run_internal(
            "entity_post_document",
            &json!({"name": "note", "type_ref": "/types/core/Object", "contents": {"a": 1}}),
            &ctx,
        )
        .await
        .unwrap();

        let a = run_internal("get_field", &json!({"name": "note", "field": "a"}), &ctx).await.unwrap();
        assert_eq!(a, Value::from(1));

        run_internal("set_field", &json!({"name": "note", "field": "b", "value": "two"}), &ctx)
            .await
            .unwrap();
        let b = run_internal("get_field", &json!({"name": "note", "field": "b"}), &ctx).await.unwrap();
        assert_eq!(b, Value::String("two".into()));
        // The untouched field must survive the shallow-merge write.
        let a_again = run_internal("get_field", &json!({"name": "note", "field": "a"}), &ctx).await.unwrap();
        assert_eq!(a_again, Value::from(1));
    }

    #[tokio::test]
    async fn get_env_set_env_round_trip() {
        let (_d, ctx) = test_ctx(None).await;
        let missing = run_internal("get_env", &json!({"key": "SOLX_TEST_NOPE"}), &ctx).await.unwrap();
        assert_eq!(missing.get("value").cloned(), Some(Value::Null));

        run_internal("set_env", &json!({"key": "SOLX_TEST_KEY", "value": "hi"}), &ctx).await.unwrap();
        let got = run_internal("get_env", &json!({"key": "SOLX_TEST_KEY"}), &ctx).await.unwrap();
        assert_eq!(got.get("value").and_then(Value::as_str), Some("hi"));
    }

    /// The CLI, MCP, and the HTTP route all reach these built-ins with no
    /// action caller, so neither can resolve a key — this is the denial that
    /// keeps a model from reading an action's secrets by calling
    /// `/builtin/get_secret` directly.
    #[tokio::test]
    async fn secrets_are_refused_without_an_action_caller() {
        let (_d, ctx) = test_ctx(None).await;
        assert!(ctx.caller.is_none());

        let err = run_internal("get_secret", &json!({"name": "FOO"}), &ctx).await.unwrap_err();
        assert!(err.contains("no action caller"), "{err}");
        let err = run_internal("set_secret", &json!({"name": "FOO", "value": "x"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.contains("no action caller"), "{err}");
    }

    #[serial_test::serial(secrets_backend)]
    #[tokio::test]
    async fn secrets_resolve_against_the_calling_actions_own_keys() {
        let (_d, mut ctx) = test_ctx(None).await;
        // Note the key comes from the *caller*, not from `ctx.action_config`
        // (which is the `/builtin/...` row's config and is always null).
        ctx.caller = Some(Caller::from_action(
            "/pkg/foo",
            Some(&json!({ "secrets": { "FOO": key_b64() } })),
        ));

        crate::secrets::set_secret_backend_for_tests(Some(fake_secret_backend()));
        run_internal("set_secret", &json!({"name": "FOO", "value": "s3cret"}), &ctx)
            .await
            .unwrap();
        let got = run_internal("get_secret", &json!({"name": "FOO"}), &ctx).await.unwrap();
        assert_eq!(got.get("value").and_then(Value::as_str), Some("s3cret"));

        // A secret the caller has no key for stays out of reach.
        let err = run_internal("get_secret", &json!({"name": "BAR"}), &ctx).await.unwrap_err();
        assert!(err.contains("no key configured"), "{err}");
        assert!(err.contains("action://pkg/foo"), "{err}");
        crate::secrets::set_secret_backend_for_tests(None);
    }

    #[test]
    fn generate_state_value_is_unique_and_hex() {
        let a = oauth::test_generate_state_value();
        let b = oauth::test_generate_state_value();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// Regression test for the receiver-loss bug: a timed-out `oauth_await`
    /// must put the receiver back so a later call for the same
    /// `state_value` can still succeed once the callback arrives. Uses
    /// `timeout_secs: 0` so the timeout branch always wins deterministically
    /// (nothing has been sent yet at that point) without any real waiting.
    #[tokio::test]
    async fn oauth_await_timeout_then_retry_succeeds() {
        let (_d, ctx) = test_ctx(None).await;
        let (tx, rx) = oneshot::channel();
        oauth::test_inbox_put("retry-state".into(), rx);

        let err = run_internal(
            "oauth_await",
            &json!({"state_value": "retry-state", "timeout_secs": 0}),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(err.contains("timed out"), "{err}");

        // The receiver must have been reinserted by the timeout path —
        // firing the sender now and retrying (without a timeout) must
        // succeed rather than erroring "no loopback registered".
        let _ = tx.send(LoopbackResult {
            code: Some("late-code".into()),
            state: Some("retry-state".into()),
            error: None,
            error_description: None,
        });

        let v = run_internal("oauth_await", &json!({"state_value": "retry-state"}), &ctx)
            .await
            .unwrap();
        assert_eq!(v.get("code").and_then(Value::as_str), Some("late-code"));
    }

    /// Regression test for the silent-bind-failure bug: `oauth_start` must
    /// surface a port-in-use error as an `Err`, not return `"started": true`
    /// for a listener that never actually bound.
    #[tokio::test]
    async fn oauth_start_surfaces_bind_failure() {
        let (_d, ctx) = test_ctx(None).await;
        // Occupy a port first so the real bind inside oauth_start fails.
        let blocker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = blocker.local_addr().unwrap().port();

        let err = run_internal("oauth_start", &json!({"port": port}), &ctx)
            .await
            .unwrap_err();
        assert!(
            err.contains("failed to bind"),
            "expected a bind-failure error, got: {err}"
        );

        drop(blocker);
    }

    // ── now / uuid / random_int / random_string ──────────────────────────

    #[tokio::test]
    async fn now_returns_rfc3339_utc() {
        let (_d, ctx) = test_ctx(None).await;
        let v = run_internal("now", &json!({}), &ctx).await.unwrap();
        let s = v.get("now").and_then(Value::as_str).expect("now field");
        // chrono::DateTime::parse_from_rfc3339 succeeds on the round-trip
        // form `Utc::now().to_rfc3339()` produces.
        let parsed = chrono::DateTime::parse_from_rfc3339(s)
            .expect("now() output must be a valid RFC 3339 string");
        assert_eq!(parsed.timezone(), chrono::FixedOffset::east_opt(0).unwrap());
    }

    #[tokio::test]
    async fn uuid_is_a_v4_string() {
        let (_d, ctx) = test_ctx(None).await;
        let a = run_internal("uuid", &json!({}), &ctx).await.unwrap();
        let b = run_internal("uuid", &json!({}), &ctx).await.unwrap();
        let a = a.get("uuid").and_then(Value::as_str).expect("uuid field");
        let b = b.get("uuid").and_then(Value::as_str).expect("uuid field");
        assert_ne!(a, b, "two uuids in a row must differ");
        let parsed = uuid::Uuid::parse_str(a).expect("uuid must parse");
        assert_eq!(parsed.get_version(), Some(uuid::Version::Random));
    }

    #[tokio::test]
    async fn random_int_is_in_range() {
        let (_d, ctx) = test_ctx(None).await;
        for _ in 0..32 {
            let v = run_internal("random_int", &json!({"lo": 5, "hi": 7}), &ctx)
                .await
                .unwrap();
            let n = v.get("value").and_then(Value::as_i64).expect("value field");
            assert!((5..=7).contains(&n), "{n} out of [5,7]");
        }
    }

    #[tokio::test]
    async fn random_int_rejects_inverted_range() {
        let (_d, ctx) = test_ctx(None).await;
        let err = run_internal("random_int", &json!({"lo": 10, "hi": 1}), &ctx)
            .await
            .unwrap_err();
        assert!(err.contains("lo"), "{err}");
    }

    #[tokio::test]
    async fn random_string_is_alphanumeric_and_requested_length() {
        let (_d, ctx) = test_ctx(None).await;
        for n in [0usize, 1, 8, 64] {
            let v = run_internal("random_string", &json!({"length": n}), &ctx)
                .await
                .unwrap();
            let s = v.get("value").and_then(Value::as_str).expect("value field");
            assert_eq!(s.len(), n, "length mismatch for n={n}");
            assert!(
                s.chars().all(|c| c.is_ascii_alphanumeric()),
                "non-alphanumeric char in {s:?}"
            );
        }
    }

    // ── dir_delete ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn dir_delete_recursively_removes_tree() {
        let (_d, ctx) = test_ctx(None).await;
        run_internal("file_put", &json!({"rel_path": "tree/a.txt", "content": "a"}), &ctx)
            .await
            .unwrap();
        run_internal("file_put", &json!({"rel_path": "tree/sub/b.txt", "content": "b"}), &ctx)
            .await
            .unwrap();
        run_internal("file_put", &json!({"rel_path": "tree/sub/deeper/c.txt", "content": "c"}), &ctx)
            .await
            .unwrap();

        let v = run_internal("dir_delete", &json!({"rel_path": "tree"}), &ctx)
            .await
            .unwrap();
        let deleted = v.get("deleted").and_then(Value::as_array).expect("deleted array");
        assert_eq!(deleted.len(), 3, "{deleted:?}");
        assert!(deleted.iter().any(|e| e.as_str() == Some("tree/a.txt")));
        assert!(deleted.iter().any(|e| e.as_str() == Some("tree/sub/b.txt")));
        assert!(deleted.iter().any(|e| e.as_str() == Some("tree/sub/deeper/c.txt")));

        // Confirm the subtree is gone (list is empty for the prefix).
        let listed = run_internal("file_list", &json!({"prefix": "tree"}), &ctx)
            .await
            .unwrap();
        let files = listed.get("files").and_then(Value::as_array).cloned().unwrap_or_default();
        assert!(files.is_empty(), "expected no files under 'tree' after dir_delete, got {files:?}");
    }

    #[tokio::test]
    async fn dir_delete_does_not_delete_siblings() {
        let (_d, ctx) = test_ctx(None).await;
        run_internal("file_put", &json!({"rel_path": "keep/x.txt", "content": "k"}), &ctx)
            .await
            .unwrap();
        run_internal("file_put", &json!({"rel_path": "drop/y.txt", "content": "d"}), &ctx)
            .await
            .unwrap();

        run_internal("dir_delete", &json!({"rel_path": "drop"}), &ctx)
            .await
            .unwrap();

        // The keep/x.txt must survive.
        let still = run_internal("file_get", &json!({"rel_path": "keep/x.txt"}), &ctx)
            .await
            .unwrap();
        assert_eq!(still.get("content").and_then(Value::as_str), Some("k"));
        // And the drop subtree is gone.
        assert!(run_internal("file_get", &json!({"rel_path": "drop/y.txt"}), &ctx)
            .await
            .is_err());
    }

    // ── get_field_at_path / set_field_at_path ─────────────────────────────

    #[tokio::test]
    async fn get_field_at_path_reads_nested_value() {
        let (_d, ctx) = test_ctx(None).await;
        run_internal(
            "entity_post_document",
            &json!({
                "name": "doc",
                "type_ref": "/types/core/Object",
                "contents": {
                    "metadata": { "tags": ["alpha", "beta"] },
                    "score": 7,
                }
            }),
            &ctx,
        )
        .await
        .unwrap();

        // Object key.
        let v = run_internal("get_field_at_path", &json!({"name": "doc", "path": "metadata"}), &ctx)
            .await
            .unwrap();
        assert_eq!(v.get("tags").and_then(Value::as_array).map(|a| a.len()), Some(2));

        // Nested object key.
        let v = run_internal(
            "get_field_at_path",
            &json!({"name": "doc", "path": "metadata/tags"}),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(
            v.as_array().map(|a| a.first().and_then(Value::as_str).unwrap_or_default()),
            Some("alpha")
        );

        // Array index.
        let v = run_internal(
            "get_field_at_path",
            &json!({"name": "doc", "path": "metadata/tags/1"}),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(v, Value::from("beta"));

        // Top-level scalar.
        let v = run_internal("get_field_at_path", &json!({"name": "doc", "path": "score"}), &ctx)
            .await
            .unwrap();
        assert_eq!(v, Value::from(7));

        // Missing path -> null (not an error).
        let v = run_internal(
            "get_field_at_path",
            &json!({"name": "doc", "path": "metadata/missing"}),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(v, Value::Null);
    }

    #[tokio::test]
    async fn set_field_at_path_overwrites_existing_path() {
        let (_d, ctx) = test_ctx(None).await;
        run_internal(
            "entity_post_document",
            &json!({
                "name": "doc",
                "type_ref": "/types/core/Object",
                "contents": { "metadata": { "tags": ["a", "b", "c"] }, "score": 1 }
            }),
            &ctx,
        )
        .await
        .unwrap();

        run_internal(
            "set_field_at_path",
            &json!({"name": "doc", "path": "metadata/tags/1", "value": "B"}),
            &ctx,
        )
        .await
        .unwrap();
        let tags = run_internal(
            "get_field_at_path",
            &json!({"name": "doc", "path": "metadata/tags"}),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(
            tags.as_array().map(|a| a.iter().map(|v| v.as_str().unwrap()).collect::<Vec<_>>()),
            Some(vec!["a", "B", "c"])
        );

        // The untouched top-level field must survive.
        let score = run_internal("get_field_at_path", &json!({"name": "doc", "path": "score"}), &ctx)
            .await
            .unwrap();
        assert_eq!(score, Value::from(1));
    }

    #[tokio::test]
    async fn set_field_at_path_errors_without_create() {
        let (_d, ctx) = test_ctx(None).await;
        run_internal(
            "entity_post_document",
            &json!({
                "name": "doc",
                "type_ref": "/types/core/Object",
                "contents": {}
            }),
            &ctx,
        )
        .await
        .unwrap();

        let err = run_internal(
            "set_field_at_path",
            &json!({"name": "doc", "path": "metadata/foo", "value": "bar"}),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(err.contains("does not exist"), "{err}");
        assert!(err.contains("create:true"), "{err}");
    }

    #[tokio::test]
    async fn set_field_at_path_creates_missing_parents() {
        let (_d, ctx) = test_ctx(None).await;
        run_internal(
            "entity_post_document",
            &json!({
                "name": "doc",
                "type_ref": "/types/core/Object",
                "contents": {}
            }),
            &ctx,
        )
        .await
        .unwrap();

        // Object key under object key.
        run_internal(
            "set_field_at_path",
            &json!({
                "name": "doc",
                "path": "metadata/foo",
                "value": "bar",
                "create": true,
            }),
            &ctx,
        )
        .await
        .unwrap();
        let v = run_internal("get_field_at_path", &json!({"name": "doc", "path": "metadata/foo"}), &ctx)
            .await
            .unwrap();
        assert_eq!(v, Value::from("bar"));

        // Numeric segment into an existing object creates an array and
        // pads with null up to the requested index.
        run_internal(
            "set_field_at_path",
            &json!({
                "name": "doc",
                "path": "arr/2",
                "value": "third",
                "create": true,
            }),
            &ctx,
        )
        .await
        .unwrap();
        let arr = run_internal("get_field_at_path", &json!({"name": "doc", "path": "arr"}), &ctx)
            .await
            .unwrap();
        let arr = arr.as_array().expect("array");
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[2], Value::from("third"));
        assert_eq!(arr[0], Value::Null);
        assert_eq!(arr[1], Value::Null);
    }

    // ── http_request ──────────────────────────────────────────────────────

    /// Spin up a single-shot HTTP echo server on a random local port and
    /// return its base URL plus a `JoinHandle` for the server task. The
    /// server replies 200 to anything sent to `/ok` with the method, body,
    /// and selected headers echoed back as JSON. Anything to `/bad`
    /// returns 503.
    async fn start_echo_server() -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let handle = tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(p) => p,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = match stream.read(&mut buf).await {
                        Ok(n) if n > 0 => n,
                        _ => return,
                    };
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let method = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().next())
                        .unwrap_or("GET")
                        .to_string();
                    let body = req
                        .split("\r\n\r\n")
                        .nth(1)
                        .unwrap_or("")
                        .to_string();
                    let has_json = req
                        .to_ascii_lowercase()
                        .contains("content-type: application/json");
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/");
                    let (status, response_body) = if path.starts_with("/bad") {
                        (503, r#"{"err":"nope"}"#.to_string())
                    } else {
                        let payload = serde_json::json!({
                            "method": method,
                            "body": body,
                            "saw_json_header": has_json,
                        });
                        (200, payload.to_string())
                    };
                    let response = format!(
                        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nX-Echo: 1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        if status == 200 { "OK" } else { "Service Unavailable" },
                        response_body.len(),
                        response_body,
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (base, handle)
    }

    #[tokio::test]
    async fn http_request_get_returns_status_and_body() {
        let (_d, ctx) = test_ctx(None).await;
        let (base, server) = start_echo_server().await;
        let v = run_internal(
            "http_request",
            &json!({"url": format!("{base}/ok"), "method": "GET"}),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(v.get("status").and_then(Value::as_u64), Some(200));
        assert_eq!(v.get("body_encoding").and_then(Value::as_str), Some("utf8"));
        let body = v.get("body").and_then(Value::as_str).expect("body");
        let parsed: Value = serde_json::from_str(body).expect("body is JSON");
        assert_eq!(parsed.get("method").and_then(Value::as_str), Some("GET"));
        assert_eq!(parsed.get("body").and_then(Value::as_str), Some(""));
        let headers = v.get("headers").and_then(Value::as_object).expect("headers");
        assert_eq!(
            headers.get("x-echo").and_then(Value::as_str),
            Some("1"),
            "response headers should be lowercased and surfaced"
        );
        server.abort();
    }

    #[tokio::test]
    async fn http_request_sends_headers_and_body() {
        let (_d, ctx) = test_ctx(None).await;
        let (base, server) = start_echo_server().await;
        let v = run_internal(
            "http_request",
            &json!({
                "url": format!("{base}/ok"),
                "method": "POST",
                "headers": { "Content-Type": "application/json", "X-Trace": "abc" },
                "body": r#"{"hi":"there"}"#,
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(v.get("status").and_then(Value::as_u64), Some(200));
        let body = v.get("body").and_then(Value::as_str).expect("body");
        let parsed: Value = serde_json::from_str(body).expect("body is JSON");
        assert_eq!(parsed.get("method").and_then(Value::as_str), Some("POST"));
        assert_eq!(parsed.get("body").and_then(Value::as_str), Some(r#"{"hi":"there"}"#));
        assert_eq!(parsed.get("saw_json_header").and_then(Value::as_bool), Some(true));
        server.abort();
    }

    #[tokio::test]
    async fn http_request_returns_response_on_non_2xx() {
        let (_d, ctx) = test_ctx(None).await;
        let (base, server) = start_echo_server().await;
        // Crucial difference from the old `fetch_html`: a 503 is *not* an
        // error — the response is returned and the caller decides.
        let v = run_internal(
            "http_request",
            &json!({"url": format!("{base}/bad")}),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(v.get("status").and_then(Value::as_u64), Some(503));
        assert_eq!(v.get("body_encoding").and_then(Value::as_str), Some("utf8"));
        let body = v.get("body").and_then(Value::as_str).expect("body");
        assert_eq!(body, r#"{"err":"nope"}"#);
        server.abort();
    }

    #[tokio::test]
    async fn http_request_rejects_zero_timeout() {
        let (_d, ctx) = test_ctx(None).await;
        let err = run_internal(
            "http_request",
            &json!({"url": "http://127.0.0.1:1/", "timeout_secs": 0}),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(err.contains("timeout_secs must be > 0"), "{err}");
    }
}
