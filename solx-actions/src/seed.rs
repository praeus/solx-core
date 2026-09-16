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
use solx_surface::internal_actions::SeedAction;
use uuid::Uuid;

use crate::db::map_db;

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

/// Subdivision of the builtin namespace for running a raw solx script
/// string immediately, without first saving it as a `Script`-typed action.
pub const SCRIPT_PATH: &str = "/builtin/script";

/// Namespace for the hand-written JSON-schema types backing built-in
/// actions' `param_type_ref` (see `solx-types/src/seed.rs`). Shared by every
/// `/builtin/*` subpath — `param_type_ref` doesn't need to mirror the
/// action's own nesting.
pub const BUILTIN_TYPES_PATH: &str = "/builtin/types";

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
        a_at(DOCUMENT_PATH, "entity-save-document", "entity-save-document", "Create or update (upsert) a document.", Some("DocumentCrudParams")),
        a_at(DOCUMENT_PATH, "entity-get-document", "entity-get-document", "Fetch a document by path+name.", Some("EntityRefParams")),
        a_at(DOCUMENT_PATH, "entity-delete-document", "entity-delete-document", "Delete a document.", Some("EntityRefParams")),
        a_at(DOCUMENT_PATH, "entity-list-documents", "entity-list-documents", "List documents, optionally filtered by path prefix.", Some("ListParams")),
        a_at(DOCUMENT_PATH, "entity-list-document-paths", "entity-list-document-paths", "List distinct document paths in use, with a count of documents at each, optionally filtered by path prefix.", Some("ListParams")),
        // Document legacy field ops (one field at a time)
        a_at(DOCUMENT_PATH, "get-field", "get-field", "Read one field from a document's contents.", Some("GetFieldParams")),
        a_at(DOCUMENT_PATH, "set-field", "set-field", "Write one field on a document's contents.", Some("SetFieldParams")),
        // Document path-style field ops (nested reads/writes via dotted path)
        a_at(DOCUMENT_PATH, "get-field-at-path", "get-field-at-path", "Read a field at a slash-separated path inside a document's contents.", Some("GetFieldAtPathParams")),
        a_at(DOCUMENT_PATH, "set-field-at-path", "set-field-at-path", "Write a field at a slash-separated path inside a document's contents (optionally creating missing parents).", Some("SetFieldAtPathParams")),
        // Document search (SQLite FTS5)
        a_at(DOCUMENT_PATH, "search-documents", "search-documents", "Full-text + faceted search over documents.", Some("SearchDocumentsParams")),
        // Action CRUD (alongside the async start/stop/poll/cancelled below)
        a_at(ACTION_PATH, "entity-save-action", "entity-save-action", "Create or update (upsert) an action.", Some("ActionCrudParams")),
        a_at(ACTION_PATH, "entity-get-action", "entity-get-action", "Fetch an action by path+name.", Some("EntityRefParams")),
        a_at(ACTION_PATH, "entity-delete-action", "entity-delete-action", "Delete an action.", Some("EntityRefParams")),
        a_at(ACTION_PATH, "entity-list-actions", "entity-list-actions", "List actions, optionally filtered by path prefix.", Some("ListParams")),
        a_at(ACTION_PATH, "entity-list-action-paths", "entity-list-action-paths", "List distinct action paths in use, with a count of actions at each, optionally filtered by path prefix.", Some("ListParams")),
        // Action search (a real FTS5 full-text index)
        a_at(ACTION_PATH, "search-actions", "search-actions", "Full-text + faceted search over actions.", Some("SearchActionsParams")),
        // Type CRUD
        a_at(TYPE_PATH, "entity-save-type", "entity-save-type", "Create or update (upsert) a type.", Some("TypeCrudParams")),
        a_at(TYPE_PATH, "entity-get-type", "entity-get-type", "Fetch a type by path+name.", Some("EntityRefParams")),
        a_at(TYPE_PATH, "entity-delete-type", "entity-delete-type", "Delete a type.", Some("EntityRefParams")),
        a_at(TYPE_PATH, "entity-list-types", "entity-list-types", "List types, optionally filtered by path prefix.", Some("ListParams")),
        a_at(TYPE_PATH, "entity-list-type-paths", "entity-list-type-paths", "List distinct type paths in use, with a count of types at each, optionally filtered by path prefix.", Some("ListParams")),
        // General-purpose file store (unrestricted rel_path access)
        a_at(FILE_PATH, "file-put", "file-put", "Write bytes to a rel-path under the files root.", Some("FilePutParams")),
        a_at(FILE_PATH, "file-get", "file-get", "Read bytes from a rel-path under the files root.", Some("FileGetParams")),
        a_at(FILE_PATH, "file-delete", "file-delete", "Delete a file at a rel-path under the files root.", Some("FileGetParams")),
        a_at(FILE_PATH, "file-list", "file-list", "List stored rel-paths under a prefix.", Some("FileListParams")),
        a_at(FILE_PATH, "file-copy", "file-copy", "Copy a file within the files root.", Some("FileCopyParams")),
        a_at(FILE_PATH, "dir-copy", "dir-copy", "Recursively copy a directory within the files root.", Some("FileCopyParams")),
        a_at(FILE_PATH, "dir-delete", "dir-delete", "Recursively delete a directory (and every file under it) within the files root.", Some("DirDeleteParams")),
        // Environment scratch store
        a_at(ENV_PATH, "get-env", "get-env", "Read a variable from the environment store, optionally from a named namespace.", Some("GetEnvParams")),
        a_at(ENV_PATH, "set-env", "set-env", "Write a variable to the environment store. In-memory by default; pass persist to also store it in solx-config.json so it survives a restart.", Some("SetEnvParams")),
        // Secrets, scoped to whichever action is currently executing
        a_at(SECRETS_PATH, "get-secret", "get-secret", "Read a secret scoped to the calling action.", Some("GetSecretParams")),
        a_at(SECRETS_PATH, "set-secret", "set-secret", "Write a secret scoped to the calling action.", Some("SetSecretParams")),
        // OAuth loopback
        a_at(OAUTH_PATH, "oauth-start", "oauth-start", "Start a local OAuth 2.0 authorization-code loopback listener.", Some("OauthStartParams")),
        a_at(OAUTH_PATH, "oauth-await", "oauth-await", "Block until the OAuth loopback for a state_value receives its callback.", Some("OauthAwaitParams")),
        a_at(OAUTH_PATH, "oauth-stop", "oauth-stop", "Stop an OAuth loopback listener.", Some("OauthStopParams")),
        // Web — a one-shot HTTP request and opening a URL in the system
        // browser; host-side streaming HTTP lives under WEB_STREAM_PATH below.
        a_at(WEB_PATH, "http-request", "http-request", "Issue an HTTP request with optional method, headers, body, and timeout.", Some("HttpRequestParams")),
        a_at(WEB_PATH, "open-url", "open-url", "Open a URL in the system browser via the platform-native handler (xdg-open / open / cmd /C start).", Some("OpenUrlParams")),
        // Scripting — parse and run a raw solx script string immediately.
        a_at(SCRIPT_PATH, "exec", "script-exec", "Parse and immediately run a solx script from a string, returning its result. Supports the same 'exec <path/name> [--json '<params>']' and 'json <value>' stages as a Script-typed action.", Some("ScriptExecParams")),
        // Action consoles (`/builtin/console/*`) and asynchronous actions
        // (`/builtin/action/{start,stop,poll,cancelled}`) are seeded by
        // `solx-console` itself — see `solx_console::actions::seed_actions`,
        // merged in by `LocalActionManager::open`'s call to `seed_builtins`.
        // Host-side streaming HTTP — for callers with no sockets or
        // cross-call state of their own. Unrestricted by caller, like the
        // OAuth loopback and the action consoles: access is a bearer
        // capability on the unguessable stream_id.
        a_at(WEB_STREAM_PATH, "start", "http-stream-start", "Issue a streaming HTTP request. Returns stream_id and status as soon as response headers arrive, without waiting for the body.", Some("HttpStreamStartParams")),
        a_at(WEB_STREAM_PATH, "poll", "http-stream-poll", "Drain newline-delimited JSON chunks buffered for a stream since cursor, optionally long-polling up to wait_secs for more.", Some("HttpStreamPollParams")),
        a_at(WEB_STREAM_PATH, "close", "http-stream-close", "Stop a stream's reader task and drop its buffer.", Some("HttpStreamCloseParams")),
    ]
}

/// Seed the built-in catalogue into `actions` (idempotent via
/// `INSERT OR IGNORE` + the table's `UNIQUE(path,name)` constraint).
/// `extra` is merged in alongside this crate's own [`builtin_actions`] —
/// entries contributed by an internal-action plugin crate (e.g.
/// `solx_console::actions::seed_actions()`), passed in here rather than
/// hard-coded into this catalogue.
pub async fn seed_builtins(conn: &Connection, extra: &[SeedAction]) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    for entry in builtin_actions().iter().chain(extra.iter()) {
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
