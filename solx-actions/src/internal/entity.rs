//! Entity CRUD and search built-ins: documents, types, actions.
//!
//! The `entity_save_action` / `entity_delete_action` handlers apply the
//! shell-/webhook-guard that prevents an MCP tool call, a WASM guest, or a
//! `.solx` script from creating or modifying an executable action — see
//! [`super::guard_executable_action`] for the shared helper.

use std::sync::Arc;

use serde_json::{json, Value};
use solx_surface::entities::{ActionInput, TypeInput};
use solx_surface::error::SolxError;
use solx_surface::managers::{ActionManager, DocManager, TypeManager};
use solx_surface::query::{ActionSearchQuery, ListOptions, SearchQuery};

use solx_surface::entities::DocumentInput;

use super::{
    guard_executable_action, parse_input, path_or_root, require_str, to_value,
};

// ── document CRUD ────────────────────────────────────────────────────────────

pub(super) async fn doc_save(params: &Value, docs: &Arc<dyn DocManager>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    let input: DocumentInput = parse_input(params)?;
    let doc = docs.save(path_or_root(params), name, input).await.map_err(|e| e.to_string())?;
    to_value(&doc)
}

pub(super) async fn doc_get(params: &Value, docs: &Arc<dyn DocManager>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    let doc = docs.get(path_or_root(params), name).await.map_err(|e| e.to_string())?;
    to_value(&doc)
}

pub(super) async fn doc_delete(params: &Value, docs: &Arc<dyn DocManager>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    docs.delete(path_or_root(params), name).await.map_err(|e| e.to_string())?;
    Ok(json!({ "deleted": true }))
}

pub(super) async fn doc_list(params: &Value, docs: &Arc<dyn DocManager>) -> Result<Value, String> {
    let opts: ListOptions = parse_input(params)?;
    let page = docs.list(opts).await.map_err(|e| e.to_string())?;
    to_value(&page)
}

// ── type CRUD ────────────────────────────────────────────────────────────────

pub(super) async fn type_save(params: &Value, types: &Arc<dyn TypeManager>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    let input: TypeInput = parse_input(params)?;
    let ty = types.save(path_or_root(params), name, input).await.map_err(|e| e.to_string())?;
    to_value(&ty)
}

pub(super) async fn type_get(params: &Value, types: &Arc<dyn TypeManager>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    let ty = types.get(path_or_root(params), name).await.map_err(|e| e.to_string())?;
    to_value(&ty)
}

pub(super) async fn type_delete(params: &Value, types: &Arc<dyn TypeManager>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    types.delete(path_or_root(params), name).await.map_err(|e| e.to_string())?;
    Ok(json!({ "deleted": true }))
}

pub(super) async fn type_list(params: &Value, types: &Arc<dyn TypeManager>) -> Result<Value, String> {
    let opts: ListOptions = parse_input(params)?;
    let page = types.list(opts).await.map_err(|e| e.to_string())?;
    to_value(&page)
}

// ── action CRUD ──────────────────────────────────────────────────────────────
//
// `action_save` / `action_delete` apply the executable-action guard that
// closes MCP/guest/script routes to creating, modifying, or removing
// Command and Webhook actions. The same check is performed by the CLI,
// MCP, and HTTP surfaces — see `super::guard_executable_action` for the
// shared helper.

pub(super) async fn action_save(params: &Value, actions: &Arc<dyn ActionManager>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    let input: ActionInput = parse_input(params)?;
    let path = path_or_root(params);

    // Apply the guard using the *existing* row's action_type (so a merge
    // upsert that omits `action_type` can't silently rewrite what an
    // established Command action shells out to) and the *incoming* one.
    let existing = match actions.get(path, name).await {
        Ok(a) => a.action_type,
        Err(SolxError::NotFound(_)) => None,
        Err(e) => return Err(e.to_string()),
    };
    guard_executable_action(existing, input.action_type, path, name, "create or modify")?;

    let a = actions.save(path, name, input).await.map_err(|e| e.to_string())?;
    to_value(&a)
}

/// `excludeHidden: true` makes a hidden action indistinguishable from a
/// missing one.
///
/// This is the dispatch-time half of the catalogue filter: a caller that
/// already knows a hidden action's reference must not be able to read it back
/// just because it skipped discovery. It reports NotFound rather than a
/// distinct "hidden" error on purpose — a caller that can tell the two apart
/// can enumerate what is hidden.
///
/// Both spellings of the flag are accepted. This handler reads its params by
/// raw key, where the convention is snake_case (`rel_path`, `stream_id`,
/// `doc_path`), but the sibling `search_actions` deserializes an
/// `ActionSearchQuery`, which is `rename_all = "camelCase"`. One concept
/// reached by two calls should not need two spellings from the caller, and a
/// guest that guesses wrong would silently get an *unfiltered* answer.
pub(super) async fn action_get(
    params: &Value,
    actions: &Arc<dyn ActionManager>,
    config: &Arc<solx_config::ConfigService>,
) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    let path = path_or_root(params);
    let a = actions.get(path, name).await.map_err(|e| e.to_string())?;
    if exclude_hidden_flag(params) && config.tool_policy().is_hidden(&a) {
        return Err(format!("action not found: {path}/{name}"));
    }
    to_value(&a)
}

fn exclude_hidden_flag(params: &Value) -> bool {
    ["excludeHidden", "exclude_hidden"]
        .iter()
        .any(|k| params.get(k).and_then(Value::as_bool) == Some(true))
}

pub(super) async fn action_delete(params: &Value, actions: &Arc<dyn ActionManager>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    let path = path_or_root(params);
    // Same reasoning as `action_save`: if these callers can't create or
    // modify an executable action, they shouldn't be able to remove one
    // either.
    let existing = match actions.get(path, name).await {
        Ok(a) => a.action_type,
        Err(SolxError::NotFound(_)) => None,
        Err(e) => return Err(e.to_string()),
    };
    guard_executable_action(existing, None, path, name, "delete")?;
    actions.delete(path, name).await.map_err(|e| e.to_string())?;
    Ok(json!({ "deleted": true }))
}

pub(super) async fn action_list(params: &Value, actions: &Arc<dyn ActionManager>) -> Result<Value, String> {
    let opts: ListOptions = parse_input(params)?;
    let page = actions.list(opts).await.map_err(|e| e.to_string())?;
    to_value(&page)
}

// ── search ───────────────────────────────────────────────────────────────────

pub(super) async fn search_documents(params: &Value, docs: &Arc<dyn DocManager>) -> Result<Value, String> {
    let query: SearchQuery = parse_input(params)?;
    let results = docs.search(query).await.map_err(|e| e.to_string())?;
    to_value(&results)
}

/// Full-text (FTS5) + structured filter search over the action catalogue —
/// `q` matches `path`/`name`/`caption`/`description`/`category`/`phrases`,
/// ranked by relevance, and composes with the same `path_prefix`/
/// `filter_field`/date filters as `entity_list_actions`.
pub(super) async fn search_actions(params: &Value, actions: &Arc<dyn ActionManager>) -> Result<Value, String> {
    let query: ActionSearchQuery = parse_input(params)?;
    let page = actions.search(query).await.map_err(|e| e.to_string())?;
    to_value(&page)
}
