//! DDL and the minimal built-in type seed.

use serde_json::{json, Value};

pub const DDL: &str = "\
CREATE TABLE IF NOT EXISTS types (\
    id TEXT PRIMARY KEY,\
    path TEXT NOT NULL,\
    name TEXT NOT NULL,\
    description TEXT NOT NULL DEFAULT '',\
    schema TEXT NOT NULL DEFAULT '{}',\
    groups TEXT NOT NULL DEFAULT '[]',\
    created_at TEXT NOT NULL,\
    updated_at TEXT NOT NULL,\
    UNIQUE(path, name)\
);";

const CORE_PATH: &str = "/types/core";
const DOCS_PATH: &str = "/types/docs";
/// Must match `solx_actions::seed::BUILTIN_TYPES_PATH` — kept as an
/// independent literal (not a shared Rust constant) since `solx-types` does
/// not depend on `solx-actions` (dependencies point the other way).
const BUILTIN_TYPES_PATH: &str = "/builtin/types";

/// A built-in type to seed: (path, name, description, schema, groups).
pub struct SeedType {
    pub path: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Value,
    pub groups: Vec<&'static str>,
}

/// The minimal built-in type set. Primitives + a permissive document base +
/// `BlogPostWithComments` (the one extraction type we keep).
pub fn builtin_types() -> Vec<SeedType> {
    let mut out = Vec::new();

    for (name, ty) in [
        ("String", "string"),
        ("Number", "number"),
        ("Integer", "integer"),
        ("Boolean", "boolean"),
        ("Object", "object"),
        ("Array", "array"),
        ("Null", "null"),
    ] {
        out.push(SeedType {
            path: CORE_PATH,
            name,
            description: "Built-in primitive type.",
            schema: json!({ "type": ty }),
            groups: vec!["primitive"],
        });
    }

    // Permissive base document type.
    out.push(SeedType {
        path: DOCS_PATH,
        name: "Document",
        description: "Generic document with arbitrary JSON contents.",
        schema: json!({ "type": "object" }),
        groups: vec!["document-type"],
    });

    out.push(SeedType {
        path: DOCS_PATH,
        name: "BlogPostWithComments",
        description: "A blog post with rich-text content and a recursive comment tree.",
        schema: blog_post_with_comments_schema(),
        groups: vec!["document-type"],
    });

    out.extend(builtin_action_param_types());

    out
}

/// Parameter schemas for the built-in internal actions seeded by
/// `solx-actions/src/seed.rs` (`param_type_ref` points at
/// `{BUILTIN_TYPES_PATH}/{name}`).
fn builtin_action_param_types() -> Vec<SeedType> {
    let path_and_name = json!({
        "path": { "type": "string", "description": "Directory-style path, e.g. /research/ai. Defaults to the root '/'." },
        "name": { "type": "string", "description": "Single path segment identifying the entity." },
    });

    vec![
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "EntityRefParams",
            description: "Reference to an entity by path+name (get/delete).",
            schema: json!({
                "type": "object",
                "required": ["name"],
                "properties": path_and_name,
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "ListParams",
            description: "Pagination/filtering options for a list operation.",
            schema: json!({
                "type": "object",
                "properties": {
                    "path_prefix": { "type": "string" },
                    "limit": { "type": "integer" },
                    "offset": { "type": "integer" },
                    "filter_field": { "type": "string" },
                    "filter_value": { "type": "string" },
                    "sort_by": { "type": "string" },
                    "sort_order": { "type": "string", "enum": ["asc", "desc"] },
                    "date_after": { "type": "string" },
                    "date_before": { "type": "string" },
                }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "DocumentCrudParams",
            description: "Params for entity_new_document / entity_set_document.",
            schema: json!({
                "type": "object",
                "required": ["name"],
                "properties": {
                    "path": path_and_name["path"],
                    "name": path_and_name["name"],
                    "title": { "type": "string" },
                    "summary": { "type": "string" },
                    "type_ref": { "type": "string", "description": "Full reference of the document's type, e.g. /types/core/Object. Required on create." },
                    "contents": {},
                    "author": { "type": "string" },
                    "pub_date": { "type": "string" },
                    "confidence": { "type": "number" },
                    "links": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["kind", "target"],
                            "properties": {
                                "kind": { "type": "string", "enum": ["doc_ref", "url"] },
                                "target": { "type": "string" },
                                "field": { "type": "string" },
                                "title": { "type": "string" },
                                "description": { "type": "string" },
                            }
                        }
                    },
                }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "TypeCrudParams",
            description: "Params for entity_new_type / entity_set_type.",
            schema: json!({
                "type": "object",
                "required": ["name"],
                "properties": {
                    "path": path_and_name["path"],
                    "name": path_and_name["name"],
                    "description": { "type": "string" },
                    "schema": { "type": "object", "description": "A JSON Schema document. Required on create." },
                    "groups": { "type": "array", "items": { "type": "string" } },
                }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "ActionCrudParams",
            description: "Params for entity_new_action / entity_set_action.",
            schema: json!({
                "type": "object",
                "required": ["name"],
                "properties": {
                    "path": path_and_name["path"],
                    "name": path_and_name["name"],
                    "caption": { "type": "string" },
                    "description": { "type": "string" },
                    "capabilities": { "type": "array", "items": { "type": "string" } },
                    "phrases": { "type": "array", "items": { "type": "string" } },
                    "category": { "type": "string" },
                    "param_type_ref": { "type": "string" },
                    "result_type_ref": { "type": "string" },
                    "action_type": { "type": "string", "enum": ["wasm", "webhook", "command", "internal", "script"], "description": "Required on create." },
                    "fn_name": { "type": "string", "description": "Command string / URL / internal op name / WASM export, depending on action_type." },
                    "bin_name": { "type": "string", "description": "Artifact file name (wasm: component; script: .solx source)." },
                    "action_config": {},
                    "trusted": { "type": "boolean" },
                }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "SearchDocumentsParams",
            description: "Full-text + faceted search over documents.",
            schema: json!({
                "type": "object",
                "properties": {
                    "q": { "type": "string", "description": "Free-text query." },
                    "path_prefix": { "type": "string" },
                    "type_ref": { "type": "string" },
                    "linked_to": { "type": "string", "description": "Full reference of a document this search's hits must link to." },
                    "limit": { "type": "integer" },
                    "offset": { "type": "integer" },
                }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "FilePutParams",
            description: "Write bytes to a rel-path under the files root.",
            schema: json!({
                "type": "object",
                "required": ["rel_path", "content"],
                "properties": {
                    "rel_path": { "type": "string" },
                    "content": { "type": "string" },
                    "encoding": { "type": "string", "enum": ["utf8", "base64"], "description": "Defaults to utf8." },
                }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "FileGetParams",
            description: "Read or delete a file at a rel-path under the files root.",
            schema: json!({
                "type": "object",
                "required": ["rel_path"],
                "properties": { "rel_path": { "type": "string" } }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "FileListParams",
            description: "List stored rel-paths under a prefix.",
            schema: json!({
                "type": "object",
                "properties": { "prefix": { "type": "string" } }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "OauthStartParams",
            description: "Start a local OAuth 2.0 authorization-code loopback listener.",
            schema: json!({
                "type": "object",
                "properties": { "port": { "type": "integer", "description": "Defaults to 8765." } }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "OauthAwaitParams",
            description: "Block until the OAuth loopback for a state_value receives its callback.",
            schema: json!({
                "type": "object",
                "required": ["state_value"],
                "properties": {
                    "state_value": { "type": "string" },
                    "timeout_secs": { "type": "integer" },
                }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "OauthStopParams",
            description: "Stop an OAuth loopback listener.",
            schema: json!({
                "type": "object",
                "required": ["state_value"],
                "properties": { "state_value": { "type": "string" } }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "GetFieldParams",
            description: "Read one field from a document's contents.",
            schema: json!({
                "type": "object",
                "required": ["name", "field"],
                "properties": {
                    "path": { "type": "string", "description": "Defaults to the root '/'." },
                    "name": { "type": "string" },
                    "field": { "type": "string" },
                }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "SetFieldParams",
            description: "Write one field on a document's contents (shallow merge — other fields are preserved).",
            schema: json!({
                "type": "object",
                "required": ["name", "field"],
                "properties": {
                    "path": { "type": "string", "description": "Defaults to the root '/'." },
                    "name": { "type": "string" },
                    "field": { "type": "string" },
                    "value": {},
                }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "FileCopyParams",
            description: "Copy a file, or recursively copy a directory, within the files root.",
            schema: json!({
                "type": "object",
                "required": ["source", "dest"],
                "properties": {
                    "source": { "type": "string" },
                    "dest": { "type": "string" },
                }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "GetEnvParams",
            description: "Read a variable from the in-process environment store.",
            schema: json!({
                "type": "object",
                "required": ["key"],
                "properties": { "key": { "type": "string" } }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "SetEnvParams",
            description: "Write a variable to the in-process environment store.",
            schema: json!({
                "type": "object",
                "required": ["key", "value"],
                "properties": {
                    "key": { "type": "string" },
                    "value": { "type": "string" },
                }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "HttpRequestParams",
            description: "Issue an HTTP request with optional method, headers, body, and timeout. Non-2xx responses are not errors — the response is returned as-is and the caller decides what to do with `status`.",
            schema: json!({
                "type": "object",
                "required": ["url"],
                "properties": {
                    "url": { "type": "string" },
                    "method": { "type": "string", "description": "HTTP method (GET, POST, PUT, DELETE, PATCH, HEAD, ...). Defaults to GET." },
                    "headers": { "type": "object", "description": "Map of header name -> string value.", "additionalProperties": { "type": "string" } },
                    "body": { "type": "string", "description": "Request body, encoded per `body_encoding` (ignored by methods that have no body)." },
                    "body_encoding": { "type": "string", "enum": ["utf8", "base64"], "description": "Encoding of `body`. Defaults to utf8." },
                    "timeout_secs": { "type": "integer", "description": "Per-request timeout. Defaults to 30." },
                }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "EmptyParams",
            description: "No parameters.",
            schema: json!({ "type": "object" }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "RandomIntParams",
            description: "Random integer in the inclusive [lo, hi] range. Defaults to [0, i64::MAX].",
            schema: json!({
                "type": "object",
                "properties": {
                    "lo": { "type": "integer" },
                    "hi": { "type": "integer" },
                }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "RandomStringParams",
            description: "Random alphanumeric string. `length` defaults to 16.",
            schema: json!({
                "type": "object",
                "properties": { "length": { "type": "integer", "minimum": 0 } }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "OpenUrlParams",
            description: "Open a URL in the system browser via the platform-native handler.",
            schema: json!({
                "type": "object",
                "required": ["url"],
                "properties": {
                    "url": { "type": "string", "description": "Absolute URL to open (http/https/file/etc.)." }
                }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "DirDeleteParams",
            description: "Recursively delete a directory (and every file under it) within the files root.",
            schema: json!({
                "type": "object",
                "required": ["rel_path"],
                "properties": { "rel_path": { "type": "string" } }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "GetFieldAtPathParams",
            description: "Read a field at a slash-separated path inside a document's contents. Returns null for missing paths.",
            schema: json!({
                "type": "object",
                "required": ["name", "path"],
                "properties": {
                    "doc_path": { "type": "string", "description": "Defaults to the root '/'." },
                    "name": { "type": "string" },
                    "path": { "type": "string", "description": "Slash-separated path inside `contents`, e.g. 'metadata/tags/0'." },
                }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "SetFieldAtPathParams",
            description: "Write a field at a slash-separated path inside a document's contents. Pass `create: true` to synthesize missing parents.",
            schema: json!({
                "type": "object",
                "required": ["name", "path"],
                "properties": {
                    "doc_path": { "type": "string", "description": "Defaults to the root '/'." },
                    "name": { "type": "string" },
                    "path": { "type": "string", "description": "Slash-separated path inside `contents`." },
                    "value": {},
                    "create": { "type": "boolean", "description": "When true, missing parents along the path are created (objects for non-numeric segments, arrays for numeric ones, padded with nulls). Defaults to false." },
                }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "GetSecretParams",
            description: "Read a secret scoped to the calling action.",
            schema: json!({
                "type": "object",
                "required": ["name"],
                "properties": { "name": { "type": "string" } }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "SetSecretParams",
            description: "Write a secret scoped to the calling action.",
            schema: json!({
                "type": "object",
                "required": ["name", "value"],
                "properties": {
                    "name": { "type": "string" },
                    "value": { "type": "string" },
                }
            }),
            groups: vec!["builtin-params"],
        },
    ]
}

/// Hand-written schema for `BlogPostWithComments`, mirroring the shape of the
/// old `sol-core` extraction type (icon/content/comments/text/paragraphs with a
/// recursive `BlogComment`).
fn blog_post_with_comments_schema() -> Value {
    json!({
        "type": "object",
        "required": ["content", "text"],
        "properties": {
            "icon": { "$ref": "#/$defs/ArtifactRef" },
            "content": { "$ref": "#/$defs/RichTextDoc" },
            "text": { "type": "string" },
            "paragraphs": { "type": "array", "items": { "type": "string" } },
            "comments": {
                "type": "array",
                "items": { "$ref": "#/$defs/BlogComment" }
            }
        },
        "$defs": {
            "BlogComment": {
                "type": "object",
                "required": ["text"],
                "properties": {
                    "author": { "type": ["string", "null"] },
                    "text": { "type": "string" },
                    "date": { "type": ["string", "null"] },
                    "replies": {
                        "type": "array",
                        "items": { "$ref": "#/$defs/BlogComment" }
                    }
                }
            }
        }
    })
}
