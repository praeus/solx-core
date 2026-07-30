//! Dispatcher-level native handler for internal actions.
//!
//! Internal actions are dispatched by `fn_name` — no WASM, no shell, no HTTP.
//! This is also where the built-in catalogue's entity CRUD, search,
//! file-store, document-field, environment-store, HTML-fetch, and secrets
//! operations live — all of it used to be a WASM component
//! (`solx-builtin-actions`, now removed) executed under wasmtime's trusted
//! `backend-action` world; native dispatch is strictly simpler (no
//! sync/async host-function bridging, no separate guest build/packaging
//! step) and was already how everything else in this module worked. WASM
//! now exists solely for third-party *custom* actions
//! (`crate::wasm_host`), which reach every one of these same operations
//! recursively via `action-exec` — no separate WASM ABI needed for them.
//!
//! One handler group predates all that: the OAuth 2.0 authorization-code
//! loopback controller, which drives the lifecycle of one or more loopback
//! HTTP listeners (defined in [`crate::oauth_loopback`]) that capture the
//! redirect from any RFC 6749 / RFC 8252 provider.
//!
//! Three modes drive the OAuth loopback:
//!
//! * `oauth_start` — binds `127.0.0.1:{port}` (default `8765`), generates a
//!   random `state_value` (CSRF token), registers a pending
//!   `oneshot::Receiver` for the callback. Returns the `port`,
//!   `redirect_uri`, and `state_value`.
//! * `oauth_await` — blocks until the registered loopback for
//!   `state_value` receives the provider's redirect (or until the loopback
//!   is stopped). Returns the captured `code` / `error`.
//! * `oauth_stop` — triggers graceful shutdown of the loopback for
//!   `state_value` and drops the inbox receiver.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use base64::Engine as _;
use serde_json::{json, Value};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use solx_surface::entities::{ActionInput, Document, DocumentInput, TypeInput};
use solx_surface::managers::{ActionManager, DocManager, FileStore, TypeManager};
use solx_surface::query::{ListOptions, SearchQuery};

use crate::oauth_loopback::{self, LoopbackResult, LoopbackState};

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
    /// The *calling* action's own `action_config` (the row whose `fn_name`
    /// dispatched to this handler) — `get_secret`/`set_secret` scope to
    /// whichever action is currently executing, exactly like the WASM host
    /// functions they replace did (see `resolve_secret_key`).
    pub action_config: Option<Value>,
}

// ── Registry ─────────────────────────────────────────────────────────────────

/// Per-listener handle held in the registry.
struct LoopbackHandle {
    shutdown_tx: oneshot::Sender<()>,
    #[allow(dead_code)]
    server_handle: JoinHandle<()>,
}

/// Registry of active loopback listeners, keyed by their `state_value`.
static REGISTRY: OnceLock<Mutex<HashMap<String, LoopbackHandle>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<String, LoopbackHandle>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

// ── Receiver inbox ───────────────────────────────────────────────────────────

type InboxMap = HashMap<String, oneshot::Receiver<LoopbackResult>>;
static INBOX: OnceLock<Mutex<InboxMap>> = OnceLock::new();

fn inbox() -> &'static Mutex<InboxMap> {
    INBOX.get_or_init(|| Mutex::new(HashMap::new()))
}

fn inbox_put(state_value: String, rx: oneshot::Receiver<LoopbackResult>) {
    if let Ok(mut m) = inbox().lock() {
        m.insert(state_value, rx);
    }
}

fn inbox_drop(state_value: &str) {
    if let Ok(mut m) = inbox().lock() {
        m.remove(state_value);
    }
}

// ── CSRF state ───────────────────────────────────────────────────────────────

fn generate_state_value() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut s = String::with_capacity(32);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ── Public entry point ───────────────────────────────────────────────────────

/// Dispatch an internal action by `fn_name`. Returns the JSON result value
/// (the caller wraps it in `ActionExecResult`).
pub async fn run_internal(fn_name: &str, params: &Value, ctx: &InternalCtx) -> Result<Value, String> {
    match fn_name {
        "oauth_start" => oauth_start(params).await,
        "oauth_await" => oauth_await(params).await,
        "oauth_stop" => oauth_stop(params).await,

        "entity_post_document" => doc_post(params, &ctx.docs).await,
        "entity_get_document" => doc_get(params, &ctx.docs).await,
        "entity_delete_document" => doc_delete(params, &ctx.docs).await,
        "entity_list_documents" => doc_list(params, &ctx.docs).await,

        "entity_post_type" => type_post(params, &ctx.types).await,
        "entity_get_type" => type_get(params, &ctx.types).await,
        "entity_delete_type" => type_delete(params, &ctx.types).await,
        "entity_list_types" => type_list(params, &ctx.types).await,

        "entity_post_action" => action_post(params, &ctx.actions).await,
        "entity_get_action" => action_get(params, &ctx.actions).await,
        "entity_delete_action" => action_delete(params, &ctx.actions).await,
        "entity_list_actions" => action_list(params, &ctx.actions).await,

        "search_documents" => search_documents(params, &ctx.docs).await,
        "search_actions" => search_actions(params, &ctx.actions).await,

        "file_put" => file_put(params, &ctx.files).await,
        "file_get" => file_get(params, &ctx.files).await,
        "file_delete" => file_delete(params, &ctx.files).await,
        "file_list" => file_list(params, &ctx.files).await,
        "file_copy" => file_copy(params, &ctx.files).await,
        "dir_copy" => dir_copy(params, &ctx.files).await,

        "get_field" => get_field(params, &ctx.docs).await,
        "set_field" => set_field(params, &ctx.docs).await,

        "get_env" => Ok(get_env(params)),
        "set_env" => Ok(set_env(params)),
        "fetch_html" => fetch_html(params).await,

        "get_secret" => Ok(get_secret(params, ctx.action_config.as_ref())),
        "set_secret" => set_secret(params, ctx.action_config.as_ref()),

        other => Err(format!("unknown internal fn_name '{other}'")),
    }
}

// ── param helpers ────────────────────────────────────────────────────────────

fn require_str<'a>(params: &'a Value, field: &str) -> Result<&'a str, String> {
    params
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing required param: {field}"))
}

fn path_or_root(params: &Value) -> &str {
    params.get("path").and_then(Value::as_str).unwrap_or("/")
}

fn parse_input<T: serde::de::DeserializeOwned>(params: &Value) -> Result<T, String> {
    serde_json::from_value(params.clone()).map_err(|e| format!("invalid params: {e}"))
}

fn to_value<T: serde::Serialize>(v: &T) -> Result<Value, String> {
    serde_json::to_value(v).map_err(|e| format!("failed to serialize result: {e}"))
}

// ── entity CRUD (fixes entity_ops' broken `path` handling — see seed.rs) ─────

async fn doc_post(params: &Value, docs: &Arc<dyn DocManager>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    let input: DocumentInput = parse_input(params)?;
    let doc = docs.post(path_or_root(params), name, input).await.map_err(|e| e.to_string())?;
    to_value(&doc)
}

async fn doc_get(params: &Value, docs: &Arc<dyn DocManager>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    let doc = docs.get(path_or_root(params), name).await.map_err(|e| e.to_string())?;
    to_value(&doc)
}

async fn doc_delete(params: &Value, docs: &Arc<dyn DocManager>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    docs.delete(path_or_root(params), name).await.map_err(|e| e.to_string())?;
    Ok(json!({ "deleted": true }))
}

async fn doc_list(params: &Value, docs: &Arc<dyn DocManager>) -> Result<Value, String> {
    let opts: ListOptions = parse_input(params)?;
    let page = docs.list(opts).await.map_err(|e| e.to_string())?;
    to_value(&page)
}

async fn type_post(params: &Value, types: &Arc<dyn TypeManager>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    let input: TypeInput = parse_input(params)?;
    let ty = types.post(path_or_root(params), name, input).await.map_err(|e| e.to_string())?;
    to_value(&ty)
}

async fn type_get(params: &Value, types: &Arc<dyn TypeManager>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    let ty = types.get(path_or_root(params), name).await.map_err(|e| e.to_string())?;
    to_value(&ty)
}

async fn type_delete(params: &Value, types: &Arc<dyn TypeManager>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    types.delete(path_or_root(params), name).await.map_err(|e| e.to_string())?;
    Ok(json!({ "deleted": true }))
}

async fn type_list(params: &Value, types: &Arc<dyn TypeManager>) -> Result<Value, String> {
    let opts: ListOptions = parse_input(params)?;
    let page = types.list(opts).await.map_err(|e| e.to_string())?;
    to_value(&page)
}

async fn action_post(params: &Value, actions: &Arc<dyn ActionManager>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    let input: ActionInput = parse_input(params)?;
    let a = actions.post(path_or_root(params), name, input).await.map_err(|e| e.to_string())?;
    to_value(&a)
}

async fn action_get(params: &Value, actions: &Arc<dyn ActionManager>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    let a = actions.get(path_or_root(params), name).await.map_err(|e| e.to_string())?;
    to_value(&a)
}

async fn action_delete(params: &Value, actions: &Arc<dyn ActionManager>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    actions.delete(path_or_root(params), name).await.map_err(|e| e.to_string())?;
    Ok(json!({ "deleted": true }))
}

async fn action_list(params: &Value, actions: &Arc<dyn ActionManager>) -> Result<Value, String> {
    let opts: ListOptions = parse_input(params)?;
    let page = actions.list(opts).await.map_err(|e| e.to_string())?;
    to_value(&page)
}

// ── search ───────────────────────────────────────────────────────────────────

async fn search_documents(params: &Value, docs: &Arc<dyn DocManager>) -> Result<Value, String> {
    let query: SearchQuery = parse_input(params)?;
    let results = docs.search(query).await.map_err(|e| e.to_string())?;
    to_value(&results)
}

/// Structured filtering over the action catalogue — actions have no
/// full-text index (only documents do, via Tantivy), so this is `list` with
/// `ListOptions`' filters, not fuzzy relevance ranking.
async fn search_actions(params: &Value, actions: &Arc<dyn ActionManager>) -> Result<Value, String> {
    let opts: ListOptions = parse_input(params)?;
    let page = actions.list(opts).await.map_err(|e| e.to_string())?;
    to_value(&page)
}

// ── file store (general-purpose, unrestricted — Internal actions carry the
// same trust level as the dispatcher itself, same as OAuth above; there is
// no sandboxing model for them the way there is for untrusted WASM guests)
// ──────────────────────────────────────────────────────────────────────────

async fn file_put(params: &Value, files: &Arc<dyn FileStore>) -> Result<Value, String> {
    let rel_path = require_str(params, "rel_path")?;
    let content = require_str(params, "content")?;
    let encoding = params.get("encoding").and_then(Value::as_str).unwrap_or("utf8");
    let bytes = match encoding {
        "utf8" => content.as_bytes().to_vec(),
        "base64" => base64::engine::general_purpose::STANDARD
            .decode(content)
            .map_err(|e| format!("invalid base64 content: {e}"))?,
        other => return Err(format!("unknown encoding '{other}'; expected \"utf8\" or \"base64\"")),
    };
    let stored = files.put(rel_path, bytes).await.map_err(|e| e.to_string())?;
    Ok(json!({ "rel_path": stored }))
}

async fn file_get(params: &Value, files: &Arc<dyn FileStore>) -> Result<Value, String> {
    let rel_path = require_str(params, "rel_path")?;
    let bytes = files.get(rel_path).await.map_err(|e| e.to_string())?;
    let (content, encoding) = match String::from_utf8(bytes.clone()) {
        Ok(s) => (s, "utf8"),
        Err(_) => (
            base64::engine::general_purpose::STANDARD.encode(&bytes),
            "base64",
        ),
    };
    Ok(json!({ "rel_path": rel_path, "encoding": encoding, "content": content }))
}

async fn file_delete(params: &Value, files: &Arc<dyn FileStore>) -> Result<Value, String> {
    let rel_path = require_str(params, "rel_path")?;
    files.delete(rel_path).await.map_err(|e| e.to_string())?;
    Ok(json!({ "deleted": true }))
}

async fn file_list(params: &Value, files: &Arc<dyn FileStore>) -> Result<Value, String> {
    let prefix = params.get("prefix").and_then(Value::as_str).unwrap_or("");
    let entries = files.list(prefix).await.map_err(|e| e.to_string())?;
    Ok(json!({ "files": entries }))
}

async fn file_copy(params: &Value, files: &Arc<dyn FileStore>) -> Result<Value, String> {
    let source = require_str(params, "source")?;
    let dest = require_str(params, "dest")?;
    let bytes = files.get(source).await.map_err(|e| e.to_string())?;
    files.put(dest, bytes).await.map_err(|e| e.to_string())?;
    Ok(json!({ "dest": dest }))
}

/// Recursively copies every entry under `source` to the matching relative
/// position under `dest`.
async fn dir_copy(params: &Value, files: &Arc<dyn FileStore>) -> Result<Value, String> {
    let source = require_str(params, "source")?;
    let dest = require_str(params, "dest")?;
    let source_prefix = source.trim_end_matches('/');
    let dest_prefix = dest.trim_end_matches('/');
    let entries = files.list(source).await.map_err(|e| e.to_string())?;
    let mut copied = Vec::with_capacity(entries.len());
    for entry in &entries {
        let rel = entry.strip_prefix(source_prefix).unwrap_or(entry).trim_start_matches('/');
        let dest_entry = format!("{dest_prefix}/{rel}");
        let bytes = files.get(entry).await.map_err(|e| e.to_string())?;
        files.put(&dest_entry, bytes).await.map_err(|e| e.to_string())?;
        copied.push(dest_entry);
    }
    Ok(json!({ "copied": copied }))
}

// ── document field ops (legacy flat get/set, one field at a time) ───────────

async fn load_document(docs: &Arc<dyn DocManager>, path: &str, name: &str) -> Result<Document, String> {
    docs.get(path, name).await.map_err(|e| e.to_string())
}

fn document_input_from(doc: &Document) -> DocumentInput {
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

async fn get_field(params: &Value, docs: &Arc<dyn DocManager>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    let field = require_str(params, "field")?;
    let doc = load_document(docs, path_or_root(params), name).await?;
    Ok(doc.contents.get(field).cloned().unwrap_or(Value::Null))
}

async fn set_field(params: &Value, docs: &Arc<dyn DocManager>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    let field = require_str(params, "field")?;
    let value = params.get("value").cloned().unwrap_or(Value::Null);
    let path = path_or_root(params).to_string();
    let existing = load_document(docs, &path, name).await?;
    let mut input = document_input_from(&existing);
    match input.contents.as_object_mut() {
        Some(obj) => {
            obj.insert(field.to_string(), value);
        }
        None => {
            let mut obj = serde_json::Map::new();
            obj.insert(field.to_string(), value);
            input.contents = Value::Object(obj);
        }
    }
    let doc = docs.post(&path, name, input).await.map_err(|e| e.to_string())?;
    to_value(&doc)
}

// ── in-process environment store ─────────────────────────────────────────────
//
// A generic process-lifetime keyed scratch store: read-only entries seeded
// once at startup from `SolxConfig.env_mappings` (via `init_env_mappings`),
// plus anything subsequently written by `set_env`. No relation to the real
// process environment — this used to back only WASM guests; it's a plain
// Internal action now, reachable by anyone (including WASM guests, via
// `action-exec`).

static ENV_STORE: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();

fn env_store() -> &'static RwLock<HashMap<String, String>> {
    ENV_STORE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Seed the environment store from the `env_mappings` allowlist:
/// `key -> system_env_var_name`. Reads each system env var once, at call
/// time, and stores it under `key`. Call once at startup.
pub fn init_env_mappings(mappings: HashMap<String, String>) {
    let mut resolved = HashMap::new();
    for (key, sys_key) in mappings {
        if let Ok(v) = std::env::var(&sys_key) {
            resolved.insert(key, v);
        }
    }
    if let Ok(mut map) = env_store().write() {
        map.extend(resolved);
    }
}

fn get_env(params: &Value) -> Value {
    let key = params.get("key").and_then(Value::as_str).unwrap_or("");
    let value = env_store().read().ok().and_then(|m| m.get(key).cloned());
    json!({ "value": value })
}

fn set_env(params: &Value) -> Value {
    let key = params.get("key").and_then(Value::as_str).unwrap_or("").to_string();
    let value = params.get("value").and_then(Value::as_str).unwrap_or("").to_string();
    if let Ok(mut m) = env_store().write() {
        m.insert(key, value);
    }
    json!({ "set": true })
}

// ── fetch_html ────────────────────────────────────────────────────────────────

async fn fetch_html(params: &Value) -> Result<Value, String> {
    let url = require_str(params, "url")?;
    let resp = reqwest::get(url).await.map_err(|e| format!("fetch failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("fetch returned HTTP {}", resp.status()));
    }
    let text = resp.text().await.map_err(|e| e.to_string())?;
    Ok(json!({ "html": text }))
}

// ── secrets ──────────────────────────────────────────────────────────────────
//
// Scoped to whichever action row is currently executing (`ctx.action_config`,
// set once by `LocalActionManager::exec` before dispatch) — the same
// scoping the WASM `secrets` host functions used, just relocated. An action
// wanting `get_secret`/`set_secret` must itself have a `name -> key`
// mapping in its own `action_config.secrets`.

fn resolve_secret_key(action_config: Option<&Value>, name: &str) -> Option<String> {
    action_config?.get("secrets")?.get(name)?.as_str().map(str::to_string)
}

fn get_secret(params: &Value, action_config: Option<&Value>) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let value = resolve_secret_key(action_config, name)
        .and_then(|key| crate::secrets::get_secret(name, &key).ok().flatten());
    json!({ "value": value })
}

fn set_secret(params: &Value, action_config: Option<&Value>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    let value = require_str(params, "value")?;
    let key = resolve_secret_key(action_config, name).ok_or_else(|| {
        format!("no key configured for secret '{name}' in the calling action's action_config.secrets")
    })?;
    crate::secrets::set_secret(name, value, &key)?;
    Ok(json!({ "set": true }))
}

// ── oauth_start ──────────────────────────────────────────────────────────────

async fn oauth_start(params: &Value) -> Result<Value, String> {
    let port = params
        .get("port")
        .and_then(Value::as_u64)
        .map(|p| p as u16)
        .unwrap_or(oauth_loopback::DEFAULT_LOOPBACK_PORT);
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);

    // Bind eagerly, before registering any state, so a port-in-use failure
    // (e.g. a second `oauth_start` on the default port before the first is
    // stopped) surfaces immediately as an `Err` here — rather than being
    // silently swallowed inside the spawned server task, which would leave
    // behind a `"started": true` state_value that can never actually
    // receive a callback.
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("failed to bind oauth loopback on {addr}: {e}"))?;

    let state_value = generate_state_value();
    let state = Arc::new(LoopbackState::new());
    let receiver = state.register(state_value.clone())?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server_state = Arc::clone(&state);
    let server_handle = tokio::spawn(async move {
        let _ = oauth_loopback::serve_loopback_with_shutdown_on(
            server_state,
            listener,
            async move {
                let _ = shutdown_rx.await;
            },
        )
        .await;
    });

    inbox_put(state_value.clone(), receiver);

    if let Ok(mut reg) = registry().lock() {
        reg.insert(
            state_value.clone(),
            LoopbackHandle {
                shutdown_tx,
                server_handle,
            },
        );
    }

    let redirect_uri = oauth_loopback::redirect_uri(port);
    let started_at = chrono::Utc::now().to_rfc3339();

    Ok(json!({
        "started": true,
        "port": port,
        "redirect_uri": redirect_uri,
        "state_value": state_value,
        "started_at": started_at,
    }))
}

// ── oauth_await ──────────────────────────────────────────────────────────────

async fn oauth_await(params: &Value) -> Result<Value, String> {
    let state_value = params
        .get("state_value")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing required param: state_value".to_string())?
        .to_string();

    let timeout_secs = params.get("timeout_secs").and_then(Value::as_u64);

    // Take the receiver out of the inbox up front. On timeout we put it
    // back (see below) so a subsequent `oauth_await` call for the same
    // state_value can still succeed if the callback arrives later — the
    // underlying loopback listener keeps running independently of this
    // call.
    let mut rx = inbox()
        .lock()
        .ok()
        .and_then(|mut m| m.remove(&state_value))
        .ok_or_else(|| format!("no loopback registered for state_value '{state_value}'"))?;

    let loopback = match timeout_secs {
        Some(secs) => {
            // `&mut rx` (rather than `rx.await`) keeps `rx` owned by this
            // stack frame — tokio's oneshot Receiver is cancel-safe when
            // polled this way, so if the sleep branch fires first, `rx`
            // is still valid afterward and can be reinserted rather than
            // being dropped along with a cancelled `tokio::time::timeout`
            // future (which would lose it permanently).
            tokio::select! {
                result = &mut rx => {
                    match result {
                        Ok(res) => res,
                        Err(_) => return Err(format!(
                            "loopback for state '{state_value}' was stopped before the callback arrived"
                        )),
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(secs)) => {
                    inbox_put(state_value.clone(), rx);
                    return Err(format!(
                        "oauth callback timed out after {secs}s for state_value \
                         '{state_value}'; the loopback is still running and a later \
                         oauth_await call for the same state_value may still succeed"
                    ));
                }
            }
        }
        None => match rx.await {
            Ok(res) => res,
            Err(_) => {
                return Err(format!(
                    "loopback for state '{state_value}' was stopped before the callback arrived"
                ))
            }
        },
    };

    let succeeded = loopback.succeeded();
    let mut obj = serde_json::Map::new();
    obj.insert("state_value".into(), Value::String(state_value));
    if let Some(code) = &loopback.code {
        obj.insert("code".into(), Value::String(code.clone()));
    }
    if let Some(state) = &loopback.state {
        obj.insert("state".into(), Value::String(state.clone()));
    }
    if let Some(error) = &loopback.error {
        obj.insert("error".into(), Value::String(error.clone()));
    }
    if let Some(error_description) = &loopback.error_description {
        obj.insert(
            "error_description".into(),
            Value::String(error_description.clone()),
        );
    }
    obj.insert("succeeded".into(), Value::Bool(succeeded));

    Ok(Value::Object(obj))
}

// ── oauth_stop ───────────────────────────────────────────────────────────────

async fn oauth_stop(params: &Value) -> Result<Value, String> {
    let state_value = params
        .get("state_value")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing required param: state_value".to_string())?
        .to_string();

    let handle = registry()
        .lock()
        .map_err(|e| format!("loopback registry poisoned: {e}"))?
        .remove(&state_value);

    inbox_drop(&state_value);

    match handle {
        Some(LoopbackHandle { shutdown_tx, .. }) => {
            let _ = shutdown_tx.send(());
            Ok(json!({ "stopped": true, "state_value": state_value }))
        }
        None => Ok(json!({
            "stopped": false,
            "state_value": state_value,
            "error": format!("no loopback registered for state_value '{state_value}'"),
        })),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a manual callback for `state_value` without spinning up a
    /// real axum server — directly installs a `oneshot::Receiver` in the
    /// inbox that resolves to `result` when awaited.
    fn install_test_callback(state_value: &str, result: LoopbackResult) {
        let (tx, rx) = oneshot::channel();
        inbox_put(state_value.into(), rx);
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
        let actions: Arc<dyn ActionManager> = actions_concrete.clone();
        actions_concrete.set_self_ref(Arc::downgrade(&actions));
        (dir, InternalCtx { docs, types, actions, files, action_config })
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

    #[tokio::test]
    async fn get_secret_and_set_secret_require_configured_key() {
        let (_d, ctx) = test_ctx(None).await;
        // No action_config at all — no key configured for the secret.
        let v = run_internal("get_secret", &json!({"name": "FOO"}), &ctx).await.unwrap();
        assert_eq!(v.get("value").cloned(), Some(Value::Null));
        let err = run_internal("set_secret", &json!({"name": "FOO", "value": "x"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.contains("no key configured"), "{err}");
    }

    #[test]
    fn generate_state_value_is_unique_and_hex() {
        let a = generate_state_value();
        let b = generate_state_value();
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
        inbox_put("retry-state".into(), rx);

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
}
