//! File-store built-ins: single-file put/get/delete, list, copy, and the
//! two recursive directory operations (`dir_copy`, `dir_delete`).
//!
//! These are general-purpose and unrestricted — Internal actions carry
//! the same trust level as the dispatcher itself, same as the OAuth
//! loopback. There is no sandboxing model for them the way there is for
//! untrusted WASM guests.

use std::sync::Arc;

use base64::Engine as _;
use serde_json::{json, Value};
use solx_surface::managers::FileStore;

use super::require_str;

pub(super) async fn file_put(params: &Value, files: &Arc<dyn FileStore>) -> Result<Value, String> {
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

pub(super) async fn file_get(params: &Value, files: &Arc<dyn FileStore>) -> Result<Value, String> {
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

pub(super) async fn file_delete(params: &Value, files: &Arc<dyn FileStore>) -> Result<Value, String> {
    let rel_path = require_str(params, "rel_path")?;
    files.delete(rel_path).await.map_err(|e| e.to_string())?;
    Ok(json!({ "deleted": true }))
}

pub(super) async fn file_list(params: &Value, files: &Arc<dyn FileStore>) -> Result<Value, String> {
    let prefix = params.get("prefix").and_then(Value::as_str).unwrap_or("");
    let entries = files.list(prefix).await.map_err(|e| e.to_string())?;
    Ok(json!({ "files": entries }))
}

pub(super) async fn file_copy(params: &Value, files: &Arc<dyn FileStore>) -> Result<Value, String> {
    let source = require_str(params, "source")?;
    let dest = require_str(params, "dest")?;
    let bytes = files.get(source).await.map_err(|e| e.to_string())?;
    files.put(dest, bytes).await.map_err(|e| e.to_string())?;
    Ok(json!({ "dest": dest }))
}

/// Recursively copies every entry under `source` to the matching relative
/// position under `dest`.
pub(super) async fn dir_copy(params: &Value, files: &Arc<dyn FileStore>) -> Result<Value, String> {
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

// ── dir_delete ───────────────────────────────────────────────────────────────
//
// The file-store trait only exposes single-file `delete`; tree removal
// needs a `list` + per-entry `delete`. This mirrors the structure of
// `dir_copy` exactly, just inverted, so the two stay symmetric.

pub(super) async fn dir_delete(params: &Value, files: &Arc<dyn FileStore>) -> Result<Value, String> {
    let rel_path = require_str(params, "rel_path")?;
    let prefix = rel_path.trim_end_matches('/');
    let entries = files.list(rel_path).await.map_err(|e| e.to_string())?;
    let mut deleted = Vec::with_capacity(entries.len());
    for entry in &entries {
        // Defensive: only delete entries actually under `prefix`. If a
        // concurrent write lands a sibling outside the subtree between
        // `list` and the per-entry `delete`, we don't follow it.
        if entry == prefix || !entry.starts_with(&format!("{prefix}/")) {
            continue;
        }
        files.delete(entry).await.map_err(|e| e.to_string())?;
        deleted.push(entry.clone());
    }
    Ok(json!({ "deleted": deleted }))
}
