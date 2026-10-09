//! [`ActionManager`] CRUD, listing and full-text search over the actions
//! database. Everything returned from here has `action_config` redacted —
//! see [`crate::mask`].

use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;
use solx_surface::entities::{Action, ActionExecResult, ActionInput};
use solx_surface::error::{Result, SolxError};
use solx_surface::managers::ActionManager;
use solx_surface::path::{full_ref, normalize_path, validate_name};
use solx_surface::query::{ActionSearchQuery, ListOptions, ListSchema, Page, PathFacet, SortOrder};
use uuid::Uuid;

use crate::db::map_db;
use crate::row::{action_type_to_str, get_row, row_to_action, SELECT};
use crate::{mask, LocalActionManager};

const DEFAULT_LIMIT: usize = 50;

/// How much to over-fetch when `exclude_hidden` forces post-query
/// filtering, so a page that loses a few rows is usually still full.
const HIDDEN_OVERFETCH: usize = 4;

/// Ceiling on that inflated fetch, so a large `limit` cannot turn into an
/// unbounded scan.
const HIDDEN_FETCH_CAP: usize = 1000;

/// Columns this store exposes to `ListOptions`. `capabilities` and `phrases`
/// are JSON arrays, so filtering them is a substring match over that text —
/// enough to find "every action tagged mcp" without a join table.
///
/// `action_config` is deliberately absent: it can hold secrets (see
/// [`crate::mask`]), and a LIKE filter over it would leak their contents by
/// letting a caller probe for substrings.
const LIST_SCHEMA: ListSchema<'static> = ListSchema {
    filterable: &[
        "name",
        "caption",
        "description",
        "category",
        "capabilities",
        "phrases",
        "action_type",
        "param_type_ref",
        "result_type_ref",
        "fn_name",
        "bin_name",
    ],
    sortable: &[
        ("name", "path,name"),
        ("path", "path,name"),
        ("category", "category"),
        ("action_type", "action_type"),
        ("created_at", "created_at"),
        ("updated_at", "updated_at"),
    ],
    default_sort: "path,name",
    date_column: Some("created_at"),
};

/// Turn free-text `q` into a safe FTS5 `MATCH` expression: each whitespace-
/// separated term becomes a quoted, prefix-matched phrase (`"term"*`), ANDed
/// together (FTS5's default for space-separated terms). Quoting every term
/// keeps it immune to FTS5 query-syntax errors from special characters
/// (`"`, `-`, `:`, `(`, ...) — a bare `foo: bar` would otherwise be parsed as
/// a `foo` column filter and fail with "no such column: foo" on any column
/// name that isn't one of `actions_fts`'s. Mirrors `solx-docs`'s
/// `fts_match_query` (that crate's doc comment used to argue action search
/// terms were unlikely to need this — a normal query like `browser: firefox`
/// proved otherwise).
fn fts_match_query(q: &str) -> String {
    q.split_whitespace()
        .map(|term| format!("\"{}\"*", term.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

impl LocalActionManager {
    /// Read a row **without** redacting `action_config`.
    ///
    /// Execution needs the real thing — `run_command` reads `cwd`,
    /// `run_webhook` reads `auth`/`headers`, and `crate::auth` resolves
    /// credentials out of it. The [`ActionManager::get`] trait method wraps
    /// this and masks; nothing that leaves the process should use this one.
    pub(crate) async fn get_unmasked(&self, path: &str, name: &str) -> Result<Action> {
        let path = normalize_path(path)?;
        validate_name(name)?;
        let fr = full_ref(&path, name)?;
        let conn = self.db.connect().await?;
        get_row(&conn, &path, name.trim())
            .await?
            .ok_or_else(|| SolxError::NotFound(format!("action {fr}")))
    }
}

#[async_trait]
impl ActionManager for LocalActionManager {
    async fn save(&self, path: &str, name: &str, input: ActionInput) -> Result<Action> {
        let path = normalize_path(path)?;
        validate_name(name)?;
        // Checked here rather than at each surface, for the same reason
        // `guard_executable_action` is shared: the CLI, MCP, HTTP, scripts,
        // and wasm guests all arrive through this one method.
        solx_config::validate_capabilities(&input.capabilities).map_err(SolxError::Invalid)?;
        let name = name.trim().to_string();
        let conn = self.db.connect().await?;
        let existing = get_row(&conn, &path, &name).await?;

        macro_rules! merge_opt {
            ($field:ident) => {
                input
                    .$field
                    .or_else(|| existing.as_ref().and_then(|a| a.$field.clone()))
            };
        }
        macro_rules! merge_vec {
            ($field:ident) => {
                if !input.$field.is_empty() {
                    input.$field.clone()
                } else {
                    existing.as_ref().map(|a| a.$field.clone()).unwrap_or_default()
                }
            };
        }

        let caption = merge_opt!(caption);
        let description = merge_opt!(description);
        let category = merge_opt!(category);
        let param_type_ref = merge_opt!(param_type_ref);
        let result_type_ref = merge_opt!(result_type_ref);
        let fn_name = merge_opt!(fn_name);
        let bin_name = merge_opt!(bin_name);
        let action_type = input
            .action_type
            .or_else(|| existing.as_ref().and_then(|a| a.action_type));
        // `get`/`list` redact secret material, so an incoming config may be
        // one the caller was never actually shown. Restore anything echoed
        // back as the mask sentinel from the stored row rather than writing
        // "***" over a real key — see `crate::mask`.
        let action_config = match input.action_config {
            Some(incoming) => Some(
                mask::unmask_merge(incoming, existing.as_ref().and_then(|a| a.action_config.as_ref()))
                    .map_err(SolxError::Invalid)?,
            ),
            None => existing.as_ref().and_then(|a| a.action_config.clone()),
        };
        let capabilities = merge_vec!(capabilities);
        let phrases = merge_vec!(phrases);
        let files = merge_vec!(files);
        let trusted = input
            .trusted
            .unwrap_or_else(|| existing.as_ref().map(|a| a.trusted).unwrap_or(false));

        let now = Utc::now();
        let now_s = now.to_rfc3339();
        let id = existing.as_ref().map(|a| a.id).unwrap_or_else(Uuid::new_v4);
        let created_at = existing.as_ref().map(|a| a.created_at).unwrap_or(now);

        let params = libsql::params![
            id.to_string(),
            path.clone(),
            name.clone(),
            caption.clone().unwrap_or_default(),
            description.clone().unwrap_or_default(),
            serde_json::to_string(&capabilities)?,
            serde_json::to_string(&phrases)?,
            category.clone().unwrap_or_default(),
            param_type_ref.clone().unwrap_or_default(),
            result_type_ref.clone().unwrap_or_default(),
            action_type_to_str(action_type),
            fn_name.clone().unwrap_or_default(),
            bin_name.clone().unwrap_or_default(),
            serde_json::to_string(&action_config)?,
            serde_json::to_string(&files)?,
            trusted,
            created_at.to_rfc3339(),
            now_s.clone(),
        ];

        if existing.is_some() {
            conn.execute(
                "UPDATE actions SET caption=?4,description=?5,capabilities=?6,phrases=?7,category=?8,param_type_ref=?9,result_type_ref=?10,action_type=?11,fn_name=?12,bin_name=?13,action_config=?14,files=?15,trusted=?16,updated_at=?18 WHERE path=?2 AND name=?3",
                params,
            )
            .await
            .map_err(map_db)?;
        } else {
            conn.execute(
                "INSERT INTO actions (id,path,name,caption,description,capabilities,phrases,category,param_type_ref,result_type_ref,action_type,fn_name,bin_name,action_config,files,trusted,created_at,updated_at) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
                params,
            )
            .await
            .map_err(map_db)?;
        }

        let mut saved = get_row(&conn, &path, &name)
            .await?
            .ok_or_else(|| SolxError::Other("action vanished after write".into()))?;
        // `get`/`list`/`search` all redact action_config.secrets/auth before
        // returning (see `crate::mask`) -- `save`'s own response must too, or
        // an upsert (which necessarily carries the real key/credential in
        // its *request*, same as any of those reads' prior write did) echoes
        // that same material straight back out in the response instead of
        // just confirming the write succeeded.
        mask::mask_action_config_opt(&mut saved.action_config);
        Ok(saved)
    }

    async fn get(&self, path: &str, name: &str) -> Result<Action> {
        let mut action = self.get_unmasked(path, name).await?;
        mask::mask_action_config_opt(&mut action.action_config);
        Ok(action)
    }

    async fn delete(&self, path: &str, name: &str) -> Result<()> {
        let path = normalize_path(path)?;
        validate_name(name)?;
        let fr = full_ref(&path, name)?;
        let conn = self.db.connect().await?;
        let affected = conn
            .execute(
                "DELETE FROM actions WHERE path=?1 AND name=?2",
                libsql::params![path.clone(), name.trim().to_string()],
            )
            .await
            .map_err(map_db)?;
        if affected == 0 {
            return Err(SolxError::NotFound(format!("action {fr}")));
        }
        Ok(())
    }

    async fn list(&self, opts: ListOptions) -> Result<Page<Action>> {
        let conn = self.db.connect().await?;
        let limit = opts.limit_or(DEFAULT_LIMIT);
        let offset = opts.offset_or_zero();

        let q = opts.to_sql(LIST_SCHEMA)?;
        let binds: Vec<libsql::Value> =
            q.binds.iter().cloned().map(libsql::Value::from).collect();

        let total = {
            let sql = format!("SELECT COUNT(*) FROM actions{}", q.where_clause);
            let mut rows = conn.query(&sql, binds.clone()).await.map_err(map_db)?;
            rows.next()
                .await
                .map_err(map_db)?
                .map(|r| r.get::<i64>(0).unwrap_or(0))
                .unwrap_or(0) as usize
        };

        let sql = format!(
            "{SELECT}{}{} LIMIT {limit} OFFSET {offset}",
            q.where_clause, q.order_clause
        );
        let mut rows = conn.query(&sql, binds).await.map_err(map_db)?;
        let mut items = Vec::new();
        while let Some(row) = rows.next().await.map_err(map_db)? {
            let mut action = row_to_action(&row)?;
            mask::mask_action_config_opt(&mut action.action_config);
            items.push(action);
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
            let sql = format!("SELECT COUNT(DISTINCT path) FROM actions{}", q.where_clause);
            let mut rows = conn.query(&sql, binds.clone()).await.map_err(map_db)?;
            rows.next()
                .await
                .map_err(map_db)?
                .map(|r| r.get::<i64>(0).unwrap_or(0))
                .unwrap_or(0) as usize
        };

        let sql = format!(
            "SELECT path, COUNT(*) FROM actions{} GROUP BY path ORDER BY path {dir} LIMIT {limit} OFFSET {offset}",
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

    /// Free-text search over `path`/`name`/`caption`/`description`/
    /// `category`/`phrases`, composed with the same `path_prefix`/
    /// `filter_field`/date filters as [`Self::list`] (rendered once via
    /// `ListOptions::to_sql`, then the `MATCH` bind is appended last so none
    /// of its `?N` placeholders need renumbering). With `query.q` absent
    /// this runs the exact same query plan as `list` — no join, no ranking.
    async fn search(&self, query: ActionSearchQuery) -> Result<Page<Action>> {
        let conn = self.db.connect().await?;
        let want = query.list.limit_or(DEFAULT_LIMIT);
        // Hidden-ness is resolved from config rules *and* the row's own
        // capabilities, so it cannot be pushed into the SQL: a LIKE over the
        // capabilities JSON would match `non-destructive` against
        // `solx:destructive`. Filtering therefore happens after the query,
        // and the fetch is inflated so a full page usually survives it —
        // the same over-fetch-and-trim solx-mcp's router already does.
        let limit = if query.exclude_hidden {
            want.saturating_mul(HIDDEN_OVERFETCH).min(HIDDEN_FETCH_CAP)
        } else {
            want
        };
        let offset = query.list.offset_or_zero();
        let q = query.list.to_sql(LIST_SCHEMA)?;
        let mut binds: Vec<libsql::Value> =
            q.binds.iter().cloned().map(libsql::Value::from).collect();

        let term = query.q.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let select = "SELECT a.id,a.path,a.name,a.caption,a.description,a.capabilities,\
                       a.phrases,a.category,a.param_type_ref,a.result_type_ref,a.action_type,\
                       a.fn_name,a.bin_name,a.action_config,a.files,a.trusted,a.created_at,\
                       a.updated_at FROM actions a";

        // `actions_fts` also has a `path` column, so a plain join would make
        // `q.where_clause`'s unqualified `path`/`created_at`/etc. ambiguous.
        // Joining through a subquery that only exposes `rowid` and FTS5's
        // built-in `rank` column (populated by `MATCH`, lower = better match)
        // keeps every other column resolvable to `actions` alone.
        let (join, where_clause, order) = match term {
            Some(t) => {
                binds.push(libsql::Value::from(fts_match_query(t)));
                let n = binds.len();
                (
                    format!(
                        " JOIN (SELECT rowid, rank FROM actions_fts WHERE actions_fts MATCH ?{n}) f ON f.rowid = a.rowid"
                    ),
                    q.where_clause.clone(),
                    " ORDER BY f.rank".to_string(),
                )
            }
            None => (String::new(), q.where_clause.clone(), q.order_clause.clone()),
        };

        let total = {
            let sql = format!("SELECT COUNT(*) FROM actions a{join}{where_clause}");
            let mut rows = conn.query(&sql, binds.clone()).await.map_err(map_db)?;
            rows.next()
                .await
                .map_err(map_db)?
                .map(|r| r.get::<i64>(0).unwrap_or(0))
                .unwrap_or(0) as usize
        };

        let sql = format!("{select}{join}{where_clause}{order} LIMIT {limit} OFFSET {offset}");
        let mut rows = conn.query(&sql, binds).await.map_err(map_db)?;
        let mut items = Vec::new();
        while let Some(row) = rows.next().await.map_err(map_db)? {
            let mut action = row_to_action(&row)?;
            mask::mask_action_config_opt(&mut action.action_config);
            items.push(action);
        }
        if query.exclude_hidden {
            let policy = self.config.tool_policy();
            items.retain(|a| !policy.is_hidden(a));
            items.truncate(want);
        }
        Ok(Page::new(items, total, want, offset))
    }

    /// Entry point for every *external* caller — the CLI, the MCP server,
    /// the HTTP route, `solx-client`. None of them is an action, so the
    /// caller is `None`; see [`LocalActionManager::exec_as`].
    async fn exec(&self, path: &str, name: &str, params: Value) -> Result<ActionExecResult> {
        self.exec_as(path, name, params, None).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{map_db, Db};
    use crate::schema::DDL;
    use crate::test_support::*;
    use solx_surface::entities::ActionType;

    /// Seed a handful of actions with distinguishable fields for list tests.
    async fn seed_list_fixtures(m: &LocalActionManager) {
        for (path, name, category, caption) in [
            ("/tools", "alpha-tool", "extraction", "Alpha the first"),
            ("/tools", "beta-tool", "search", "Beta the second"),
            ("/other", "gamma-tool", "extraction", "Gamma the third"),
        ] {
            m.save(
                path,
                name,
                ActionInput {
                    action_type: Some(ActionType::Command),
                    fn_name: Some("echo".into()),
                    category: Some(category.into()),
                    caption: Some(caption.into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn list_filters_by_name_substring() {
        let (_d, _c, m) = setup().await;
        seed_list_fixtures(&m).await;
        let page = m
            .list(ListOptions {
                filter_field: Some("name".into()),
                filter_value: Some("ALPHA".into()), // case-insensitive
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].name, "alpha-tool");
    }

    #[tokio::test]
    async fn list_filters_by_category_and_caption() {
        let (_d, _c, m) = setup().await;
        seed_list_fixtures(&m).await;

        let by_category = m
            .list(ListOptions {
                filter_field: Some("category".into()),
                filter_value: Some("extraction".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(by_category.total, 2);

        let by_caption = m
            .list(ListOptions {
                filter_field: Some("caption".into()),
                filter_value: Some("the third".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(by_caption.total, 1);
        assert_eq!(by_caption.items[0].name, "gamma-tool");
    }

    #[tokio::test]
    async fn list_filter_composes_with_path_prefix() {
        let (_d, _c, m) = setup().await;
        seed_list_fixtures(&m).await;
        // "extraction" matches two actions, but only one lives under /tools.
        let page = m
            .list(ListOptions {
                path_prefix: Some("/tools".into()),
                filter_field: Some("category".into()),
                filter_value: Some("extraction".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].name, "alpha-tool");
    }

    #[tokio::test]
    async fn paths_groups_by_path_with_counts() {
        let (_d, _c, m) = setup().await;
        seed_list_fixtures(&m).await;

        // /tools has two actions, /other has one -- `paths` groups by path,
        // so `total` counts distinct paths, not rows.
        let page = m
            .paths(ListOptions { path_prefix: Some("/tools".into()), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].path, "/tools");
        assert_eq!(page.items[0].count, 2);
    }

    #[tokio::test]
    async fn search_matches_caption_via_fts() {
        let (_d, _c, m) = setup().await;
        seed_list_fixtures(&m).await;
        let page = m
            .search(ActionSearchQuery {
                q: Some("Beta".into()),
                list: ListOptions::default(),
                        exclude_hidden: false,
                    })
            .await
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].name, "beta-tool");
    }

    #[tokio::test]
    async fn search_composes_q_with_path_prefix() {
        let (_d, _c, m) = setup().await;
        seed_list_fixtures(&m).await;
        // "extraction" matches two actions by category, but only one lives under /tools.
        let page = m
            .search(ActionSearchQuery {
                q: Some("extraction".into()),
                list: ListOptions {
                    path_prefix: Some("/tools".into()),
                    ..Default::default()
                },
                exclude_hidden: false,
            })
            .await
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].name, "alpha-tool");
    }

    #[tokio::test]
    async fn search_without_q_behaves_like_list() {
        let (_d, _c, m) = setup().await;
        seed_list_fixtures(&m).await;
        let page = m
            .search(ActionSearchQuery {
                q: None,
                list: ListOptions {
                    path_prefix: Some("/tools".into()),
                    ..Default::default()
                },
                exclude_hidden: false,
            })
            .await
            .unwrap();
        assert_eq!(page.total, 2);
    }

    /// Regression test for the backfill bug: a row inserted directly via raw
    /// SQL *before* `actions_fts` ever existed (simulating an actions.db
    /// from before this migration shipped) must still be searchable after
    /// `LocalActionManager::open` runs — i.e. the one-time `'rebuild'` on
    /// first creation actually indexes pre-existing rows, unlike the
    /// original `INSERT ... WHERE rowid NOT IN (SELECT rowid FROM
    /// actions_fts)` backfill, which — because `actions_fts` is
    /// `content='actions'` and a bare `SELECT` against it passes through to
    /// `actions` — always compared `actions.rowid` against itself and
    /// silently indexed nothing.
    #[tokio::test]
    async fn search_finds_a_row_that_predates_the_fts_index() {
        let dir = tempfile::tempdir().unwrap();
        let actions_db_path = dir.path().join("actions.db");

        // Write a row with only the plain `actions` table present — no
        // `actions_fts`, no triggers — exactly what an actions.db created
        // before this migration looks like.
        {
            let db = Db::open(&actions_db_path).await.unwrap();
            let conn = db.connect().await.unwrap();
            conn.execute_batch(DDL).await.map_err(map_db).unwrap();
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "INSERT INTO actions (id,path,name,caption,description,capabilities,phrases,category,param_type_ref,result_type_ref,action_type,fn_name,bin_name,action_config,files,trusted,created_at,updated_at) \
                 VALUES (?1,?2,?3,?4,?5,'[]','[]','','','',?6,?7,'','null','[]',0,?8,?8)",
                libsql::params![
                    Uuid::new_v4().to_string(),
                    "/tools",
                    "old-timer",
                    "Import your old LiveJournal posts",
                    "Migrates entries from a LiveJournal export.",
                    "command",
                    "echo hi",
                    now,
                ],
            )
            .await
            .map_err(map_db)
            .unwrap();
        }

        // Now open it through the normal manager path — this is where the
        // migration (and, with it, the one-time index rebuild) runs.
        let (_cfg, m) = open_manager(dir.path(), &actions_db_path).await;

        let page = m
            .search(ActionSearchQuery {
                q: Some("livejournal".into()),
                list: ListOptions::default(),
                        exclude_hidden: false,
                    })
            .await
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].name, "old-timer");
    }

    #[tokio::test]
    async fn list_ignores_unknown_filter_field() {
        let (_d, _c, m) = setup().await;
        seed_list_fixtures(&m).await;
        let all = m.list(ListOptions::default()).await.unwrap().total;
        let page = m
            .list(ListOptions {
                filter_field: Some("name) OR 1=1 --".into()),
                filter_value: Some("x".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.total, all);
    }

    #[tokio::test]
    async fn list_never_filters_on_action_config() {
        let (_d, _c, m) = setup().await;
        m.save(
            "/tools",
            "s",
            ActionInput {
                action_type: Some(ActionType::Command),
                fn_name: Some("echo".into()),
                action_config: Some(cfg_with_secret("c3VwZXItc2VjcmV0LWtleS1oZXJlLXBhZGRpbmc=")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let all = m.list(ListOptions::default()).await.unwrap().total;
        // action_config is not filterable, so a probe for a secret substring
        // cannot narrow the result set and thereby confirm a guess.
        let probe = m
            .list(ListOptions {
                filter_field: Some("action_config".into()),
                filter_value: Some("super-secret".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(probe.total, all);
    }

    #[tokio::test]
    async fn list_sorts_by_name_in_both_directions() {
        let (_d, _c, m) = setup().await;
        seed_list_fixtures(&m).await;
        let names = |p: Page<Action>| -> Vec<String> {
            p.items.into_iter().map(|a| format!("{}/{}", a.path, a.name)).collect()
        };
        let asc = names(
            m.list(ListOptions {
                path_prefix: Some("/tools".into()),
                sort_by: Some("name".into()),
                sort_order: solx_surface::query::SortOrder::Asc,
                ..Default::default()
            })
            .await
            .unwrap(),
        );
        let desc = names(
            m.list(ListOptions {
                path_prefix: Some("/tools".into()),
                sort_by: Some("name".into()),
                sort_order: solx_surface::query::SortOrder::Desc,
                ..Default::default()
            })
            .await
            .unwrap(),
        );
        assert_eq!(asc, vec!["/tools/alpha-tool", "/tools/beta-tool"]);
        assert_eq!(desc, vec!["/tools/beta-tool", "/tools/alpha-tool"]);
    }

    #[tokio::test]
    async fn get_and_list_redact_secrets_but_exec_reads_them_raw() {
        let (_d, _c, m) = setup().await;
        let input = ActionInput {
            action_type: Some(ActionType::Command),
            fn_name: Some("echo".into()),
            action_config: Some(cfg_with_secret("c3VwZXItc2VjcmV0LWtleS1oZXJlLXBhZGRpbmc=")),
            ..Default::default()
        };
        m.save("/tools", "s", input).await.unwrap();

        // Outbound reads are redacted...
        let got = m.get("/tools", "s").await.unwrap();
        assert_eq!(got.action_config.as_ref().unwrap()["secrets"]["API_TOKEN"], serde_json::json!("***"));
        // ...but non-secret fields survive, so the config stays inspectable.
        assert_eq!(got.action_config.as_ref().unwrap()["cwd"], serde_json::json!("/work"));

        let page = m.list(ListOptions { path_prefix: Some("/tools".into()), ..Default::default() })
            .await
            .unwrap();
        let listed = page.items.iter().find(|a| a.name == "s").unwrap();
        assert_eq!(listed.action_config.as_ref().unwrap()["secrets"]["API_TOKEN"], serde_json::json!("***"));

        // Execution reads through `get_unmasked`, so it still sees the key.
        let raw = m.get_unmasked("/tools", "s").await.unwrap();
        assert_eq!(
            raw.action_config.as_ref().unwrap()["secrets"]["API_TOKEN"],
            serde_json::json!("c3VwZXItc2VjcmV0LWtleS1oZXJlLXBhZGRpbmc=")
        );
    }

    #[tokio::test]
    async fn search_without_the_flag_still_returns_hidden_actions() {
        // Opt-in: the CLI and admin routes inventory the registry and must
        // keep seeing everything.
        let (_d, _c, m) = setup().await;
        m.save("/tools", "visible", ActionInput::default()).await.unwrap();
        m.save(
            "/tools",
            "secret",
            ActionInput { capabilities: vec![solx_config::CAP_HIDDEN.into()], ..Default::default() },
        )
        .await
        .unwrap();

        let page = m
            .search(ActionSearchQuery {
                list: ListOptions { path_prefix: Some("/tools".into()), ..Default::default() },
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.items.len(), 2, "{:?}", page.items.iter().map(|a| &a.name).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn search_with_exclude_hidden_drops_tagged_actions() {
        let (_d, _c, m) = setup().await;
        m.save("/tools", "visible", ActionInput::default()).await.unwrap();
        m.save(
            "/tools",
            "secret",
            ActionInput { capabilities: vec![solx_config::CAP_HIDDEN.into()], ..Default::default() },
        )
        .await
        .unwrap();

        let page = m
            .search(ActionSearchQuery {
                list: ListOptions { path_prefix: Some("/tools".into()), ..Default::default() },
                exclude_hidden: true,
                ..Default::default()
            })
            .await
            .unwrap();
        let names: Vec<&String> = page.items.iter().map(|a| &a.name).collect();
        assert_eq!(names, vec![&"visible".to_string()], "{names:?}");
    }

    #[tokio::test]
    async fn exclude_hidden_still_fills_a_page_when_it_can() {
        // The over-fetch: filtering happens after the query, so without it a
        // limit-2 page containing one hidden row would come back with one
        // item even though a second visible one exists.
        let (_d, _c, m) = setup().await;
        m.save("/tools", "a", ActionInput::default()).await.unwrap();
        m.save(
            "/tools",
            "b",
            ActionInput { capabilities: vec![solx_config::CAP_HIDDEN.into()], ..Default::default() },
        )
        .await
        .unwrap();
        m.save("/tools", "c", ActionInput::default()).await.unwrap();

        let page = m
            .search(ActionSearchQuery {
                list: ListOptions {
                    path_prefix: Some("/tools".into()),
                    limit: Some(2),
                    ..Default::default()
                },
                exclude_hidden: true,
                ..Default::default()
            })
            .await
            .unwrap();
        let names: Vec<&String> = page.items.iter().map(|a| &a.name).collect();
        assert_eq!(names, vec![&"a".to_string(), &"c".to_string()], "{names:?}");
        assert_eq!(page.limit, 2, "the caller's limit, not the inflated one");
    }

    #[tokio::test]
    async fn save_rejects_an_unknown_reserved_capability() {
        let (_d, _c, m) = setup().await;
        let input = ActionInput {
            capabilities: vec!["document".into(), "solx:destructiv".into()],
            ..Default::default()
        };
        let err = m.save("/tools", "typo", input).await.unwrap_err();
        assert!(
            err.to_string().contains("solx:destructiv"),
            "a mistyped reserved tag must fail the save, not silently leave the              action ungated: {err}"
        );
    }

    #[tokio::test]
    async fn save_accepts_reserved_and_free_form_capabilities() {
        let (_d, _c, m) = setup().await;
        let input = ActionInput {
            capabilities: vec![
                "document".into(),
                solx_config::CAP_HIDDEN.into(),
                solx_config::CAP_DESTRUCTIVE.into(),
            ],
            ..Default::default()
        };
        let saved = m.save("/tools", "flagged", input).await.unwrap();
        assert!(saved.capabilities.iter().any(|c| c == solx_config::CAP_HIDDEN));
        assert!(saved.capabilities.iter().any(|c| c == solx_config::CAP_DESTRUCTIVE));
    }

    /// Regression test: `save`'s own response must be redacted too, not just
    /// subsequent `get`/`list` calls -- an upsert's *request* necessarily
    /// carries the real secret (that's how it gets stored), but echoing that
    /// same value back in the *response* is the same leak `get`/`list`
    /// exist to prevent, just one call earlier.
    #[tokio::test]
    async fn save_response_redacts_secrets_too() {
        let (_d, _c, m) = setup().await;
        let input = ActionInput {
            action_type: Some(ActionType::Command),
            fn_name: Some("echo".into()),
            action_config: Some(cfg_with_secret("c3VwZXItc2VjcmV0LWtleS1oZXJlLXBhZGRpbmc=")),
            ..Default::default()
        };
        let saved = m.save("/tools", "s", input).await.unwrap();
        assert_eq!(saved.action_config.as_ref().unwrap()["secrets"]["API_TOKEN"], serde_json::json!("***"));
        assert_eq!(saved.action_config.as_ref().unwrap()["cwd"], serde_json::json!("/work"));
    }

    /// The round trip that would otherwise destroy a key: fetch (redacted),
    /// edit an unrelated field, save the whole thing back.
    #[tokio::test]
    async fn saving_back_a_redacted_config_preserves_the_key() {
        let (_d, _c, m) = setup().await;
        let real_key = "c3VwZXItc2VjcmV0LWtleS1oZXJlLXBhZGRpbmc=";
        m.save(
            "/tools",
            "s",
            ActionInput {
                action_type: Some(ActionType::Command),
                fn_name: Some("echo".into()),
                action_config: Some(cfg_with_secret(real_key)),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let mut fetched = m.get("/tools", "s").await.unwrap().action_config.unwrap();
        fetched["cwd"] = serde_json::json!("/elsewhere");
        m.save(
            "/tools",
            "s",
            ActionInput { action_config: Some(fetched), ..Default::default() },
        )
        .await
        .unwrap();

        let raw = m.get_unmasked("/tools", "s").await.unwrap().action_config.unwrap();
        assert_eq!(raw["secrets"]["API_TOKEN"], serde_json::json!(real_key));
        assert_eq!(raw["cwd"], serde_json::json!("/elsewhere"));
    }

    #[tokio::test]
    async fn saving_an_invented_mask_sentinel_is_rejected() {
        let (_d, _c, m) = setup().await;
        let err = m
            .save(
                "/tools",
                "s",
                ActionInput {
                    action_type: Some(ActionType::Command),
                    action_config: Some(serde_json::json!({ "secrets": { "NEW": "***" } })),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no stored value to restore"), "{err}");
    }

    #[tokio::test]
    async fn save_get_delete() {
        let (_d, _c, m) = setup().await;
        let input = ActionInput {
            description: Some("echoes".into()),
            action_type: Some(ActionType::Command),
            fn_name: Some("echo".into()),
            ..Default::default()
        };
        let a = m.save("/tools", "echo", input).await.unwrap();
        assert_eq!(a.path, "/tools");
        assert_eq!(a.action_type, Some(ActionType::Command));

        assert!(m.get("/tools", "echo").await.is_ok());
        m.delete("/tools", "echo").await.unwrap();
        assert!(m.get("/tools", "echo").await.is_err());
    }
}
