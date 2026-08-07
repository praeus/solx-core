//! Document field ops:
//!
//! * `get_field` / `set_field` — read or write a single **top-level** key
//!   in `contents`. `set_field` is a shallow merge, so other fields are
//!   preserved.
//! * `get_field_at_path` / `set_field_at_path` — read or write a nested
//!   value via a `/`-separated JSON path
//!   (e.g. `metadata/tags/0`). `set_field_at_path` with `create: true`
//!   synthesizes missing parent containers (objects for non-numeric
//!   segments, arrays for numeric ones, padded with `null`).
//!
//! On its entity-store path, the path-style ops use `doc_path` (defaults
//! to root) to avoid overloading `path`, which is the JSON path inside
//! the document. The shared `doc_path_or_root` helper handles that.

use std::sync::Arc;

use serde_json::Value;
use solx_surface::managers::DocManager;

use super::{
    doc_path_or_root, document_input_from, load_document, path_or_root, require_str, to_value,
};

// ── legacy flat get/set (one field at a time) ───────────────────────────────

pub(super) async fn get_field(params: &Value, docs: &Arc<dyn DocManager>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    let field = require_str(params, "field")?;
    let doc = load_document(docs, path_or_root(params), name).await?;
    Ok(doc.contents.get(field).cloned().unwrap_or(Value::Null))
}

pub(super) async fn set_field(params: &Value, docs: &Arc<dyn DocManager>) -> Result<Value, String> {
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

// ── path-style field ops (nested reads/writes via slash-separated path) ────
//
// `path` is a `/`-separated JSON path into `contents`. Each segment is
// either a key into an object (treated as an object key) or an integer
// index into an array (parsed as `usize`). Empty segments and `..` are
// rejected. Pathological input isn't a concern because the dispatching
// `param_type_ref` schema already constrains the surface.

fn parse_path(path: &str) -> Result<Vec<String>, String> {
    if path.is_empty() {
        return Err("path must not be empty".to_string());
    }
    if path == "/" {
        return Ok(Vec::new());
    }
    let trimmed = path.trim_start_matches('/').trim_end_matches('/');
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for seg in trimmed.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(format!("invalid path segment '{seg}' in '{path}'"));
        }
        out.push(seg.to_string());
    }
    Ok(out)
}

/// Look up `segments` inside `value`, returning a clone of the leaf.
fn value_at_path<'a>(value: &'a Value, segments: &[String]) -> Result<&'a Value, String> {
    let mut cur = value;
    for seg in segments {
        cur = match cur {
            Value::Object(map) => map
                .get(seg.as_str())
                .ok_or_else(|| format!("no key '{seg}' at this path"))?,
            Value::Array(arr) => {
                let idx: usize = seg
                    .parse()
                    .map_err(|_| format!("array segment '{seg}' is not a valid array index"))?;
                arr.get(idx)
                    .ok_or_else(|| format!("array index {idx} out of bounds"))?
            }
            _ => return Err("cannot descend into a non-container value".to_string()),
        };
    }
    Ok(cur)
}

/// Replace the value at `segments` inside `value` with `new`. Returns
/// `Err` if any parent is missing (so the caller can decide whether to
/// create the path) or if a segment is invalid.
fn set_at_path(value: &mut Value, segments: &[String], new: Value) -> Result<(), String> {
    if segments.is_empty() {
        *value = new;
        return Ok(());
    }
    let (last, parents) = segments.split_last().unwrap();
    let mut cur = value;
    for seg in parents {
        cur = match cur {
            Value::Object(map) => match map.get_mut(seg.as_str()) {
                Some(v) => v,
                None => return Err(format!("no key '{seg}' at this path")),
            },
            Value::Array(arr) => {
                let idx: usize = seg
                    .parse()
                    .map_err(|_| format!("array segment '{seg}' is not a valid array index"))?;
                match arr.get_mut(idx) {
                    Some(v) => v,
                    None => return Err(format!("array index {idx} out of bounds")),
                }
            }
            _ => return Err("cannot descend into a non-container value".to_string()),
        };
    }
    match cur {
        Value::Object(map) => {
            map.insert(last.clone(), new);
            Ok(())
        }
        Value::Array(arr) => {
            let idx: usize = last
                .parse()
                .map_err(|_| format!("array segment '{last}' is not a valid array index"))?;
            let target = arr
                .get_mut(idx)
                .ok_or_else(|| format!("array index {idx} out of bounds"))?;
            *target = new;
            Ok(())
        }
        _ => Err("parent is a non-container; cannot set a key/index on it".to_string()),
    }
}

/// Walk `segments` and synthesize empty containers for any missing
/// segments along the way. The last segment is treated as a key/index
/// container too — when creation is enabled, a write of a primitive at
/// `metadata.foo` where `metadata` is missing creates `{}` at `metadata`
/// before inserting `foo`.
///
/// The kind of each synthesized parent is decided by the *next* segment:
/// if the segment after this one parses as a `usize`, the current segment
/// is materialized as an `Array` (so the next segment can be used as an
/// index); otherwise it's materialized as an `Object`. This matches
/// `value_at_path`'s dispatch so the subsequent `set_at_path` finds the
/// container it expects.
fn create_missing_parents(value: &mut Value, segments: &[String]) -> Result<(), String> {
    let mut cur = value;
    for (i, seg) in segments.iter().enumerate() {
        // `next_is_index` is the kind decision for the *current* segment:
        // a numeric segment after this one means "current is an array".
        let next_is_index = segments
            .get(i + 1)
            .map(|s| s.parse::<usize>().is_ok())
            .unwrap_or(false);
        let next = match cur {
            Value::Object(map) => {
                if !map.contains_key(seg) {
                    let placeholder = if next_is_index {
                        Value::Array(Vec::new())
                    } else {
                        Value::Object(serde_json::Map::new())
                    };
                    map.insert(seg.clone(), placeholder);
                }
                map.get_mut(seg).unwrap()
            }
            Value::Array(arr) => {
                let idx: usize = seg
                    .parse()
                    .map_err(|_| format!("array segment '{seg}' is not a valid array index"))?;
                // Extend the array up to `idx` with Null sentinels so the
                // target index is reachable.
                while arr.len() <= idx {
                    arr.push(Value::Null);
                }
                &mut arr[idx]
            }
            _ => return Err("cannot descend into a non-container value".to_string()),
        };
        cur = next;
    }
    Ok(())
}

pub(super) async fn get_field_at_path(params: &Value, docs: &Arc<dyn DocManager>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    let path = require_str(params, "path")?;
    let segments = parse_path(path)?;
    let doc = load_document(docs, doc_path_or_root(params), name).await?;
    match value_at_path(&doc.contents, &segments) {
        Ok(v) => Ok(v.clone()),
        Err(_) => Ok(Value::Null),
    }
}

pub(super) async fn set_field_at_path(params: &Value, docs: &Arc<dyn DocManager>) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    let path = require_str(params, "path")?;
    let create = params
        .get("create")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let value = params.get("value").cloned().unwrap_or(Value::Null);
    let segments = parse_path(path)?;
    let doc_path = doc_path_or_root(params).to_string();

    let mut existing = load_document(docs, &doc_path, name).await?;

    // Probe-only: does the path resolve? If not, either error out or
    // synthesize the missing parents depending on `create`. We do this
    // before the real write so we own `value` only once.
    let path_exists = value_at_path(&existing.contents, &segments).is_ok();
    if path_exists {
        set_at_path(&mut existing.contents, &segments, value)
            .map_err(|e| format!("set_field_at_path: {e}"))?;
    } else {
        if !create {
            return Err(format!(
                "path '{path}' does not exist in document '{name}' (pass create:true to create it)"
            ));
        }
        // Build the missing parent containers in-place: walk the
        // segments and synthesize an empty `Object` (or `Array` for
        // numeric segments) wherever the path is missing.
        create_missing_parents(&mut existing.contents, &segments)?;
        set_at_path(&mut existing.contents, &segments, value)
            .map_err(|e| format!("set_field_at_path after create: {e}"))?;
    }

    let input = document_input_from(&existing);
    let doc = docs.post(&doc_path, name, input).await.map_err(|e| e.to_string())?;
    to_value(&doc)
}
