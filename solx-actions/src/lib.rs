//! `solx-actions` — the action store (its own libsql database).
//!
//! Actions are organized by `path` + `name` (unique together) and reference
//! their parameter/result types by full path string. Execution dispatches by
//! `action_type`: `Command` (shell), `Webhook` (HTTP), `Internal` (native
//! handlers — the built-in catalogue: entity CRUD, search, file store,
//! OAuth loopback, etc., see `crate::internal`), and `Wasm` (a *custom*,
//! third-party component executed under wasmtime — built-ins no longer use
//! WASM at all).

mod db;
mod exec;
pub mod internal;
pub mod oauth_loopback;
mod seed;
pub mod secrets;
pub mod wasm_host;
pub mod webhook_auth;

use std::path::Path;
use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use libsql::Connection;
use serde_json::Value;
use solx_config::ConfigService;
use solx_surface::entities::{Action, ActionExecResult, ActionInput, ActionType, FileRef};
use solx_surface::error::{Result, SolxError};
use solx_surface::managers::{ActionManager, DocManager, FileStore, TypeManager};
use solx_surface::path::{full_ref, normalize_path, validate_name};
use solx_surface::query::{ListOptions, Page};
use uuid::Uuid;

use db::{map_db, Db};

pub use seed::BUILTIN_PATH;

const DDL: &str = "\
CREATE TABLE IF NOT EXISTS actions (\
    id TEXT PRIMARY KEY,\
    path TEXT NOT NULL,\
    name TEXT NOT NULL,\
    caption TEXT NOT NULL DEFAULT '',\
    description TEXT NOT NULL DEFAULT '',\
    capabilities TEXT NOT NULL DEFAULT '[]',\
    phrases TEXT NOT NULL DEFAULT '[]',\
    category TEXT NOT NULL DEFAULT '',\
    param_type_ref TEXT NOT NULL DEFAULT '',\
    result_type_ref TEXT NOT NULL DEFAULT '',\
    action_type TEXT NOT NULL DEFAULT '',\
    fn_name TEXT NOT NULL DEFAULT '',\
    bin_name TEXT NOT NULL DEFAULT '',\
    action_config TEXT NOT NULL DEFAULT 'null',\
    files TEXT NOT NULL DEFAULT '[]',\
    trusted INTEGER NOT NULL DEFAULT 0,\
    created_at TEXT NOT NULL,\
    updated_at TEXT NOT NULL,\
    UNIQUE(path,name)\
);";

const DEFAULT_LIMIT: usize = 50;

/// libsql-backed [`ActionManager`] with command/webhook/internal/WASM
/// execution.
pub struct LocalActionManager {
    db: Db,
    config: Arc<ConfigService>,
    types: Arc<dyn TypeManager>,
    docs: Arc<dyn DocManager>,
    files: Arc<dyn FileStore>,
    /// Set once, right after construction, to this manager's own `Arc<dyn
    /// ActionManager>` — needed so WASM guests can recursively call back
    /// into `entity_exec`/`action-exec` (see `wasm_host`). `&self` methods
    /// can't hand out `Arc<Self>` on their own, hence the `OnceLock`.
    self_ref: OnceLock<Weak<dyn ActionManager>>,
}

impl LocalActionManager {
    /// Open the actions database, seed built-in WASM actions, and prepare
    /// execution. `docs`/`types`/`files` are the sibling stores WASM host
    /// functions call into; call [`Self::set_self_ref`] once after
    /// wrapping the result in an `Arc` so recursive action execution works.
    pub async fn open(
        db_path: &Path,
        config: Arc<ConfigService>,
        types: Arc<dyn TypeManager>,
        docs: Arc<dyn DocManager>,
        files: Arc<dyn FileStore>,
    ) -> Result<Self> {
        let db = Db::open(db_path).await?;
        let conn = db.connect().await?;
        conn.execute_batch(DDL).await.map_err(map_db)?;
        seed::seed_builtins(&conn).await?;
        Ok(LocalActionManager {
            db,
            config,
            types,
            docs,
            files,
            self_ref: OnceLock::new(),
        })
    }

    /// Provide this manager's own `Arc<dyn ActionManager>` handle for
    /// recursive WASM `entity_exec`/`action-exec` calls. Must be called
    /// exactly once, right after the manager is wrapped in an `Arc` (e.g.
    /// `let m = Arc::new(LocalActionManager::open(...).await?);
    /// m.set_self_ref(Arc::downgrade(&m) as _);`). A `Weak` is used (not a
    /// strong `Arc`) so the manager doesn't hold a reference cycle to
    /// itself.
    pub fn set_self_ref(&self, self_ref: Weak<dyn ActionManager>) {
        let _ = self.self_ref.set(self_ref);
    }

    /// Resolve a WASM action's `bin_name` to bytes: try the shared
    /// artifact location first, then the action's own scratch space.
    async fn load_wasm_bytes(&self, action: &Action, bin_name: &str) -> Result<Vec<u8>> {
        let shared = solx_files::shared_action_file_path(bin_name);
        if let Ok(bytes) = self.files.get(&shared).await {
            return Ok(bytes);
        }
        let owned = solx_files::action_file_path(&action.id.to_string(), bin_name);
        self.files.get(&owned).await.map_err(|_| {
            SolxError::Exec(format!(
                "wasm artifact '{bin_name}' not found (tried '{shared}' and '{owned}')"
            ))
        })
    }
}

fn opt(s: String) -> Option<String> {
    Some(s).filter(|v| !v.is_empty())
}

fn action_type_to_str(t: Option<ActionType>) -> String {
    match t {
        Some(ActionType::Wasm) => "wasm",
        Some(ActionType::Webhook) => "webhook",
        Some(ActionType::Command) => "command",
        Some(ActionType::Internal) => "internal",
        None => "",
    }
    .to_string()
}

fn action_type_from_str(s: &str) -> Option<ActionType> {
    match s {
        "wasm" => Some(ActionType::Wasm),
        "webhook" => Some(ActionType::Webhook),
        "command" => Some(ActionType::Command),
        "internal" => Some(ActionType::Internal),
        _ => None,
    }
}

fn parse_dt(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.into())
        .map_err(|e| SolxError::Db(e.to_string()))
}

fn row_to_action(row: &libsql::Row) -> Result<Action> {
    let id = Uuid::parse_str(&row.get::<String>(0).map_err(map_db)?)
        .map_err(|e| SolxError::Db(e.to_string()))?;
    let path: String = row.get(1).map_err(map_db)?;
    let name: String = row.get(2).map_err(map_db)?;
    let caption = opt(row.get::<String>(3).map_err(map_db)?);
    let description = opt(row.get::<String>(4).map_err(map_db)?);
    let capabilities: Vec<String> =
        serde_json::from_str(&row.get::<String>(5).map_err(map_db)?).unwrap_or_default();
    let phrases: Vec<String> =
        serde_json::from_str(&row.get::<String>(6).map_err(map_db)?).unwrap_or_default();
    let category = opt(row.get::<String>(7).map_err(map_db)?);
    let param_type_ref = opt(row.get::<String>(8).map_err(map_db)?);
    let result_type_ref = opt(row.get::<String>(9).map_err(map_db)?);
    let action_type = action_type_from_str(&row.get::<String>(10).map_err(map_db)?);
    let fn_name = opt(row.get::<String>(11).map_err(map_db)?);
    let bin_name = opt(row.get::<String>(12).map_err(map_db)?);
    let action_config: Option<Value> =
        serde_json::from_str(&row.get::<String>(13).map_err(map_db)?).unwrap_or(None);
    let files: Vec<FileRef> =
        serde_json::from_str(&row.get::<String>(14).map_err(map_db)?).unwrap_or_default();
    let trusted: bool = row.get::<i64>(15).map_err(map_db)? != 0;
    let created_at = parse_dt(&row.get::<String>(16).map_err(map_db)?)?;
    let updated_at = parse_dt(&row.get::<String>(17).map_err(map_db)?)?;
    Ok(Action {
        id,
        path,
        name,
        caption,
        description,
        capabilities,
        phrases,
        category,
        param_type_ref,
        result_type_ref,
        action_type,
        fn_name,
        bin_name,
        action_config,
        files,
        trusted,
        created_at,
        updated_at,
    })
}

const SELECT: &str = "SELECT id,path,name,caption,description,capabilities,phrases,category,param_type_ref,result_type_ref,action_type,fn_name,bin_name,action_config,files,trusted,created_at,updated_at FROM actions";

async fn get_row(conn: &Connection, path: &str, name: &str) -> Result<Option<Action>> {
    let mut rows = conn
        .query(
            &format!("{SELECT} WHERE path=?1 AND name=?2"),
            libsql::params![path.to_string(), name.to_string()],
        )
        .await
        .map_err(map_db)?;
    match rows.next().await.map_err(map_db)? {
        Some(row) => Ok(Some(row_to_action(&row)?)),
        None => Ok(None),
    }
}

#[async_trait]
impl ActionManager for LocalActionManager {
    async fn post(&self, path: &str, name: &str, input: ActionInput) -> Result<Action> {
        let path = normalize_path(path)?;
        validate_name(name)?;
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
        let action_config = input
            .action_config
            .or_else(|| existing.as_ref().and_then(|a| a.action_config.clone()));
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

        get_row(&conn, &path, &name)
            .await?
            .ok_or_else(|| SolxError::Other("action vanished after write".into()))
    }

    async fn get(&self, path: &str, name: &str) -> Result<Action> {
        let path = normalize_path(path)?;
        validate_name(name)?;
        let fr = full_ref(&path, name)?;
        let conn = self.db.connect().await?;
        get_row(&conn, &path, name.trim())
            .await?
            .ok_or_else(|| SolxError::NotFound(format!("action {fr}")))
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

        let (where_sql, like) = match &opts.path_prefix {
            Some(p) => {
                let p = normalize_path(p)?;
                let like = if p == "/" { "/%".to_string() } else { format!("{p}/%") };
                (" WHERE (path=?1 OR path LIKE ?2)".to_string(), Some((p, like)))
            }
            None => (String::new(), None),
        };

        let total = {
            let sql = format!("SELECT COUNT(*) FROM actions{where_sql}");
            let mut rows = match &like {
                Some((p, l)) => conn
                    .query(&sql, libsql::params![p.clone(), l.clone()])
                    .await
                    .map_err(map_db)?,
                None => conn.query(&sql, ()).await.map_err(map_db)?,
            };
            rows.next()
                .await
                .map_err(map_db)?
                .map(|r| r.get::<i64>(0).unwrap_or(0))
                .unwrap_or(0) as usize
        };

        let sql = format!("{SELECT}{where_sql} ORDER BY path,name LIMIT {limit} OFFSET {offset}");
        let mut rows = match &like {
            Some((p, l)) => conn
                .query(&sql, libsql::params![p.clone(), l.clone()])
                .await
                .map_err(map_db)?,
            None => conn.query(&sql, ()).await.map_err(map_db)?,
        };
        let mut items = Vec::new();
        while let Some(row) = rows.next().await.map_err(map_db)? {
            items.push(row_to_action(&row)?);
        }
        Ok(Page::new(items, total, limit, offset))
    }

    async fn exec(&self, path: &str, name: &str, params: Value) -> Result<ActionExecResult> {
        let action = self.get(path, name).await?;
        let action_ref = full_ref(&action.path, &action.name)?;

        // Validate params against the declared parameter type, if any.
        if let Some(tr) = &action.param_type_ref {
            self.types.validate(&params, tr).await?;
        }

        let result = match action.action_type {
            Some(ActionType::Command) => {
                let fn_name = action.fn_name.as_deref().ok_or_else(|| {
                    SolxError::Exec("command action has no fn_name (the command to run)".into())
                })?;
                exec::run_command(&self.config, fn_name, &action.action_config, &params)?
            }
            Some(ActionType::Webhook) => {
                let url = action.fn_name.as_deref().ok_or_else(|| {
                    SolxError::Exec("webhook action has no fn_name (URL)".into())
                })?;
                exec::run_webhook(self, &action.path, &action.name, url, &action.action_config, &params).await?
            }
            Some(ActionType::Internal) => {
                let fn_name = action.fn_name.as_deref().ok_or_else(|| {
                    SolxError::Exec("internal action has no fn_name (operation)".into())
                })?;
                let actions_arc = self.self_ref.get().and_then(|w| w.upgrade()).ok_or_else(|| {
                    SolxError::Exec(
                        "action manager self-reference not set (internal wiring bug — \
                         call LocalActionManager::set_self_ref after construction)"
                            .into(),
                    )
                })?;
                let ctx = internal::InternalCtx {
                    docs: self.docs.clone(),
                    types: self.types.clone(),
                    actions: actions_arc,
                    files: self.files.clone(),
                    action_config: action.action_config.clone(),
                };
                internal::run_internal(fn_name, &params, &ctx)
                    .await
                    .map_err(SolxError::Exec)?
            }
            Some(ActionType::Wasm) => {
                let bin_name = action.bin_name.as_deref().ok_or_else(|| {
                    SolxError::Exec("wasm action has no bin_name (artifact)".into())
                })?;
                let bytes = self.load_wasm_bytes(&action, bin_name).await?;
                let actions_arc = self.self_ref.get().and_then(|w| w.upgrade()).ok_or_else(|| {
                    SolxError::Exec(
                        "action manager self-reference not set (internal wiring bug — \
                         call LocalActionManager::set_self_ref after construction)"
                            .into(),
                    )
                })?;
                // WASM execution reports its own success/message (a guest can
                // report a handled failure without erroring the host call), so
                // it returns a full ActionExecResult directly rather than
                // going through the common Value-wrapping below.
                return wasm_host::exec(
                    actions_arc,
                    self.files.clone(),
                    &bytes,
                    action.fn_name.as_deref(),
                    &params,
                    Some(&action_ref),
                )
                .await;
            }
            None => {
                return Err(SolxError::Exec(format!(
                    "action {action_ref} has no action_type to execute"
                )))
            }
        };

        Ok(ActionExecResult {
            action: action_ref,
            result,
            success: true,
            message: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solx_docs::LocalDocManager;
    use solx_files::LocalFileStore;
    use solx_types::LocalTypeManager;

    async fn setup() -> (tempfile::TempDir, Arc<ConfigService>, LocalActionManager) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Arc::new(ConfigService::open_in(dir.path()).unwrap());
        let types: Arc<dyn TypeManager> = Arc::new(
            LocalTypeManager::open(&dir.path().join("types.db"))
                .await
                .unwrap(),
        );
        let docs: Arc<dyn DocManager> = Arc::new(
            LocalDocManager::open(
                &dir.path().join("docs.db"),
                &dir.path().join("idx"),
                types.clone(),
            )
            .await
            .unwrap(),
        );
        let files: Arc<dyn FileStore> = Arc::new(LocalFileStore::new(dir.path().join("files")));
        let m = LocalActionManager::open(
            &dir.path().join("actions.db"),
            cfg.clone(),
            types,
            docs,
            files,
        )
        .await
        .unwrap();
        (dir, cfg, m)
    }

    #[tokio::test]
    async fn post_get_delete() {
        let (_d, _c, m) = setup().await;
        let input = ActionInput {
            description: Some("echoes".into()),
            action_type: Some(ActionType::Command),
            fn_name: Some("echo".into()),
            ..Default::default()
        };
        let a = m.post("/tools", "echo", input).await.unwrap();
        assert_eq!(a.path, "/tools");
        assert_eq!(a.action_type, Some(ActionType::Command));

        assert!(m.get("/tools", "echo").await.is_ok());
        m.delete("/tools", "echo").await.unwrap();
        assert!(m.get("/tools", "echo").await.is_err());
    }

    #[tokio::test]
    async fn exec_command_runs_fn_name_directly() {
        let (_d, _c, m) = setup().await;
        // fn_name is the literal command to run — no config registration
        // needed. Echo a bare number so the output is valid JSON on both
        // cmd.exe and sh without quote handling.
        let input = ActionInput {
            action_type: Some(ActionType::Command),
            fn_name: Some("echo 42".into()),
            ..Default::default()
        };
        m.post("/tools", "echo", input).await.unwrap();

        let res = m
            .exec("/tools", "echo", serde_json::json!({}))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.result, serde_json::json!(42));
    }

    #[tokio::test]
    async fn exec_wasm_missing_artifact_errors() {
        let (_d, _c, m) = setup().await;
        let input = ActionInput {
            action_type: Some(ActionType::Wasm),
            bin_name: Some("x.wasm".into()),
            ..Default::default()
        };
        m.post("/tools", "w", input).await.unwrap();
        let err = m
            .exec("/tools", "w", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("x.wasm"), "{err}");
    }

    #[tokio::test]
    async fn exec_wasm_without_bin_name_errors() {
        let (_d, _c, m) = setup().await;
        let input = ActionInput {
            action_type: Some(ActionType::Wasm),
            ..Default::default()
        };
        m.post("/tools", "w", input).await.unwrap();
        let err = m
            .exec("/tools", "w", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("bin_name"), "{err}");
    }

    #[tokio::test]
    async fn seeded_builtin_actions_are_discoverable() {
        let (_d, _c, m) = setup().await;
        let page = m
            .list(ListOptions {
                path_prefix: Some("/builtin".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(page.total > 0, "expected seeded built-in actions under /builtin");
        let doc_get = page
            .items
            .iter()
            .find(|a| a.name == "entity_get_document")
            .expect("entity_get_document should be seeded");
        assert!(doc_get.trusted, "seeded built-ins should be trusted");
        // Entity CRUD is dispatched natively (`internal`), not through the
        // WASM guest — see solx-actions/src/seed.rs's module docs: the WASM
        // entity-ops guest silently ignores the `path` parameter.
        assert_eq!(doc_get.action_type, Some(ActionType::Internal));

        // Every built-in is native dispatch now — there's no more WASM
        // built-in catalogue (see solx-actions/src/seed.rs's module docs).
        assert!(
            page.items.iter().all(|a| a.action_type == Some(ActionType::Internal)),
            "every seeded /builtin action should be action_type=internal"
        );
    }
}
