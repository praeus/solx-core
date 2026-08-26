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
//! *custom* actions (`crate::wasm`), which reach every one of these
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
//! * [`env`] — the in-process environment store (`get_env`/`set_env`) and
//!   the `init_env_mappings`/`init_persisted_env` startup hooks
//! * [`secrets`] — the per-caller `get_secret`/`set_secret`
//! * [`oauth`] — the OAuth 2.0 authorization-code loopback controllers
//!   and their registry/inbox state
//!
//! All of these are reached through the single [`run_internal`] dispatch
//! table below; there is no public per-submodule API beyond what
//! non-`internal` modules of this crate need (the OAuth registry is
//! referenced by `crate::loopback::oauth`, and `init_env_mappings` is
//! called once at startup by `solx-manager`).

use std::sync::Arc;

use serde_json::Value;

use solx_surface::entities::{ActionType, Document, DocumentInput};
use solx_surface::internal_actions::{InternalActionRegistry, InternalCallCtx};
use solx_surface::managers::{ActionManager, DocManager, FileStore, TypeManager};

use crate::caller::Caller;
use crate::LocalActionManager;

pub mod doc_fields;
pub mod entity;
pub mod env;
pub mod file;
pub mod http;
pub mod http_stream;
pub mod oauth;
pub mod open_url;
pub mod secrets;

// Re-export `init_env_mappings` so the existing call site
// `solx_actions::internal::init_env_mappings` (used by `solx-manager`
// at startup) keeps working without modification. The function lives
// in `env.rs` next to the env store it manages.
pub use env::{init_env_mappings, init_persisted_env, DEFAULT_NAMESPACE};

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
    /// Needed by `set_env` to write persisted variables through to
    /// `SolxConfig.env_vars`. Every other built-in handler reaches its
    /// state through the managers above.
    pub config: Arc<solx_config::ConfigService>,
    /// Concrete manager handle, distinct from `actions` (`Arc<dyn
    /// ActionManager>`) above — needed by the WASM recursive `action-exec`
    /// hop, which isn't on the `ActionManager` trait.
    pub local: Arc<LocalActionManager>,
    /// Every internal-action plugin registration (currently `solx-console`'s
    /// `console_*`/`action_{start,stop,poll,cancelled}`), consulted by
    /// [`run_internal`]'s catch-all arm after the hard-coded built-ins
    /// above. See `solx_surface::internal_actions`.
    pub registry: Arc<InternalActionRegistry>,
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

impl InternalCtx {
    /// Narrow this context down to what a pluggable
    /// [`solx_surface::internal_actions::InternalActionHandler`] can reach —
    /// see that trait's doc for why it's deliberately smaller than this one.
    fn as_call_ctx(&self) -> InternalCallCtx {
        InternalCallCtx {
            docs: self.docs.clone(),
            types: self.types.clone(),
            actions: self.actions.clone(),
            files: self.files.clone(),
            caller: self.caller.clone(),
        }
    }
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
        "entity_save_document" => entity::doc_save(params, &ctx.docs).await,
        "entity_get_document" => entity::doc_get(params, &ctx.docs).await,
        "entity_delete_document" => entity::doc_delete(params, &ctx.docs).await,
        "entity_list_documents" => entity::doc_list(params, &ctx.docs).await,

        "entity_save_type" => entity::type_save(params, &ctx.types).await,
        "entity_get_type" => entity::type_get(params, &ctx.types).await,
        "entity_delete_type" => entity::type_delete(params, &ctx.types).await,
        "entity_list_types" => entity::type_list(params, &ctx.types).await,

        "entity_save_action" => entity::action_save(params, &ctx.actions).await,
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
        "get_env" => Ok(env::get_env(params)),
        "set_env" => env::set_env(params, &ctx.config),

        // ── HTTP ─────────────────────────────────────────────────────────
        "http_request" => http::http_request(params).await,

        // ── HTTP streaming ──────────────────────────────────────────────
        "http_stream_start" => http_stream::start(params, &ctx.config).await,
        "http_stream_poll" => http_stream::poll(params).await,
        "http_stream_close" => http_stream::close(params).await,

        // ── system integration ────────────────────────────────────────────
        "open_url" => open_url::open_url(params).await,

        // ── secrets (per-caller scoped) ──────────────────────────────────
        "get_secret" => secrets::get_secret(params, ctx.caller.as_ref()).await,
        "set_secret" => secrets::set_secret(params, ctx.caller.as_ref()).await,

        // ── everything else: consult the plugin registry ──────────────────
        // Action consoles (`console_*`) and asynchronous actions
        // (`action_{start,stop,poll,cancelled}`) are registered here by
        // `solx-console` rather than hard-coded above — see
        // `solx_console::actions::plugin` and `LocalActionManager::set_self_ref`.
        other => match ctx.registry.get(other) {
            Some(handler) => handler.call(params, &ctx.as_call_ctx()).await,
            None => Err(format!("unknown internal fn_name '{other}'")),
        },
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

/// Used by `entity::action_save` / `entity::action_delete` to apply the
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
    use std::time::Duration;
    use base64::Engine as _;
    use serde_json::json;
    use tokio::sync::oneshot;

    use crate::loopback::oauth::LoopbackResult;

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
            solx_docs::LocalDocManager::open(&dir.path().join("docs.db"), types.clone())
                .await
                .unwrap(),
        );
        let files: Arc<dyn FileStore> = Arc::new(solx_files::LocalFileStore::new(dir.path().join("files")));
        let cfg = Arc::new(solx_config::ConfigService::open_in(dir.path()).unwrap());
        let actions_concrete = Arc::new(
            crate::LocalActionManager::open(
                &dir.path().join("actions.db"),
                cfg.clone(),
                types.clone(),
                docs.clone(),
                files.clone(),
            )
            .await
            .unwrap(),
        );
        actions_concrete.set_self_ref(Arc::downgrade(&actions_concrete));
        let local = actions_concrete.clone();
        let registry = actions_concrete.plugin_registry();
        let actions: Arc<dyn ActionManager> = actions_concrete;
        (
            dir,
            InternalCtx {
                docs,
                types,
                actions,
                files,
                config: cfg,
                local,
                registry,
                action_config,
                caller: None,
            },
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
            "entity_save_document",
            &json!({"path": "/research/ai", "name": "note", "typeRef": "/types/core/Object", "contents": {"a": 1}}),
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
            "entity_save_document",
            &json!({"path": "/notes", "name": "a", "typeRef": "/types/core/Object", "contents": {}, "title": "Hello world"}),
            &ctx,
        )
        .await
        .unwrap();
        let results = run_internal("search_documents", &json!({"q": "Hello"}), &ctx)
            .await
            .unwrap();
        assert!(results.get("total").and_then(Value::as_u64).unwrap_or(0) >= 1);

        let actions = run_internal("search_actions", &json!({"pathPrefix": "/"}), &ctx)
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
            "entity_save_document",
            &json!({"name": "note", "typeRef": "/types/core/Object", "contents": {"a": 1}}),
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

    /// The env store is a process-global static, so each of these tests uses
    /// its own namespace rather than serializing them.
    #[tokio::test]
    async fn env_namespaces_isolate_the_same_key() {
        let (_d, ctx) = test_ctx(None).await;
        let set = |ns: &str, v: &str| json!({"namespace": ns, "key": "cursor", "value": v});

        run_internal("set_env", &set("ns_iso_a", "alpha"), &ctx).await.unwrap();
        run_internal("set_env", &set("ns_iso_b", "beta"), &ctx).await.unwrap();

        let a = run_internal("get_env", &json!({"namespace": "ns_iso_a", "key": "cursor"}), &ctx).await.unwrap();
        let b = run_internal("get_env", &json!({"namespace": "ns_iso_b", "key": "cursor"}), &ctx).await.unwrap();
        assert_eq!(a.get("value").and_then(Value::as_str), Some("alpha"));
        assert_eq!(b.get("value").and_then(Value::as_str), Some("beta"));

        // A namespace nobody wrote to resolves to null, not to another's value.
        let miss = run_internal("get_env", &json!({"namespace": "ns_iso_c", "key": "cursor"}), &ctx).await.unwrap();
        assert_eq!(miss.get("value").cloned(), Some(Value::Null));
    }

    /// Persistence is a property of the variable, not of the individual write:
    /// once persisted, a later write with no `persist` flag must keep updating
    /// `solx-config.json` rather than silently dropping to memory-only.
    #[tokio::test]
    async fn set_env_persist_is_sticky_and_writes_config() {
        let (dir, ctx) = test_ctx(None).await;
        let cfg = solx_config::ConfigService::open_in(dir.path()).unwrap();

        let res = run_internal(
            "set_env",
            &json!({"namespace": "ns_sticky", "key": "cursor", "value": "/?skip=10", "persist": true}),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(res.get("persisted").and_then(Value::as_bool), Some(true));
        assert_eq!(
            cfg.env_vars().get("ns_sticky").and_then(|m| m.get("cursor")).map(String::as_str),
            Some("/?skip=10")
        );

        // No `persist` this time — the config entry must still advance.
        let res = run_internal(
            "set_env",
            &json!({"namespace": "ns_sticky", "key": "cursor", "value": "/?skip=20"}),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(res.get("persisted").and_then(Value::as_bool), Some(true));
        assert_eq!(
            cfg.env_vars().get("ns_sticky").and_then(|m| m.get("cursor")).map(String::as_str),
            Some("/?skip=20")
        );

        // A different key in the same namespace stays ephemeral unless asked.
        run_internal(
            "set_env",
            &json!({"namespace": "ns_sticky", "key": "scratch", "value": "x"}),
            &ctx,
        )
        .await
        .unwrap();
        assert!(cfg.env_vars().get("ns_sticky").and_then(|m| m.get("scratch")).is_none());
    }

    /// What makes a cursor survive a restart: startup reloads `env_vars` into
    /// the store, and those variables come back already marked persistent.
    #[tokio::test]
    async fn init_persisted_env_reloads_into_the_store() {
        let (_d, ctx) = test_ctx(None).await;
        let mut ns = std::collections::HashMap::new();
        ns.insert("cursor".to_string(), "/?skip=90".to_string());
        let mut vars = std::collections::HashMap::new();
        vars.insert("ns_reload".to_string(), ns);
        init_persisted_env(vars);

        let got = run_internal("get_env", &json!({"namespace": "ns_reload", "key": "cursor"}), &ctx).await.unwrap();
        assert_eq!(got.get("value").and_then(Value::as_str), Some("/?skip=90"));

        // Reloaded variables are persistent, so a plain write still writes through.
        let res = run_internal(
            "set_env",
            &json!({"namespace": "ns_reload", "key": "cursor", "value": "/?skip=100"}),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(res.get("persisted").and_then(Value::as_bool), Some(true));
    }

    /// The CLI, MCP, and the HTTP route all reach these built-ins with no
    /// action caller, so neither can resolve a key — this is the denial that
    /// keeps a model from reading an action's secrets by calling
    /// `/builtin/secrets/get_secret` directly.
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
            "entity_save_document",
            &json!({
                "name": "doc",
                "typeRef": "/types/core/Object",
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
            "entity_save_document",
            &json!({
                "name": "doc",
                "typeRef": "/types/core/Object",
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
            "entity_save_document",
            &json!({
                "name": "doc",
                "typeRef": "/types/core/Object",
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
            "entity_save_document",
            &json!({
                "name": "doc",
                "typeRef": "/types/core/Object",
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

    // ── http_stream_start / poll / close ──────────────────────────────────

    /// Spin up an HTTP server that answers `/stream` with a chunked-encoding
    /// response emitting `lines` (each already newline-terminated or not —
    /// a trailing `\n` is added if missing) with a short delay between each,
    /// then closes the connection. `/hang` sends one line then holds the
    /// connection open indefinitely (for idle-TTL testing) until the test
    /// aborts the server task.
    async fn start_stream_server(lines: Vec<String>) -> (String, tokio::task::JoinHandle<()>) {
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
                let lines = lines.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = match stream.read(&mut buf).await {
                        Ok(n) if n > 0 => n,
                        _ => return,
                    };
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/");

                    let header = "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\n\r\n";
                    if stream.write_all(header.as_bytes()).await.is_err() {
                        return;
                    }

                    for line in &lines {
                        let mut data = line.clone();
                        if !data.ends_with('\n') {
                            data.push('\n');
                        }
                        let framed = format!("{:x}\r\n{data}\r\n", data.len());
                        if stream.write_all(framed.as_bytes()).await.is_err() {
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(30)).await;
                    }

                    if path.starts_with("/hang") {
                        // Never send the terminating chunk — hold the
                        // connection open until the server task is aborted.
                        tokio::time::sleep(Duration::from_secs(3600)).await;
                        return;
                    }

                    let _ = stream.write_all(b"0\r\n\r\n").await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (base, handle)
    }

    #[tokio::test]
    async fn http_stream_start_returns_immediately_with_status() {
        let (_d, ctx) = test_ctx(None).await;
        let (base, server) = start_stream_server(vec![
            r#"{"n":1}"#.to_string(),
            r#"{"n":2}"#.to_string(),
        ])
        .await;

        let v = run_internal(
            "http_stream_start",
            &json!({"url": format!("{base}/stream")}),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(v.get("status").and_then(Value::as_u64), Some(200));
        assert!(v.get("stream_id").and_then(Value::as_str).is_some());
        server.abort();
    }

    #[tokio::test]
    async fn http_stream_poll_drains_chunks_in_order_and_reports_done() {
        let (_d, ctx) = test_ctx(None).await;
        let (base, server) = start_stream_server(vec![
            r#"{"n":1}"#.to_string(),
            r#"{"n":2}"#.to_string(),
            r#"{"n":3}"#.to_string(),
        ])
        .await;

        let started = run_internal(
            "http_stream_start",
            &json!({"url": format!("{base}/stream")}),
            &ctx,
        )
        .await
        .unwrap();
        let stream_id = started.get("stream_id").and_then(Value::as_str).unwrap();

        // Long-poll until the stream reports done — the writer trickles
        // chunks in with a delay, so a single immediate poll may race ahead
        // of them.
        let mut chunks = Vec::new();
        let mut cursor = 0i64;
        for _ in 0..50 {
            let v = run_internal(
                "http_stream_poll",
                &json!({"stream_id": stream_id, "cursor": cursor, "wait_secs": 2}),
                &ctx,
            )
            .await
            .unwrap();
            chunks.extend(v.get("chunks").and_then(Value::as_array).cloned().unwrap_or_default());
            cursor = v.get("next_cursor").and_then(Value::as_i64).unwrap();
            if v.get("done").and_then(Value::as_bool) == Some(true) {
                break;
            }
        }

        assert_eq!(chunks.len(), 3, "{chunks:?}");
        assert_eq!(chunks[0].get("n").and_then(Value::as_i64), Some(1));
        assert_eq!(chunks[1].get("n").and_then(Value::as_i64), Some(2));
        assert_eq!(chunks[2].get("n").and_then(Value::as_i64), Some(3));

        run_internal("http_stream_close", &json!({"stream_id": stream_id}), &ctx)
            .await
            .unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn http_stream_poll_missing_cursor_reads_from_the_start() {
        let (_d, ctx) = test_ctx(None).await;
        let (base, server) = start_stream_server(vec![r#"{"n":1}"#.to_string()]).await;

        let started = run_internal(
            "http_stream_start",
            &json!({"url": format!("{base}/stream")}),
            &ctx,
        )
        .await
        .unwrap();
        let stream_id = started.get("stream_id").and_then(Value::as_str).unwrap();

        let v = run_internal(
            "http_stream_poll",
            &json!({"stream_id": stream_id, "wait_secs": 2}),
            &ctx,
        )
        .await
        .unwrap();
        let chunks = v.get("chunks").and_then(Value::as_array).cloned().unwrap_or_default();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].get("n").and_then(Value::as_i64), Some(1));

        run_internal("http_stream_close", &json!({"stream_id": stream_id}), &ctx)
            .await
            .unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn http_stream_poll_unknown_stream_id_errors() {
        let (_d, ctx) = test_ctx(None).await;
        let err = run_internal("http_stream_poll", &json!({"stream_id": "nope"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.contains("no stream registered"), "{err}");
    }

    #[tokio::test]
    async fn http_stream_close_aborts_and_further_polls_fail() {
        let (_d, ctx) = test_ctx(None).await;
        let (base, server) = start_stream_server(vec![r#"{"n":1}"#.to_string()]).await;

        let started = run_internal(
            "http_stream_start",
            &json!({"url": format!("{base}/stream")}),
            &ctx,
        )
        .await
        .unwrap();
        let stream_id = started.get("stream_id").and_then(Value::as_str).unwrap().to_string();

        let closed = run_internal("http_stream_close", &json!({"stream_id": stream_id}), &ctx)
            .await
            .unwrap();
        assert_eq!(closed.get("closed").and_then(Value::as_bool), Some(true));

        let err = run_internal("http_stream_poll", &json!({"stream_id": stream_id}), &ctx)
            .await
            .unwrap_err();
        assert!(err.contains("no stream registered"), "{err}");

        // Closing again is a no-op, not an error.
        let closed_again = run_internal("http_stream_close", &json!({"stream_id": stream_id}), &ctx)
            .await
            .unwrap();
        assert_eq!(closed_again.get("closed").and_then(Value::as_bool), Some(false));

        server.abort();
    }

    #[tokio::test]
    async fn http_stream_reports_dropped_chunks_once_over_the_buffer_cap() {
        let (dir, ctx) = test_ctx(None).await;
        let cfg = solx_config::ConfigService::open_in(dir.path()).unwrap();
        cfg.patch(json!({ "http_stream_max_buffer_bytes": 1 })).unwrap();
        // `test_ctx`'s InternalCtx holds its own Arc<ConfigService> over the
        // same directory; reopening and patching here mutates the same
        // on-disk config, which the handler reads fresh via `snapshot()`.

        let (base, server) = start_stream_server(vec![
            r#"{"n":1}"#.to_string(),
            r#"{"n":2}"#.to_string(),
            r#"{"n":3}"#.to_string(),
        ])
        .await;

        let started = run_internal(
            "http_stream_start",
            &json!({"url": format!("{base}/stream")}),
            &ctx,
        )
        .await
        .unwrap();
        let stream_id = started.get("stream_id").and_then(Value::as_str).unwrap();

        let mut last = json!({});
        for _ in 0..50 {
            last = run_internal(
                "http_stream_poll",
                &json!({"stream_id": stream_id, "cursor": 0, "wait_secs": 2}),
                &ctx,
            )
            .await
            .unwrap();
            if last.get("done").and_then(Value::as_bool) == Some(true) {
                break;
            }
        }
        assert!(
            last.get("dropped").and_then(Value::as_i64).unwrap_or(0) > 0,
            "{last:?}"
        );

        run_internal("http_stream_close", &json!({"stream_id": stream_id}), &ctx)
            .await
            .unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn http_stream_self_terminates_after_idle_ttl() {
        let (dir, ctx) = test_ctx(None).await;
        let cfg = solx_config::ConfigService::open_in(dir.path()).unwrap();
        cfg.patch(json!({ "http_stream_idle_ttl_secs": 1 })).unwrap();

        let (base, server) = start_stream_server(vec![r#"{"n":1}"#.to_string()]).await;

        let started = run_internal(
            "http_stream_start",
            &json!({"url": format!("{base}/hang")}),
            &ctx,
        )
        .await
        .unwrap();
        let stream_id = started.get("stream_id").and_then(Value::as_str).unwrap().to_string();

        // Poll once so the reader task starts, then don't poll again until
        // past the (patched, 1s) idle TTL.
        run_internal(
            "http_stream_poll",
            &json!({"stream_id": stream_id, "cursor": 0}),
            &ctx,
        )
        .await
        .unwrap();

        tokio::time::sleep(Duration::from_millis(1500)).await;

        let v = run_internal("http_stream_poll", &json!({"stream_id": stream_id}), &ctx)
            .await
            .unwrap();
        assert_eq!(v.get("done").and_then(Value::as_bool), Some(true));
        let error = v.get("error").and_then(Value::as_str).unwrap_or_default();
        assert!(error.contains("idle"), "{v:?}");

        server.abort();
    }

    // ── action consoles ──────────────────────────────────────────────────

    /// Mirrors `secrets_are_refused_without_an_action_caller`: the CLI, MCP,
    /// and HTTP route all reach built-ins with no action caller, so
    /// `console_print` — which always targets *the calling action's own*
    /// console — has nothing to resolve.
    #[tokio::test]
    async fn console_print_is_refused_without_an_action_caller() {
        let (_d, ctx) = test_ctx(None).await;
        assert!(ctx.caller.is_none());
        let err = run_internal("console_print", &json!({"message": "hi"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.contains("no action caller"), "{err}");
    }

    #[tokio::test]
    async fn console_print_writes_to_the_callers_own_console() {
        let (_d, mut ctx) = test_ctx(None).await;
        ctx.caller = Some(Caller::from_action("/pkg/foo", None));

        let v = run_internal("console_print", &json!({"message": "hello"}), &ctx)
            .await
            .unwrap();
        assert_eq!(v.get("seq").and_then(Value::as_i64), Some(1));

        let read = run_internal("console_read", &json!({"action_ref": "/pkg/foo"}), &ctx)
            .await
            .unwrap();
        let entries = read.get("entries").and_then(Value::as_array).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].get("message").and_then(Value::as_str), Some("hello"));
        assert_eq!(entries[0].get("source").and_then(Value::as_str), Some("guest"));
        assert_eq!(entries[0].get("level").and_then(Value::as_str), Some("info"));
    }

    #[tokio::test]
    async fn console_print_defaults_level_and_honors_an_explicit_one() {
        let (_d, mut ctx) = test_ctx(None).await;
        ctx.caller = Some(Caller::from_action("/pkg/foo", None));

        run_internal("console_print", &json!({"message": "a"}), &ctx).await.unwrap();
        run_internal("console_print", &json!({"level": "warn", "message": "b"}), &ctx)
            .await
            .unwrap();

        let read = run_internal("console_read", &json!({"action_ref": "/pkg/foo"}), &ctx)
            .await
            .unwrap();
        let entries = read.get("entries").and_then(Value::as_array).unwrap();
        assert_eq!(entries[0].get("level").and_then(Value::as_str), Some("info"));
        assert_eq!(entries[1].get("level").and_then(Value::as_str), Some("warn"));
    }

    #[tokio::test]
    async fn console_read_requires_action_ref() {
        let (_d, ctx) = test_ctx(None).await;
        let err = run_internal("console_read", &json!({}), &ctx).await.unwrap_err();
        assert!(err.contains("missing required param: action_ref"), "{err}");
    }

    #[tokio::test]
    async fn console_read_of_an_unwritten_console_is_empty_not_an_error() {
        let (_d, ctx) = test_ctx(None).await;
        let v = run_internal("console_read", &json!({"action_ref": "/never/printed"}), &ctx)
            .await
            .unwrap();
        assert_eq!(v.get("entries").and_then(Value::as_array).map(Vec::len), Some(0));
    }

    #[tokio::test]
    async fn console_read_respects_from_seq_cursor() {
        let (_d, mut ctx) = test_ctx(None).await;
        ctx.caller = Some(Caller::from_action("/pkg/foo", None));
        for i in 0..3 {
            run_internal("console_print", &json!({"message": format!("m{i}")}), &ctx)
                .await
                .unwrap();
        }

        let v = run_internal(
            "console_read",
            &json!({"action_ref": "/pkg/foo", "from_seq": 2}),
            &ctx,
        )
        .await
        .unwrap();
        let entries = v.get("entries").and_then(Value::as_array).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].get("seq").and_then(Value::as_i64), Some(2));
        assert_eq!(v.get("next_cursor").and_then(Value::as_i64), Some(4));
    }

    /// A caller does *not* need to be the action it's reading — this is
    /// what lets an orchestrator watch a child's console.
    #[tokio::test]
    async fn console_read_is_not_restricted_to_the_callers_own_console() {
        let (_d, mut ctx) = test_ctx(None).await;
        ctx.caller = Some(Caller::from_action("/pkg/child", None));
        run_internal("console_print", &json!({"message": "child status"}), &ctx)
            .await
            .unwrap();

        // Read it back as an unrelated caller (here: no caller at all).
        ctx.caller = None;
        let v = run_internal("console_read", &json!({"action_ref": "/pkg/child"}), &ctx)
            .await
            .unwrap();
        assert_eq!(v.get("entries").and_then(Value::as_array).map(Vec::len), Some(1));
    }

    #[tokio::test]
    async fn console_tail_returns_immediately_when_entries_exist() {
        let (_d, mut ctx) = test_ctx(None).await;
        ctx.caller = Some(Caller::from_action("/pkg/foo", None));
        run_internal("console_print", &json!({"message": "hi"}), &ctx).await.unwrap();
        ctx.caller = None;

        let v = run_internal(
            "console_tail",
            &json!({"action_ref": "/pkg/foo", "wait_secs": 5}),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(v.get("entries").and_then(Value::as_array).map(Vec::len), Some(1));
    }

    #[tokio::test]
    async fn console_clear_drops_from_the_front() {
        let (_d, mut ctx) = test_ctx(None).await;
        ctx.caller = Some(Caller::from_action("/pkg/foo", None));
        for i in 0..3 {
            run_internal("console_print", &json!({"message": format!("m{i}")}), &ctx)
                .await
                .unwrap();
        }
        ctx.caller = None;

        let v = run_internal(
            "console_clear",
            &json!({"action_ref": "/pkg/foo", "before_seq": 2}),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(v.get("removed").and_then(Value::as_i64), Some(1));

        let read = run_internal("console_read", &json!({"action_ref": "/pkg/foo"}), &ctx)
            .await
            .unwrap();
        assert_eq!(read.get("entries").and_then(Value::as_array).map(Vec::len), Some(2));
    }

    #[tokio::test]
    async fn console_list_reflects_written_consoles() {
        let (_d, mut ctx) = test_ctx(None).await;
        ctx.caller = Some(Caller::from_action("/pkg/a", None));
        run_internal("console_print", &json!({"message": "x"}), &ctx).await.unwrap();
        ctx.caller = Some(Caller::from_action("/pkg/b", None));
        run_internal("console_print", &json!({"message": "y"}), &ctx).await.unwrap();
        ctx.caller = None;

        let v = run_internal("console_list", &json!({"prefix": "/pkg"}), &ctx)
            .await
            .unwrap();
        let consoles = v.get("consoles").and_then(Value::as_array).unwrap();
        assert_eq!(consoles.len(), 2);
    }

    #[tokio::test]
    async fn action_cancelled_is_refused_without_an_action_caller() {
        let (_d, ctx) = test_ctx(None).await;
        assert!(ctx.caller.is_none());
        let err = run_internal("action_cancelled", &json!({}), &ctx).await.unwrap_err();
        assert!(err.contains("no action caller"), "{err}");
    }

    #[tokio::test]
    async fn action_cancelled_reflects_the_callers_own_invocation() {
        let (_d, mut ctx) = test_ctx(None).await;
        // A fixed id (not `Caller::from_action`'s random mint) so the test
        // can seed and cancel exactly this invocation's row.
        ctx.caller = Some(Caller::with_invocation("/pkg/foo", None, "inv-fixed"));

        // No row exists yet for "inv-fixed" — must fail closed to `false`,
        // not error, mirroring the loopback's `/cancelled` route.
        let before = run_internal("action_cancelled", &json!({}), &ctx).await.unwrap();
        assert_eq!(before.get("cancelled"), Some(&Value::Bool(false)));

        ctx.local.invocations().create("inv-fixed", "/pkg/foo", 0).await.unwrap();
        ctx.local.invocations().request_cancel("inv-fixed").await.unwrap();

        let after = run_internal("action_cancelled", &json!({}), &ctx).await.unwrap();
        assert_eq!(after.get("cancelled"), Some(&Value::Bool(true)));
    }
}
