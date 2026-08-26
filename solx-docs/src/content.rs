//! Schema-aware content extraction for full-text indexing.
//!
//! `walk_contents` turns a document's `contents` JSON into two things: a flat
//! bag of searchable text (`content_text`, indexed by SQLite FTS5) and the
//! list of `DocRef` targets it points at (the `linked_to` facet, stored in
//! the `document_refs` table). When a type schema is available the walk is
//! schema-aware — `DocRef` fields are recognized by their `$ref`, and
//! rich-text fields (`x-sol-editor: "rich_text"` or `$ref` to `RichTextDoc`)
//! are converted to plain text before indexing. With no usable schema it
//! falls back to collecting every leaf string.

/// Output of walking a document's `contents` against its type schema.
pub struct ContentExtract {
    /// Names found in DocRef-typed fields.
    pub doc_ref_names: Vec<String>,
    /// All leaf string values for full-text indexing.
    pub content_text_parts: Vec<String>,
}

/// Walk `contents` using `type_schema` to identify `DocRef` fields and
/// rich-text fields. Falls back to collecting all leaf strings when the
/// schema has no `properties` map.
pub fn walk_contents(
    contents: &serde_json::Value,
    type_schema: &serde_json::Value,
) -> ContentExtract {
    let mut out = ContentExtract {
        doc_ref_names: Vec::new(),
        content_text_parts: Vec::new(),
    };

    let props = match type_schema.pointer("/properties").and_then(|v| v.as_object()) {
        Some(m) => m,
        None => {
            // No schema properties — collect all leaf strings for full-text.
            collect_strings(contents, &mut out.content_text_parts);
            return out;
        }
    };

    let contents_obj = match contents.as_object() {
        Some(m) => m,
        None => {
            collect_strings(contents, &mut out.content_text_parts);
            return out;
        }
    };

    for (field_name, field_schema) in props {
        let field_value = match contents_obj.get(field_name) {
            Some(v) => v,
            None => continue,
        };

        let ref_target = schema_ref_target(field_schema).unwrap_or("");

        match ref_target {
            "#/$defs/DocRef" => {
                // Extract the doc ref target's full reference for faceted
                // linking (`linked_to` in SearchQuery).
                if let Some(target) = doc_ref_target_string(field_value) {
                    out.doc_ref_names.push(target.clone());
                    out.content_text_parts.push(target);
                }
            }
            _ => {
                // Check for rich-text fields.
                if is_rich_text_field_schema(field_schema) {
                    if let Some(plain) = rich_text_to_plain(field_value) {
                        if !plain.trim().is_empty() {
                            out.content_text_parts.push(plain);
                        }
                        continue;
                    }
                }
                collect_strings(field_value, &mut out.content_text_parts);
            }
        }
    }

    out
}

/// Recursively collect all leaf string values from `val` into `out`.
fn collect_strings(val: &serde_json::Value, out: &mut Vec<String>) {
    // Try rich-text extraction first.
    if let Some(plain) = rich_text_to_plain(val) {
        if !plain.trim().is_empty() {
            out.push(plain);
        }
        return;
    }

    match val {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(arr) => {
            for v in arr {
                collect_strings(v, out);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values() {
                collect_strings(v, out);
            }
        }
        _ => {}
    }
}

/// Build the full reference (`/path/name`) a `DocRef` value points at, from
/// its `path`/`name` fields. Falls back to a bare `name` when `path` is
/// absent (a legacy/relative reference), and to `None` when neither is set.
fn doc_ref_target_string(field_value: &serde_json::Value) -> Option<String> {
    let name = field_value
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let path = field_value
        .get("path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    match (path, name) {
        (Some(p), Some(n)) => solx_surface::path::full_ref(p, n).ok(),
        (None, Some(n)) => Some(n.to_string()),
        _ => None,
    }
}

fn schema_ref_target(schema: &serde_json::Value) -> Option<&str> {
    schema
        .get("$ref")
        .and_then(|v| v.as_str())
        .or_else(|| {
            schema
                .get("allOf")
                .and_then(|v| v.as_array())
                .and_then(|items| {
                    items
                        .iter()
                        .find_map(|item| item.get("$ref").and_then(|v| v.as_str()))
                })
        })
}

fn is_rich_text_field_schema(schema: &serde_json::Value) -> bool {
    schema
        .get("x-sol-editor")
        .and_then(|v| v.as_str())
        .map(|value| value == "rich_text")
        .unwrap_or(false)
        || schema_ref_target(schema)
            .map(|target| target.contains("RichTextDoc"))
            .unwrap_or(false)
}

/// Extract plain text from a rich-text document value.
///
/// A rich-text doc is a JSON object with a `type` field (e.g. `"doc"`) and
/// `content` array of nodes. We walk the node tree and collect all `text`
/// leaf values.
fn rich_text_to_plain(val: &serde_json::Value) -> Option<String> {
    let obj = val.as_object()?;
    // Must have a "type" field to be a rich-text node.
    let _node_type = obj.get("type")?.as_str()?;
    let mut out = String::new();
    collect_rich_text(val, &mut out);
    let trimmed = out.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn collect_rich_text(val: &serde_json::Value, out: &mut String) {
    if let serde_json::Value::Object(obj) = val {
        if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
            out.push_str(text);
            out.push(' ');
        }
        if let Some(content) = obj.get("content").and_then(|v| v.as_array()) {
            for node in content {
                collect_rich_text(node, out);
            }
        }
    }
}

/// Flatten every string found in a JSON value into a single space-joined bag,
/// for full-text indexing. This is the naive fallback when no type schema is
/// available.
pub fn flatten_strings(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::String(s) => {
            out.push_str(s);
            out.push(' ');
        }
        serde_json::Value::Array(a) => a.iter().for_each(|v| flatten_strings(v, out)),
        serde_json::Value::Object(o) => o.values().for_each(|v| flatten_strings(v, out)),
        _ => {}
    }
}
