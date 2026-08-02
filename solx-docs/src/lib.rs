//! `solx-docs` — the document store (its own libsql database + Tantivy index).
//!
//! Documents are organized by `path` + `name` (unique together). Each document
//! references its type by full path string; on write the contents are validated
//! against that type through an injected [`TypeManager`] (which lives in its own
//! database). Links (doc→doc and doc→URL) and file references are stored as JSON
//! columns — the bytes themselves live in the files store.

mod db;
mod search;

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
use solx_surface::query::{ListOptions, Page, SearchQuery, SearchResults};
use uuid::Uuid;

use db::{map_db, Db};
use search::{flatten_strings, walk_contents, DocIndexWriter, DocSearcher};
use tokio::sync::{mpsc, oneshot};

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
    created_at TEXT NOT NULL,\
    updated_at TEXT NOT NULL,\
    UNIQUE(path,name)\
);";

const DEFAULT_LIMIT: usize = 50;

/// One staged index mutation plus a channel to report the commit back on.
struct IndexRequest {
    op: IndexOp,
    /// Resolved once the batch containing this op has been committed, so a
    /// `post` that has returned is always visible to a subsequent `search`.
    done: oneshot::Sender<std::result::Result<(), String>>,
}

enum IndexOp {
    /// Boxed: the add variant is much larger than the delete one, and this
    /// keeps every queued request small.
    Add(Box<IndexDoc>),
    Delete(String),
}

struct IndexDoc {
    id: String,
    path: String,
    name: String,
    name_raw: String,
    title: Option<String>,
    summary: Option<String>,
    type_ref: String,
    doc_ref_names: Vec<String>,
    content_text: String,
}

/// libsql + Tantivy backed [`DocManager`].
///
/// Tantivy is entirely synchronous, so neither half of it may run inline in
/// these async methods. Writes are queued to a dedicated thread that batches
/// them behind a single commit; reads clone the (cheap, Arc-backed) searcher
/// and run on the blocking pool.
///
/// This replaced a single `std::sync::Mutex<DocIndex>` that committed —
/// segment serialize, `fsync`, reader reload — on **every individual
/// document write**, on an async worker, with all searches serialized behind
/// the same lock.
pub struct LocalDocManager {
    db: Db,
    searcher: DocSearcher,
    index_tx: mpsc::UnboundedSender<IndexRequest>,
    types: Arc<dyn TypeManager>,
}

/// Own the writer on a dedicated OS thread and drain the queue in batches.
///
/// A plain thread rather than a tokio task: every operation here is blocking
/// tantivy work, so a task would just park a worker for the whole commit.
fn spawn_index_writer(mut writer: DocIndexWriter, mut rx: mpsc::UnboundedReceiver<IndexRequest>) {
    std::thread::Builder::new()
        .name("solx-docs-index".into())
        .spawn(move || {
            while let Some(first) = rx.blocking_recv() {
                // Take everything already queued so a burst of writes costs
                // one commit rather than N.
                let mut batch = vec![first];
                while let Ok(next) = rx.try_recv() {
                    batch.push(next);
                }

                let mut staged = Ok(());
                for req in &batch {
                    let r = match &req.op {
                        IndexOp::Add(d) => writer.stage_doc(
                            &d.id,
                            &d.path,
                            &d.name,
                            &d.name_raw,
                            d.title.as_deref(),
                            d.summary.as_deref(),
                            &d.type_ref,
                            &d.doc_ref_names,
                            &d.content_text,
                        ),
                        IndexOp::Delete(name_raw) => {
                            writer.stage_delete(name_raw.as_str());
                            Ok(())
                        }
                    };
                    if let Err(e) = r {
                        staged = Err(e.to_string());
                        break;
                    }
                }

                let outcome = match staged {
                    Ok(()) => writer.commit().map_err(|e| e.to_string()),
                    Err(e) => Err(e),
                };
                // A failure is reported to everyone in the batch: they share
                // a commit, so they share its fate.
                for req in batch {
                    let _ = req.done.send(outcome.clone());
                }
            }
        })
        .expect("failed to spawn document index writer thread");
}

impl LocalDocManager {
    /// Open the documents database and search index, validating contents
    /// against types resolved via `types`.
    pub async fn open(
        db_path: &Path,
        index_dir: &Path,
        types: Arc<dyn TypeManager>,
    ) -> Result<Self> {
        let db = Db::open(db_path).await?;
        let conn = db.connect().await?;
        conn.execute_batch(DDL).await.map_err(map_db)?;
        let (writer, searcher, created_fresh) = DocIndexWriter::open(index_dir)?;
        let (index_tx, index_rx) = mpsc::unbounded_channel();
        spawn_index_writer(writer, index_rx);
        let manager = LocalDocManager {
            db,
            searcher,
            index_tx,
            types,
        };
        // A freshly-created index (first run, or an incompatible on-disk
        // schema that was just wiped and recreated) has no documents in it —
        // repopulate from the database so search results survive an index
        // schema upgrade.
        if created_fresh {
            manager.reindex_all().await?;
        }
        Ok(manager)
    }

    /// The type manager this store validates against.
    pub fn types(&self) -> Arc<dyn TypeManager> {
        self.types.clone()
    }

    /// Re-index every document currently in the database. Called
    /// automatically by [`Self::open`] after a fresh/recreated search index;
    /// safe to call at any time (e.g. to recover from an out-of-band index
    /// corruption). Returns the number of documents re-indexed.
    pub async fn reindex_all(&self) -> Result<usize> {
        let conn = self.db.connect().await?;
        let mut rows = conn.query(SELECT, ()).await.map_err(map_db)?;
        let mut count = 0usize;
        while let Some(row) = rows.next().await.map_err(map_db)? {
            let doc = row_to_doc(&row)?;
            self.reindex(&doc).await?;
            count += 1;
        }
        Ok(count)
    }

    async fn reindex(&self, doc: &Document) -> Result<()> {
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

        // Schema-aware content walking: resolve the type to get its schema,
        // then walk contents to extract DocRef targets and rich-text plain
        // text. Fall back to naive flatten_strings if the type can't be
        // resolved.
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

        let name_raw = full_ref(&doc.path, &doc.name)?;
        self.submit(IndexOp::Add(Box::new(IndexDoc {
            id: doc.id.to_string(),
            path: doc.path.clone(),
            name: doc.name.clone(),
            name_raw,
            title: doc.title.clone(),
            summary: doc.summary.clone(),
            type_ref: doc.type_ref.clone(),
            doc_ref_names,
            content_text,
        })))
        .await
    }

    /// Queue an index mutation and wait for the batch containing it to
    /// commit. Awaiting the commit is what preserves read-your-writes;
    /// batching only ever merges genuinely concurrent writers.
    async fn submit(&self, op: IndexOp) -> Result<()> {
        let (done, wait) = oneshot::channel();
        self.index_tx
            .send(IndexRequest { op, done })
            .map_err(|_| SolxError::Other("document index writer thread has stopped".into()))?;
        wait.await
            .map_err(|_| SolxError::Other("document index writer dropped the request".into()))?
            .map_err(SolxError::Other)
    }
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

const SELECT: &str = "SELECT id,path,name,title,summary,type_ref,contents,author,pub_date,confidence,links,files,created_at,updated_at FROM documents";

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

#[async_trait]
impl DocManager for LocalDocManager {
    async fn post(&self, path: &str, name: &str, input: DocumentInput) -> Result<Document> {
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

        let params = libsql::params![
            id.to_string(),
            path.clone(),
            name.clone(),
            title.clone().unwrap_or_default(),
            summary.clone().unwrap_or_default(),
            type_ref.clone(),
            contents.to_string(),
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
                "UPDATE documents SET title=?4,summary=?5,type_ref=?6,contents=?7,author=?8,pub_date=?9,confidence=?10,links=?11,files=?12,updated_at=?14 WHERE path=?2 AND name=?3",
                params,
            )
            .await
            .map_err(map_db)?;
        } else {
            conn.execute(
                "INSERT INTO documents (id,path,name,title,summary,type_ref,contents,author,pub_date,confidence,links,files,created_at,updated_at) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                params,
            )
            .await
            .map_err(map_db)?;
        }

        let doc = get_row(&conn, &path, &name)
            .await?
            .ok_or_else(|| SolxError::Other("document vanished after write".into()))?;
        self.reindex(&doc).await?;
        Ok(doc)
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
        self.submit(IndexOp::Delete(fr)).await?;
        Ok(())
    }

    async fn list(&self, opts: ListOptions) -> Result<Page<Document>> {
        let conn = self.db.connect().await?;
        let limit = opts.limit_or(DEFAULT_LIMIT);
        let offset = opts.offset_or_zero();

        // Build WHERE clauses dynamically.
        let mut conditions: Vec<String> = Vec::new();
        let mut values: Vec<libsql::Value> = Vec::new();

        // Path prefix filter.
        if let Some(p) = &opts.path_prefix {
            let p = normalize_path(p)?;
            if p == "/" {
                conditions.push(format!("path LIKE ?{}", values.len() + 1));
                values.push(libsql::Value::from("/%".to_string()));
            } else {
                let i1 = values.len() + 1;
                let i2 = values.len() + 2;
                conditions.push(format!("(path=?{i1} OR path LIKE ?{i2})"));
                values.push(libsql::Value::from(p.clone()));
                values.push(libsql::Value::from(format!("{p}/%")));
            }
        }

        // Date range filter.
        if let Some(after) = &opts.date_after {
            let idx = values.len() + 1;
            conditions.push(format!("created_at >= ?{idx}"));
            values.push(libsql::Value::from(after.clone()));
        }
        if let Some(before) = &opts.date_before {
            let idx = values.len() + 1;
            conditions.push(format!("created_at <= ?{idx}"));
            values.push(libsql::Value::from(before.clone()));
        }

        // LIKE filter on a column.
        if let (Some(field), Some(value)) = (&opts.filter_field, &opts.filter_value) {
            if !value.is_empty() {
                let col = match field.as_str() {
                    "type_ref" => "type_ref",
                    "author" => "author",
                    "title" => "title",
                    "summary" => "summary",
                    "name" => "name",
                    _ => "",
                };
                if !col.is_empty() {
                    let idx = values.len() + 1;
                    conditions.push(format!(
                        "lower(COALESCE({col},'')) LIKE lower(?{idx})"
                    ));
                    values.push(libsql::Value::from(format!("%{value}%")));
                }
            }
        }

        let where_sql = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };

        // Sort.
        let sort_col = match opts.sort_by.as_deref() {
            Some("name") | Some("path") => "path,name",
            Some("created_at") => "created_at",
            Some("updated_at") => "updated_at",
            Some("title") => "title",
            _ => "path,name",
        };
        let sort_dir = match opts.sort_order {
            solx_surface::query::SortOrder::Desc => "DESC",
            solx_surface::query::SortOrder::Asc => "ASC",
        };

        // Total count.
        let count_sql = format!("SELECT COUNT(*) FROM documents{where_sql}");
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
            "{SELECT}{where_sql} ORDER BY {sort_col} {sort_dir} LIMIT {limit} OFFSET {offset}"
        );
        let mut rows = conn.query(&sql, values).await.map_err(map_db)?;
        let mut items = Vec::new();
        while let Some(row) = rows.next().await.map_err(map_db)? {
            items.push(row_to_doc(&row)?);
        }
        Ok(Page::new(items, total, limit, offset))
    }

    async fn search(&self, query: SearchQuery) -> Result<SearchResults> {
        // Tantivy search is synchronous CPU work plus mmap page faults that
        // can reach real disk, so it goes to the blocking pool. The searcher
        // is a cheap Arc-backed clone, so no lock is held and concurrent
        // searches no longer serialize behind each other or behind a commit.
        let searcher = self.searcher.clone();
        tokio::task::spawn_blocking(move || searcher.search(&query))
            .await
            .map_err(|e| SolxError::Other(format!("search task panicked: {e}")))?
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
        let m = LocalDocManager::open(
            &dir.path().join("docs.db"),
            &dir.path().join("idx"),
            Arc::new(types),
        )
        .await
        .unwrap();
        (dir, m)
    }

    #[tokio::test]
    async fn post_get_list_search_delete() {
        let (_d, m) = setup().await;
        let input = DocumentInput {
            title: Some("AI note".into()),
            type_ref: Some("/types/docs/Document".into()),
            contents: json!({ "body": "neural networks and transformers" }),
            ..Default::default()
        };
        let doc = m.post("/research/ai", "note", input).await.unwrap();
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
        assert_eq!(res.hits[0].name, "note");

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
        m.types().post("/types/custom", "Person", ty).await.unwrap();
        let bad = DocumentInput {
            type_ref: Some("/types/custom/Person".into()),
            contents: json!({ "wrong": 1 }),
            ..Default::default()
        };
        assert!(m.post("/people", "x", bad).await.is_err());
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
        m.types().post("/types/custom", "Employee", ty).await.unwrap();

        m.post(
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

        m.post(
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
        assert_eq!(res.hits[0].name, "charles");

        let res_none = m
            .search(SearchQuery {
                linked_to: Some("/people/nonexistent".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(res_none.total, 0);
    }

    #[tokio::test]
    async fn reindex_all_repopulates_after_incompatible_index() {
        let dir = tempfile::tempdir().unwrap();
        let types = Arc::new(LocalTypeManager::open(&dir.path().join("types.db")).await.unwrap());
        let db_path = dir.path().join("docs.db");
        let idx_path = dir.path().join("idx");

        {
            let m = LocalDocManager::open(&db_path, &idx_path, types.clone())
                .await
                .unwrap();
            m.post(
                "/a",
                "one",
                DocumentInput {
                    type_ref: Some("/types/docs/Document".into()),
                    contents: json!({ "body": "searchable content" }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }

        // Simulate a stale on-disk index missing a required field (e.g. from
        // before `doc_ref_names` was added) by replacing it with a minimal
        // incompatible schema.
        {
            use tantivy::schema::{SchemaBuilder, STORED, STRING};
            let mut b = SchemaBuilder::new();
            b.add_text_field("id", STRING | STORED);
            let schema = b.build();
            std::fs::remove_dir_all(&idx_path).unwrap();
            std::fs::create_dir_all(&idx_path).unwrap();
            tantivy::Index::create_in_dir(&idx_path, schema).unwrap();
        }

        // Reopening must detect the incompatibility, recreate the index, and
        // automatically repopulate it from the documents database.
        let m2 = LocalDocManager::open(&db_path, &idx_path, types.clone())
            .await
            .unwrap();
        let res = m2
            .search(SearchQuery {
                q: Some("searchable".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(res.total, 1);
        assert_eq!(res.hits[0].name, "one");
    }

    // ── Index writer thread ──────────────────────────────────────────────

    async fn post_doc(m: &LocalDocManager, path: &str, name: &str, body: &str) {
        m.post(
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

    /// Writes now go through a channel to a dedicated thread that batches
    /// them behind one commit — but a `post` that has returned must still be
    /// immediately findable. `submit` awaits its batch's commit for exactly
    /// this reason.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_returned_post_is_immediately_searchable() {
        let (_d, m) = setup().await;
        for i in 0..5 {
            post_doc(&m, "/rw", &format!("doc{i}"), "quixotic").await;
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
        post_doc(&m, "/rw", "gone", "ephemeral").await;
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

    /// Concurrent writers and searchers used to serialize behind one
    /// `std::sync::Mutex`, with a full commit inside it per document.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_writes_and_searches_all_land() {
        let (_d, m) = setup().await;
        let m = Arc::new(m);

        let mut tasks = Vec::new();
        for i in 0..16 {
            let m = m.clone();
            tasks.push(tokio::spawn(async move {
                post_doc(&m, "/bulk", &format!("d{i}"), "concurrent").await;
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
}
