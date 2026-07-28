//! JSON-schema validation, ported (and slimmed) from the old `sol-core`.
//!
//! Before validating, the schema is enriched with canonical `$defs` for
//! `DocRef`, `Link`, `ArtifactRef`, and a permissive `RichTextDoc`, so custom
//! type schemas can `$ref` them (e.g. `{ "$ref": "#/$defs/DocRef" }`).

use serde_json::{json, Value};
use solx_surface::error::{Result, SolxError};

/// Validate `value` against `type_schema`.
pub fn validate_against_schema(value: &Value, type_schema: &Value) -> Result<()> {
    let enriched = enrich_schema(type_schema);
    let compiled = jsonschema::validator_for(&enriched)
        .map_err(|e| SolxError::Validation(format!("invalid schema: {e}")))?;
    let errors: Vec<String> = compiled
        .iter_errors(value)
        .map(|e| format!("{} at {}", e, e.instance_path))
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(SolxError::Validation(errors.join("; ")))
    }
}

/// Check that `type_schema` is a syntactically valid JSON schema.
pub fn validate_schema_syntax(type_schema: &Value) -> Result<()> {
    jsonschema::validator_for(&enrich_schema(type_schema))
        .map(|_| ())
        .map_err(|e| SolxError::Validation(format!("invalid schema: {e}")))
}

/// Inject canonical `$defs` into a schema (idempotent; preserves existing defs).
pub fn enrich_schema(schema: &Value) -> Value {
    let mut enriched = schema.clone();
    let Some(obj) = enriched.as_object_mut() else {
        return enriched;
    };

    let doc_ref_def = json!({
        "type": "object",
        "properties": {
            "id":   { "type": ["string", "null"] },
            "name": { "type": ["string", "null"] },
            "path": { "type": ["string", "null"] }
        }
    });
    let link_def = json!({
        "type": "object",
        "properties": {
            "url":         { "type": "string" },
            "title":       { "type": ["string", "null"] },
            "description": { "type": ["string", "null"] }
        }
    });
    let artifact_ref_def = json!({
        "type": "object",
        "properties": { "name": { "type": "string" } },
        "required": ["name"]
    });
    // Permissive rich-text node (we don't ship the full rich-text model here).
    let rich_text_def = json!({ "type": "object" });

    let defs = obj.entry("$defs").or_insert_with(|| json!({}));
    if let Some(map) = defs.as_object_mut() {
        map.entry("DocRef").or_insert_with(|| doc_ref_def.clone());
        map.entry("Link").or_insert_with(|| link_def.clone());
        map.entry("ArtifactRef").or_insert_with(|| artifact_ref_def.clone());
        map.entry("RichTextDoc").or_insert_with(|| rich_text_def.clone());
    }
    enriched
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_and_enriches() {
        let schema = json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": { "type": "string" },
                "manager": { "$ref": "#/$defs/DocRef" }
            }
        });
        assert!(validate_against_schema(&json!({"name": "x"}), &schema).is_ok());
        assert!(
            validate_against_schema(&json!({"name": "x", "manager": {"path": "/p/m"}}), &schema)
                .is_ok()
        );
        assert!(validate_against_schema(&json!({}), &schema).is_err());
    }
}
