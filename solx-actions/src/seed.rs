//! Built-in action catalogue, seeded into the actions database on open.
//! Mirrors `solx-types::seed`'s pattern: a static catalogue +
//! `INSERT OR IGNORE` so re-seeding on every startup is idempotent.
//!
//! Every built-in is dispatched natively (`action_type='internal'`, see
//! `crate::internal`) — there is no WASM-hosted built-in catalogue anymore.
//! Entity CRUD, search, general-purpose file-store access, document field
//! ops, an environment scratch store, HTML fetch, and scoped secrets all
//! used to be a `solx-builtin-actions` WASM component executed under
//! wasmtime's trusted `backend-action` world; that world (and the
//! component) is gone. WASM now exists solely for third-party *custom*
//! actions (`crate::wasm_host`), which reach every one of these same
//! operations recursively via `action-exec` — no separate WASM ABI needed
//! for them.
//!
//! NOTE: seeding is `INSERT OR IGNORE`, so an actions DB seeded before this
//! change keeps its old `action_type='wasm'` rows for these names forever —
//! delete `db/solx-actions.db` once after upgrading to pick them up. The
//! same applies to the `/builtin/<area>/*` reorg below: it moves several
//! actions to a new `path`, which `INSERT OR IGNORE` cannot retarget for an
//! existing row — delete `db/solx-actions.db` once after upgrading past that
//! change too.

use chrono::Utc;
use libsql::Connection;
use solx_surface::error::Result;
use uuid::Uuid;

use crate::db::map_db;

/// Path all built-in actions are seeded under, out of the way of
/// user-created actions. Holds only `random_string` directly — everything
/// else with a natural grouping lives under a `/builtin/<area>` subpath
/// below.
pub const BUILTIN_PATH: &str = "/builtin";

/// Subdivision of the builtin namespace for console operations.
pub const CONSOLE_PATH: &str = "/builtin/console";

/// Subdivision of the builtin namespace for action operations: the
/// asynchronous `start`/`stop`/`poll`/`cancelled` alternative to `exec` (see
/// `docs/async-actions-plan.md`), plus action-entity CRUD and search.
pub const ACTION_PATH: &str = "/builtin/action";

/// Subdivision of the builtin namespace for document-entity CRUD, field
/// ops, and search.
pub const DOCUMENT_PATH: &str = "/builtin/document";

/// Subdivision of the builtin namespace for type-entity CRUD.
pub const TYPE_PATH: &str = "/builtin/type";

/// Subdivision of the builtin namespace for the general-purpose file store.
pub const FILE_PATH: &str = "/builtin/file";

/// Subdivision of the builtin namespace for the environment scratch store.
pub const ENV_PATH: &str = "/builtin/env";

/// Subdivision of the builtin namespace for per-caller-scoped secrets.
pub const SECRETS_PATH: &str = "/builtin/secrets";

/// Subdivision of the builtin namespace for the OAuth 2.0 loopback listener.
pub const OAUTH_PATH: &str = "/builtin/oauth";

/// Subdivision of the builtin namespace for everything that deals in URLs:
/// a one-shot HTTP request, opening a URL in the system browser, and (under
/// `WEB_STREAM_PATH`) host-side streaming HTTP.
pub const WEB_PATH: &str = "/builtin/web";

/// Subdivision of `WEB_PATH` for host-side streaming HTTP (`start`/`poll`/
/// `close`) — for callers with no sockets or cross-call state of their own
/// (chiefly WASM guests). See
/// `solx-packages/solx-ollama/docs/streaming-design.md`.
pub const WEB_STREAM_PATH: &str = "/builtin/web/stream";

/// Subdivision of the builtin namespace for widget operations — mirrors the
/// `widget` WIT interface (`open`/`close`/`show`/`hide`/`get`/`set`/`exec`)
/// as internal actions, so a `.solx` script or any other action can drive a
/// widget without being a WASM guest. See `docs/widget-actions.md` §6.
pub const WIDGET_PATH: &str = "/builtin/widget";

/// Namespace for the hand-written JSON-schema types backing built-in
/// actions' `param_type_ref` (see `solx-types/src/seed.rs`). Shared by every
/// `/builtin/*` subpath — `param_type_ref` doesn't need to mirror the
/// action's own nesting.
pub const BUILTIN_TYPES_PATH: &str = "/builtin/types";

/// One built-in action to seed.
pub struct SeedAction {
    pub path: &'static str,
    /// The entity name. `fn_name` (below) is the dispatch key
    /// `crate::internal::run_internal` matches on — for a flat `/builtin`
    /// entry the two are identical, but a nested entry (e.g. `console/print`)
    /// needs a `name` too generic to double as a global dispatch key, hence
    /// the split.
    pub name: &'static str,
    pub fn_name: &'static str,
    pub description: &'static str,
    /// Name under `BUILTIN_TYPES_PATH` giving this action's `param_type_ref`
    /// a real JSON Schema, if one is seeded (see `solx-types/src/seed.rs`).
    pub param_type: Option<&'static str>,
}

/// A flat `/builtin/<name>` entry — `name` and `fn_name` are the same.
const fn a(name: &'static str, description: &'static str, param_type: Option<&'static str>) -> SeedAction {
    SeedAction { path: BUILTIN_PATH, name, fn_name: name, description, param_type }
}

/// A nested entry under some `path` other than the flat `/builtin` root,
/// with its own dispatch key distinct from its (possibly generic) `name`.
const fn a_at(
    path: &'static str,
    name: &'static str,
    fn_name: &'static str,
    description: &'static str,
    param_type: Option<&'static str>,
) -> SeedAction {
    SeedAction { path, name, fn_name, description, param_type }
}

/// The built-in action catalogue.
pub fn builtin_actions() -> Vec<SeedAction> {
    vec![
        // Document CRUD
        a_at(DOCUMENT_PATH, "entity_save_document", "entity_save_document", "Create or update (upsert) a document.", Some("DocumentCrudParams")),
        a_at(DOCUMENT_PATH, "entity_get_document", "entity_get_document", "Fetch a document by path+name.", Some("EntityRefParams")),
        a_at(DOCUMENT_PATH, "entity_delete_document", "entity_delete_document", "Delete a document.", Some("EntityRefParams")),
        a_at(DOCUMENT_PATH, "entity_list_documents", "entity_list_documents", "List documents, optionally filtered by path prefix.", Some("ListParams")),
        // Document legacy field ops (one field at a time)
        a_at(DOCUMENT_PATH, "get_field", "get_field", "Read one field from a document's contents.", Some("GetFieldParams")),
        a_at(DOCUMENT_PATH, "set_field", "set_field", "Write one field on a document's contents.", Some("SetFieldParams")),
        // Document path-style field ops (nested reads/writes via dotted path)
        a_at(DOCUMENT_PATH, "get_field_at_path", "get_field_at_path", "Read a field at a slash-separated path inside a document's contents.", Some("GetFieldAtPathParams")),
        a_at(DOCUMENT_PATH, "set_field_at_path", "set_field_at_path", "Write a field at a slash-separated path inside a document's contents (optionally creating missing parents).", Some("SetFieldAtPathParams")),
        // Document search (a real Tantivy full-text index)
        a_at(DOCUMENT_PATH, "search_documents", "search_documents", "Full-text + faceted search over documents.", Some("SearchDocumentsParams")),
        // Action CRUD (alongside the async start/stop/poll/cancelled below)
        a_at(ACTION_PATH, "entity_save_action", "entity_save_action", "Create or update (upsert) an action.", Some("ActionCrudParams")),
        a_at(ACTION_PATH, "entity_get_action", "entity_get_action", "Fetch an action by path+name.", Some("EntityRefParams")),
        a_at(ACTION_PATH, "entity_delete_action", "entity_delete_action", "Delete an action.", Some("EntityRefParams")),
        a_at(ACTION_PATH, "entity_list_actions", "entity_list_actions", "List actions, optionally filtered by path prefix.", Some("ListParams")),
        // Action search (actions have no full-text index, so this is a
        // structured filter, not fuzzy relevance ranking)
        a_at(ACTION_PATH, "search_actions", "search_actions", "List/filter actions by path prefix and other fields (structured filter, no full-text index for actions).", Some("ListParams")),
        // Type CRUD
        a_at(TYPE_PATH, "entity_save_type", "entity_save_type", "Create or update (upsert) a type.", Some("TypeCrudParams")),
        a_at(TYPE_PATH, "entity_get_type", "entity_get_type", "Fetch a type by path+name.", Some("EntityRefParams")),
        a_at(TYPE_PATH, "entity_delete_type", "entity_delete_type", "Delete a type.", Some("EntityRefParams")),
        a_at(TYPE_PATH, "entity_list_types", "entity_list_types", "List types, optionally filtered by path prefix.", Some("ListParams")),
        // General-purpose file store (unrestricted rel_path access)
        a_at(FILE_PATH, "file_put", "file_put", "Write bytes to a rel-path under the files root.", Some("FilePutParams")),
        a_at(FILE_PATH, "file_get", "file_get", "Read bytes from a rel-path under the files root.", Some("FileGetParams")),
        a_at(FILE_PATH, "file_delete", "file_delete", "Delete a file at a rel-path under the files root.", Some("FileGetParams")),
        a_at(FILE_PATH, "file_list", "file_list", "List stored rel-paths under a prefix.", Some("FileListParams")),
        a_at(FILE_PATH, "file_copy", "file_copy", "Copy a file within the files root.", Some("FileCopyParams")),
        a_at(FILE_PATH, "dir_copy", "dir_copy", "Recursively copy a directory within the files root.", Some("FileCopyParams")),
        a_at(FILE_PATH, "dir_delete", "dir_delete", "Recursively delete a directory (and every file under it) within the files root.", Some("DirDeleteParams")),
        // Environment scratch store
        a_at(ENV_PATH, "get_env", "get_env", "Read a variable from the environment store, optionally from a named namespace.", Some("GetEnvParams")),
        a_at(ENV_PATH, "set_env", "set_env", "Write a variable to the environment store. In-memory by default; pass persist to also store it in solx-config.json so it survives a restart.", Some("SetEnvParams")),
        // Small utility built-ins. `now`/`uuid`/`random_int` were removed as
        // not model/user-facing and unused by any package script;
        // `random_string` stays — it's load-bearing for `solx-google`'s
        // install script (per-install secret-encryption key generation). Too
        // small a group to justify its own subpath, so it stays flat.
        a("random_string", "Return a random alphanumeric string of the given length.", Some("RandomStringParams")),
        // Secrets, scoped to whichever action is currently executing
        a_at(SECRETS_PATH, "get_secret", "get_secret", "Read a secret scoped to the calling action.", Some("GetSecretParams")),
        a_at(SECRETS_PATH, "set_secret", "set_secret", "Write a secret scoped to the calling action.", Some("SetSecretParams")),
        // OAuth loopback
        a_at(OAUTH_PATH, "oauth_start", "oauth_start", "Start a local OAuth 2.0 authorization-code loopback listener.", Some("OauthStartParams")),
        a_at(OAUTH_PATH, "oauth_await", "oauth_await", "Block until the OAuth loopback for a state_value receives its callback.", Some("OauthAwaitParams")),
        a_at(OAUTH_PATH, "oauth_stop", "oauth_stop", "Stop an OAuth loopback listener.", Some("OauthStopParams")),
        // Web — a one-shot HTTP request and opening a URL in the system
        // browser; host-side streaming HTTP lives under WEB_STREAM_PATH below.
        a_at(WEB_PATH, "http_request", "http_request", "Issue an HTTP request with optional method, headers, body, and timeout.", Some("HttpRequestParams")),
        a_at(WEB_PATH, "open_url", "open_url", "Open a URL in the system browser via the platform-native handler (xdg-open / open / cmd /C start).", Some("OpenUrlParams")),
        // Action consoles — one per action ref, written to by every layer of
        // that action's execution. See `crate::console` and
        // `docs/console-implementation-plan.md`.
        a_at(CONSOLE_PATH, "print", "console_print", "Write one entry to the calling action's own console. Requires an action caller — this cannot be called directly from the CLI, MCP, or HTTP.", Some("ConsolePrintParams")),
        a_at(CONSOLE_PATH, "read", "console_read", "Read entries from an action's console, oldest first, starting at from_seq.", Some("ConsoleReadParams")),
        a_at(CONSOLE_PATH, "tail", "console_tail", "Like read, but if nothing new is available yet, long-polls up to wait_secs before returning.", Some("ConsoleTailParams")),
        a_at(CONSOLE_PATH, "clear", "console_clear", "Drop entries from the front of an action's console, freeing retention.", Some("ConsoleClearParams")),
        a_at(CONSOLE_PATH, "list", "console_list", "List known consoles, most recently written first.", Some("ConsoleListParams")),
        // Asynchronous actions — start/stop/poll as an async alternative to
        // exec. See `crate::invocations` and `docs/async-actions-plan.md`.
        a_at(ACTION_PATH, "start", "action_start", "Start an action detached: returns an invocation_id immediately while it runs in the background. Requires a long-lived host (solx-server/solx-mcp), not the CLI.", Some("ActionStartParams")),
        a_at(ACTION_PATH, "stop", "action_stop", "Request that a detached invocation stop. Cooperative first (the running action notices and exits on its own); force-aborted after a grace period.", Some("ActionStopParams")),
        a_at(ACTION_PATH, "poll", "action_poll", "Check a detached invocation's status, optionally long-polling until it finishes.", Some("ActionPollParams")),
        a_at(ACTION_PATH, "cancelled", "action_cancelled", "Check whether the calling action's own invocation has had a stop requested. Requires an action caller.", Some("EmptyParams")),
        // Host-side streaming HTTP — for callers with no sockets or
        // cross-call state of their own. Unrestricted by caller, like the
        // OAuth loopback and the action consoles: access is a bearer
        // capability on the unguessable stream_id.
        a_at(WEB_STREAM_PATH, "start", "http_stream_start", "Issue a streaming HTTP request. Returns stream_id and status as soon as response headers arrive, without waiting for the body.", Some("HttpStreamStartParams")),
        a_at(WEB_STREAM_PATH, "poll", "http_stream_poll", "Drain newline-delimited JSON chunks buffered for a stream since cursor, optionally long-polling up to wait_secs for more.", Some("HttpStreamPollParams")),
        a_at(WEB_STREAM_PATH, "close", "http_stream_close", "Stop a stream's reader task and drop its buffer.", Some("HttpStreamCloseParams")),
        // Widgets — open/drive a client-side UI widget's loopback. See
        // `crate::loopback::widget` and `docs/widget-actions.md`.
        a_at(WIDGET_PATH, "open", "widget_open", "Open a widget: serves its JS bundle and a websocket for the frontend to connect to. Returns a descriptor.", Some("WidgetOpenParams")),
        a_at(WIDGET_PATH, "close", "widget_close", "Close a widget and tear down its loopback registration and websocket.", Some("WidgetRefParams")),
        a_at(WIDGET_PATH, "show", "widget_show", "Show a widget (pushed to its frontend if connected).", Some("WidgetRefParams")),
        a_at(WIDGET_PATH, "hide", "widget_hide", "Hide a widget (pushed to its frontend if connected).", Some("WidgetRefParams")),
        a_at(WIDGET_PATH, "get", "widget_get", "Read one field (or, if field is omitted, the whole fields object) from a widget.", Some("WidgetGetParams")),
        a_at(WIDGET_PATH, "set", "widget_set", "Write one field on a widget, pushed to its frontend if connected.", Some("WidgetSetParams")),
        a_at(WIDGET_PATH, "exec", "widget_exec", "Dispatch an event to a widget's frontend code.", Some("WidgetExecParams")),
    ]
}

/// Seed the built-in catalogue into `actions` (idempotent via
/// `INSERT OR IGNORE` + the table's `UNIQUE(path,name)` constraint).
pub async fn seed_builtins(conn: &Connection) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    for entry in builtin_actions() {
        let param_type_ref = entry
            .param_type
            .map(|n| format!("{BUILTIN_TYPES_PATH}/{n}"))
            .unwrap_or_default();
        conn.execute(
            "INSERT OR IGNORE INTO actions \
             (id,path,name,caption,description,capabilities,phrases,category,\
              param_type_ref,result_type_ref,action_type,fn_name,bin_name,\
              action_config,files,trusted,created_at,updated_at) \
             VALUES (?1,?2,?3,'',?4,'[]','[]','',\
                     ?5,'','internal',?6,'',\
                     'null','[]',1,?7,?7)",
            libsql::params![
                Uuid::new_v4().to_string(),
                entry.path.to_string(),
                entry.name.to_string(),
                entry.description.to_string(),
                param_type_ref,
                entry.fn_name.to_string(),
                now.clone(),
            ],
        )
        .await
        .map_err(map_db)?;
    }
    Ok(())
}
