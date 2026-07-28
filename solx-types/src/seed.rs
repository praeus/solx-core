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

    out
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
