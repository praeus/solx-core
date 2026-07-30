//! Turning a resolved `TypeEntity.schema` into an MCP tool's `input_schema`.

use std::sync::Arc;

use rmcp::model::JsonObject;
use serde_json::Value;

/// `{"type": "object"}` — used when an action has no `param_type_ref`.
pub fn permissive_object_schema() -> Arc<JsonObject> {
    Arc::new(object_of(Value::String("object".into())))
}

/// A resolved `TypeEntity.schema` is already real JSON Schema — use it
/// directly as a tool's `input_schema`.
pub fn schema_from_type_value(schema: &Value) -> Arc<JsonObject> {
    match schema.as_object() {
        Some(obj) => Arc::new(obj.clone()),
        // Defensive fallback: a stored type schema should always be a JSON
        // object, but don't let a malformed one panic tool listing.
        None => permissive_object_schema(),
    }
}

fn object_of(type_value: Value) -> JsonObject {
    let mut m = JsonObject::new();
    m.insert("type".to_string(), type_value);
    m
}
