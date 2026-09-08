//! `solx-types` — the type registry (its own libsql database).
//!
//! Types are JSON schemas organized by `path` + `name` (unique together) and
//! tagged with type groups. Documents and actions reference a type by its full
//! path string; [`LocalTypeManager::resolve`]/[`validate`](LocalTypeManager::validate)
//! are the cross-database entry points they call.

mod db;
mod seed;
mod validation;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use libsql::Connection;
use serde_json::Value;
use solx_surface::entities::{TypeEntity, TypeInput};
use solx_surface::error::{Result, SolxError};
use solx_surface::managers::TypeManager;
use solx_surface::path::{full_ref, normalize_path, split_ref, validate_name};
use solx_surface::query::{ListOptions, ListSchema, Page, PathFacet, SortOrder};
use uuid::Uuid;

use db::{map_db, Db};
pub use validation::{enrich_schema, validate_against_schema, validate_schema_syntax};

const DEFAULT_LIMIT: usize = 100;

/// libsql-backed [`TypeManager`].
pub struct LocalTypeManager {
    db: Db,
}

impl LocalTypeManager {
    /// Open the types database at `path`, applying DDL and seeding built-ins.
    pub async fn open(path: &std::path::Path) -> Result<Self> {
        let db = Db::open(path).await?;
        let conn = db.connect().await?;
        conn.execute_batch(seed::DDL).await.map_err(map_db)?;
        seed_builtins(&conn).await?;
        Ok(LocalTypeManager { db })
    }
}

async fn seed_builtins(conn: &Connection) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    for t in seed::builtin_types() {
        let groups = serde_json::to_string(&t.groups)?;
        conn.execute(
            "INSERT OR IGNORE INTO types (id,path,name,description,schema,groups,created_at,updated_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            libsql::params![
                Uuid::new_v4().to_string(),
                t.path.to_string(),
                t.name.to_string(),
                t.description.to_string(),
                t.schema.to_string(),
                groups,
                now.clone(),
                now.clone(),
            ],
        )
        .await
        .map_err(map_db)?;
    }
    Ok(())
}

fn parse_dt(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.into())
        .map_err(|e| SolxError::Db(e.to_string()))
}

fn row_to_type(row: &libsql::Row) -> Result<TypeEntity> {
    let id = Uuid::parse_str(&row.get::<String>(0).map_err(map_db)?)
        .map_err(|e| SolxError::Db(e.to_string()))?;
    let path: String = row.get(1).map_err(map_db)?;
    let name: String = row.get(2).map_err(map_db)?;
    let description: Option<String> = row.get::<String>(3).ok().filter(|s| !s.is_empty());
    let schema: Value = serde_json::from_str(&row.get::<String>(4).map_err(map_db)?)?;
    let groups: Vec<String> =
        serde_json::from_str(&row.get::<String>(5).map_err(map_db)?).unwrap_or_default();
    let created_at = parse_dt(&row.get::<String>(6).map_err(map_db)?)?;
    let updated_at = parse_dt(&row.get::<String>(7).map_err(map_db)?)?;
    Ok(TypeEntity {
        id,
        path,
        name,
        description,
        schema,
        groups,
        created_at,
        updated_at,
    })
}

const SELECT: &str =
    "SELECT id,path,name,description,schema,groups,created_at,updated_at FROM types";

/// Columns this store exposes to `ListOptions`. `groups` is stored as a JSON
/// array, so filtering it is a substring match over that text — enough to pick
/// out a group tag without a join table.
const LIST_SCHEMA: ListSchema<'static> = ListSchema {
    filterable: &["name", "description", "groups"],
    sortable: &[
        ("name", "path,name"),
        ("path", "path,name"),
        ("created_at", "created_at"),
        ("updated_at", "updated_at"),
    ],
    default_sort: "path,name",
    date_column: Some("created_at"),
};

async fn get_row(conn: &Connection, path: &str, name: &str) -> Result<Option<TypeEntity>> {
    let mut rows = conn
        .query(
            &format!("{SELECT} WHERE path=?1 AND name=?2"),
            libsql::params![path.to_string(), name.to_string()],
        )
        .await
        .map_err(map_db)?;
    match rows.next().await.map_err(map_db)? {
        Some(row) => Ok(Some(row_to_type(&row)?)),
        None => Ok(None),
    }
}

#[async_trait]
impl TypeManager for LocalTypeManager {
    async fn save(&self, path: &str, name: &str, input: TypeInput) -> Result<TypeEntity> {
        let path = normalize_path(path)?;
        validate_name(name)?;
        let name = name.trim().to_string();
        let conn = self.db.connect().await?;

        let existing = get_row(&conn, &path, &name).await?;

        // Merge absent fields with any existing row (create-or-replace).
        let schema = match input
            .schema
            .or_else(|| existing.as_ref().map(|e| e.schema.clone()))
        {
            Some(s) => s,
            None => {
                return Err(SolxError::Invalid(
                    "a schema is required to create a type".into(),
                ))
            }
        };
        validate_schema_syntax(&schema)?;
        let description = input
            .description
            .or_else(|| existing.as_ref().and_then(|e| e.description.clone()))
            .unwrap_or_default();
        let groups = if !input.groups.is_empty() {
            input.groups
        } else {
            existing.as_ref().map(|e| e.groups.clone()).unwrap_or_default()
        };
        let groups_json = serde_json::to_string(&groups)?;
        let now = Utc::now().to_rfc3339();

        if existing.is_some() {
            conn.execute(
                "UPDATE types SET description=?1,schema=?2,groups=?3,updated_at=?4 WHERE path=?5 AND name=?6",
                libsql::params![
                    description,
                    schema.to_string(),
                    groups_json,
                    now,
                    path.clone(),
                    name.clone(),
                ],
            )
            .await
            .map_err(map_db)?;
        } else {
            conn.execute(
                "INSERT INTO types (id,path,name,description,schema,groups,created_at,updated_at) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                libsql::params![
                    Uuid::new_v4().to_string(),
                    path.clone(),
                    name.clone(),
                    description,
                    schema.to_string(),
                    groups_json,
                    now.clone(),
                    now.clone(),
                ],
            )
            .await
            .map_err(map_db)?;
        }

        get_row(&conn, &path, &name)
            .await?
            .ok_or_else(|| SolxError::Other("type vanished after write".into()))
    }

    async fn get(&self, path: &str, name: &str) -> Result<TypeEntity> {
        let path = normalize_path(path)?;
        validate_name(name)?;
        let conn = self.db.connect().await?;
        let fr = full_ref(&path, name)?;
        get_row(&conn, &path, name.trim())
            .await?
            .ok_or_else(|| SolxError::NotFound(format!("type {fr}")))
    }

    async fn delete(&self, path: &str, name: &str) -> Result<()> {
        let path = normalize_path(path)?;
        validate_name(name)?;
        let conn = self.db.connect().await?;
        let fr = full_ref(&path, name)?;
        let affected = conn
            .execute(
                "DELETE FROM types WHERE path=?1 AND name=?2",
                libsql::params![path.clone(), name.trim().to_string()],
            )
            .await
            .map_err(map_db)?;
        if affected == 0 {
            return Err(SolxError::NotFound(format!("type {fr}")));
        }
        Ok(())
    }

    async fn list(&self, opts: ListOptions) -> Result<Page<TypeEntity>> {
        let conn = self.db.connect().await?;
        let limit = opts.limit_or(DEFAULT_LIMIT);
        let offset = opts.offset_or_zero();

        let q = opts.to_sql(LIST_SCHEMA)?;
        let binds: Vec<libsql::Value> =
            q.binds.iter().cloned().map(libsql::Value::from).collect();

        let total = {
            let sql = format!("SELECT COUNT(*) FROM types{}", q.where_clause);
            let mut rows = conn.query(&sql, binds.clone()).await.map_err(map_db)?;
            let row = rows.next().await.map_err(map_db)?;
            row.map(|r| r.get::<i64>(0).unwrap_or(0)).unwrap_or(0) as usize
        };

        let sql = format!(
            "{SELECT}{}{} LIMIT {limit} OFFSET {offset}",
            q.where_clause, q.order_clause
        );
        let mut rows = conn.query(&sql, binds).await.map_err(map_db)?;
        let mut items = Vec::new();
        while let Some(row) = rows.next().await.map_err(map_db)? {
            items.push(row_to_type(&row)?);
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
            let sql = format!("SELECT COUNT(DISTINCT path) FROM types{}", q.where_clause);
            let mut rows = conn.query(&sql, binds.clone()).await.map_err(map_db)?;
            rows.next()
                .await
                .map_err(map_db)?
                .map(|r| r.get::<i64>(0).unwrap_or(0))
                .unwrap_or(0) as usize
        };

        let sql = format!(
            "SELECT path, COUNT(*) FROM types{} GROUP BY path ORDER BY path {dir} LIMIT {limit} OFFSET {offset}",
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

    async fn resolve(&self, type_ref: &str) -> Result<TypeEntity> {
        let (path, name) = split_ref(type_ref)?;
        self.get(&path, &name).await
    }

    async fn validate(&self, value: &Value, type_ref: &str) -> Result<()> {
        let ty = self.resolve(type_ref).await?;
        validate_against_schema(value, &ty.schema)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn mgr() -> (tempfile::TempDir, LocalTypeManager) {
        let dir = tempfile::tempdir().unwrap();
        let m = LocalTypeManager::open(&dir.path().join("types.db"))
            .await
            .unwrap();
        (dir, m)
    }

    #[tokio::test]
    async fn seeds_builtins_including_blog_post() {
        let (_d, m) = mgr().await;
        let t = m.resolve("/types/docs/BlogPostWithComments").await.unwrap();
        assert_eq!(t.name, "BlogPostWithComments");
        let s = m.resolve("/types/core/String").await.unwrap();
        assert_eq!(s.schema["type"], "string");
    }

    #[tokio::test]
    async fn save_get_validate_roundtrip() {
        let (_d, m) = mgr().await;
        let input = TypeInput {
            description: Some("a person".into()),
            schema: Some(serde_json::json!({
                "type": "object",
                "required": ["name"],
                "properties": { "name": { "type": "string" } }
            })),
            groups: vec!["document-type".into()],
        };
        let t = m.save("/types/custom", "Person", input).await.unwrap();
        assert_eq!(t.path, "/types/custom");
        assert_eq!(t.groups, vec!["document-type".to_string()]);

        assert!(m
            .validate(&serde_json::json!({"name": "Ada"}), "/types/custom/Person")
            .await
            .is_ok());
        assert!(m
            .validate(&serde_json::json!({}), "/types/custom/Person")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn list_filters_by_name_substring() {
        let (_d, m) = mgr().await;
        let page = m
            .list(ListOptions {
                filter_field: Some("name".into()),
                filter_value: Some("blogpost".into()), // case-insensitive
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(page.total >= 1);
        assert!(page
            .items
            .iter()
            .all(|t| t.name.to_lowercase().contains("blogpost")));
    }

    #[tokio::test]
    async fn list_ignores_unknown_filter_field() {
        let (_d, m) = mgr().await;
        let all = m.list(ListOptions::default()).await.unwrap().total;
        // An unrecognized column must be dropped, not interpolated.
        let page = m
            .list(ListOptions {
                filter_field: Some("schema) OR 1=1 --".into()),
                filter_value: Some("x".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.total, all);
    }

    #[tokio::test]
    async fn list_sorts_by_name_in_both_directions() {
        let (_d, m) = mgr().await;
        let asc = m
            .list(ListOptions {
                sort_by: Some("name".into()),
                sort_order: solx_surface::query::SortOrder::Asc,
                ..Default::default()
            })
            .await
            .unwrap();
        let desc = m
            .list(ListOptions {
                sort_by: Some("name".into()),
                sort_order: solx_surface::query::SortOrder::Desc,
                ..Default::default()
            })
            .await
            .unwrap();

        let asc_refs: Vec<_> = asc.items.iter().map(|t| (&t.path, &t.name)).collect();
        let desc_refs: Vec<_> = desc.items.iter().map(|t| (&t.path, &t.name)).collect();
        assert!(asc_refs.len() > 1);
        assert_ne!(asc_refs, desc_refs, "sort_order had no effect");

        let mut reversed = desc_refs.clone();
        reversed.reverse();
        assert_eq!(asc_refs, reversed, "desc must be the exact reverse of asc");
    }

    #[tokio::test]
    async fn list_paginates_without_repeating_rows() {
        let (_d, m) = mgr().await;
        let mut seen = std::collections::HashSet::new();
        for page_no in 0..4 {
            let page = m
                .list(ListOptions {
                    limit: Some(5),
                    offset: Some(page_no * 5),
                    sort_by: Some("updated_at".into()),
                    ..Default::default()
                })
                .await
                .unwrap();
            for t in page.items {
                // The (path,name) tiebreak keeps LIMIT/OFFSET stable even
                // though every seeded row shares an updated_at.
                assert!(seen.insert(full_ref(&t.path, &t.name).unwrap()), "row repeated across pages");
            }
        }
        assert_eq!(seen.len(), 20);
    }

    #[tokio::test]
    async fn list_with_path_prefix_and_delete() {
        let (_d, m) = mgr().await;
        let page = m
            .list(ListOptions {
                path_prefix: Some("/types/core".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(page.total >= 7);
        assert!(page.items.iter().all(|t| t.path == "/types/core"));

        m.delete("/types/core", "Null").await.unwrap();
        assert!(m.get("/types/core", "Null").await.is_err());
    }

    #[tokio::test]
    async fn paths_groups_by_path_with_counts() {
        let (_d, m) = mgr().await;
        let input = || TypeInput {
            description: None,
            schema: Some(serde_json::json!({ "type": "string" })),
            groups: vec![],
        };
        m.save("/types/custom", "Alpha", input()).await.unwrap();
        m.save("/types/custom", "Beta", input()).await.unwrap();
        m.save("/types/custom/nested", "Gamma", input()).await.unwrap();

        let page = m
            .paths(ListOptions {
                path_prefix: Some("/types/custom".into()),
                ..Default::default()
            })
            .await
            .unwrap();

        // Two distinct paths under the prefix -- /types/custom (2 rows) and
        // /types/custom/nested (1 row) -- so `total` counts paths, not rows.
        assert_eq!(page.total, 2);
        let facet = page.items.iter().find(|f| f.path == "/types/custom").unwrap();
        assert_eq!(facet.count, 2);
        let nested = page.items.iter().find(|f| f.path == "/types/custom/nested").unwrap();
        assert_eq!(nested.count, 1);
    }
}
