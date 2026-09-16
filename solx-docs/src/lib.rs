//! `solx-docs` — the document store (its own libsql database, with SQLite
//! FTS5 for full-text search).
//!
//! Documents are organized by `path` + `name` (unique together). Each document
//! references its type by full path string; on write the contents are validated
//! against that type through an injected [`TypeManager`] (which lives in its own
//! database). Links (doc→doc and doc→URL) and file references are stored as JSON
//! columns — the bytes themselves live in the files store.

mod content;
mod db;

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use libsql::Connection;
use serde_json::{json, Value};
use solx_surface::entities::{DocLink, Document, DocumentInput, FileRef};
use solx_surface::error::{Result, SolxError};
use solx_surface::managers::{DocManager, TypeManager};
use solx_surface::path::{full_ref, normalize_path, validate_name};
use solx_surface::query::{ListOptions, ListSchema, Page, PathFacet, SearchQuery, SortOrder};
use uuid::Uuid;

use content::{flatten_strings, walk_contents};
use db::{map_db, Db};

const DDL: &str = "\
CREATE TABLE IF NOT EXISTS documents (\
    id TEXT PRIMARY KEY,\
    path TEXT NOT NULL,\
    name TEXT NOT NULL,\
    title TEXT NOT NULL DEFAULT '',\
    summary TEXT NOT NULL DEFAULT '',\
    type_ref TEXT NOT NULL,\
    contents TEXT NOT NULL DEFAULT '{}',\
    author TEXT NOT NULL DEFAULT '',\
    pub_date TEXT NOT NULL DEFAULT '',\
    confidence REAL,\
    links TEXT NOT NULL DEFAULT '[]',\
    files TEXT NOT NULL DEFAULT '[]',\
    content_text TEXT NOT NULL DEFAULT '',\
    created_at TEXT NOT NULL,\
    updated_at TEXT NOT NULL,\
    UNIQUE(path,name)\
);\
CREATE TABLE IF NOT EXISTS document_refs (\
    document_id TEXT NOT NULL,\
    target TEXT NOT NULL\
);\
CREATE INDEX IF NOT EXISTS document_refs_target ON document_refs(target);\
CREATE INDEX IF NOT EXISTS document_refs_document_id ON document_refs(document_id);";

/// External-content FTS5 index over `documents.name`/`content_text`, kept in
/// sync purely by trigger — `save`/`delete` never touch this table directly.
/// `id` is `TEXT PRIMARY KEY` (not `INTEGER PRIMARY KEY`), so `documents`
/// keeps SQLite's implicit `rowid`, which is what `content_rowid='rowid'`
/// links against. Mirrors `solx-actions`'s `actions_fts` pattern exactly —
/// see that crate's `FTS_DDL` for the general shape.
///
/// `title`/`summary` aren't indexed separately: `content_text` already folds
/// them in (see [`LocalDocManager::compute_content`]), same as the old
/// Tantivy index only ever searched `name` + `content_text`.
const FTS_DDL: &str = "\
CREATE VIRTUAL TABLE IF NOT EXISTS documents_fts USING fts5(\
    name, content_text, \
    content='documents', content_rowid='rowid', tokenize='porter unicode61'\
);\
CREATE TRIGGER IF NOT EXISTS documents_ai AFTER INSERT ON documents BEGIN \
  INSERT INTO documents_fts(rowid,name,content_text)\
  VALUES (new.rowid,new.name,new.content_text);\
END;\
CREATE TRIGGER IF NOT EXISTS documents_ad AFTER DELETE ON documents BEGIN \
  INSERT INTO documents_fts(documents_fts,rowid,name,content_text)\
  VALUES('delete',old.rowid,old.name,old.content_text);\
  DELETE FROM document_refs WHERE document_id = old.id;\
END;\
CREATE TRIGGER IF NOT EXISTS documents_au AFTER UPDATE ON documents BEGIN \
  INSERT INTO documents_fts(documents_fts,rowid,name,content_text)\
  VALUES('delete',old.rowid,old.name,old.content_text);\
  INSERT INTO documents_fts(rowid,name,content_text)\
  VALUES (new.rowid,new.name,new.content_text);\
END;";

const DEFAULT_LIMIT: usize = 50;

/// libsql + FTS5 backed [`DocManager`].
pub struct LocalDocManager {
    db: Db,
    types: Arc<dyn TypeManager>,
}

impl LocalDocManager {
    /// Open the documents database, validating contents against types
    /// resolved via `types`.
    pub async fn open(db_path: &Path, types: Arc<dyn TypeManager>) -> Result<Self> {
        let db = Db::open(db_path).await?;
        let conn = db.connect().await?;
        conn.execute_batch(DDL).await.map_err(map_db)?;

        // Pre-existing databases predate `content_text`; add it so the FTS5
        // triggers created below have something to index.
        let needs_backfill = !column_exists(&conn, "documents", "content_text").await?;
        if needs_backfill {
            conn.execute(
                "ALTER TABLE documents ADD COLUMN content_text TEXT NOT NULL DEFAULT ''",
                (),
            )
            .await
            .map_err(map_db)?;
        }

        conn.execute_batch(FTS_DDL).await.map_err(map_db)?;
        if needs_backfill {
            // Any row that predates `documents_fts` was never indexed by the
            // `documents_ai` trigger, so the `documents_au` trigger's
            // delete-then-reinsert (which `reindex_all`'s per-row `UPDATE`
            // below will trigger) would try to delete an entry that was
            // never there — silently corrupting the FTS5 shadow tables
            // (surfaces later as "database disk image is malformed"). The
            // documented fix for seeding an external-content FTS5 index from
            // pre-existing rows is this one-time `'rebuild'` command (same
            // as solx-actions's backfill), run *before* any trigger-driven
            // write touches those rows.
            conn.execute("INSERT INTO documents_fts(documents_fts) VALUES('rebuild')", ())
                .await
                .map_err(map_db)?;
        }
        // Done with this connection before handing off to `reindex_all`,
        // which opens its own — an unfinished statement left open here would
        // otherwise hold a read lock that blocks that connection's writes.
        drop(conn);

        let manager = LocalDocManager { db, types };
        if needs_backfill {
            // `'rebuild'` only seeded `documents_fts` with `content_text`'s
            // current (empty, for pre-existing rows) value; this computes
            // and writes the real per-row content.
            manager.reindex_all().await?;
        }
        Ok(manager)
    }

    /// The type manager this store validates against.
    pub fn types(&self) -> Arc<dyn TypeManager> {
        self.types.clone()
    }

    /// Recompute `content_text`/`document_refs` for every document currently
    /// in the database, via a real `UPDATE` per row so the `documents_au`
    /// trigger picks up the new `content_text` into `documents_fts`
    /// automatically. Called once by [`Self::open`] to backfill a database
    /// that predates `content_text`; safe to call at any other time too
    /// (e.g. to recover search results after a type's schema changed which
    /// fields are `DocRef`s). Returns the number of documents re-indexed.
    pub async fn reindex_all(&self) -> Result<usize> {
        let conn = self.db.connect().await?;
        let mut rows = conn.query(SELECT, ()).await.map_err(map_db)?;
        let mut docs = Vec::new();
        while let Some(row) = rows.next().await.map_err(map_db)? {
            docs.push(row_to_doc(&row)?);
        }
        let count = docs.len();
        for doc in docs {
            let (content_text, doc_ref_names) = self.compute_content(&doc).await;
            conn.execute(
                "UPDATE documents SET content_text=?1 WHERE id=?2",
                libsql::params![content_text, doc.id.to_string()],
            )
            .await
            .map_err(map_db)?;
            write_doc_refs(&conn, &doc.id.to_string(), &doc_ref_names).await?;
        }
        Ok(count)
    }

    /// Compute the full-text bag (`name` + `title` + `summary` + walked
    /// `contents`) and the `DocRef` targets `contents` points at, resolving
    /// `doc.type_ref`'s schema for a schema-aware walk when possible.
    async fn compute_content(&self, doc: &Document) -> (String, Vec<String>) {
        let mut content_text = String::new();
        content_text.push_str(&doc.name);
        content_text.push(' ');
        if let Some(t) = &doc.title {
            content_text.push_str(t);
            content_text.push(' ');
        }
        if let Some(s) = &doc.summary {
            content_text.push_str(s);
            content_text.push(' ');
        }

        let type_schema = self.types.resolve(&doc.type_ref).await.ok().map(|t| t.schema);
        let doc_ref_names = match type_schema {
            Some(ref schema) => {
                let extract = walk_contents(&doc.contents, schema);
                for part in &extract.content_text_parts {
                    content_text.push_str(part);
                    content_text.push(' ');
                }
                extract.doc_ref_names
            }
            None => {
                flatten_strings(&doc.contents, &mut content_text);
                Vec::new()
            }
        };

        (content_text, doc_ref_names)
    }
}

async fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    // `table` is always a hardcoded literal from this module, never caller
    // input — PRAGMA statements don't support parameter binding for names.
    let mut rows = conn
        .query(&format!("PRAGMA table_info({table})"), ())
        .await
        .map_err(map_db)?;
    // Drained to `None` rather than returning as soon as a match is found:
    // an abandoned `Rows` cursor holds a read lock that outlives dropping it
    // (this isn't released until the statement is stepped to completion),
    // which would otherwise block a write from another connection later in
    // `open` — see the (much longer-lived) version of this bug that used to
    // corrupt `documents_fts` before `reindex_all` ran.
    let mut found = false;
    while let Some(row) = rows.next().await.map_err(map_db)? {
        let name: String = row.get(1).map_err(map_db)?;
        if name == column {
            found = true;
        }
    }
    Ok(found)
}

async fn write_doc_refs(conn: &Connection, document_id: &str, targets: &[String]) -> Result<()> {
    conn.execute(
        "DELETE FROM document_refs WHERE document_id=?1",
        libsql::params![document_id.to_string()],
    )
    .await
    .map_err(map_db)?;
    for target in targets {
        conn.execute(
            "INSERT INTO document_refs (document_id, target) VALUES (?1, ?2)",
            libsql::params![document_id.to_string(), target.clone()],
        )
        .await
        .map_err(map_db)?;
    }
    Ok(())
}

/// Turn free-text `q` into a safe FTS5 `MATCH` expression: each whitespace-
/// separated term becomes a quoted, prefix-matched phrase (`"term"*`), ANDed
/// together (FTS5's default for space-separated terms). Quoting every term
/// keeps it immune to FTS5 query-syntax errors from special characters in
/// document text (`"`, `-`, `:`, `(`, ...). `solx-actions` mirrors this as
/// its own `fts_match_query` — any free-text search term can hit the same
/// FTS5 syntax errors, not just document content. This mirrors the old
/// Tantivy bare-term path's auto-prefix behavior; Tantivy's advanced
/// operator syntax (`~` fuzzy, `^` boost) has no FTS5 equivalent and isn't
/// exercised by any caller today.
fn fts_match_query(q: &str) -> String {
    q.split_whitespace()
        .map(|term| format!("\"{}\"*", term.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn opt(s: String) -> Option<String> {
    Some(s).filter(|v| !v.is_empty())
}

fn row_to_doc(row: &libsql::Row) -> Result<Document> {
    let id = Uuid::parse_str(&row.get::<String>(0).map_err(map_db)?)
        .map_err(|e| SolxError::Db(e.to_string()))?;
    let path: String = row.get(1).map_err(map_db)?;
    let name: String = row.get(2).map_err(map_db)?;
    let title = opt(row.get::<String>(3).map_err(map_db)?);
    let summary = opt(row.get::<String>(4).map_err(map_db)?);
    let type_ref: String = row.get(5).map_err(map_db)?;
    let contents: Value = serde_json::from_str(&row.get::<String>(6).map_err(map_db)?)?;
    let author = opt(row.get::<String>(7).map_err(map_db)?);
    let pub_date = opt(row.get::<String>(8).map_err(map_db)?);
    let confidence = row.get::<f64>(9).ok();
    let links: Vec<DocLink> =
        serde_json::from_str(&row.get::<String>(10).map_err(map_db)?).unwrap_or_default();
    let files: Vec<FileRef> =
        serde_json::from_str(&row.get::<String>(11).map_err(map_db)?).unwrap_or_default();
    let created_at = parse_dt(&row.get::<String>(12).map_err(map_db)?)?;
    let updated_at = parse_dt(&row.get::<String>(13).map_err(map_db)?)?;
    Ok(Document {
        id,
        path,
        name,
        title,
        summary,
        type_ref,
        contents,
        author,
        pub_date,
        confidence,
        links,
        files,
        created_at,
        updated_at,
    })
}

fn parse_dt(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.into())
        .map_err(|e| SolxError::Db(e.to_string()))
}

async fn get_row(conn: &Connection, path: &str, name: &str) -> Result<Option<Document>> {
    let mut rows = conn
        .query(
            &format!("{SELECT} WHERE path=?1 AND name=?2"),
            libsql::params![path.to_string(), name.to_string()],
        )
        .await
        .map_err(map_db)?;
    match rows.next().await.map_err(map_db)? {
        Some(row) => Ok(Some(row_to_doc(&row)?)),
        None => Ok(None),
    }
}

const SELECT: &str = "SELECT id,path,name,title,summary,type_ref,contents,author,pub_date,confidence,links,files,created_at,updated_at FROM documents";

/// Columns this store exposes to `ListOptions`.
///
/// `contents` is deliberately absent: it holds the whole document body, and a
/// LIKE filter over it would be both slow and a worse answer than the FTS5
/// index that `search` already provides.
const LIST_SCHEMA: ListSchema<'static> = ListSchema {
    filterable: &["name", "title", "summary", "author", "type_ref"],
    sortable: &[
        ("name", "path,name"),
        ("path", "path,name"),
        ("title", "title"),
        ("created_at", "created_at"),
        ("updated_at", "updated_at"),
    ],
    default_sort: "path,name",
    date_column: Some("created_at"),
};

#[async_trait]
impl DocManager for LocalDocManager {
    async fn save(&self, path: &str, name: &str, input: DocumentInput) -> Result<Document> {
        let path = normalize_path(path)?;
        validate_name(name)?;
        let name = name.trim().to_string();
        let conn = self.db.connect().await?;
        let existing = get_row(&conn, &path, &name).await?;

        let type_ref = input
            .type_ref
            .or_else(|| existing.as_ref().map(|d| d.type_ref.clone()))
            .ok_or_else(|| SolxError::Invalid("a type_ref is required to create a document".into()))?;

        // Coerce absent contents: keep existing, else default to {}.
        let contents = if input.contents.is_null() {
            existing
                .as_ref()
                .map(|d| d.contents.clone())
                .unwrap_or_else(|| json!({}))
        } else {
            input.contents
        };
        // Validate against the type's schema (cross-database call).
        self.types.validate(&contents, &type_ref).await?;

        let title = input.title.or_else(|| existing.as_ref().and_then(|d| d.title.clone()));
        let summary = input.summary.or_else(|| existing.as_ref().and_then(|d| d.summary.clone()));
        let author = input.author.or_else(|| existing.as_ref().and_then(|d| d.author.clone()));
        let pub_date = input.pub_date.or_else(|| existing.as_ref().and_then(|d| d.pub_date.clone()));
        let confidence = input.confidence.or_else(|| existing.as_ref().and_then(|d| d.confidence));
        let links = if !input.links.is_empty() {
            input.links
        } else {
            existing.as_ref().map(|d| d.links.clone()).unwrap_or_default()
        };
        let files = if !input.files.is_empty() {
            input.files
        } else {
            existing.as_ref().map(|d| d.files.clone()).unwrap_or_default()
        };

        let now = Utc::now();
        let now_s = now.to_rfc3339();
        let id = existing.as_ref().map(|d| d.id).unwrap_or_else(Uuid::new_v4);
        let created_at = existing.as_ref().map(|d| d.created_at).unwrap_or(now);

        // Computed before the write so the FTS5 triggers see the final
        // `content_text` value in the same statement that lands the row —
        // `walk_contents` needs an async `TypeManager.resolve()` call, which
        // is why this can't just be a pure-SQL trigger the way `content_text`
        // itself is consumed by one.
        let (content_text, doc_ref_names) = self
            .compute_content(&Document {
                id,
                path: path.clone(),
                name: name.clone(),
                title: title.clone(),
                summary: summary.clone(),
                type_ref: type_ref.clone(),
                contents: contents.clone(),
                author: author.clone(),
                pub_date: pub_date.clone(),
                confidence,
                links: links.clone(),
                files: files.clone(),
                created_at,
                updated_at: now,
            })
            .await;

        let params = libsql::params![
            id.to_string(),
            path.clone(),
            name.clone(),
            title.clone().unwrap_or_default(),
            summary.clone().unwrap_or_default(),
            type_ref.clone(),
            contents.to_string(),
            content_text,
            author.clone().unwrap_or_default(),
            pub_date.clone().unwrap_or_default(),
            confidence,
            serde_json::to_string(&links)?,
            serde_json::to_string(&files)?,
            created_at.to_rfc3339(),
            now_s.clone(),
        ];

        if existing.is_some() {
            conn.execute(
                "UPDATE documents SET title=?4,summary=?5,type_ref=?6,contents=?7,content_text=?8,author=?9,pub_date=?10,confidence=?11,links=?12,files=?13,updated_at=?15 WHERE path=?2 AND name=?3",
                params,
            )
            .await
            .map_err(map_db)?;
        } else {
            conn.execute(
                "INSERT INTO documents (id,path,name,title,summary,type_ref,contents,content_text,author,pub_date,confidence,links,files,created_at,updated_at) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
                params,
            )
            .await
            .map_err(map_db)?;
        }
        write_doc_refs(&conn, &id.to_string(), &doc_ref_names).await?;

        get_row(&conn, &path, &name)
            .await?
            .ok_or_else(|| SolxError::Other("document vanished after write".into()))
    }

    async fn get(&self, path: &str, name: &str) -> Result<Document> {
        let path = normalize_path(path)?;
        validate_name(name)?;
        let fr = full_ref(&path, name)?;
        let conn = self.db.connect().await?;
        get_row(&conn, &path, name.trim())
            .await?
            .ok_or_else(|| SolxError::NotFound(format!("document {fr}")))
    }

    async fn delete(&self, path: &str, name: &str) -> Result<()> {
        let path = normalize_path(path)?;
        validate_name(name)?;
        let name = name.trim().to_string();
        let fr = full_ref(&path, &name)?;
        let conn = self.db.connect().await?;
        let affected = conn
            .execute(
                "DELETE FROM documents WHERE path=?1 AND name=?2",
                libsql::params![path.clone(), name.clone()],
            )
            .await
            .map_err(map_db)?;
        if affected == 0 {
            return Err(SolxError::NotFound(format!("document {fr}")));
        }
        // The `documents_ad` trigger removes the row from `documents_fts`
        // and clears its `document_refs` — no separate cleanup needed here.
        Ok(())
    }

    async fn list(&self, opts: ListOptions) -> Result<Page<Document>> {
        let conn = self.db.connect().await?;
        let limit = opts.limit_or(DEFAULT_LIMIT);
        let offset = opts.offset_or_zero();

        let q = opts.to_sql(LIST_SCHEMA)?;
        let values: Vec<libsql::Value> =
            q.binds.iter().cloned().map(libsql::Value::from).collect();

        // Total count.
        let count_sql = format!("SELECT COUNT(*) FROM documents{}", q.where_clause);
        let total = {
            let mut rows = conn
                .query(&count_sql, values.clone())
                .await
                .map_err(map_db)?;
            rows.next()
                .await
                .map_err(map_db)?
                .map(|r| r.get::<i64>(0).unwrap_or(0))
                .unwrap_or(0) as usize
        };

        // Paginated query.
        let sql = format!(
            "{SELECT}{}{} LIMIT {limit} OFFSET {offset}",
            q.where_clause, q.order_clause
        );
        let mut rows = conn.query(&sql, values).await.map_err(map_db)?;
        let mut items = Vec::new();
        while let Some(row) = rows.next().await.map_err(map_db)? {
            items.push(row_to_doc(&row)?);
        }
        Ok(Page::new(items, total, limit, offset))
    }

    async fn paths(&self, opts: ListOptions) -> Result<Page<PathFacet>> {
        let conn = self.db.connect().await?;
        let limit = opts.limit_or(DEFAULT_LIMIT);
        let offset = opts.offset_or_zero();

        let q = opts.to_sql(LIST_SCHEMA)?;
        let binds: Vec<libsql::Value> =
            q.binds.iter().cloned().map(libsql::Value::from).collect();
        let dir = match opts.sort_order {
            SortOrder::Desc => "DESC",
            SortOrder::Asc => "ASC",
        };

        let total = {
            let sql = format!("SELECT COUNT(DISTINCT path) FROM documents{}", q.where_clause);
            let mut rows = conn.query(&sql, binds.clone()).await.map_err(map_db)?;
            rows.next()
                .await
                .map_err(map_db)?
                .map(|r| r.get::<i64>(0).unwrap_or(0))
                .unwrap_or(0) as usize
        };

        let sql = format!(
            "SELECT path, COUNT(*) FROM documents{} GROUP BY path ORDER BY path {dir} LIMIT {limit} OFFSET {offset}",
            q.where_clause
        );
        let mut rows = conn.query(&sql, binds).await.map_err(map_db)?;
        let mut items = Vec::new();
        while let Some(row) = rows.next().await.map_err(map_db)? {
            let path: String = row.get(0).map_err(map_db)?;
            let count: i64 = row.get(1).map_err(map_db)?;
            items.push(PathFacet { path, count: count as usize });
        }
        Ok(Page::new(items, total, limit, offset))
    }

    /// Free-text search over `name`/`content_text` (a schema-aware flattening
    /// of the document's title/summary/contents, see
    /// [`Self::compute_content`]), composed with `path_prefix`/`type_ref`/
    /// `linked_to` facets. Joins to `documents_fts` through a `rowid, rank`
    /// subquery rather than directly, so the FTS table's own `name` column
    /// can't collide with `documents.name` — same reasoning as
    /// `solx-actions`'s `search()`. With `query.q` absent this runs the same
    /// query plan as a plain facet filter — no join, no ranking.
    async fn search(&self, query: SearchQuery) -> Result<Page<Document>> {
        let conn = self.db.connect().await?;
        let limit = query.limit.unwrap_or(20).max(1);
        let offset = query.offset.unwrap_or(0);

        let mut conditions: Vec<String> = Vec::new();
        let mut binds: Vec<libsql::Value> = Vec::new();

        if let Some(prefix) = query.path_prefix.as_deref().filter(|s| !s.is_empty()) {
            let p = normalize_path(prefix)?;
            if p == "/" {
                binds.push(libsql::Value::from("/%".to_string()));
                conditions.push(format!("d.path LIKE ?{}", binds.len()));
            } else {
                binds.push(libsql::Value::from(p.clone()));
                let exact = binds.len();
                binds.push(libsql::Value::from(format!("{p}/%")));
                let under = binds.len();
                conditions.push(format!("(d.path=?{exact} OR d.path LIKE ?{under})"));
            }
        }
        if let Some(tr) = query.type_ref.as_deref().filter(|s| !s.is_empty()) {
            binds.push(libsql::Value::from(tr.to_string()));
            conditions.push(format!("d.type_ref=?{}", binds.len()));
        }
        if let Some(target) = query.linked_to.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            binds.push(libsql::Value::from(target.to_string()));
            conditions.push(format!(
                "EXISTS (SELECT 1 FROM document_refs r WHERE r.document_id = d.id AND r.target = ?{})",
                binds.len()
            ));
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };

        let term = query.q.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let (join, order) = match term {
            Some(t) => {
                binds.push(libsql::Value::from(fts_match_query(t)));
                let n = binds.len();
                (
                    format!(" JOIN (SELECT rowid, rank FROM documents_fts WHERE documents_fts MATCH ?{n}) f ON f.rowid = d.rowid"),
                    " ORDER BY f.rank".to_string(),
                )
            }
            None => (String::new(), " ORDER BY d.path ASC, d.name ASC".to_string()),
        };

        let total: usize = {
            let sql = format!("SELECT COUNT(*) FROM documents d{join}{where_clause}");
            let mut rows = conn.query(&sql, binds.clone()).await.map_err(map_db)?;
            rows.next()
                .await
                .map_err(map_db)?
                .map(|r| r.get::<i64>(0).unwrap_or(0))
                .unwrap_or(0) as usize
        };

        // No `score`/rank column selected: `ORDER BY f.rank` above already
        // puts the best FTS5 match first, and returning full `Document` rows
        // (via the same `row_to_doc` `get`/`list` already use) means a
        // caller no longer needs a second round-trip to read a hit's actual
        // contents — see `solx-actions::search-actions`, which took the same
        // approach first (server-side rank order, no numeric score exposed).
        let sql = format!(
            "SELECT d.id,d.path,d.name,d.title,d.summary,d.type_ref,d.contents,d.author,d.pub_date,d.confidence,d.links,d.files,d.created_at,d.updated_at \
             FROM documents d{join}{where_clause}{order} LIMIT {limit} OFFSET {offset}"
        );
        let mut rows = conn.query(&sql, binds).await.map_err(map_db)?;
        let mut items = Vec::new();
        while let Some(row) = rows.next().await.map_err(map_db)? {
            items.push(row_to_doc(&row)?);
        }
        Ok(Page::new(items, total, limit, offset))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solx_surface::query::SearchQuery;
    use solx_types::LocalTypeManager;

    async fn setup() -> (tempfile::TempDir, LocalDocManager) {
        let dir = tempfile::tempdir().unwrap();
        let types = LocalTypeManager::open(&dir.path().join("types.db")).await.unwrap();
        let m = LocalDocManager::open(&dir.path().join("docs.db"), Arc::new(types))
            .await
            .unwrap();
        (dir, m)
    }

    #[tokio::test]
    async fn save_get_list_search_delete() {
        let (_d, m) = setup().await;
        let input = DocumentInput {
            title: Some("AI note".into()),
            type_ref: Some("/types/docs/Document".into()),
            contents: json!({ "body": "neural networks and transformers" }),
            ..Default::default()
        };
        let doc = m.save("/research/ai", "note", input).await.unwrap();
        assert_eq!(doc.path, "/research/ai");
        assert_eq!(doc.name, "note");

        let got = m.get("/research/ai", "note").await.unwrap();
        assert_eq!(got.title.as_deref(), Some("AI note"));

        let page = m
            .list(ListOptions { path_prefix: Some("/research".into()), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(page.total, 1);

        let res = m
            .search(SearchQuery { q: Some("transform".into()), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(res.total, 1);
        assert_eq!(res.items[0].name, "note");

        // path-facet search
        let res2 = m
            .search(SearchQuery { path_prefix: Some("/research/ai".into()), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(res2.total, 1);
        let res3 = m
            .search(SearchQuery { path_prefix: Some("/other".into()), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(res3.total, 0);

        m.delete("/research/ai", "note").await.unwrap();
        assert!(m.get("/research/ai", "note").await.is_err());
        let res4 = m
            .search(SearchQuery { q: Some("transform".into()), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(res4.total, 0);
    }

    #[tokio::test]
    async fn rejects_invalid_contents() {
        let (_d, m) = setup().await;
        // Person type requires "name".
        let ty = solx_surface::entities::TypeInput {
            schema: Some(json!({"type":"object","required":["name"],"properties":{"name":{"type":"string"}}})),
            ..Default::default()
        };
        m.types().save("/types/custom", "Person", ty).await.unwrap();
        let bad = DocumentInput {
            type_ref: Some("/types/custom/Person".into()),
            contents: json!({ "wrong": 1 }),
            ..Default::default()
        };
        assert!(m.save("/people", "x", bad).await.is_err());
    }

    #[tokio::test]
    async fn search_facets_by_linked_document() {
        let (_d, m) = setup().await;

        // A type with a DocRef-typed field.
        let ty = solx_surface::entities::TypeInput {
            schema: Some(json!({
                "type": "object",
                "properties": { "manager": { "$ref": "#/$defs/DocRef" } }
            })),
            ..Default::default()
        };
        m.types().save("/types/custom", "Employee", ty).await.unwrap();

        m.save(
            "/people",
            "ada",
            DocumentInput {
                type_ref: Some("/types/docs/Document".into()),
                contents: json!({}),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        m.save(
            "/people",
            "charles",
            DocumentInput {
                type_ref: Some("/types/custom/Employee".into()),
                contents: json!({ "manager": { "path": "/people", "name": "ada" } }),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let res = m
            .search(SearchQuery {
                linked_to: Some("/people/ada".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(res.total, 1);
        assert_eq!(res.items[0].name, "charles");

        let res_none = m
            .search(SearchQuery {
                linked_to: Some("/people/nonexistent".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(res_none.total, 0);
    }

    /// A database created before `content_text` existed (no such column, no
    /// `documents_fts` table) must have its rows backfilled and made
    /// searchable on open — mirroring the bug `solx-actions` hit and fixed
    /// (see this crate's `FTS_DDL` doc comment), except here the source
    /// column itself is new rather than just the FTS wiring.
    #[tokio::test]
    async fn a_pre_content_text_database_is_backfilled_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let types = Arc::new(LocalTypeManager::open(&dir.path().join("types.db")).await.unwrap());
        let db_path = dir.path().join("docs.db");

        // Seed a `documents` table shaped like the pre-migration schema (no
        // `content_text` column), with one row already in it, via raw SQL —
        // simulating a database that predates this crate's FTS5 support.
        {
            let db = Db::open(&db_path).await.unwrap();
            let conn = db.connect().await.unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS documents (\
                    id TEXT PRIMARY KEY, path TEXT NOT NULL, name TEXT NOT NULL, \
                    title TEXT NOT NULL DEFAULT '', summary TEXT NOT NULL DEFAULT '', \
                    type_ref TEXT NOT NULL, contents TEXT NOT NULL DEFAULT '{}', \
                    author TEXT NOT NULL DEFAULT '', pub_date TEXT NOT NULL DEFAULT '', \
                    confidence REAL, links TEXT NOT NULL DEFAULT '[]', \
                    files TEXT NOT NULL DEFAULT '[]', created_at TEXT NOT NULL, \
                    updated_at TEXT NOT NULL, UNIQUE(path,name));",
            )
            .await
            .unwrap();
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "INSERT INTO documents (id,path,name,title,summary,type_ref,contents,author,pub_date,confidence,links,files,created_at,updated_at) \
                 VALUES (?1,'/a','one','','', '/types/docs/Document', '{\"body\":\"searchable content\"}', '', '', NULL, '[]', '[]', ?2, ?2)",
                libsql::params![Uuid::new_v4().to_string(), now],
            )
            .await
            .unwrap();
        }

        let m = LocalDocManager::open(&db_path, types).await.unwrap();
        let res = m
            .search(SearchQuery {
                q: Some("searchable".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(res.total, 1);
        assert_eq!(res.items[0].name, "one");
    }

    async fn save_doc(m: &LocalDocManager, path: &str, name: &str, body: &str) {
        m.save(
            path,
            name,
            DocumentInput {
                type_ref: Some("/types/docs/Document".into()),
                contents: json!({ "body": body }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }

    /// Writes go through a single synchronous SQL statement (row write plus
    /// its FTS5 trigger), so a `save` that has returned must be immediately
    /// findable — no separate commit-and-await step is needed the way the
    /// old Tantivy writer thread required.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_returned_save_is_immediately_searchable() {
        let (_d, m) = setup().await;
        for i in 0..5 {
            save_doc(&m, "/rw", &format!("doc{i}"), "quixotic").await;
            let res = m
                .search(SearchQuery { q: Some("quixotic".into()), ..Default::default() })
                .await
                .unwrap();
            assert_eq!(
                res.total,
                i + 1,
                "write {i} was not visible to the search that followed it"
            );
        }
    }

    /// A delete must be visible straight away too.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_returned_delete_is_immediately_reflected() {
        let (_d, m) = setup().await;
        save_doc(&m, "/rw", "gone", "ephemeral").await;
        let before = m
            .search(SearchQuery { q: Some("ephemeral".into()), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(before.total, 1);

        m.delete("/rw", "gone").await.unwrap();
        let after = m
            .search(SearchQuery { q: Some("ephemeral".into()), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(after.total, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_writes_and_searches_all_land() {
        let (_d, m) = setup().await;
        let m = Arc::new(m);

        let mut tasks = Vec::new();
        for i in 0..16 {
            let m = m.clone();
            tasks.push(tokio::spawn(async move {
                save_doc(&m, "/bulk", &format!("d{i}"), "concurrent").await;
            }));
        }
        for i in 0..8 {
            let m = m.clone();
            tasks.push(tokio::spawn(async move {
                let _ = m
                    .search(SearchQuery {
                        q: Some(format!("concurrent{i}")),
                        ..Default::default()
                    })
                    .await
                    .unwrap();
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }

        let res = m
            .search(SearchQuery {
                q: Some("concurrent".into()),
                limit: Some(50),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(res.total, 16, "every concurrent write should be indexed");
    }

    #[tokio::test]
    async fn paths_groups_by_path_with_counts() {
        let (_d, m) = setup().await;
        let input = || DocumentInput {
            type_ref: Some("/types/docs/Document".into()),
            ..Default::default()
        };
        m.save("/blog", "one", input()).await.unwrap();
        m.save("/blog", "two", input()).await.unwrap();
        m.save("/blog/drafts", "three", input()).await.unwrap();

        let page = m
            .paths(ListOptions { path_prefix: Some("/blog".into()), ..Default::default() })
            .await
            .unwrap();

        // Two distinct paths -- /blog (2 rows) and /blog/drafts (1 row) --
        // so `total` counts paths, not rows.
        assert_eq!(page.total, 2);
        let blog = page.items.iter().find(|f| f.path == "/blog").unwrap();
        assert_eq!(blog.count, 2);
        let drafts = page.items.iter().find(|f| f.path == "/blog/drafts").unwrap();
        assert_eq!(drafts.count, 1);
    }
}
