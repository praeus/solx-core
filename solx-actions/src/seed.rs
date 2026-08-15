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
//! delete `db/solx-actions.db` once after upgrading to pick them up.

use chrono::Utc;
use libsql::Connection;
use solx_surface::error::Result;
use uuid::Uuid;

use crate::db::map_db;

/// Path all built-in actions are seeded under, out of the way of
/// user-created actions.
pub const BUILTIN_PATH: &str = "/builtin";

/// Subdivision of the builtin namespace for console operations — the first
/// of what's meant to become several (`/builtin/<area>/*`) as the flat
/// `/builtin` catalogue grows.
pub const CONSOLE_PATH: &str = "/builtin/console";

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
        a("entity_save_document", "Create or update (upsert) a document.", Some("DocumentCrudParams")),
        a("entity_get_document", "Fetch a document by path+name.", Some("EntityRefParams")),
        a("entity_delete_document", "Delete a document.", Some("EntityRefParams")),
        a("entity_list_documents", "List documents, optionally filtered by path prefix.", Some("ListParams")),
        // Document legacy field ops (one field at a time)
        a("get_field", "Read one field from a document's contents.", Some("GetFieldParams")),
        a("set_field", "Write one field on a document's contents.", Some("SetFieldParams")),
        // Document path-style field ops (nested reads/writes via dotted path)
        a("get_field_at_path", "Read a field at a slash-separated path inside a document's contents.", Some("GetFieldAtPathParams")),
        a("set_field_at_path", "Write a field at a slash-separated path inside a document's contents (optionally creating missing parents).", Some("SetFieldAtPathParams")),
        // Action CRUD
        a("entity_save_action", "Create or update (upsert) an action.", Some("ActionCrudParams")),
        a("entity_get_action", "Fetch an action by path+name.", Some("EntityRefParams")),
        a("entity_delete_action", "Delete an action.", Some("EntityRefParams")),
        a("entity_list_actions", "List actions, optionally filtered by path prefix.", Some("ListParams")),
        // Type CRUD
        a("entity_save_type", "Create or update (upsert) a type.", Some("TypeCrudParams")),
        a("entity_get_type", "Fetch a type by path+name.", Some("EntityRefParams")),
        a("entity_delete_type", "Delete a type.", Some("EntityRefParams")),
        a("entity_list_types", "List types, optionally filtered by path prefix.", Some("ListParams")),
        // Search (documents have a real Tantivy full-text index; actions
        // have no index, so search_actions is a structured filter, not
        // fuzzy relevance ranking)
        a("search_documents", "Full-text + faceted search over documents.", Some("SearchDocumentsParams")),
        a("search_actions", "List/filter actions by path prefix and other fields (structured filter, no full-text index for actions).", Some("ListParams")),
        // General-purpose file store (unrestricted rel_path access)
        a("file_put", "Write bytes to a rel-path under the files root.", Some("FilePutParams")),
        a("file_get", "Read bytes from a rel-path under the files root.", Some("FileGetParams")),
        a("file_delete", "Delete a file at a rel-path under the files root.", Some("FileGetParams")),
        a("file_list", "List stored rel-paths under a prefix.", Some("FileListParams")),
        a("file_copy", "Copy a file within the files root.", Some("FileCopyParams")),
        a("dir_copy", "Recursively copy a directory within the files root.", Some("FileCopyParams")),
        a("dir_delete", "Recursively delete a directory (and every file under it) within the files root.", Some("DirDeleteParams")),
        // Environment scratch store
        a("get_env", "Read a variable from the environment store, optionally from a named namespace.", Some("GetEnvParams")),
        a("set_env", "Write a variable to the environment store. In-memory by default; pass persist to also store it in solx-config.json so it survives a restart.", Some("SetEnvParams")),
        // Web
        a("http_request", "Issue an HTTP request with optional method, headers, body, and timeout.", Some("HttpRequestParams")),
        // Small utility built-ins
        a("now", "Return the current UTC time as an RFC 3339 string.", Some("EmptyParams")),
        a("uuid", "Return a fresh v4 UUID as a string.", Some("EmptyParams")),
        a("random_int", "Return a random integer in the inclusive [lo, hi] range.", Some("RandomIntParams")),
        a("random_string", "Return a random alphanumeric string of the given length.", Some("RandomStringParams")),
        // System integration (launch external handlers)
        a("open_url", "Open a URL in the system browser via the platform-native handler (xdg-open / open / cmd /C start).", Some("OpenUrlParams")),
        // Secrets, scoped to whichever action is currently executing
        a("get_secret", "Read a secret scoped to the calling action.", Some("GetSecretParams")),
        a("set_secret", "Write a secret scoped to the calling action.", Some("SetSecretParams")),
        // OAuth loopback
        a("oauth_start", "Start a local OAuth 2.0 authorization-code loopback listener.", Some("OauthStartParams")),
        a("oauth_await", "Block until the OAuth loopback for a state_value receives its callback.", Some("OauthAwaitParams")),
        a("oauth_stop", "Stop an OAuth loopback listener.", Some("OauthStopParams")),
        // Action consoles — one per action ref, written to by every layer of
        // that action's execution. See `crate::console` and
        // `docs/console-implementation-plan.md`.
        a_at(CONSOLE_PATH, "print", "console_print", "Write one entry to the calling action's own console. Requires an action caller — this cannot be called directly from the CLI, MCP, or HTTP.", Some("ConsolePrintParams")),
        a_at(CONSOLE_PATH, "read", "console_read", "Read entries from an action's console, oldest first, starting at from_seq.", Some("ConsoleReadParams")),
        a_at(CONSOLE_PATH, "tail", "console_tail", "Like read, but if nothing new is available yet, long-polls up to wait_secs before returning.", Some("ConsoleTailParams")),
        a_at(CONSOLE_PATH, "clear", "console_clear", "Drop entries from the front of an action's console, freeing retention.", Some("ConsoleClearParams")),
        a_at(CONSOLE_PATH, "list", "console_list", "List known consoles, most recently written first.", Some("ConsoleListParams")),
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
