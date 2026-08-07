//! `solx-actions` — the action store (its own libsql database).
//!
//! Actions are organized by `path` + `name` (unique together) and reference
//! their parameter/result types by full path string. Execution dispatches by
//! `action_type`: `Command` (shell), `Webhook` (HTTP), `Internal` (native
//! handlers — the built-in catalogue: entity CRUD, search, file store,
//! OAuth loopback, etc., see `crate::internal`), `Wasm` (a *custom*,
//! third-party component executed under wasmtime — built-ins no longer use
//! WASM at all), and `Script` (a `solx-scripts` script artifact, see
//! `crate::script`).

pub mod caller;
mod db;
mod exec;
pub mod internal;
mod mask;
pub mod oauth_loopback;
pub mod script;
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

use caller::Caller;
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

/// libsql-backed [`ActionManager`] with command/webhook/internal/WASM/script
/// execution.
pub struct LocalActionManager {
    db: Db,
    config: Arc<ConfigService>,
    types: Arc<dyn TypeManager>,
    docs: Arc<dyn DocManager>,
    files: Arc<dyn FileStore>,
    /// Set once, right after construction, to this manager's own
    /// `Arc<LocalActionManager>` — needed so WASM guests can recursively
    /// call back into `action-exec` (see `wasm_host`). `&self` methods
    /// can't hand out `Arc<Self>` on their own, hence the `OnceLock`.
    ///
    /// Concrete rather than `Weak<dyn ActionManager>` so the recursive hop
    /// can reach [`Self::exec_as`], which carries the calling action's
    /// identity. The trait's `exec` has no room for it, and deliberately
    /// so — see [`crate::caller`].
    self_ref: OnceLock<Weak<LocalActionManager>>,
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

    /// Provide this manager's own handle for recursive WASM `action-exec`
    /// calls. Must be called exactly once, right after the manager is
    /// wrapped in an `Arc` (e.g.
    /// `let m = Arc::new(LocalActionManager::open(...).await?);
    /// m.set_self_ref(Arc::downgrade(&m));`). A `Weak` is used (not a
    /// strong `Arc`) so the manager doesn't hold a reference cycle to
    /// itself.
    pub fn set_self_ref(&self, self_ref: Weak<LocalActionManager>) {
        let _ = self.self_ref.set(self_ref);
    }

    /// Upgrade [`Self::self_ref`], or explain the wiring bug.
    fn self_arc(&self) -> Result<Arc<LocalActionManager>> {
        self.self_ref.get().and_then(Weak::upgrade).ok_or_else(|| {
            SolxError::Exec(
                "action manager self-reference not set (internal wiring bug — \
                 call LocalActionManager::set_self_ref after construction)"
                    .into(),
            )
        })
    }

    /// Read a row **without** redacting `action_config`.
    ///
    /// Execution needs the real thing — `run_command` reads `cwd`,
    /// `run_webhook` reads `auth`/`headers`, and `webhook_auth` resolves
    /// credentials out of it. The [`ActionManager::get`] trait method wraps
    /// this and masks; nothing that leaves the process should use this one.
    async fn get_unmasked(&self, path: &str, name: &str) -> Result<Action> {
        let path = normalize_path(path)?;
        validate_name(name)?;
        let fr = full_ref(&path, name)?;
        let conn = self.db.connect().await?;
        get_row(&conn, &path, name.trim())
            .await?
            .ok_or_else(|| SolxError::NotFound(format!("action {fr}")))
    }

    /// Resolve a WASM action's `bin_name` to bytes: try the shared
    /// artifact location first, then the action's own scratch space.
    ///
    /// Returned behind an `Arc` because the bytes are handed to
    /// `wasm_host::exec`, which moves them onto the blocking pool to
    /// compile on a cache miss.
    async fn load_wasm_bytes(&self, action: &Action, bin_name: &str) -> Result<Arc<Vec<u8>>> {
        let shared = solx_files::shared_action_file_path(bin_name);
        if let Ok(bytes) = self.files.get(&shared).await {
            return Ok(Arc::new(bytes));
        }
        let owned = solx_files::action_file_path(&action.id.to_string(), bin_name);
        self.files.get(&owned).await.map(Arc::new).map_err(|_| {
            SolxError::Exec(format!(
                "wasm artifact '{bin_name}' not found (tried '{shared}' and '{owned}')"
            ))
        })
    }

    /// Resolve a `Script` action's `bin_name` to its `.solx` source text —
    /// same shared/owned lookup order as [`Self::load_wasm_bytes`].
    async fn load_script_source(&self, action: &Action, bin_name: &str) -> Result<String> {
        let shared = solx_files::shared_action_file_path(bin_name);
        let bytes = match self.files.get(&shared).await {
            Ok(bytes) => bytes,
            Err(_) => {
                let owned = solx_files::action_file_path(&action.id.to_string(), bin_name);
                self.files.get(&owned).await.map_err(|_| {
                    SolxError::Exec(format!(
                        "script artifact '{bin_name}' not found (tried '{shared}' and '{owned}')"
                    ))
                })?
            }
        };
        String::from_utf8(bytes).map_err(|e| {
            SolxError::Exec(format!("script artifact '{bin_name}' is not valid UTF-8: {e}"))
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
        Some(ActionType::Script) => "script",
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
        "script" => Some(ActionType::Script),
        _ => None,
    }
}

fn parse_dt(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.into())
        .map_err(|e| SolxError::Db(e.to_string()))
}

/// Per-action wall-clock ceiling from `action_config.timeout_secs`, using
/// the same untyped-config convention as `cwd`. `None` means the backend's
/// own default applies.
fn timeout_secs(action_config: &Option<Value>) -> Option<u64> {
    action_config.as_ref()?.get("timeout_secs")?.as_u64()
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

        get_row(&conn, &path, &name)
            .await?
            .ok_or_else(|| SolxError::Other("action vanished after write".into()))
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

        let (where_sql, like) = match &opts.path_prefix {
            Some(p) => {
                let p = normalize_path(p)?;
                let like = if p == "/" { "/%".to_string() } else { format!("{p}/%").to_string() };
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
            let mut action = row_to_action(&row)?;
            mask::mask_action_config_opt(&mut action.action_config);
            items.push(action);
        }
        Ok(Page::new(items, total, limit, offset))
    }

    /// Entry point for every *external* caller — the CLI, the MCP server,
    /// the HTTP route, `solx-client`. None of them is an action, so the
    /// caller is `None`; see [`LocalActionManager::exec_as`].
    async fn exec(&self, path: &str, name: &str, params: Value) -> Result<ActionExecResult> {
        self.exec_as(path, name, params, None).await
    }
}

impl LocalActionManager {
    /// Execute an action, optionally attributed to the action that invoked
    /// it.
    ///
    /// `caller` is `Some` only on the recursive hop: a WASM guest calling
    /// `action-exec` (see [`crate::wasm_host`]), which is the sole
    /// re-entrant path into execution. It scopes `get_secret`/`set_secret`
    /// to the *calling* action's own keys.
    pub async fn exec_as(
        &self,
        path: &str,
        name: &str,
        params: Value,
        caller: Option<&Caller>,
    ) -> Result<ActionExecResult> {
        let action = self.get_unmasked(path, name).await?;
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
                exec::run_command(
                    &self.config,
                    fn_name,
                    &action.action_config,
                    &params,
                    timeout_secs(&action.action_config),
                )
                .await?
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
                let ctx = internal::InternalCtx {
                    docs: self.docs.clone(),
                    types: self.types.clone(),
                    actions: self.self_arc()?,
                    files: self.files.clone(),
                    action_config: action.action_config.clone(),
                    caller: caller.cloned(),
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
                // A new caller frame: anything this guest invokes is invoked
                // by *this* action, not by whoever invoked it. The incoming
                // `caller` is therefore dropped rather than forwarded, so a
                // guest can never reach an outer action's secret keys.
                let frame = Caller::from_action(&action_ref, action.action_config.as_ref());
                // WASM execution reports its own success/message (a guest can
                // report a handled failure without erroring the host call), so
                // it returns a full ActionExecResult directly rather than
                // going through the common Value-wrapping below.
                return wasm_host::exec(
                    self.self_arc()?,
                    self.files.clone(),
                    bytes,
                    action.fn_name.as_deref(),
                    &params,
                    frame,
                    timeout_secs(&action.action_config),
                )
                .await;
            }
            Some(ActionType::Script) => {
                let bin_name = action.bin_name.as_deref().ok_or_else(|| {
                    SolxError::Exec("script action has no bin_name (artifact)".into())
                })?;
                let source = self.load_script_source(&action, bin_name).await?;
                // Same reasoning as the Wasm arm above: a fresh caller frame
                // for this action's own identity, not the incoming one — a
                // script's nested `exec` calls must never reach an outer
                // action's secret keys.
                let frame = Caller::from_action(&action_ref, action.action_config.as_ref());
                let runner = script::ActionCommandRunner {
                    actions: self.self_arc()?,
                    caller: frame,
                };
                let initial = std::collections::HashMap::from([("params".to_string(), params.clone())]);
                let run = solx_scripts::execute_script_with_vars(&runner, &source, initial);
                let budget = std::time::Duration::from_secs(
                    timeout_secs(&action.action_config).unwrap_or(exec::DEFAULT_TIMEOUT_SECS),
                );
                tokio::time::timeout(budget, run).await.map_err(|_| {
                    SolxError::Exec(format!("script action {action_ref} timed out"))
                })??
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

    /// A manager wired for recursive execution, as `solx-manager` does it.
    async fn setup_wired() -> (tempfile::TempDir, Arc<LocalActionManager>) {
        let (dir, _cfg, m) = setup().await;
        let m = Arc::new(m);
        m.set_self_ref(Arc::downgrade(&m));
        (dir, m)
    }

    fn cfg_with_secret(key: &str) -> Value {
        serde_json::json!({ "cwd": "/work", "secrets": { "API_TOKEN": key } })
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
        m.post("/tools", "s", input).await.unwrap();

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

    /// The round trip that would otherwise destroy a key: fetch (redacted),
    /// edit an unrelated field, post the whole thing back.
    #[tokio::test]
    async fn posting_back_a_redacted_config_preserves_the_key() {
        let (_d, _c, m) = setup().await;
        let real_key = "c3VwZXItc2VjcmV0LWtleS1oZXJlLXBhZGRpbmc=";
        m.post(
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
        m.post(
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
    async fn posting_an_invented_mask_sentinel_is_rejected() {
        let (_d, _c, m) = setup().await;
        let err = m
            .post(
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

    // ── entity_post_action: no self-granting shell ───────────────────────
    //
    // These go through `exec` on `/builtin/entity_post_action`, which is the
    // exact path an MCP tool call, a WASM guest's `action-exec`, and a
    // `.solx` script all take. A direct `m.post(...)` is the CLI's path and
    // stays allowed — that's the whole distinction being enforced.

    async fn exec_builtin(m: &Arc<LocalActionManager>, fn_name: &str, params: Value) -> Result<Value> {
        m.exec(BUILTIN_PATH, fn_name, params).await.map(|r| r.result)
    }

    #[tokio::test]
    async fn entity_post_action_refuses_to_create_command_or_webhook() {
        let (_d, m) = setup_wired().await;

        for ty in ["command", "webhook"] {
            let err = exec_builtin(
                &m,
                "entity_post_action",
                serde_json::json!({
                    "path": "/evil", "name": "shell",
                    "action_type": ty, "fn_name": "rm -rf /"
                }),
            )
            .await
            .unwrap_err();
            assert!(err.to_string().contains("use the solx CLI"), "{ty}: {err}");
            assert!(m.get_unmasked("/evil", "shell").await.is_err(), "{ty} was created anyway");
        }
    }

    /// `post` is a merge-upsert, so a payload with no `action_type` at all
    /// would otherwise silently rewrite an existing Command action's shell
    /// command. The guard has to consult the stored row, not just the input.
    #[tokio::test]
    async fn entity_post_action_refuses_to_repoint_an_existing_command() {
        let (_d, m) = setup_wired().await;
        m.post(
            "/tools",
            "safe",
            ActionInput {
                action_type: Some(ActionType::Command),
                fn_name: Some("echo 42".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let err = exec_builtin(
            &m,
            "entity_post_action",
            serde_json::json!({ "path": "/tools", "name": "safe", "fn_name": "rm -rf /" }),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("use the solx CLI"), "{err}");
        assert_eq!(
            m.get_unmasked("/tools", "safe").await.unwrap().fn_name.as_deref(),
            Some("echo 42")
        );
    }

    #[tokio::test]
    async fn entity_delete_action_refuses_to_remove_a_command() {
        let (_d, m) = setup_wired().await;
        m.post(
            "/tools",
            "safe",
            ActionInput {
                action_type: Some(ActionType::Command),
                fn_name: Some("echo 42".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let err = exec_builtin(
            &m,
            "entity_delete_action",
            serde_json::json!({ "path": "/tools", "name": "safe" }),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("use the solx CLI"), "{err}");
        assert!(m.get_unmasked("/tools", "safe").await.is_ok(), "action was deleted anyway");
    }

    /// The lockdown is limited to the two executable types — everything else
    /// an agent legitimately does through this built-in still works.
    #[tokio::test]
    async fn entity_post_action_still_allows_non_executable_types() {
        let (_d, m) = setup_wired().await;
        exec_builtin(
            &m,
            "entity_post_action",
            serde_json::json!({
                "path": "/tools", "name": "w",
                "action_type": "wasm", "bin_name": "x.wasm"
            }),
        )
        .await
        .unwrap();
        assert!(m.get_unmasked("/tools", "w").await.is_ok());
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

    // ── Script action tests ─────────────────────────────────────────────────

    /// Like `setup_wired`, but also hands back the `FileStore` so a test can
    /// upload a `.solx` artifact before posting the action that references it.
    async fn setup_script() -> (tempfile::TempDir, Arc<LocalActionManager>, Arc<dyn FileStore>) {
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
            cfg,
            types,
            docs,
            files.clone(),
        )
        .await
        .unwrap();
        let m = Arc::new(m);
        m.set_self_ref(Arc::downgrade(&m));
        (dir, m, files)
    }

    async fn post_script_artifact(files: &Arc<dyn FileStore>, name: &str, source: &str) {
        files
            .put(
                &solx_files::shared_action_file_path(name),
                source.as_bytes().to_vec(),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn exec_script_reads_params_and_calls_nested_action() {
        let (_d, m, files) = setup_script().await;
        // The bare `exec /builtin/uuid` (no `json` wrapping needed) becomes
        // the script's result directly: the callee's whole ActionExecResult,
        // JSON-encoded — same as `handle_exec` in the CLI.
        // `json`'s argument is parsed as JSON, and `tokenize_stage` strips
        // one layer of quoting to form the token — so a JSON string literal
        // needs the outer '...' shell-style quoting plus inner \"...\" JSON
        // quotes, same as the CLI's `json` command (`solx script -e 'json
        // \'"big"\''`).
        post_script_artifact(
            &files,
            "hello.solx",
            "if $params.go == true; exec /builtin/uuid; else; json '\"skipped\"'; endif",
        )
        .await;
        m.post(
            "/tools",
            "hello",
            ActionInput {
                action_type: Some(ActionType::Script),
                bin_name: Some("hello.solx".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let ran = m.exec("/tools", "hello", serde_json::json!({"go": true})).await.unwrap();
        assert_eq!(ran.result["success"], serde_json::json!(true));
        assert!(
            ran.result["result"]["uuid"].as_str().is_some_and(|s| !s.is_empty()),
            "{:?}",
            ran.result
        );

        let skipped = m.exec("/tools", "hello", serde_json::json!({"go": false})).await.unwrap();
        assert_eq!(skipped.result, serde_json::json!("skipped"));
    }

    #[tokio::test]
    async fn exec_script_missing_bin_name_errors() {
        let (_d, _c, m) = setup().await;
        let input = ActionInput {
            action_type: Some(ActionType::Script),
            ..Default::default()
        };
        m.post("/tools", "s", input).await.unwrap();
        let err = m.exec("/tools", "s", serde_json::json!({})).await.unwrap_err();
        assert!(err.to_string().contains("bin_name"), "{err}");
    }

    #[tokio::test]
    async fn exec_script_missing_artifact_errors() {
        let (_d, _c, m) = setup().await;
        let input = ActionInput {
            action_type: Some(ActionType::Script),
            bin_name: Some("missing.solx".into()),
            ..Default::default()
        };
        m.post("/tools", "s", input).await.unwrap();
        let err = m.exec("/tools", "s", serde_json::json!({})).await.unwrap_err();
        assert!(err.to_string().contains("missing.solx"), "{err}");
    }

    #[tokio::test]
    async fn exec_script_unsupported_stage_verb_errors() {
        let (_d, m, files) = setup_script().await;
        post_script_artifact(&files, "bad.solx", "post doc /a --json '{}'").await;
        m.post(
            "/tools",
            "bad",
            ActionInput {
                action_type: Some(ActionType::Script),
                bin_name: Some("bad.solx".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let err = m.exec("/tools", "bad", serde_json::json!({})).await.unwrap_err();
        assert!(err.to_string().contains("unsupported script stage"), "{err}");
    }

    #[tokio::test]
    async fn exec_script_action_not_blocked_by_executable_action_guard() {
        let (_d, m, _files) = setup_script().await;
        // Unlike `command`/`webhook`, `script` may be created through the
        // guarded `entity_post_action` built-in the same way `wasm` can.
        exec_builtin(
            &m,
            "entity_post_action",
            serde_json::json!({
                "path": "/tools", "name": "s",
                "action_type": "script", "bin_name": "x.solx"
            }),
        )
        .await
        .unwrap();
        assert!(m.get_unmasked("/tools", "s").await.is_ok());
    }

    #[tokio::test]
    async fn exec_script_times_out() {
        let (_d, m, files) = setup_script().await;
        post_script_artifact(&files, "slow.solx", "wait 5").await;
        m.post(
            "/tools",
            "slow",
            ActionInput {
                action_type: Some(ActionType::Script),
                bin_name: Some("slow.solx".into()),
                action_config: Some(serde_json::json!({ "timeout_secs": 1 })),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let err = m.exec("/tools", "slow", serde_json::json!({})).await.unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
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
