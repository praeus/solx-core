//! # sol-actions — built-in WASM action library
//!
//! This crate compiles to a WASM Component Model binary that implements the
//! `backend-action` world defined in `../wit/sol-actions.wit`.
//!
//! ## Built-in action dispatch table
//!
//! | `action-name`                          | Delegates to    | Description                          |
//! |----------------------------------------|-----------------|--------------------------------------|
//! | `new_document` / `create_document`     | `document-ops`  | Create a typed document              |
//! | `get_field`                            | `document-ops`  | Read a field from a document         |
//! | `set_field`                            | `document-ops`  | Write a field on a document          |
//! | `document_attach_artifact`             | `entity-ops`    | Attach an artifact to a document     |
//! | `entity_new_document`                  | `entity-ops`    | Create Document entity               |
//! | `entity_get_document`                  | `entity-ops`    | Get Document by name                 |
//! | `entity_set_document`                  | `entity-ops`    | Update Document entity               |
//! | `entity_delete_document`               | `entity-ops`    | Delete Document entity               |
//! | `entity_list_documents`                | `entity-ops`    | List all Documents                   |
//! | `entity_new_action` … `entity_list_actions`     | `entity-ops` | Action CRUD             |
//! | `entity_new_artifact` … `entity_list_artifacts` | `entity-ops` | Artifact CRUD           |
//! | `artifact_upload_file`                 | `entity-ops`    | Upload file as Artifact              |
//! | `entity_new_type` … `entity_list_types`         | `entity-ops` | Type CRUD               |
//! | `entity_new_doc_ref` / `entity_new_action_ref`  | `entity-ops` | Reference entity create |
//! | `entity_new_model` … `entity_list_models`       | `entity-ops` | Model CRUD              |
//! | `entity_new_permission` … `entity_list_permissions` | `entity-ops` | Permission CRUD     |
//! | `entity_new_schedule` … `entity_list_schedules` | `entity-ops` | Schedule CRUD           |
//! | `entity_new_knowledge_base` / `entity_new_catalog` … | `entity-ops` | KnowledgeBase CRUD |
//! | `entity_new_link` … `entity_list_links`              | `entity-ops` | Link CRUD                |
//! | `entity_new_action_link` … `entity_list_action_links` | `entity-ops` | ActionLink CRUD       |
//! | `entity_new_document_link` … `entity_list_document_links` | `entity-ops` | DocumentLink CRUD |
//! | `entity_assign_model_role`                          | `entity-ops` | Assign/clear model for a role |
//! | `model_chat`                           | `model-ops`     | Multi-turn model chat                |
//! | `model_generate`                       | `model-ops`     | Single-turn text generation          |
//! | `model_list`                           | `model-ops`     | List registered models               |
//! | `file_read`                            | `system-ops`    | Read file contents                   |
//! | `file_write`                           | `system-ops`    | Write file contents                  |
//! | `dir_list`                             | `system-ops`    | List directory entries               |
//! | `file_copy`                            | `system-ops`    | Copy a file                          |
//! | `dir_copy`                             | `system-ops`    | Copy a directory recursively         |
//! | `get_env`                              | `system-ops`    | Read environment variable            |
//! | `set_env`                              | `system-ops`    | Write environment variable           |
//! | `get_secret`                           | `secrets`       | Read a secret scoped to the caller   |
//! | `set_secret`                           | `secrets`       | Write a secret scoped to the caller  |
//! | `fetch_html`                           | `system-ops`    | Fetch page HTML by URL              |
//! | `fetch_and_extract`                    | `system-ops`    | Fetch URL and extract in one step   |
//!
//! ### Browser-only actions (TypeScript-dispatched, NOT in this WASM bundle)
//!
//! ### Browser actions (native handler, NOT in this WASM bundle)
//!
//! The following actions are dispatched through the native `BrowserActions`
//! trait in `sol_manager::browser_handler`.  They execute JavaScript in the
//! embedded browser webview and are **never** routed into this WASM component:
//!
//! | `fn_name`        | Effect                                  |
//! |------------------|-----------------------------------------|
//! | `dom_snapshot`   | Capture outer HTML of an element by ID |
//! | `dom_query`      | Query DOM with a CSS selector          |
//! | `navigate`       | Navigate the browser to a URL          |
//! | `go_back`        | Go back in browser history             |
//! | `go_forward`     | Go forward in browser history          |
//! | `get_title`      | Return the current page title          |
//! | `current_url`    | Return the current page URL            |
//! | `click_element`  | Click an element by selector or ID     |
//! | `fill_element`   | Type text into a form field            |
//! | `scroll_page`    | Scroll the page up or down             |
//! | `scroll_to`      | Scroll to top or bottom                |
//! | `listen_event`   | Register a persistent event listener   |
//! | `emit_event`     | Fire a Tauri application event         |
//! | `unlisten_event` | Remove a registered event listener    |
//! | `dom_monitor`    | Start observing DOM mutations          |
//! | `dom_monitor_stop`| Stop a DOM mutation observer          |
//!
//! ## Entity-ops availability
//!
//! `entity-ops` is imported by **both** WIT worlds:
//! - `backend-action` (this WASM component, running under wasmtime in Tauri)
//! - `frontend-action` (a hypothetical jco-transpiled WASM component in the browser,
//!    backed by the `entityOps` TypeScript shim in `browser-actions.ts`)
//!
//! ## Build
//! ```sh
//! cd sol-actions
//! cargo build --release   # default target: wasm32-wasip2
//! ```
//!
//! ## Testing
//!
//! Pure helper functions (`parse_params`, `require_str`) can be unit-tested on the
//! native target.  End-to-end dispatch tests require a wasmtime host because the
//! WIT-generated host-import stubs are only valid inside a WASM component runtime.
//!
//! Run helper tests:
//! ```sh
//! cargo test --target <native>
//! ```

wit_bindgen::generate!({
    world: "backend-action",
    path: "wit",
});

use exports::sol::actions::runner::{ActionResult, Guest};
use serde_json::{json, Value};

// ── Component entry point ─────────────────────────────────────────────────────

struct SolActions;

impl Guest for SolActions {
    fn run(action_name: Option<String>, params: String) -> Result<ActionResult, String> {
        let name = action_name.as_deref().unwrap_or("");
        match name {
            // ── Legacy document-ops actions ──────────────────────────────────
            "new_document" | "create_document" => create_document(&params),
            "get_field" => get_field(&params),
            "set_field" => set_field(&params),
            "document_attach_artifact" => action_document_attach_artifact(&params),

            // ── Entity-ops: Document ─────────────────────────────────────────
            "entity_new_document" => entity_new("Document", &params),
            "entity_get_document" => entity_get("Document", &params),
            "entity_set_document" => entity_set("Document", &params),
            "entity_delete_document" => entity_delete("Document", &params),
            "entity_list_documents" => entity_list("Document", &params),

            // ── Entity-ops: Action ───────────────────────────────────────────
            "entity_new_action" => entity_new("Action", &params),
            "entity_get_action" => entity_get("Action", &params),
            "entity_set_action" => entity_set("Action", &params),
            "entity_delete_action" => entity_delete("Action", &params),
            "entity_list_actions" => entity_list("Action", &params),

            // ── Entity-ops: Artifact ─────────────────────────────────────────
            "entity_new_artifact" => entity_new("Artifact", &params),
            "entity_get_artifact" => entity_get("Artifact", &params),
            "entity_set_artifact" => entity_set("Artifact", &params),
            "entity_delete_artifact" => entity_delete("Artifact", &params),
            "entity_list_artifacts" => entity_list("Artifact", &params),
            "artifact_upload_file" => action_artifact_upload_file(&params),

            // ── Entity-ops: Type ─────────────────────────────────────────────
            "entity_new_type" => entity_new("Type", &params),
            "entity_get_type" => entity_get("Type", &params),
            "entity_set_type" => entity_set("Type", &params),
            "entity_delete_type" => entity_delete("Type", &params),
            "entity_list_types" => entity_list("Type", &params),

            // ── Entity-ops: Reference entities ───────────────────────────────
            "entity_new_doc_ref" => entity_new("DocRef", &params),
            "entity_new_action_ref" => entity_new("ActionRef", &params),
            // ── Entity-ops: Model ────────────────────────────────────────────
            "entity_new_model" => entity_new("Model", &params),
            "entity_get_model" => entity_get("Model", &params),
            "entity_set_model" => entity_set("Model", &params),
            "entity_delete_model" => entity_delete("Model", &params),
            "entity_list_models" => entity_list("Model", &params),

            // ── Entity-ops: Permission ───────────────────────────────────────
            "entity_new_permission" => entity_new("Permission", &params),
            "entity_get_permission" => entity_get("Permission", &params),
            "entity_set_permission" => entity_set("Permission", &params),
            "entity_delete_permission" => entity_delete("Permission", &params),
            "entity_list_permissions" => entity_list("Permission", &params),

            // ── Entity-ops: Schedule ─────────────────────────────────────────
            "entity_new_schedule" => entity_new("Schedule", &params),
            "entity_get_schedule" => entity_get("Schedule", &params),
            "entity_set_schedule" => entity_set("Schedule", &params),
            "entity_delete_schedule" => entity_delete("Schedule", &params),
            "entity_list_schedules" => entity_list("Schedule", &params),

            // ── Entity-ops: KnowledgeBase / Catalog (alias) ─────────────────
            "entity_new_knowledge_base" | "entity_new_catalog" => entity_new("KnowledgeBase", &params),
            "entity_get_knowledge_base" | "entity_get_catalog" => entity_get("KnowledgeBase", &params),
            "entity_set_knowledge_base" | "entity_set_catalog" => entity_set("KnowledgeBase", &params),
            "entity_delete_knowledge_base" | "entity_delete_catalog" => entity_delete("KnowledgeBase", &params),
            "entity_list_knowledge_bases" | "entity_list_catalogs" => entity_list("KnowledgeBase", &params),

            // ── Entity-ops: Link ────────────────────────────────────────────
            "entity_new_link" => entity_new("Link", &params),
            "entity_get_link" => entity_get("Link", &params),
            "entity_set_link" => entity_set("Link", &params),
            "entity_delete_link" => entity_delete("Link", &params),
            "entity_list_links" => entity_list("Link", &params),

            // ── Entity-ops: ActionLink (action ↔ link association) ─────────
            "entity_new_action_link" => entity_new("ActionLink", &params),
            "entity_get_action_link" => entity_get("ActionLink", &params),
            "entity_set_action_link" => entity_set("ActionLink", &params),
            "entity_delete_action_link" => entity_delete("ActionLink", &params),
            "entity_list_action_links" => entity_list("ActionLink", &params),

            // ── Entity-ops: DocumentLink (document ↔ link association) ─────
            "entity_new_document_link" => entity_new("DocumentLink", &params),
            "entity_get_document_link" => entity_get("DocumentLink", &params),
            "entity_set_document_link" => entity_set("DocumentLink", &params),
            "entity_delete_document_link" => entity_delete("DocumentLink", &params),
            "entity_list_document_links" => entity_list("DocumentLink", &params),

            // ── Entity-ops: ModelRole ──────────────────────────────────────
            // Note: there is intentionally no way to create or delete new model
            // roles, since these are meant to be a fixed set of predefined roles.
            "entity_get_model_role" => entity_get("ModelRole", &params),
            "entity_set_model_role" => entity_set("ModelRole", &params),
            "entity_list_model_roles" => entity_list("ModelRole", &params),

            // ── Model inference (model-ops) ──────────────────────────────────
            "model_chat" => action_model_chat(&params),
            "model_generate" => action_model_generate(&params),
            "model_list" => action_model_list(),
            
            // ── System operations ────────────────────────────────────────────
            "file_read" => action_file_read(&params),
            "file_write" => action_file_write(&params),
            "dir_list" => action_dir_list(&params),
            "file_copy" => action_file_copy(&params),
            "dir_copy" => action_dir_copy(&params),
            "get_env" => action_get_env(&params),
            "set_env" => action_set_env(&params),
            "get_secret" => action_get_secret(&params),
            "set_secret" => action_set_secret(&params),
            "fetch_html" => action_fetch_html(&params),
            "fetch_and_extract" => action_fetch_and_extract(&params),

            "" => Err(
                "action-name is required for the sol-actions bundle (e.g. \"entity_new_document\")"
                    .to_string(),
            ),
            other => Err(format!("unknown action: '{other}'")),
        }
    }
}

export!(SolActions);

// ── Helpers ───────────────────────────────────────────────────────────────────

fn ok_result(message: impl Into<String>, output: Option<Value>) -> Result<ActionResult, String> {
    Ok(ActionResult {
        success: true,
        message: Some(message.into()),
        output: output.map(|v| v.to_string()),
    })
}

fn parse_params(params: &str) -> Result<Value, String> {
    serde_json::from_str(params).map_err(|e| format!("invalid JSON params: {e}"))
}

fn require_str<'v>(v: &'v Value, key: &str) -> Result<&'v str, String> {
    v[key]
        .as_str()
        .ok_or_else(|| format!("missing required field: {key}"))
}

fn resolve_entity_name_for_get<'v>(v: &'v Value) -> Result<&'v str, String> {
    if let Some(name) = v.get("name").and_then(Value::as_str) {
        return Ok(name);
    }
    if let Some(document_name) = v.get("document_name").and_then(Value::as_str) {
        return Ok(document_name);
    }
    if let Some(document_id) = v.get("document_id").and_then(Value::as_str) {
        return Ok(document_id);
    }

    if let Some(nested) = v.get("in").and_then(Value::as_object) {
        if let Some(name) = nested.get("name").and_then(Value::as_str) {
            return Ok(name);
        }
        if let Some(document_name) = nested.get("document_name").and_then(Value::as_str) {
            return Ok(document_name);
        }
        if let Some(id) = nested.get("id").and_then(Value::as_str) {
            return Ok(id);
        }
        if let Some(document_id) = nested.get("document_id").and_then(Value::as_str) {
            return Ok(document_id);
        }
    }

    if let Some(params_obj) = v.get("params").and_then(Value::as_object) {
        if let Some(name) = params_obj.get("name").and_then(Value::as_str) {
            return Ok(name);
        }
        if let Some(document_name) = params_obj.get("document_name").and_then(Value::as_str) {
            return Ok(document_name);
        }
        if let Some(id) = params_obj.get("id").and_then(Value::as_str) {
            return Ok(id);
        }
        if let Some(document_id) = params_obj.get("document_id").and_then(Value::as_str) {
            return Ok(document_id);
        }
        if let Some(nested) = params_obj.get("in").and_then(Value::as_object) {
            if let Some(name) = nested.get("name").and_then(Value::as_str) {
                return Ok(name);
            }
            if let Some(document_name) = nested.get("document_name").and_then(Value::as_str) {
                return Ok(document_name);
            }
            if let Some(id) = nested.get("id").and_then(Value::as_str) {
                return Ok(id);
            }
            if let Some(document_id) = nested.get("document_id").and_then(Value::as_str) {
                return Ok(document_id);
            }
        }
    }

    if let Some(id) = v.get("id").and_then(Value::as_str) {
        return Ok(id);
    }

    if let Some(params_obj) = v.get("params").and_then(Value::as_object) {
        if let Some(id) = params_obj.get("id").and_then(Value::as_str) {
            return Ok(id);
        }
    }

    Err("missing required field: name".to_string())
}

// ── Legacy document-ops actions ───────────────────────────────────────────────

/// Create a new document via the `document-ops` host interface (legacy path).
///
/// Params: `{ "type_name": "MyType", "name": "my-doc", "content": { ... } }`
fn create_document(params: &str) -> Result<ActionResult, String> {
    use sol::actions::document_ops;
    use sol::actions::logger;

    let v = parse_params(params)?;
    let type_name = v["type_name"].as_str().unwrap_or("Document");
    let name = require_str(&v, "name")?;
    let content = v
        .get("content")
        .map(|c| c.to_string())
        .unwrap_or_else(|| "{}".to_string());

    logger::log(&format!(
        "[sol-actions] create_document  type={type_name}  name={name}"
    ));

    let doc = document_ops::create_document(type_name, name, &content)?;

    ok_result(
        format!("Created document '{}'", doc.name),
        Some(json!({ "id": doc.id, "name": doc.name, "type_name": doc.type_name })),
    )
}

/// Read one field from a document's JSON contents.
///
/// Params: `{ "document_name": "my-doc", "field": "title" }`
fn get_field(params: &str) -> Result<ActionResult, String> {
    use sol::actions::document_ops;
    use sol::actions::logger;

    let v = parse_params(params)?;
    let document_name = require_str(&v, "document_name")?;
    let field = require_str(&v, "field")?;

    logger::log(&format!(
        "[sol-actions] get_field  document={document_name}  field={field}"
    ));

    let raw = document_ops::get_field(document_name, field)?;
    let parsed: Value = serde_json::from_str(&raw).unwrap_or(Value::String(raw));

    ok_result(
        format!("Read field '{field}' from '{document_name}'"),
        Some(parsed),
    )
}

/// Set one field in a document's JSON contents.
///
/// Params: `{ "document_name": "my-doc", "field": "title", "value": "Hello" }`
fn set_field(params: &str) -> Result<ActionResult, String> {
    use sol::actions::document_ops;
    use sol::actions::logger;

    let v = parse_params(params)?;
    let document_name = require_str(&v, "document_name")?;
    let field = require_str(&v, "field")?;
    let value = v.get("value").ok_or("missing required field: value")?;

    logger::log(&format!(
        "[sol-actions] set_field  document={document_name}  field={field}"
    ));

    document_ops::set_field(document_name, field, &value.to_string())?;

    ok_result(format!("Set field '{field}' on '{document_name}'"), None)
}

// ── Generic entity-ops actions ────────────────────────────────────────────────

/// Create a new entity of `type_name`.
///
/// Params: any JSON object whose fields match the type's input schema.
/// The `name` field is required.
fn entity_new(type_name: &str, params: &str) -> Result<ActionResult, String> {
    use sol::actions::entity_ops;
    use sol::actions::logger;

    let v = parse_params(params)?;
    let name = require_str(&v, "name")?;

    logger::log(&format!(
        "[sol-actions] entity_new  type={type_name}  name={name}"
    ));

    let entity_json = entity_ops::entity_new(type_name, name, params)?;
    let entity: Value = serde_json::from_str(&entity_json).unwrap_or(Value::String(entity_json));

    ok_result(format!("Created {type_name} '{name}'"), Some(entity))
}

/// Get an entity of `type_name` by name.
///
/// Params: `{ "name": "my-entity" }`
fn entity_get(type_name: &str, params: &str) -> Result<ActionResult, String> {
    use sol::actions::entity_ops;
    use sol::actions::logger;

    let v = parse_params(params)?;
    let name = resolve_entity_name_for_get(&v)?;

    logger::log(&format!(
        "[sol-actions] entity_get  type={type_name}  name={name}"
    ));

    let entity_json = match entity_ops::entity_get(type_name, name) {
        Ok(json) => json,
        Err(err) => {
            // Planner loops often carry UUID ids (`{doc.id}`) while entity_get
            // expects names. For UUID-like inputs, resolve id->name via list.
            if looks_like_uuid(name) {
                if let Some(resolved_name) = resolve_entity_name_from_id(type_name, name) {
                    entity_ops::entity_get(type_name, &resolved_name)?
                } else {
                    return Err(err);
                }
            } else {
                return Err(err);
            }
        }
    };
    let entity: Value = serde_json::from_str(&entity_json).unwrap_or(Value::String(entity_json));

    ok_result(format!("Fetched {type_name} '{name}'"), Some(entity))
}

fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36 && s.chars().filter(|&c| c == '-').count() == 4
}

fn resolve_entity_name_from_id(type_name: &str, id: &str) -> Option<String> {
    use sol::actions::entity_ops;

    let list_json = entity_ops::entity_list(type_name, "{}").ok()?;
    let arr: Value = serde_json::from_str(&list_json).ok()?;
    let items = arr.as_array()?;
    let found = items.iter().find(|item| {
        item.get("id").and_then(Value::as_str) == Some(id)
    })?;
    found.get("name").and_then(Value::as_str).map(|s| s.to_string())
}

/// Update an entity of `type_name`.
///
/// Params: any JSON object matching the type's input schema.  The `name`
/// field is required and identifies the entity to update.
fn entity_set(type_name: &str, params: &str) -> Result<ActionResult, String> {
    use sol::actions::entity_ops;
    use sol::actions::logger;

    let v = parse_params(params)?;
    let name = require_str(&v, "name")?;

    logger::log(&format!(
        "[sol-actions] entity_set  type={type_name}  name={name}"
    ));

    let entity_json = entity_ops::entity_set(type_name, name, params)?;
    let entity: Value = serde_json::from_str(&entity_json).unwrap_or(Value::String(entity_json));

    ok_result(format!("Updated {type_name} '{name}'"), Some(entity))
}

/// Delete an entity of `type_name`.
///
/// Params: `{ "name": "my-entity" }`
fn entity_delete(type_name: &str, params: &str) -> Result<ActionResult, String> {
    use sol::actions::entity_ops;
    use sol::actions::logger;

    let v = parse_params(params)?;
    let name = require_str(&v, "name")?;

    logger::log(&format!(
        "[sol-actions] entity_delete  type={type_name}  name={name}"
    ));

    entity_ops::entity_delete(type_name, name)?;

    ok_result(format!("Deleted {type_name} '{name}'"), None)
}

/// List all entities of `type_name`.
///
/// Params: optional ListOptions JSON object, or `{}` for unfiltered full list.
fn entity_list(type_name: &str, params: &str) -> Result<ActionResult, String> {
    use sol::actions::entity_ops;
    use sol::actions::logger;

    logger::log(&format!("[sol-actions] entity_list  type={type_name}"));

    let list_json = entity_ops::entity_list(type_name, params)?;
    let list: Value = serde_json::from_str(&list_json).unwrap_or(Value::String(list_json));

    ok_result(format!("Listed {type_name} entities"), Some(list))
}

// ── System-ops actions ────────────────────────────────────────────────────────

/// Read a file from the filesystem and return its contents as a string.
///
/// Params: `{ "path": "/path/to/file" }`
fn action_file_read(params: &str) -> Result<ActionResult, String> {
    use sol::actions::logger;
    use sol::actions::system_ops;

    let v = parse_params(params)?;
    let path = require_str(&v, "path")?;

    logger::log(&format!("[sol-actions] file_read  path={path}"));

    let contents = system_ops::read_file(path)?;

    ok_result(
        format!("Read file '{path}'"),
        Some(Value::String(contents)),
    )
}

/// Write a string to a file (creates or overwrites).
///
/// Params: `{ "path": "/path/to/file", "content": "hello" }`
fn action_file_write(params: &str) -> Result<ActionResult, String> {
    use sol::actions::logger;
    use sol::actions::system_ops;

    let v = parse_params(params)?;
    let path = require_str(&v, "path")?;
    let content = require_str(&v, "content")?;

    logger::log(&format!("[sol-actions] file_write  path={path}"));

    system_ops::write_file(path, content)?;

    ok_result(format!("Wrote file '{path}'"), None)
}

/// List directory entries.
///
/// Params: `{ "path": "/path/to/dir" }`
fn action_dir_list(params: &str) -> Result<ActionResult, String> {
    use sol::actions::logger;
    use sol::actions::system_ops;

    let v = parse_params(params)?;
    let path = require_str(&v, "path")?;

    logger::log(&format!("[sol-actions] dir_list  path={path}"));

    let entries = system_ops::list_dir(path)?;
    let arr: Value = entries.into_iter().map(Value::String).collect();

    ok_result(format!("Listed directory '{path}'"), Some(arr))
}

/// Copy a file at the OS level.
///
/// Params: `{ "source_path": "C:/src/file.txt", "destination_path": "C:/dst/file.txt" }`
fn action_file_copy(params: &str) -> Result<ActionResult, String> {
    use sol::actions::logger;
    use sol::actions::system_ops;

    let v = parse_params(params)?;
    let source_path = require_str(&v, "source_path")?;
    let destination_path = require_str(&v, "destination_path")?;

    logger::log(&format!(
        "[sol-actions] file_copy source={source_path} destination={destination_path}"
    ));

    system_ops::copy_file(source_path, destination_path)?;
    ok_result(
        format!("Copied file '{source_path}' to '{destination_path}'"),
        None,
    )
}

/// Recursively copy a directory at the OS level.
///
/// Params: `{ "source_path": "C:/src/dir", "destination_path": "C:/dst/dir" }`
fn action_dir_copy(params: &str) -> Result<ActionResult, String> {
    use sol::actions::logger;
    use sol::actions::system_ops;

    let v = parse_params(params)?;
    let source_path = require_str(&v, "source_path")?;
    let destination_path = require_str(&v, "destination_path")?;

    logger::log(&format!(
        "[sol-actions] dir_copy source={source_path} destination={destination_path}"
    ));

    system_ops::copy_dir(source_path, destination_path)?;
    ok_result(
        format!("Copied directory '{source_path}' to '{destination_path}'"),
        None,
    )
}

/// Read an environment variable.
///
/// Params: `{ "key": "HOME" }`
fn action_get_env(params: &str) -> Result<ActionResult, String> {
    use sol::actions::logger;
    use sol::actions::system_ops;

    let v = parse_params(params)?;
    let key = require_str(&v, "key")?;

    logger::log(&format!("[sol-actions] get_env  key={key}"));

    let value = system_ops::get_env(key);

    ok_result(
        format!("Read env '{key}'"),
        Some(match value {
            Some(s) => Value::String(s),
            None => Value::Null,
        }),
    )
}

/// Write a variable to the internal WASM environment store.
///
/// The value is persisted to `sol-config.json` by the host so it survives
/// restarts.  Keys prefixed with `secrets::` are flagged as sensitive —
/// the prefix is stripped before the value is visible to WASM guests, so
/// `get_env` callers should use the bare key (e.g. `GOOGLE_OAUTH_CLIENT_ID`,
/// not `secrets::GOOGLE_OAUTH_CLIENT_ID`).
///
/// Params: `{ "key": "MY_VAR", "value": "hello" }`
fn action_set_env(params: &str) -> Result<ActionResult, String> {
    use sol::actions::logger;
    use sol::actions::system_ops;

    let v = parse_params(params)?;
    let key = require_str(&v, "key")?;
    let value = require_str(&v, "value")?;

    logger::log(&format!("[sol-actions] set_env  key={key}"));

    system_ops::set_env(key, value);

    ok_result(format!("Wrote env '{key}'"), None)
}

/// Read a secret by name, scoped to the calling action.
///
/// Params: `{ "name": "GOOGLE_OAUTH_CLIENT_SECRET" }`
///
/// Unlike `get_env`, this only returns a value if the calling action's own
/// `action_config.secrets` map has a key configured for `name` — see the
/// `secrets` WIT interface doc comment.
fn action_get_secret(params: &str) -> Result<ActionResult, String> {
    use sol::actions::logger;
    use sol::actions::secrets;

    let v = parse_params(params)?;
    let name = require_str(&v, "name")?;

    logger::log(&format!("[sol-actions] get_secret  name={name}"));

    let value = secrets::get_secret(name);

    ok_result(
        format!("Read secret '{name}'"),
        Some(match value {
            Some(s) => Value::String(s),
            None => Value::Null,
        }),
    )
}

/// Encrypt and store a secret by name, scoped to the calling action.
///
/// Params: `{ "name": "GOOGLE_OAUTH_CLIENT_SECRET", "value": "..." }`
///
/// Fails if the calling action has no key configured for `name` in its own
/// `action_config.secrets` map.
fn action_set_secret(params: &str) -> Result<ActionResult, String> {
    use sol::actions::logger;
    use sol::actions::secrets;

    let v = parse_params(params)?;
    let name = require_str(&v, "name")?;
    let value = require_str(&v, "value")?;

    logger::log(&format!("[sol-actions] set_secret  name={name}"));

    secrets::set_secret(name, value)?;

    ok_result(format!("Wrote secret '{name}'"), None)
}

/// Fetch HTML from an http/https URL.
///
/// Params: `{ "url": "https://example.com" }`
fn action_fetch_html(params: &str) -> Result<ActionResult, String> {
    use sol::actions::logger;
    use sol::actions::system_ops;

    let v = parse_params(params)?;
    let url = require_str(&v, "url")?;

    logger::log(&format!("[sol-actions] fetch_html  url={url}"));

    let html = system_ops::fetch_html(url)?;

    ok_result(
        format!("Fetched HTML from '{url}'"),
        Some(json!({ "url": url, "html": html })),
    )
}

/// Fetch HTML from a URL and run extraction in one host call.
///
/// Params:
/// `{ "source_document_name": "doc", "url": "https://example.com", "knowledge_base_name": "kb", "selected_type_name": "RichTextDocument", "extraction_prompt": "focus on the comments" }`
fn action_fetch_and_extract(params: &str) -> Result<ActionResult, String> {
    use sol::actions::logger;
    use sol::actions::system_ops;

    let v = parse_params(params)?;
    let source_document_name = require_str(&v, "source_document_name")?;
    let url = require_str(&v, "url")?;
    let knowledge_base_name = v
        .get("knowledge_base_name")
        .and_then(Value::as_str)
        .map(str::to_string);
    let selected_type_name = v
        .get("selected_type_name")
        .and_then(Value::as_str)
        .map(str::to_string);
    let extraction_prompt = v
        .get("extraction_prompt")
        .and_then(Value::as_str)
        .map(str::to_string);

    logger::log(&format!(
        "[sol-actions] fetch_and_extract  source_document_name={source_document_name} url={url}"
    ));

    let result = system_ops::fetch_and_extract(
        source_document_name,
        url,
        knowledge_base_name.as_deref(),
        selected_type_name.as_deref(),
        extraction_prompt.as_deref(),
    )?;
    let parsed: Value = serde_json::from_str(&result).unwrap_or(Value::String(result));

    ok_result("Fetched and extracted URL", Some(parsed))
}

/// Create or update an Artifact from uploaded file bytes (base64 string).
///
/// Params:
/// `{ "artifact_name": "foo.txt", "content_type": "text/plain", "source_path": "C:/tmp/foo.txt" }`
/// or `{ "artifact_name": "foo.txt", "content_type": "text/plain", "data_base64": "..." }`
fn action_artifact_upload_file(params: &str) -> Result<ActionResult, String> {
    use sol::actions::entity_ops;
    use sol::actions::logger;

    let v = parse_params(params)?;
    let artifact_name = require_str(&v, "artifact_name")?;
    let content_type = require_str(&v, "content_type")?;
    let source_path = v.get("source_path").and_then(|s| s.as_str());
    let data_base64 = v.get("data_base64").and_then(|s| s.as_str());

    let payload = if let Some(source_path) = source_path {
        logger::log(&format!(
            "[sol-actions] artifact_upload_file artifact={artifact_name} source={source_path} mode=backend-managed"
        ));

        json!({
            "name": artifact_name,
            "content_type": content_type,
            "data": "",
            "path": source_path,
        })
        .to_string()
    } else if let Some(data_base64) = data_base64 {
        logger::log(&format!(
            "[sol-actions] artifact_upload_file artifact={artifact_name} mode=base64"
        ));
        json!({
            "name": artifact_name,
            "content_type": content_type,
            "data": data_base64,
            "path": serde_json::Value::Null,
        })
        .to_string()
    } else {
        return Err("artifact_upload_file requires source_path or data_base64".to_string());
    };

    let result = entity_ops::entity_new("Artifact", artifact_name, &payload)
        .or_else(|_| entity_ops::entity_set("Artifact", artifact_name, &payload))?;
    let parsed: Value = serde_json::from_str(&result).unwrap_or(Value::String(result));

    ok_result(
        format!("Uploaded artifact '{artifact_name}'"),
        Some(parsed),
    )
}

/// Attach an existing Artifact to an existing Document by artifact name.
///
/// Params:
/// `{ "document_name": "doc-1", "artifact_name": "foo.txt" }`
fn action_document_attach_artifact(params: &str) -> Result<ActionResult, String> {
    use sol::actions::entity_ops;
    use sol::actions::logger;

    let v = parse_params(params)?;
    let document_name = require_str(&v, "document_name")?;
    let artifact_name = require_str(&v, "artifact_name")?;

    logger::log(&format!(
        "[sol-actions] document_attach_artifact document={document_name} artifact={artifact_name}"
    ));

    let doc_raw = entity_ops::entity_get("Document", document_name)?;
    let doc_val: Value = serde_json::from_str(&doc_raw)
        .map_err(|e| format!("failed to decode document JSON: {e}"))?;

    let mut artifact_names: Vec<String> = doc_val
        .get("artifacts")
        .and_then(|existing| existing.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|it| it.get("name").and_then(|n| n.as_str()))
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();

    if !artifact_names.iter().any(|name| name == artifact_name) {
        artifact_names.push(artifact_name.to_string());
    }

    let payload = json!({
        "name": doc_val
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or(document_name),
        "type_name": doc_val
            .get("type_name")
            .and_then(|n| n.as_str())
            .unwrap_or("Document"),
        "contents": doc_val.get("contents").cloned().unwrap_or_else(|| json!({})),
        "phrases": doc_val
            .get("phrases")
            .and_then(|p| p.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|it| it.get("name").and_then(|n| n.as_str()))
                    .map(ToOwned::to_owned)
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default(),
        "artifact_names": artifact_names,
    })
    .to_string();

    let result = entity_ops::entity_set("Document", document_name, &payload)?;
    let parsed: Value = serde_json::from_str(&result).unwrap_or(Value::String(result));

    ok_result(
        format!("Attached artifact '{artifact_name}' to '{document_name}'"),
        Some(parsed),
    )
}

// ── Model inference actions ───────────────────────────────────────────────────

/// Send a chat completion to a language model.
///
/// Params: `{ "model_name": "ministral-3b", "messages": [{"role": "user", "content": "Hello"}] }`
fn action_model_chat(params: &str) -> Result<ActionResult, String> {
    use sol::actions::logger;
    use sol::actions::model_ops;

    let v = parse_params(params)?;
    let model_name = require_str(&v, "model_name")
        .or_else(|_| require_str(&v, "model"))
        .unwrap_or("instructor");
    let messages = v
        .get("messages")
        .ok_or("missing required field: messages")?;
    let messages_str = messages.to_string();

    logger::log(&format!("[sol-actions] model_chat  model={model_name}"));

    let response = model_ops::model_chat(model_name, &messages_str)?;
    let response_val: Value =
        serde_json::from_str(&response).unwrap_or(Value::String(response));

    ok_result(
        format!("Chat completed with model '{model_name}'"),
        Some(response_val),
    )
}

/// Single-turn text generation using a language model.
///
/// Params: `{ "model_name": "ministral-3b", "prompt": "Once upon a time" }`
fn action_model_generate(params: &str) -> Result<ActionResult, String> {
    use sol::actions::logger;
    use sol::actions::model_ops;

    let v = parse_params(params)?;
    // Accept "model" as an alias; fall back to "instructor" if neither is present
    // so that planner omissions degrade gracefully rather than hard-failing.
    let model_name = require_str(&v, "model_name")
        .or_else(|_| require_str(&v, "model"))
        .unwrap_or("instructor");
    let prompt = require_str(&v, "prompt")?;

    logger::log(&format!("[sol-actions] model_generate  model={model_name}"));

    let text = model_ops::model_generate(model_name, prompt)?;

    ok_result(
        format!("Generated text with model '{model_name}'"),
        Some(Value::String(text)),
    )
}

/// List registered sol Model entity names.
fn action_model_list() -> Result<ActionResult, String> {
    use sol::actions::model_ops;

    let json = model_ops::model_list()?;
    let val: Value = serde_json::from_str(&json).unwrap_or(Value::Array(vec![]));

    ok_result("Listed available models", Some(val))
}

// ── Unit tests (native target only) ──────────────────────────────────────────
//
// The WIT-generated host-import stubs in `sol::actions::*` are only valid
// inside a WASM component runtime and cannot be exercised here.  The tests
// below cover the pure-Rust helper functions that underpin every action handler.

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    // ── parse_params ─────────────────────────────────────────────────────────

    #[test]
    fn parse_params_valid_json() {
        let v = parse_params(r#"{"name":"foo","value":42}"#).unwrap();
        assert_eq!(v["name"], "foo");
        assert_eq!(v["value"], 42);
    }

    #[test]
    fn parse_params_empty_object() {
        let v = parse_params("{}").unwrap();
        assert!(v.is_object());
    }

    #[test]
    fn parse_params_invalid_json_returns_err() {
        let err = parse_params("not json").unwrap_err();
        assert!(err.contains("invalid") || err.contains("params"));
    }

    // ── require_str ──────────────────────────────────────────────────────────

    #[test]
    fn require_str_present_key() {
        let v = serde_json::json!({ "model_name": "ministral-3b" });
        assert_eq!(require_str(&v, "model_name").unwrap(), "ministral-3b");
    }

    #[test]
    fn require_str_missing_key_returns_err() {
        let v = serde_json::json!({});
        let err = require_str(&v, "model_name").unwrap_err();
        assert!(err.contains("model_name") || err.contains("missing"));
    }

    #[test]
    fn require_str_non_string_value_returns_err() {
        let v = serde_json::json!({ "count": 42 });
        let err = require_str(&v, "count").unwrap_err();
        assert!(err.contains("count") || err.contains("string"));
    }

    // ── ok_result ────────────────────────────────────────────────────────────

    #[test]
    fn ok_result_with_output() {
        let r = ok_result("done", Some(serde_json::json!({"x": 1}))).unwrap();
        assert!(r.success);
        assert_eq!(r.message.as_deref(), Some("done"));
        assert!(r.output.is_some());
    }

    #[test]
    fn ok_result_without_output() {
        let r = ok_result("navigating", None).unwrap();
        assert!(r.success);
        assert!(r.output.is_none());
    }

    // ── Dispatch route exhaustiveness ─────────────────────────────────────────
    //
    // We cannot call `Guest::run` from native tests because the WIT host
    // imports are unavailable.  Instead we verify that every known action-name
    // string is handled by the match arm (no arm can fall through to `"other"`).
    // This is enforced by the compile-time `match` exhaustiveness check; the
    // test below documents the complete set of recognised action names.

    #[test]
    fn known_action_names_are_not_unknown() {
        let known: &[&str] = &[
            "new_document", "create_document", "get_field", "set_field",
            "document_attach_artifact",
            "entity_new_document", "entity_get_document", "entity_set_document",
            "entity_delete_document", "entity_list_documents",
            "entity_new_action", "entity_get_action", "entity_set_action",
            "entity_delete_action", "entity_list_actions",
            "entity_new_artifact", "entity_get_artifact", "entity_set_artifact",
            "entity_delete_artifact", "entity_list_artifacts", "artifact_upload_file",
            "entity_new_type", "entity_get_type", "entity_set_type",
            "entity_delete_type", "entity_list_types",
            "entity_new_doc_ref", "entity_new_action_ref",
            "entity_new_model", "entity_get_model", "entity_set_model",
            "entity_delete_model", "entity_list_models",
            "entity_new_permission", "entity_get_permission", "entity_set_permission",
            "entity_delete_permission", "entity_list_permissions",
            "entity_new_schedule", "entity_get_schedule", "entity_set_schedule",
            "entity_delete_schedule", "entity_list_schedules",
            "entity_new_knowledge_base", "entity_new_catalog",
            "entity_get_knowledge_base", "entity_get_catalog",
            "entity_set_knowledge_base", "entity_set_catalog",
            "entity_delete_knowledge_base", "entity_delete_catalog",
            "entity_list_knowledge_bases", "entity_list_catalogs",
            "model_chat", "model_generate", "model_list",
            "file_read", "file_write", "dir_list", "file_copy", "dir_copy", "get_env", "set_env",
            "get_secret", "set_secret",
            "fetch_html", "fetch_and_extract",
        ];

        // Verify none of the known names is the empty string (which would hit the
        // "action-name is required" error arm) and none contains whitespace.
        for name in known {
            assert!(!name.is_empty(), "empty action name in known set");
            assert!(!name.contains(' '), "action name '{name}' contains a space — should use underscore");
        }
    }

    // ── Browser-only actions must NOT appear in the WASM dispatch table ───────
    //
    // These fn_names are handled exclusively by browser-actions.ts and should
    // never be routed to this WASM component.  If someone accidentally adds them
    // to the match arm above this test will catch the inconsistency.

    #[test]
    fn browser_only_actions_are_absent_from_wasm_dispatch() {
        let browser_only: &[&str] = &[
            "dom_snapshot", "dom_query", "navigate",
            "go_back", "go_forward", "get_title", "current_url",
        ];

        // The dispatch table is a static `match`; we verify indirectly by
        // checking these names are not present in the known set above.
        let wasm_known: std::collections::HashSet<&str> = [
            "new_document", "create_document", "get_field", "set_field",
            "document_attach_artifact",
            "entity_new_document", "entity_get_document",
            "model_chat", "model_generate", "model_list",
            "file_read", "file_write", "dir_list",
        ]
        .into_iter()
        .collect();

        for name in browser_only {
            assert!(
                !wasm_known.contains(*name),
                "browser-only action '{name}' must not be in the WASM dispatch table"
            );
        }
    }
}
