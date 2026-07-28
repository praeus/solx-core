//! Tantivy full-text + faceted search over documents.
//!
//! Facets supported: `path_prefix` (matches a directory and everything under
//! it, via indexed ancestor terms), `type_ref` (exact), and `linked_to`
//! (exact — documents whose contents contain a `DocRef` to the given target).
//! Free-text `q` runs over the document name + a flattened bag of its string
//! content, with bare terms auto-converted to prefix queries.
//!
//! When a type schema is available, content walking is schema-aware:
//! `DocRef` fields are extracted into the `doc_ref_names` facet (and folded
//! into free-text too), and rich-text fields (`x-sol-editor: "rich_text"` or
//! `$ref` to `RichTextDoc`) are converted to plain text before indexing.

use std::path::Path;

use solx_surface::error::{Result, SolxError};
use solx_surface::query::{SearchHit, SearchQuery, SearchResults};
use tantivy::collector::{Count, TopDocs};
use tantivy::query::{AllQuery, BooleanQuery, Occur, Query, QueryParser, RegexQuery, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, SchemaBuilder, TantivyDocument, Value, STORED, STRING, TEXT,
};
use tantivy::{DocAddress, Index, IndexReader, IndexWriter, ReloadPolicy, Term};

fn se<E: std::fmt::Display>(e: E) -> SolxError {
    SolxError::Other(format!("search: {e}"))
}

const REQUIRED_FIELDS: &[&str] = &[
    "id",
    "name_raw",
    "path",
    "name",
    "title",
    "summary",
    "type_ref",
    "path_prefixes",
    "doc_ref_names",
    "content_text",
];

#[derive(Clone)]
struct Fields {
    id: Field,
    name_raw: Field,
    path: Field,
    name: Field,
    title: Field,
    summary: Field,
    type_ref: Field,
    path_prefixes: Field,
    doc_ref_names: Field,
    content_text: Field,
}

fn build_schema() -> (Schema, Fields) {
    let mut b = SchemaBuilder::new();
    let id = b.add_text_field("id", STRING | STORED);
    let name_raw = b.add_text_field("name_raw", STRING | STORED);
    let path = b.add_text_field("path", STRING | STORED);
    let name = b.add_text_field("name", TEXT | STORED);
    let title = b.add_text_field("title", STRING | STORED);
    let summary = b.add_text_field("summary", STRING | STORED);
    let type_ref = b.add_text_field("type_ref", STRING | STORED);
    let path_prefixes = b.add_text_field("path_prefixes", STRING);
    let doc_ref_names = b.add_text_field("doc_ref_names", STRING);
    let content_text = b.add_text_field("content_text", TEXT);
    let schema = b.build();
    let fields = Fields {
        id,
        name_raw,
        path,
        name,
        title,
        summary,
        type_ref,
        path_prefixes,
        doc_ref_names,
        content_text,
    };
    (schema, fields)
}

fn fields_from(schema: &Schema) -> Result<Fields> {
    let f = |n: &str| schema.get_field(n).map_err(|_| se(format!("missing field {n}")));
    Ok(Fields {
        id: f("id")?,
        name_raw: f("name_raw")?,
        path: f("path")?,
        name: f("name")?,
        title: f("title")?,
        summary: f("summary")?,
        type_ref: f("type_ref")?,
        path_prefixes: f("path_prefixes")?,
        doc_ref_names: f("doc_ref_names")?,
        content_text: f("content_text")?,
    })
}

/// A document search index backed by Tantivy.
pub struct DocIndex {
    index: Index,
    writer: IndexWriter,
    reader: IndexReader,
    fields: Fields,
}

impl DocIndex {
    /// Open an existing index at `index_dir`, or create a fresh one. Returns
    /// whether the index was just (re)created from scratch — either because
    /// it didn't exist yet, or because an incompatible on-disk schema was
    /// wiped and recreated — so the caller can repopulate it from the
    /// document database (see [`crate::LocalDocManager::reindex_all`]).
    pub fn open(index_dir: &Path) -> Result<(Self, bool)> {
        std::fs::create_dir_all(index_dir)?;
        let (desired, _) = build_schema();
        let (index, created_fresh) = if index_dir.join("meta.json").exists() {
            let opened = Index::open_in_dir(index_dir).map_err(se)?;
            if REQUIRED_FIELDS.iter().all(|n| opened.schema().get_field(n).is_ok()) {
                (opened, false)
            } else {
                drop(opened);
                std::fs::remove_dir_all(index_dir)?;
                std::fs::create_dir_all(index_dir)?;
                (Index::create_in_dir(index_dir, desired.clone()).map_err(se)?, true)
            }
        } else {
            (Index::create_in_dir(index_dir, desired.clone()).map_err(se)?, true)
        };
        let fields = fields_from(&index.schema())?;
        let writer = index.writer(50_000_000).map_err(se)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map_err(se)?;
        Ok((
            DocIndex {
                index,
                writer,
                reader,
                fields,
            },
            created_fresh,
        ))
    }

    /// Index (or re-index) one document.
    #[allow(clippy::too_many_arguments)]
    pub fn index_doc(
        &mut self,
        id: &str,
        path: &str,
        name: &str,
        name_raw: &str,
        title: Option<&str>,
        summary: Option<&str>,
        type_ref: &str,
        doc_ref_names: &[String],
        content_text: &str,
    ) -> Result<()> {
        // delete-then-add by the unique name_raw key
        self.writer
            .delete_term(Term::from_field_text(self.fields.name_raw, name_raw));
        let mut doc = TantivyDocument::default();
        doc.add_text(self.fields.id, id);
        doc.add_text(self.fields.name_raw, name_raw);
        doc.add_text(self.fields.path, path);
        doc.add_text(self.fields.name, name);
        if let Some(t) = title {
            doc.add_text(self.fields.title, t);
        }
        if let Some(s) = summary {
            doc.add_text(self.fields.summary, s);
        }
        doc.add_text(self.fields.type_ref, type_ref);
        for anc in ancestors(path) {
            doc.add_text(self.fields.path_prefixes, &anc);
        }
        for target in doc_ref_names {
            doc.add_text(self.fields.doc_ref_names, target);
        }
        doc.add_text(self.fields.content_text, content_text);
        self.writer.add_document(doc).map_err(se)?;
        self.commit()
    }

    pub fn delete_doc(&mut self, name_raw: &str) -> Result<()> {
        self.writer
            .delete_term(Term::from_field_text(self.fields.name_raw, name_raw));
        self.commit()
    }

    fn commit(&mut self) -> Result<()> {
        self.writer.commit().map_err(se)?;
        self.reader.reload().map_err(se)?;
        Ok(())
    }

    pub fn search(&self, query: &SearchQuery) -> Result<SearchResults> {
        let limit = query.limit.unwrap_or(20).max(1);
        let offset = query.offset.unwrap_or(0);
        let q = self.build_query(query)?;
        let searcher = self.reader.searcher();

        let total: usize = searcher.search(&q, &Count).map_err(se)?;
        let top: Vec<(f32, DocAddress)> = searcher
            .search(&q, &TopDocs::with_limit(limit.saturating_add(offset)))
            .map_err(se)?;

        let mut hits = Vec::new();
        for (score, addr) in top.into_iter().skip(offset).take(limit) {
            let doc: TantivyDocument = searcher.doc(addr).map_err(se)?;
            hits.push(SearchHit {
                id: text(&doc, self.fields.id).unwrap_or_default(),
                path: text(&doc, self.fields.path).unwrap_or_default(),
                name: text(&doc, self.fields.name).unwrap_or_default(),
                title: text(&doc, self.fields.title).filter(|s| !s.is_empty()),
                summary: text(&doc, self.fields.summary).filter(|s| !s.is_empty()),
                type_ref: text(&doc, self.fields.type_ref).unwrap_or_default(),
                score,
            });
        }
        Ok(SearchResults {
            hits,
            total,
            limit,
            offset,
        })
    }

    fn build_query(&self, query: &SearchQuery) -> Result<Box<dyn Query>> {
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

        match query.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(q_str) => {
                let has_ops = q_str.contains(|c: char| matches!(c, '*' | '~' | '"' | ':' | '^'));
                if has_ops {
                    // Advanced query syntax — hand off to the parser as-is.
                    let parser = QueryParser::for_index(
                        &self.index,
                        vec![self.fields.name, self.fields.content_text],
                    );
                    clauses.push((Occur::Must, parser.parse_query(q_str).map_err(se)?));
                } else {
                    // Bare terms: each term is a prefix match over name OR
                    // content_text (AND across terms, OR across fields).
                    for term in q_str.split_whitespace() {
                        let per_field: Vec<(Occur, Box<dyn Query>)> = [self.fields.name, self.fields.content_text]
                            .into_iter()
                            .map(|field| {
                                let pattern = format!("{}.*", regex_escape(&term.to_lowercase()));
                                let rq = RegexQuery::from_pattern(&pattern, field).map_err(se)?;
                                Ok((Occur::Should, Box::new(rq) as Box<dyn Query>))
                            })
                            .collect::<Result<_>>()?;
                        clauses.push((Occur::Must, Box::new(BooleanQuery::new(per_field))));
                    }
                }
            }
            None => clauses.push((Occur::Must, Box::new(AllQuery))),
        }

        if let Some(prefix) = query.path_prefix.as_deref().filter(|s| !s.is_empty()) {
            let normalized = solx_surface::path::normalize_path(prefix)?;
            clauses.push(term_clause(self.fields.path_prefixes, &normalized));
        }
        if let Some(tr) = query.type_ref.as_deref().filter(|s| !s.is_empty()) {
            clauses.push(term_clause(self.fields.type_ref, tr));
        }
        if let Some(target) = query.linked_to.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            clauses.push(term_clause(self.fields.doc_ref_names, target));
        }

        Ok(Box::new(BooleanQuery::new(clauses)))
    }
}

/// Escape regex metacharacters in a search term so it is matched literally
/// (before the trailing `.*` prefix wildcard is appended).
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if !c.is_alphanumeric() {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn term_clause(field: Field, value: &str) -> (Occur, Box<dyn Query>) {
    let term = Term::from_field_text(field, value);
    (
        Occur::Must,
        Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
    )
}

fn text(doc: &TantivyDocument, field: Field) -> Option<String> {
    doc.get_first(field).and_then(|v| v.as_str().map(|s| s.to_string()))
}

/// All ancestor path terms for a normalized path, including the root and the
/// path itself: `/a/b` -> `["/", "/a", "/a/b"]`.
fn ancestors(path: &str) -> Vec<String> {
    let mut out = vec!["/".to_string()];
    if path == "/" {
        return out;
    }
    let mut cur = String::new();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        cur.push('/');
        cur.push_str(seg);
        out.push(cur.clone());
    }
    out
}

// ── Schema-aware content walking ────────────────────────────────────────────

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
    match val {
        serde_json::Value::Object(obj) => {
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
        _ => {}
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
