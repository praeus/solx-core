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

    out.push(SeedType {
        path: BUILTIN_TYPES_PATH,
        name: "MediaDocument",
        description: "Result of a solx-media extraction. Persisted to solx-server via entity_save_document by the solx-media actions.",
        schema: media_document_schema(),
        groups: vec!["media", "document-type"],
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
            description: "Read a variable from the environment store.",
            schema: json!({
                "type": "object",
                "required": ["key"],
                "properties": {
                    "key": { "type": "string" },
                    "namespace": {
                        "type": "string",
                        "description": "Namespace to read from. Defaults to 'default', which is also where env_mappings entries land."
                    }
                }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "SetEnvParams",
            description: "Write a variable to the environment store, optionally persisting it across restarts.",
            schema: json!({
                "type": "object",
                "required": ["key", "value"],
                "properties": {
                    "key": { "type": "string" },
                    "value": { "type": "string" },
                    "namespace": {
                        "type": "string",
                        "description": "Namespace to write to. Defaults to 'default'."
                    },
                    "persist": {
                        "type": "boolean",
                        "description": "Also write the variable to solx-config.json under env_vars, so it survives a restart. Persistence is sticky: once a variable is persisted, later writes keep updating the config even without this flag. Stored in plaintext — use set_secret for anything sensitive. Remove a persisted variable by deleting it from solx-config.json."
                    }
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
        // Action consoles — see `solx-actions::console` and
        // `docs/console-implementation-plan.md`.
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "ConsolePrintParams",
            description: "Write one entry to the calling action's own console. Requires an action caller.",
            schema: json!({
                "type": "object",
                "properties": {
                    "level": { "type": "string", "description": "Defaults to 'info'. Free-form — debug/info/warn/error/chunk are the conventional values." },
                    "message": { "type": "string" },
                    "data": { "description": "Optional structured payload, any JSON value." },
                }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "ConsoleReadParams",
            description: "Read entries from an action's console, oldest first, starting at from_seq.",
            schema: json!({
                "type": "object",
                "required": ["action_ref"],
                "properties": {
                    "action_ref": { "type": "string", "description": "Full reference of the console to read, e.g. /packages/solx-ollama/ollama-chat." },
                    "from_seq": { "type": "integer", "description": "Inclusive lower bound. Defaults to the oldest retained entry." },
                    "limit": { "type": "integer", "description": "Defaults to 200, capped at 1000." },
                }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "ConsoleTailParams",
            description: "Like console/read, but if nothing new is available yet, long-polls up to wait_secs before returning an empty result.",
            schema: json!({
                "type": "object",
                "required": ["action_ref"],
                "properties": {
                    "action_ref": { "type": "string" },
                    "cursor": { "type": "integer", "description": "Pass the previous call's next_cursor to continue from there." },
                    "limit": { "type": "integer", "description": "Defaults to 200, capped at 1000." },
                    "wait_secs": { "type": "integer", "description": "Long-poll ceiling, capped at 60. Omit to return immediately (empty if nothing new)." },
                }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "ConsoleClearParams",
            description: "Drop entries from the front of an action's console, freeing retention.",
            schema: json!({
                "type": "object",
                "required": ["action_ref"],
                "properties": {
                    "action_ref": { "type": "string" },
                    "before_seq": { "type": "integer", "description": "Drop everything with seq < this. Omit to drop everything currently retained." },
                }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "ConsoleListParams",
            description: "List known consoles, most recently written first.",
            schema: json!({
                "type": "object",
                "properties": {
                    "prefix": { "type": "string", "description": "Only consoles whose action_ref starts with this." },
                    "limit": { "type": "integer", "description": "Defaults to 100, capped at 1000." },
                }
            }),
            groups: vec!["builtin-params"],
        },
        // Asynchronous actions — see `solx-actions::invocations` and
        // `docs/async-actions-plan.md`.
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "ActionStartParams",
            description: "Start an action detached: returns an invocation_id immediately while it runs in the background.",
            schema: json!({
                "type": "object",
                "required": ["name"],
                "properties": {
                    "path": { "type": "string", "description": "Directory-style path of the action to start, e.g. /research/ai. Defaults to the root '/'." },
                    "name": { "type": "string", "description": "Single path segment identifying the action to start." },
                    "params": { "description": "Parameters passed to the started action, exactly as exec's params. Defaults to {}." },
                }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "ActionStopParams",
            description: "Request that a detached invocation stop.",
            schema: json!({
                "type": "object",
                "required": ["invocation_id"],
                "properties": {
                    "invocation_id": { "type": "string" },
                    "force": { "type": "boolean", "description": "Skip the cooperative grace period and abort immediately. Defaults to false." },
                    "grace_secs": { "type": "integer", "description": "Overrides the configured stop_grace_secs for this call." },
                }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "ActionPollParams",
            description: "Check a detached invocation's status, optionally long-polling until it finishes.",
            schema: json!({
                "type": "object",
                "required": ["invocation_id"],
                "properties": {
                    "invocation_id": { "type": "string" },
                    "wait_secs": { "type": "integer", "description": "Long-poll ceiling, capped at 60. Omit to return the current status immediately." },
                }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "HttpStreamStartParams",
            description: "Issue a streaming HTTP request. Returns as soon as response headers arrive; the body is read into a cursor-addressable buffer by http_stream/poll.",
            schema: json!({
                "type": "object",
                "required": ["url"],
                "properties": {
                    "url": { "type": "string" },
                    "method": { "type": "string", "description": "HTTP method (GET, POST, PUT, DELETE, PATCH, HEAD, ...). Defaults to GET." },
                    "headers": { "type": "object", "description": "Map of header name -> string value.", "additionalProperties": { "type": "string" } },
                    "body": { "type": "string", "description": "Request body, encoded per `body_encoding` (ignored by methods that have no body)." },
                    "body_encoding": { "type": "string", "enum": ["utf8", "base64"], "description": "Encoding of `body`. Defaults to utf8." },
                    "timeout_secs": { "type": "integer", "description": "Connect timeout only - a stream has no overall timeout. Defaults to 30." },
                }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "HttpStreamPollParams",
            description: "Drain newline-delimited JSON chunks buffered for a stream since cursor, optionally long-polling for more.",
            schema: json!({
                "type": "object",
                "required": ["stream_id"],
                "properties": {
                    "stream_id": { "type": "string" },
                    "cursor": { "type": "integer", "description": "Pass the previous call's next_cursor to continue from there. Omit to read from the start." },
                    "wait_secs": { "type": "integer", "description": "Long-poll ceiling, capped at 60. Omit to return immediately (possibly empty) if nothing new." },
                }
            }),
            groups: vec!["builtin-params"],
        },
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "HttpStreamCloseParams",
            description: "Stop a stream's reader task and drop its buffer.",
            schema: json!({
                "type": "object",
                "required": ["stream_id"],
                "properties": {
                    "stream_id": { "type": "string" },
                }
            }),
            groups: vec!["builtin-params"],
        },
        // Widgets — an action that renders a UI declares this as its
        // `result_type_ref` and returns a value of this shape. There is no
        // widget runtime on the backend: the frontend fetches the bundle from
        // `GET /files/{bin_name}` and the widget calls back in via
        // `POST /actions/{ref}`. See `docs/widget-actions.md`.
        SeedType {
            path: BUILTIN_TYPES_PATH,
            name: "WidgetDescriptor",
            description: "Result shape of an action that renders a UI. Set an action's result_type_ref to this to mark it as a widget.",
            schema: json!({
                "type": "object",
                "required": ["tag_name", "bin_name"],
                "properties": {
                    "tag_name": { "type": "string", "description": "The custom-element tag name the bundle registers." },
                    "bin_name": { "type": "string", "description": "File-store path of the widget's JS/ESM bundle." },
                    "fields": { "description": "Initial data handed to the element on mount, any JSON value." },
                }
            }),
            groups: vec!["builtin-params"],
        },
    ]
}

/// Client-side preview template for `BlogPostWithComments` (consumed by
/// `DocumentPreview.tsx` in solx-web via `x-sol-template`). Renders the
/// extracted `paragraphs` as `<p>` tags — the rich-text `content` field is
/// intentionally not rendered here, since it duplicates `paragraphs`/`text`
/// and is far less readable as a preview. Also renders the post-level `icon`
/// (as an `<img>` resolved through the files store) and the `comments` tree
/// (recursively, via a self-referencing helper declared inline in the EJS
/// scriptlet) — each comment's own `icon`, if present, renders directly as
/// a hotlinked `<img src>` since it's a plain URL, not an ArtifactRef.
const BLOG_POST_WITH_COMMENTS_TEMPLATE: &str = r#"<div class="doc-preview blog-preview">
  <% function iconRelPath(icon) {
    if (typeof icon === "string") return icon;
    if (icon && typeof icon === "object") {
      if (typeof icon.relPath === "string" && icon.relPath) return icon.relPath;
      if (typeof icon.name === "string" && icon.name) return "files/docs/shared/" + icon.name;
    }
    return "";
  } %>
  <% const iconPath = iconRelPath(contents.icon); %>
  <% if (iconPath) { %>
    <img class="blog-preview__icon" src="/api/files/raw?relPath=<%- encodeURIComponent(iconPath) %>" alt="" />
  <% } %>
  <h1><%= document.title || document.name %></h1>
  <p class="doc-preview__meta">
    <code><%= document.path %>/<%= document.name %></code>
  </p>
  <% if (document.summary) { %><p class="doc-preview__summary"><%= document.summary %></p><% } %>

  <% const paragraphs = (contents.paragraphs && contents.paragraphs.length)
       ? contents.paragraphs
       : (contents.text ? contents.text.split(/\n\s*\n/).filter(Boolean) : []); %>
  <% paragraphs.forEach(function (para) { %>
    <p><%= para %></p>
  <% }); %>

  <% if (contents.comments && contents.comments.length) { %>
    <h2>Comments (<%= contents.comments.length %>)</h2>
    <ul class="blog-preview__comments">
      <% function renderComment(c) { %>
        <li class="blog-preview__comment">
          <div class="blog-preview__comment-meta">
            <% if (c.icon) { %><img class="blog-preview__comment-icon" src="<%- c.icon %>" alt="" /><% } %>
            <strong><%= c.author || "Anonymous" %></strong>
            <% if (c.date) { %><span class="blog-preview__comment-date"><%= c.date %></span><% } %>
          </div>
          <p class="blog-preview__comment-text"><%= c.text %></p>
          <% if (c.replies && c.replies.length) { %>
            <ul class="blog-preview__comment-replies">
              <% c.replies.forEach(function (reply) { renderComment(reply); }); %>
            </ul>
          <% } %>
        </li>
      <% } %>
      <% contents.comments.forEach(function (c) { renderComment(c); }); %>
    </ul>
  <% } %>
</div>"#;

/// Hand-written schema for `BlogPostWithComments`, mirroring the shape of the
/// old `sol-core` extraction type (icon/content/comments/text/paragraphs with a
/// recursive `BlogComment`).
fn blog_post_with_comments_schema() -> Value {
    json!({
        "type": "object",
        "required": ["content", "text"],
        "properties": {
            "icon": {
                "$ref": "#/$defs/ArtifactRef",
                "title": "Icon",
                "description": "Optional preview image (resolved via the files store)."
            },
            "content": {
                "$ref": "#/$defs/RichTextDoc",
                "title": "Rich Content",
                "description": "Full Tiptap rich-text document."
            },
            "text": {
                "type": "string",
                "title": "Plain Text",
                "description": "Plain-text version of the post body."
            },
            "paragraphs": {
                "type": "array",
                "items": { "type": "string" },
                "title": "Paragraphs",
                "description": "Body split into paragraphs (preview renders these)."
            },
            "comments": {
                "type": "array",
                "items": { "$ref": "#/$defs/BlogComment" },
                "title": "Comments",
                "description": "Top-level comments; each comment may carry nested replies."
            }
        },
        "x-sol-template": BLOG_POST_WITH_COMMENTS_TEMPLATE,
        "$defs": {
            "BlogComment": {
                "type": "object",
                "required": ["text"],
                "properties": {
                    "author": {
                        "type": ["string", "null"],
                        "title": "Author",
                        "description": "Display name; null for anonymous."
                    },
                    "icon": {
                        "type": ["string", "null"],
                        "title": "Icon",
                        "description": "URL of the commenter's userpic/avatar, if any. Unlike the post-level `icon` (an ArtifactRef into the files store), this is a direct hotlinked URL — extractors don't download per-commenter avatars."
                    },
                    "text": {
                        "type": "string",
                        "title": "Comment Text"
                    },
                    "date": {
                        "type": ["string", "null"],
                        "title": "Date",
                        "description": "ISO-8601 date; null when unknown."
                    },
                    "replies": {
                        "type": "array",
                        "items": { "$ref": "#/$defs/BlogComment" },
                        "title": "Replies",
                        "description": "Nested replies to this comment (recursive)."
                    }
                }
            }
        }
    })
}

/// Schema for the `MediaDocument` shape returned by the solx-media package
/// (`solx-media` action results, registered as `/builtin/types/MediaDocument`).
///
/// One flat shape covers all four extraction modes (`image-text`,
/// `audio-transcript`, `video-transcript`, `materialized-html`). The `kind`
/// discriminator picks which fields are populated. Future fields can be added
/// without a schema migration — unknown fields are ignored on read.
fn media_document_schema() -> Value {
    json!({
        "type": "object",
        "required": ["kind", "document_name", "contents"],
        "properties": {
            "kind": {
                "type": "string",
                "title": "Kind",
                "enum": [
                    "image-text",
                    "audio-transcript",
                    "video-transcript",
                    "materialized-html"
                ],
                "description": "Discriminator for which extraction mode produced this document."
            },
            "document_name": {
                "type": "string",
                "title": "Document Name",
                "description": "Suggested document name (used as the basename under the persisted path)."
            },
            "title": {
                "type": ["string", "null"],
                "title": "Title",
                "description": "Display title; null when not provided."
            },
            "summary": {
                "type": ["string", "null"],
                "title": "Summary",
                "description": "Short summary; null when not provided."
            },
            "author": {
                "type": ["string", "null"],
                "title": "Author",
                "description": "Attributed author; null when not provided."
            },
            "contents": {
                "type": "object",
                "title": "Contents",
                "description": "Free-form JSON contents of the document. Specific shape depends on `kind`."
            },
            "artifacts": {
                "type": "array",
                "items": { "$ref": "#/$defs/EmbeddedArtifact" },
                "title": "Artifacts",
                "description": "Embedded artifacts (e.g. images materialized from HTML)."
            },
            "transcript": {
                "type": "string",
                "title": "Transcript",
                "description": "For audio/video: full transcript text concatenated."
            },
            "segments": {
                "type": "array",
                "items": { "$ref": "#/$defs/TimecodedSegment" },
                "title": "Segments",
                "description": "For audio/video: timecoded transcript segments from whisper."
            },
            "scene_captions": {
                "type": "array",
                "items": { "$ref": "#/$defs/TimecodedSegment" },
                "title": "Scene Captions",
                "description": "For video: per-frame vision captions (text only, no speaker)."
            },
            "description": {
                "type": "string",
                "title": "Description",
                "description": "Synthesized description (audio/video) or extracted description (image)."
            },
            "notes": {
                "type": "array",
                "items": { "type": "string" },
                "title": "Notes",
                "description": "Free-form notes (e.g. transcription availability warnings)."
            }
        },
        "$defs": {
            "EmbeddedArtifact": {
                "type": "object",
                "required": ["name", "content_type", "data"],
                "properties": {
                    "name": { "type": "string", "title": "Name" },
                    "content_type": { "type": "string", "title": "Content Type" },
                    "data": {
                        "type": "string",
                        "title": "Data",
                        "description": "Base64-encoded artifact bytes."
                    }
                }
            },
            "TimecodedSegment": {
                "type": "object",
                "required": ["start_ms", "end_ms", "text"],
                "properties": {
                    "start_ms": { "type": "integer", "minimum": 0 },
                    "end_ms": { "type": "integer", "minimum": 0 },
                    "speaker": { "type": ["string", "null"] },
                    "text": { "type": "string" }
                }
            }
        }
    })
}
