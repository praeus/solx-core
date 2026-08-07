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

/// Namespace for the hand-written JSON-schema types backing built-in
/// actions' `param_type_ref` (see `solx-types/src/seed.rs`).
pub const BUILTIN_TYPES_PATH: &str = "/builtin/types";

/// One built-in action to seed. `name` is the `fn_name` matched in
/// `crate::internal::run_internal`.
pub struct SeedAction {
    pub name: &'static str,
    pub description: &'static str,
    /// Name under `BUILTIN_TYPES_PATH` giving this action's `param_type_ref`
    /// a real JSON Schema, if one is seeded (see `solx-types/src/seed.rs`).
    pub param_type: Option<&'static str>,
}

const fn a(name: &'static str, description: &'static str, param_type: Option<&'static str>) -> SeedAction {
    SeedAction { name, description, param_type }
}

/// The built-in action catalogue.
pub fn builtin_actions() -> Vec<SeedAction> {
    vec![
        // Document CRUD
        a("entity_post_document", "Create or update (upsert) a document.", Some("DocumentCrudParams")),
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
        a("entity_post_action", "Create or update (upsert) an action.", Some("ActionCrudParams")),
        a("entity_get_action", "Fetch an action by path+name.", Some("EntityRefParams")),
        a("entity_delete_action", "Delete an action.", Some("EntityRefParams")),
        a("entity_list_actions", "List actions, optionally filtered by path prefix.", Some("ListParams")),
        // Type CRUD
        a("entity_post_type", "Create or update (upsert) a type.", Some("TypeCrudParams")),
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
        a("get_env", "Read a variable from the in-process environment store.", Some("GetEnvParams")),
        a("set_env", "Write a variable to the in-process environment store.", Some("SetEnvParams")),
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
                     ?5,'','internal',?3,'',\
                     'null','[]',1,?6,?6)",
            libsql::params![
                Uuid::new_v4().to_string(),
                BUILTIN_PATH.to_string(),
                entry.name.to_string(),
                entry.description.to_string(),
                param_type_ref,
                now.clone(),
            ],
        )
        .await
        .map_err(map_db)?;
    }
    Ok(())
}
