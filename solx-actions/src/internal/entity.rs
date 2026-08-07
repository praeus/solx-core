//! Entity CRUD and search built-ins: documents, types, actions.
//!
//! The `entity_post_action` / `entity_delete_action` handlers apply the
//! shell-/webhook-guard that prevents an MCP tool call, a WASM guest, or a
//! `.solx` script from creating or modifying an executable action — see
//! [`super::guard_executable_action`] for the shared helper.

use std::sync::Arc;

use serde_json::{json, Value};
use solx_surface::entities::{ActionInput, TypeInput};
use solx_surface::error::SolxError;
use solx_surface::managers::{ActionManager, DocManager, TypeManager};
use solx_surface::query::{ListOptions, SearchQuery};

use solx_surface::entities::DocumentInput;

use super::{
    guard_executable_action, parse_input, path_or_root, require_str, to_value,
};

// ── document CRUD ────────────────────────────────────────────────────────────

pub(super) async fn doc_post(params: &Value, docs: &Arc<dyn DocManager>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    let input: DocumentInput = parse_input(params)?;
    let doc = docs.post(path_or_root(params), name, input).await.map_err(|e| e.to_string())?;
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

pub(super) async fn type_post(params: &Value, types: &Arc<dyn TypeManager>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    let input: TypeInput = parse_input(params)?;
    let ty = types.post(path_or_root(params), name, input).await.map_err(|e| e.to_string())?;
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
// `action_post` / `action_delete` apply the executable-action guard that
// closes MCP/guest/script routes to creating, modifying, or removing
// Command and Webhook actions. The same check is performed by the CLI,
// MCP, and HTTP surfaces — see `super::guard_executable_action` for the
// shared helper.

pub(super) async fn action_post(params: &Value, actions: &Arc<dyn ActionManager>) -> Result<Value, String> {
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

    let a = actions.post(path, name, input).await.map_err(|e| e.to_string())?;
    to_value(&a)
}

pub(super) async fn action_get(params: &Value, actions: &Arc<dyn ActionManager>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    let a = actions.get(path_or_root(params), name).await.map_err(|e| e.to_string())?;
    to_value(&a)
}

pub(super) async fn action_delete(params: &Value, actions: &Arc<dyn ActionManager>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    let path = path_or_root(params);
    // Same reasoning as `action_post`: if these callers can't create or
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

/// Structured filtering over the action catalogue — actions have no
/// full-text index (only documents do, via Tantivy), so this is `list` with
/// `ListOptions`' filters, not fuzzy relevance ranking.
pub(super) async fn search_actions(params: &Value, actions: &Arc<dyn ActionManager>) -> Result<Value, String> {
    let opts: ListOptions = parse_input(params)?;
    let page = actions.list(opts).await.map_err(|e| e.to_string())?;
    to_value(&page)
}
