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

pub mod auth;
pub mod caller;
mod db;
mod exec;
pub mod internal;
pub mod loopback;
mod mask;
pub mod net;
pub mod script;
mod seed;
pub mod secrets;
pub mod wasm;

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use libsql::Connection;
use serde_json::{json, Value};
use solx_config::ConfigService;
use solx_console::{console, invocations, ConsoleStore, InvocationStore};
use solx_surface::entities::{Action, ActionExecResult, ActionInput, ActionType, FileRef};
use solx_surface::error::{Result, SolxError};
use solx_surface::internal_actions::{ActionExecutor, InternalActionRegistry};
use solx_surface::managers::{ActionManager, DocManager, FileStore, TypeManager};
use solx_surface::path::{full_ref, normalize_path, validate_name};
use solx_surface::query::{ActionSearchQuery, ListOptions, ListSchema, Page, PathFacet, SortOrder};
use tokio::task::AbortHandle;
use uuid::Uuid;

use caller::Caller;
use db::{map_db, Db};

/// Whether this process is a long-lived host (`solx-server`, `solx-mcp`) —
/// the only kind that can safely run a detached `action-start` invocation.
/// A `tokio::spawn`'d task is not cancelled when an axum handler future
/// drops, which is what lets detachment survive client disconnect; but a
/// CLI process exits the moment `exec` returns, which would kill a
/// newly-started task before it could ever be polled. See
/// `docs/async-actions-plan.md` §6.
///
/// A process-wide flag rather than a field threaded through
/// `LocalActionManager::open` because the fact is about the *process*, not
/// about any one manager instance — every construction site (tests
/// included) would otherwise have to carry it.
static LONG_LIVED_HOST: AtomicBool = AtomicBool::new(false);

/// Call once at startup from a long-lived host's `main()` — `solx-server`
/// and `solx-mcp` do this; `solx-cli` deliberately never does.
pub fn set_long_lived_host(long_lived: bool) {
    LONG_LIVED_HOST.store(long_lived, Ordering::Relaxed);
}

fn is_long_lived_host() -> bool {
    LONG_LIVED_HOST.load(Ordering::Relaxed)
}

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

/// External-content FTS5 index over the action catalogue, kept in sync with
/// `actions` purely by trigger — `save`/`delete`/`seed_builtins` never touch
/// this table directly. `id` is `TEXT PRIMARY KEY` (not `INTEGER PRIMARY
/// KEY`), so `actions` keeps SQLite's implicit `rowid`, which is what
/// `content_rowid='rowid'` links against.
const FTS_DDL: &str = "\
CREATE VIRTUAL TABLE IF NOT EXISTS actions_fts USING fts5(\
    path, name, caption, description, category, phrases, \
    content='actions', content_rowid='rowid', tokenize='porter unicode61'\
);\
CREATE TRIGGER IF NOT EXISTS actions_ai AFTER INSERT ON actions BEGIN \
  INSERT INTO actions_fts(rowid,path,name,caption,description,category,phrases)\
  VALUES (new.rowid,new.path,new.name,new.caption,new.description,new.category,new.phrases);\
END;\
CREATE TRIGGER IF NOT EXISTS actions_ad AFTER DELETE ON actions BEGIN \
  INSERT INTO actions_fts(actions_fts,rowid,path,name,caption,description,category,phrases)\
  VALUES('delete',old.rowid,old.path,old.name,old.caption,old.description,old.category,old.phrases);\
END;\
CREATE TRIGGER IF NOT EXISTS actions_au AFTER UPDATE ON actions BEGIN \
  INSERT INTO actions_fts(actions_fts,rowid,path,name,caption,description,category,phrases)\
  VALUES('delete',old.rowid,old.path,old.name,old.caption,old.description,old.category,old.phrases);\
  INSERT INTO actions_fts(rowid,path,name,caption,description,category,phrases)\
  VALUES (new.rowid,new.path,new.name,new.caption,new.description,new.category,new.phrases);\
END;";

const DEFAULT_LIMIT: usize = 50;

/// libsql-backed [`ActionManager`] with command/webhook/internal/WASM/script
/// execution.
pub struct LocalActionManager {
    db: Db,
    config: Arc<ConfigService>,
    types: Arc<dyn TypeManager>,
    docs: Arc<dyn DocManager>,
    files: Arc<dyn FileStore>,
    /// Owned by `solx-console`, its own separate database file — see that
    /// crate's module docs.
    console: Arc<ConsoleStore>,
    /// Status/cancel-flag state for `action-start`/`action-stop`/
    /// `action-poll` — see `solx_console::invocations` for why the console
    /// alone isn't enough. Also owned by `solx-console`.
    invocations: Arc<InvocationStore>,
    /// Every `fn_name -> handler` registration contributed by an internal-
    /// action plugin crate (currently just `solx-console`, for
    /// `console_*`/`action_{start,stop,poll,cancelled}`) — consulted by
    /// `internal::run_internal`'s catch-all arm after its own hard-coded
    /// built-ins. See `solx_surface::internal_actions`.
    ///
    /// Built lazily in [`Self::set_self_ref`] rather than in [`Self::open`]:
    /// `solx_console::actions::plugin` needs an `Arc<dyn ActionExecutor>`
    /// for `action-start`/`stop`/`poll`, which requires this manager to
    /// already be wrapped in an `Arc` — the same reason [`Self::self_ref`]
    /// itself is a `OnceLock` rather than a plain field.
    plugin_registry: OnceLock<Arc<InternalActionRegistry>>,
    /// Abort handles for in-flight detached (`action-start`) tasks, keyed by
    /// `invocation_id`. A field, not a process global — unlike the
    /// loopback's registry (see `solx_console::loopback`'s doc on why *that* one
    /// has to be a `OnceCell`), nothing here needs to survive across
    /// `LocalActionManager` instances, so keeping it per-manager is simpler
    /// and keeps tests isolated from one another.
    running: Arc<Mutex<HashMap<String, AbortHandle>>>,
    /// Set once, right after construction, to this manager's own
    /// `Arc<LocalActionManager>` — needed so WASM guests can recursively
    /// call back into `action-exec` (see `wasm`). `&self` methods
    /// can't hand out `Arc<Self>` on their own, hence the `OnceLock`.
    ///
    /// Concrete rather than `Weak<dyn ActionManager>` so the recursive hop
    /// can reach [`Self::exec_as`], which carries the calling action's
    /// identity. The trait's `exec` has no room for it, and deliberately
    /// so — see [`crate::caller`].
    self_ref: OnceLock<Weak<LocalActionManager>>,
}

#[async_trait]
impl ActionExecutor for LocalActionManager {
    async fn start_invocation(&self, path: &str, name: &str, params: Value) -> Result<Value> {
        LocalActionManager::start_invocation(self, path, name, params).await
    }

    async fn stop_invocation(&self, invocation_id: &str, force: bool, grace_secs: Option<u64>) -> Result<Value> {
        LocalActionManager::stop_invocation(self, invocation_id, force, grace_secs).await
    }

    async fn poll_invocation(&self, invocation_id: &str, wait_secs: Option<u64>) -> Result<Value> {
        LocalActionManager::poll_invocation(self, invocation_id, wait_secs).await
    }
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
        // `actions_fts` is `content='actions'` (external content), so a bare
        // `SELECT ... FROM actions_fts` doesn't read the FTS index — it
        // passes straight through to `actions`. An earlier version of this
        // backfill did `INSERT ... SELECT ... WHERE rowid NOT IN (SELECT
        // rowid FROM actions_fts)`, which for exactly that reason always
        // compared `actions.rowid` against itself and silently indexed
        // nothing for a DB that had rows before this table existed — new
        // rows still indexed fine via the triggers below, which give
        // `actions_fts` real column values directly. The fix, and the
        // documented way to populate an external-content FTS5 index from
        // existing data, is `INSERT INTO actions_fts(actions_fts) VALUES
        // ('rebuild')` — run once, only the first time the table is
        // created (mirroring `solx-docs`'s `created_fresh` reindex), so a
        // normal restart doesn't pay for a full rebuild on every start.
        let fts_existed_before: bool = {
            let mut rows = conn
                .query(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name='actions_fts'",
                    (),
                )
                .await
                .map_err(map_db)?;
            rows.next().await.map_err(map_db)?.is_some()
        };
        conn.execute_batch(FTS_DDL).await.map_err(map_db)?;
        if !fts_existed_before {
            conn.execute("INSERT INTO actions_fts(actions_fts) VALUES('rebuild')", ())
                .await
                .map_err(map_db)?;
        }
        // Console/invocation entries are contributed by `solx-console` as
        // plain data here — seeding them doesn't need that crate's stores
        // or an `Arc<Self>` (unlike the full plugin registry, built later
        // in `set_self_ref` once one exists).
        seed::seed_builtins(&conn, &solx_console::actions::seed_actions()).await?;

        // Separate physical database file from `actions` (no DB-level FKs
        // link them, only the app-level `action_ref` string) — see
        // `solx-config::ConfigService::console_db_path`.
        let (console, invocations) = solx_console::open(config.clone()).await?;
        // Best-effort: a sweep failure shouldn't block startup.
        if let Err(e) = console.sweep_expired().await {
            tracing::warn!("console TTL sweep failed: {e}");
        }
        // A row still `running`/`cancelling` from before this process last
        // exited (crash, kill, or an ungraceful restart) has no task behind
        // it anymore — flip it before anything can `poll` a status that
        // will never change on its own.
        if let Err(e) = invocations.mark_orphans().await {
            tracing::warn!("marking orphaned invocations failed: {e}");
        }
        if let Err(e) = invocations.sweep_expired().await {
            tracing::warn!("invocation TTL sweep failed: {e}");
        }

        Ok(LocalActionManager {
            db,
            config,
            types,
            docs,
            files,
            console,
            invocations,
            plugin_registry: OnceLock::new(),
            running: Arc::new(Mutex::new(HashMap::new())),
            self_ref: OnceLock::new(),
        })
    }

    /// Shared handle to this manager's console store — used by `wasm::host`
    /// to redirect a WASM guest's `logger.log` calls without going through
    /// a synthetic `action-exec` round trip.
    pub fn console(&self) -> &Arc<ConsoleStore> {
        &self.console
    }

    /// Shared handle to this manager's invocation-state store — see
    /// `solx_console::invocations`.
    pub fn invocations(&self) -> &Arc<InvocationStore> {
        &self.invocations
    }

    /// This manager's merged internal-action plugin registry (currently
    /// just `solx-console`'s), building it on first access. Empty (not an
    /// error) if [`Self::set_self_ref`] hasn't been called yet — every real
    /// wiring path calls it immediately after construction, so this only
    /// matters for a test harness that skips it, in which case
    /// `console-print`/`action-start`/etc. simply report "unknown internal
    /// fn_name" rather than panicking.
    pub(crate) fn plugin_registry(&self) -> Arc<InternalActionRegistry> {
        self.plugin_registry.get().cloned().unwrap_or_default()
    }

    /// Provide this manager's own handle for recursive WASM `action-exec`
    /// calls, and build [`Self::plugin_registry`] (which needs the same
    /// `Arc<Self>` to hand `solx-console` an `Arc<dyn ActionExecutor>`).
    /// Must be called exactly once, right after the manager is wrapped in
    /// an `Arc` (e.g. `let m = Arc::new(LocalActionManager::open(...).await?);
    /// m.set_self_ref(Arc::downgrade(&m));`). A `Weak` is used (not a
    /// strong `Arc`) so the manager doesn't hold a reference cycle to
    /// itself.
    pub fn set_self_ref(&self, self_ref: Weak<LocalActionManager>) {
        if let Some(strong) = self_ref.upgrade() {
            let executor: Arc<dyn ActionExecutor> = strong;
            let plugin = solx_console::actions::plugin(self.console.clone(), self.invocations.clone(), executor);
            let _ = self.plugin_registry.set(Arc::new(plugin));
        }
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
    /// `run_webhook` reads `auth`/`headers`, and `crate::auth` resolves
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
    /// `wasm::exec`, which moves them onto the blocking pool to
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

pub(crate) fn opt(s: String) -> Option<String> {
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

/// Columns this store exposes to `ListOptions`. `capabilities` and `phrases`
/// are JSON arrays, so filtering them is a substring match over that text —
/// enough to find "every action tagged mcp" without a join table.
///
/// `action_config` is deliberately absent: it can hold secrets (see
/// [`mask`]), and a LIKE filter over it would leak their contents by
/// letting a caller probe for substrings.
/// How much to over-fetch when `exclude_hidden` forces post-query
/// filtering, so a page that loses a few rows is usually still full.
const HIDDEN_OVERFETCH: usize = 4;

/// Ceiling on that inflated fetch, so a large `limit` cannot turn into an
/// unbounded scan.
const HIDDEN_FETCH_CAP: usize = 1000;

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
    /// of its `?N` placeholders need renumbering). Joins to `actions_fts`
    /// through a `rowid, rank`-only subquery rather than directly, so the
    /// FTS table's own `path` column can't collide with `q.where_clause`'s
    /// unqualified one. With `query.q` absent this runs the exact same query
    /// plan as `list` — no join, no ranking.
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

impl LocalActionManager {
    /// Execute an action, optionally attributed to the action that invoked
    /// it. Equivalent to [`Self::exec_as_with`] with `invocation_id: None` —
    /// i.e. mint a fresh one at dispatch time, the behavior every existing
    /// caller of this method already gets.
    ///
    /// `caller` is `Some` only on the recursive hop: a WASM guest calling
    /// `action-exec` (see [`crate::wasm`]), which is the sole
    /// re-entrant path into execution. It scopes `get-secret`/`set-secret`
    /// to the *calling* action's own keys.
    pub async fn exec_as(
        &self,
        path: &str,
        name: &str,
        params: Value,
        caller: Option<&Caller>,
    ) -> Result<ActionExecResult> {
        self.exec_as_with(path, name, params, caller, None).await
    }

    /// Like [`Self::exec_as`], but lets the caller fix the `invocation_id`
    /// up front instead of letting one be minted at dispatch time.
    ///
    /// Only [`Self::start_invocation`] ever passes `Some` — a detached run
    /// needs the id known *before* execution begins, so `action-stop`/
    /// `action-poll` have something to address before the run finishes (or
    /// even starts). `invocation_id.is_some()` doubles as "this is a
    /// detached run" for the timeout default below; every other caller goes
    /// through [`Self::exec_as`], which always passes `None`.
    ///
    /// **Why a manually boxed future, not `async fn`:** this method's
    /// `Internal` arm can dispatch to `action-start`, whose handler calls
    /// [`Self::start_invocation`], which `tokio::spawn`s a task that calls
    /// back into *this very method*. An `async fn`'s return type is an
    /// anonymous, structurally-inferred generator — with that indirect
    /// self-reference in the call graph, the compiler cannot resolve
    /// whether the type is `Send` (it would need to already know the answer
    /// to answer the question) and rejects it outright. Returning an
    /// explicit `Pin<Box<dyn Future + Send>>` gives the method a *nominal*
    /// type with a `Send` bound checked once against its own body, which
    /// every recursive call site (including the one inside
    /// `start_invocation`'s spawned task) can then simply trust — the same
    /// trick `#[async_trait]` uses under the hood for object-safe trait
    /// methods, applied by hand here since this isn't a trait method.
    pub fn exec_as_with<'a>(
        &'a self,
        path: &'a str,
        name: &'a str,
        params: Value,
        caller: Option<&'a Caller>,
        invocation_id: Option<&'a str>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ActionExecResult>> + Send + 'a>> {
        Box::pin(self.exec_as_with_inner(path, name, params, caller, invocation_id))
    }

    async fn exec_as_with_inner(
        &self,
        path: &str,
        name: &str,
        params: Value,
        caller: Option<&Caller>,
        invocation_id: Option<&str>,
    ) -> Result<ActionExecResult> {
        let action = self.get_unmasked(path, name).await?;
        let action_ref = full_ref(&action.path, &action.name)?;

        // Internal built-ins mix two wire-key conventions (camelCase
        // entity/search structs, snake_case raw-key handlers) — see
        // `internal::normalize_params`. Must run *before* validation, not
        // just before dispatch: several builtin schemas in
        // `solx-types/src/seed.rs` declare a required property under only
        // one spelling (`FilePutParams`' `rel_path`, `OauthAwaitParams`'
        // `state_value`, ...), so a caller guessing the other spelling would
        // fail validation here and never reach `run_internal` at all.
        // Scoped to `Internal` only — Command/Webhook/Script/Wasm actions are
        // user-authored external contracts (a webhook body, a command argv)
        // that must never get synthesized alias keys injected into them.
        let params = if action.action_type == Some(ActionType::Internal) {
            internal::normalize_params(&params)
        } else {
            params
        };

        // Validate params against the declared parameter type, if any.
        if let Some(tr) = &action.param_type_ref {
            self.types.validate(&params, tr).await?;
        }

        let detached = invocation_id.is_some();
        let minted;
        let invocation_id: &str = match invocation_id {
            Some(id) => id,
            None => {
                minted = Uuid::new_v4().to_string();
                &minted
            }
        };
        // A detached run should not silently inherit the 300s sync default
        // — it is expected to outlive any one caller. `action_config`'s own
        // `timeout_secs` still wins either way.
        let effective_timeout = if detached {
            Some(timeout_secs(&action.action_config).unwrap_or_else(|| self.config.background_timeout_secs()))
        } else {
            timeout_secs(&action.action_config)
        };

        let result = match action.action_type {
            Some(ActionType::Command) => {
                let fn_name = action.fn_name.as_deref().ok_or_else(|| {
                    SolxError::Exec("command action has no fn_name (the command to run)".into())
                })?;
                exec::run_command(
                    &self.config,
                    self.console.clone(),
                    self.invocations.clone(),
                    &action_ref,
                    invocation_id,
                    fn_name,
                    &action.action_config,
                    &params,
                    effective_timeout,
                )
                .await?
            }
            Some(ActionType::Webhook) => {
                let url = action.fn_name.as_deref().ok_or_else(|| {
                    SolxError::Exec("webhook action has no fn_name (URL)".into())
                })?;
                exec::run_webhook(
                    &self.config,
                    self,
                    self.console.clone(),
                    &action_ref,
                    invocation_id,
                    &action.path,
                    &action.name,
                    url,
                    &action.action_config,
                    &params,
                )
                .await?
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
                    config: self.config.clone(),
                    local: self.self_arc()?,
                    registry: self.plugin_registry(),
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
                // `with_invocation` rather than `from_action` so a detached
                // run's guest is stamped with the id `start_invocation`
                // already committed to the `invocations` row — for a plain
                // `exec_as` call this is exactly the freshly-minted id above,
                // so behavior is unchanged from before this method existed.
                let frame = Caller::with_invocation(&action_ref, action.action_config.as_ref(), invocation_id);
                // WASM execution reports its own success/message (a guest can
                // report a handled failure without erroring the host call), so
                // it returns a full ActionExecResult directly rather than
                // going through the common Value-wrapping below.
                return wasm::exec(
                    self.self_arc()?,
                    self.files.clone(),
                    bytes,
                    action.fn_name.as_deref(),
                    &params,
                    frame,
                    effective_timeout,
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
                let frame = Caller::with_invocation(&action_ref, action.action_config.as_ref(), invocation_id);
                let runner = script::ActionCommandRunner {
                    actions: self.self_arc()?,
                    caller: frame,
                };
                let initial = std::collections::HashMap::from([("params".to_string(), params.clone())]);
                let run = solx_scripts::execute_script_with_vars(&runner, &source, initial);
                let budget = std::time::Duration::from_secs(
                    effective_timeout.unwrap_or(exec::DEFAULT_TIMEOUT_SECS),
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

    /// Start `path/name` detached: returns as soon as an `invocations` row
    /// exists, while the actual execution runs on its own `tokio::spawn`ed
    /// task. See `docs/async-actions-plan.md` §4.
    ///
    /// Refuses unless this process has called [`set_long_lived_host`] — a
    /// spawned task is not what keeps a process alive, so under a CLI
    /// process (which exits the instant `exec` returns) a "successfully
    /// started" invocation would be killed before it could ever be polled.
    pub async fn start_invocation(&self, path: &str, name: &str, params: Value) -> Result<Value> {
        if !is_long_lived_host() {
            return Err(SolxError::Exec(
                "action-start requires a long-lived host (solx-server or solx-mcp). \
                 This process exits when exec returns, which would kill the invocation \
                 before it could be polled. Point the CLI at a running server, or use exec."
                    .into(),
            ));
        }

        // Resolved (and validated) up front, outside the spawned task, so a
        // bad path/name/action_type fails the `start` call itself rather
        // than silently producing a row that immediately goes `failed`.
        let action = self.get_unmasked(path, name).await?;
        let action_ref = full_ref(&action.path, &action.name)?;
        if action.action_type.is_none() {
            return Err(SolxError::Exec(format!(
                "action {action_ref} has no action_type to execute"
            )));
        }

        let invocation_id = Uuid::new_v4().to_string();
        // Captured before the row exists, so a poller can jump straight to
        // this run's own console output — see `ConsoleStore::current_next_seq`.
        let console_seq_start = self.console.current_next_seq(&action_ref).await?;
        self.invocations.create(&invocation_id, &action_ref, console_seq_start).await?;

        let manager = self.self_arc()?;
        let path = path.to_string();
        let name = name.to_string();
        let task_id = invocation_id.clone();
        let handle = tokio::spawn(async move {
            let outcome = manager.exec_as_with(&path, &name, params, None, Some(&task_id)).await;
            let (status, result, error) = match outcome {
                Ok(r) if r.success => (invocations::status::OK, Some(r.result), None),
                Ok(r) => (invocations::status::FAILED, Some(r.result), r.message),
                Err(e) => (invocations::status::FAILED, None, Some(e.to_string())),
            };
            // Best-effort: if this write fails there is nothing left to do
            // with the error — the task is finished either way.
            let _ = manager.invocations.finish(&task_id, status, result, error).await;
            if let Ok(mut running) = manager.running.lock() {
                running.remove(&task_id);
            }
        });
        if let Ok(mut running) = self.running.lock() {
            running.insert(invocation_id.clone(), handle.abort_handle());
        }

        Ok(json!({
            "invocation_id": invocation_id,
            "action_ref": action_ref,
            "console_seq_start": console_seq_start,
        }))
    }

    /// Request that a detached invocation stop. Cooperative first: sets the
    /// flag a running Command child (via the loopback `/cancelled` route)
    /// or a running Wasm/Script/Internal caller (via `action-cancelled`)
    /// can observe and exit on its own. If it hasn't gone terminal within
    /// `grace_secs` (default `stop_grace_secs`), the task is force-aborted —
    /// dropping the future, which reaps a Command child via the existing
    /// `kill_on_drop(true)` and unwinds a Wasm guest's fiber.
    ///
    /// Returns immediately; does **not** block for the grace period unless
    /// `force` is set.
    ///
    /// Caveats:
    /// - Carried over unchanged from `exec::run_command`: force-abort kills
    ///   the *shell* a Command action spawned, not its descendants.
    /// - **Force-abort is process-local.** `cancel_requested` lives in the
    ///   shared `invocations` table, so the cooperative signal reaches the
    ///   running task regardless of which process calls `stop`. The
    ///   `AbortHandle` in [`Self::running`], though, only exists in the
    ///   memory of whichever process's [`Self::start_invocation`] actually
    ///   spawned the task. Calling `stop` on a *different* process's
    ///   `LocalActionManager` (e.g. a CLI run locally against the same
    ///   appdata dir a `solx-server` is also using, rather than proxied to
    ///   it via `server_url`) can mark the row `cancelled` in the database
    ///   without ever reaching the real task. The documented usage pattern
    ///   — a CLI pointed at `solx-server` via `server_url`, so `stop`/`poll`
    ///   execute inside the same process that ran `start` — doesn't hit
    ///   this; two independent `LocalActionManager`s sharing one database
    ///   file do.
    ///
    /// Cooperative exit is the reliable path; force is the backstop, not a
    /// guarantee.
    pub async fn stop_invocation(&self, invocation_id: &str, force: bool, grace_secs: Option<u64>) -> Result<Value> {
        let Some(inv) = self.invocations.request_cancel(invocation_id).await? else {
            return Err(SolxError::NotFound(format!("invocation {invocation_id}")));
        };
        if invocations::is_terminal(&inv.status) {
            return Ok(inv.to_json());
        }

        if force {
            self.force_abort(invocation_id).await?;
            let inv = self
                .invocations
                .get(invocation_id)
                .await?
                .ok_or_else(|| SolxError::NotFound(format!("invocation {invocation_id}")))?;
            return Ok(inv.to_json());
        }

        let grace = Duration::from_secs(grace_secs.unwrap_or_else(|| self.config.stop_grace_secs()));
        let manager = self.self_arc()?;
        let id = invocation_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            let _ = manager.force_abort(&id).await;
        });

        Ok(inv.to_json())
    }

    /// Abort the task behind `invocation_id` if it is still running, and
    /// mark the row `cancelled`. A no-op (not an error) if the task already
    /// finished on its own — [`invocations::InvocationStore::mark_aborted`]
    /// guards against exactly that race.
    async fn force_abort(&self, invocation_id: &str) -> Result<()> {
        if let Ok(mut running) = self.running.lock() {
            if let Some(handle) = running.remove(invocation_id) {
                handle.abort();
            }
        }
        self.invocations.mark_aborted(invocation_id).await
    }

    /// Current status of a detached invocation, optionally long-polling
    /// (reusing `console`'s 250ms/60s long-poll cadence) until it goes
    /// terminal.
    pub async fn poll_invocation(&self, invocation_id: &str, wait_secs: Option<u64>) -> Result<Value> {
        let wait = wait_secs.map(|s| Duration::from_secs(s.min(console::MAX_TAIL_WAIT_SECS)));
        let deadline = wait.map(|w| tokio::time::Instant::now() + w);

        loop {
            let inv = self
                .invocations
                .get(invocation_id)
                .await?
                .ok_or_else(|| SolxError::NotFound(format!("invocation {invocation_id}")))?;
            if invocations::is_terminal(&inv.status) {
                return Ok(inv.to_json());
            }
            match deadline {
                Some(d) => {
                    let now = tokio::time::Instant::now();
                    if now >= d {
                        return Ok(inv.to_json());
                    }
                    tokio::time::sleep(console::TAIL_POLL_INTERVAL.min(d - now)).await;
                }
                None => return Ok(inv.to_json()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solx_docs::LocalDocManager;
    use solx_files::LocalFileStore;
    use solx_types::LocalTypeManager;

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
        let cfg = Arc::new(ConfigService::open_in(dir.path()).unwrap());
        let types: Arc<dyn TypeManager> =
            Arc::new(LocalTypeManager::open(&dir.path().join("types.db")).await.unwrap());
        let docs: Arc<dyn DocManager> = Arc::new(
            LocalDocManager::open(&dir.path().join("docs.db"), types.clone())
                .await
                .unwrap(),
        );
        let files: Arc<dyn FileStore> = Arc::new(LocalFileStore::new(dir.path().join("files")));
        let m = LocalActionManager::open(&actions_db_path, cfg, types, docs, files)
            .await
            .unwrap();

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

    async fn setup() -> (tempfile::TempDir, Arc<ConfigService>, LocalActionManager) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Arc::new(ConfigService::open_in(dir.path()).unwrap());
        let types: Arc<dyn TypeManager> = Arc::new(
            LocalTypeManager::open(&dir.path().join("types.db"))
                .await
                .unwrap(),
        );
        let docs: Arc<dyn DocManager> = Arc::new(
            LocalDocManager::open(&dir.path().join("docs.db"), types.clone())
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
    async fn setup_wired() -> (tempfile::TempDir, Arc<ConfigService>, Arc<LocalActionManager>) {
        let (dir, cfg, m) = setup().await;
        let m = Arc::new(m);
        m.set_self_ref(Arc::downgrade(&m));
        (dir, cfg, m)
    }

    /// Registers `command` under `command_actions` keyed by `key`, then
    /// saves a Command action whose `fn_name` is that key. `fn_name` is
    /// never a literal shell string once the allowlist is in effect — see
    /// `docs/next-steps.md` §1.
    fn allow_command(cfg: &ConfigService, key: &str, command: &str) {
        cfg.register_command(
            key,
            solx_config::CommandDef { command: command.into(), description: None, cwd: None },
        )
        .unwrap();
    }

    /// Appends `prefix` to the `allowed_base_urls` allowlist.
    fn allow_webhook_prefix(cfg: &ConfigService, prefix: &str) {
        cfg.add_allowed_base_url(prefix).unwrap();
    }

    fn cfg_with_secret(key: &str) -> Value {
        serde_json::json!({ "cwd": "/work", "secrets": { "API_TOKEN": key } })
    }

    /// Proves the aliasing chokepoint lives ahead of param-type validation,
    /// not just ahead of dispatch: `FilePutParams` requires `rel_path`
    /// (`solx-types/src/seed.rs`), so a `run_internal`-only fix would have
    /// rejected this call at `self.types.validate` before ever reaching the
    /// handler. Going through `exec` end-to-end (not `run_internal`
    /// directly) is what exercises that ordering.
    #[tokio::test]
    async fn exec_accepts_camel_case_for_a_required_snake_case_builtin_param() {
        let (_d, _c, m) = setup_wired().await;
        let result = m
            .exec("/builtin/file", "file-put", serde_json::json!({"relPath": "a.txt", "content": "hi"}))
            .await
            .unwrap();
        assert_eq!(result.result.get("rel_path").and_then(Value::as_str), Some("a.txt"));
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

    #[tokio::test]
    async fn exec_command_resolves_fn_name_through_the_allowlist() {
        let (_d, c, m) = setup().await;
        // fn_name is a key into `command_actions`, never a literal command —
        // register it first. Echo a bare number so the output is valid JSON
        // on both cmd.exe and sh without quote handling.
        allow_command(&c, "echo-42", "echo 42");
        let input = ActionInput {
            action_type: Some(ActionType::Command),
            fn_name: Some("echo-42".into()),
            ..Default::default()
        };
        m.save("/tools", "echo", input).await.unwrap();

        let res = m
            .exec("/tools", "echo", serde_json::json!({}))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.result, serde_json::json!(42));
    }

    #[tokio::test]
    async fn exec_command_with_unregistered_key_is_denied() {
        let (_d, _c, m) = setup().await;
        // Deny-by-default: an empty/absent allowlist rejects every key,
        // including one that happens to look like a real shell command.
        let input = ActionInput {
            action_type: Some(ActionType::Command),
            fn_name: Some("echo 42".into()),
            ..Default::default()
        };
        m.save("/tools", "echo", input).await.unwrap();

        let err = m.exec("/tools", "echo", serde_json::json!({})).await.unwrap_err();
        assert!(err.to_string().contains("command_actions"), "{err}");
    }

    /// Minimal single-shot HTTP server for the webhook logging tests below —
    /// always replies 200 with a tiny JSON body, regardless of what was sent.
    async fn start_ok_server() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else { break };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 8192];
                    let _ = stream.read(&mut buf).await;
                    let body = r#"{"ok":true}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body,
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn exec_webhook_logs_start_and_success_to_the_console() {
        let (_d, c, m) = setup().await;
        let (url, server) = start_ok_server().await;
        allow_webhook_prefix(&c, "http://127.0.0.1");

        m.save(
            "/tools",
            "hook",
            ActionInput {
                action_type: Some(ActionType::Webhook),
                fn_name: Some(url),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let res = m.exec("/tools", "hook", serde_json::json!({"x": 1})).await.unwrap();
        assert!(res.success);

        let entries = m.console().read("/tools/hook", None, 10).await.unwrap().entries;
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert_eq!(entries[0].source, "webhook");
        assert!(entries[0].message.as_deref().unwrap().starts_with("POST http"));
        assert!(entries[1].message.as_deref().unwrap().contains("succeeded"));
        // Params must never be logged — they can carry secrets a nested
        // reader of the console has no business seeing.
        assert!(!entries.iter().any(|e| e.message.as_deref().unwrap_or("").contains("\"x\"")));

        server.abort();
    }

    #[tokio::test]
    async fn exec_webhook_logs_failure_to_the_console() {
        let (_d, c, m) = setup().await;
        allow_webhook_prefix(&c, "http://127.0.0.1");
        m.save(
            "/tools",
            "deadhook",
            ActionInput {
                action_type: Some(ActionType::Webhook),
                // Nothing listens here — a fast, deterministic transport failure.
                fn_name: Some("http://127.0.0.1:1".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let err = m.exec("/tools", "deadhook", serde_json::json!({})).await;
        assert!(err.is_err());

        let entries = m.console().read("/tools/deadhook", None, 10).await.unwrap().entries;
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert_eq!(entries[1].level, "warn");
        assert!(entries[1].message.as_deref().unwrap().contains("failed"));
    }

    #[tokio::test]
    async fn exec_webhook_with_unlisted_url_is_denied_before_any_request() {
        let (_d, _c, m) = setup().await;
        // Deny-by-default, and no console entries at all — the allowlist
        // check happens before the "POST ..." start log is written.
        m.save(
            "/tools",
            "hook",
            ActionInput {
                action_type: Some(ActionType::Webhook),
                fn_name: Some("http://127.0.0.1:1".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let err = m.exec("/tools", "hook", serde_json::json!({})).await.unwrap_err();
        assert!(err.to_string().contains("allowed_base_urls"), "{err}");

        let entries = m.console().read("/tools/hook", None, 10).await.unwrap().entries;
        assert!(entries.is_empty(), "{entries:?}");
    }

    // ── entity-save-action: no self-granting shell ───────────────────────
    //
    // These go through `exec` on `/builtin/action/entity-save-action`, which is the
    // exact path an MCP tool call, a WASM guest's `action-exec`, and a
    // `.solx` script all take. A direct `m.save(...)` is the CLI's path and
    // stays allowed — that's the whole distinction being enforced.

    // Only ever used for `entity-save-action`/`entity-delete-action`, which
    // live under `ACTION_PATH` (action-entity CRUD, alongside the async
    // start/stop/poll/cancelled actions), not the flat `/builtin` root.
    async fn exec_builtin(m: &Arc<LocalActionManager>, fn_name: &str, params: Value) -> Result<Value> {
        m.exec(seed::ACTION_PATH, fn_name, params).await.map(|r| r.result)
    }

    #[tokio::test]
    async fn entity_save_action_refuses_to_create_command_or_webhook() {
        let (_d, _cfg, m) = setup_wired().await;

        for ty in ["command", "webhook"] {
            let err = exec_builtin(
                &m,
                "entity-save-action",
                serde_json::json!({
                    "path": "/evil", "name": "shell",
                    "actionType": ty, "fnName": "rm -rf /"
                }),
            )
            .await
            .unwrap_err();
            assert!(err.to_string().contains("use the solx CLI"), "{ty}: {err}");
            assert!(m.get_unmasked("/evil", "shell").await.is_err(), "{ty} was created anyway");
        }
    }

    /// `save` is a merge-upsert, so a payload with no `action_type` at all
    /// would otherwise silently rewrite an existing Command action's shell
    /// command. The guard has to consult the stored row, not just the input.
    #[tokio::test]
    async fn entity_save_action_refuses_to_repoint_an_existing_command() {
        let (_d, _cfg, m) = setup_wired().await;
        m.save(
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
            "entity-save-action",
            serde_json::json!({ "path": "/tools", "name": "safe", "fnName": "rm -rf /" }),
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
        let (_d, _cfg, m) = setup_wired().await;
        m.save(
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
            "entity-delete-action",
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
    async fn entity_save_action_still_allows_non_executable_types() {
        let (_d, _cfg, m) = setup_wired().await;
        exec_builtin(
            &m,
            "entity-save-action",
            serde_json::json!({
                "path": "/tools", "name": "w",
                "actionType": "wasm", "binName": "x.wasm"
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
        m.save("/tools", "w", input).await.unwrap();
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
        m.save("/tools", "w", input).await.unwrap();
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
            LocalDocManager::open(&dir.path().join("docs.db"), types.clone())
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
        // The bare `exec /builtin/action/entity-list-actions` (no `json`
        // wrapping needed) becomes the script's result directly: the callee's
        // whole ActionExecResult, JSON-encoded — same as `handle_exec` in the
        // CLI. `json`'s argument is parsed as JSON, and `tokenize_stage`
        // strips one layer of quoting to form the token — so a JSON string
        // literal needs the outer '...' shell-style quoting plus inner \"...\"
        // JSON quotes, same as the CLI's `json` command (`solx script -e 'json
        // \'"big"\''`).
        post_script_artifact(
            &files,
            "hello.solx",
            "if $params.go == true; exec /builtin/action/entity-list-actions; else; json '\"skipped\"'; endif",
        )
        .await;
        m.save(
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
        // The nested action's result is a list page (an object).
        assert!(
            ran.result["result"].is_object(),
            "expected a list-page object, got: {:?}",
            ran.result
        );

        let skipped = m.exec("/tools", "hello", serde_json::json!({"go": false})).await.unwrap();
        assert_eq!(skipped.result, serde_json::json!("skipped"));
    }

    #[tokio::test]
    async fn exec_script_logs_each_stage_to_the_console() {
        let (_d, m, files) = setup_script().await;
        post_script_artifact(
            &files,
            "count.solx",
            "exec /builtin/action/entity-list-actions; exec /builtin/document/entity-list-documents",
        )
        .await;
        m.save(
            "/tools",
            "count",
            ActionInput {
                action_type: Some(ActionType::Script),
                bin_name: Some("count.solx".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        m.exec("/tools", "count", serde_json::json!({})).await.unwrap();

        let entries = m.console().read("/tools/count", None, 10).await.unwrap().entries;
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert_eq!(entries[0].source, "script");
        assert_eq!(entries[0].message.as_deref(), Some("exec /builtin/action/entity-list-actions"));
        assert_eq!(entries[1].message.as_deref(), Some("exec /builtin/document/entity-list-documents"));
        // Both stages share the one script invocation's identity.
        assert_eq!(entries[0].invocation_id, entries[1].invocation_id);
    }

    #[tokio::test]
    async fn exec_script_logs_a_stage_failure() {
        let (_d, m, files) = setup_script().await;
        post_script_artifact(&files, "bad.solx", "exec /builtin/nope").await;
        m.save(
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

        let err = m.exec("/tools", "bad", serde_json::json!({})).await;
        assert!(err.is_err());

        let entries = m.console().read("/tools/bad", None, 10).await.unwrap().entries;
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert_eq!(entries[1].level, "warn");
        assert!(entries[1].message.as_deref().unwrap().contains("failed"));
    }

    #[tokio::test]
    async fn exec_script_missing_bin_name_errors() {
        let (_d, _c, m) = setup().await;
        let input = ActionInput {
            action_type: Some(ActionType::Script),
            ..Default::default()
        };
        m.save("/tools", "s", input).await.unwrap();
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
        m.save("/tools", "s", input).await.unwrap();
        let err = m.exec("/tools", "s", serde_json::json!({})).await.unwrap_err();
        assert!(err.to_string().contains("missing.solx"), "{err}");
    }

    #[tokio::test]
    async fn exec_script_unsupported_stage_verb_errors() {
        let (_d, m, files) = setup_script().await;
        post_script_artifact(&files, "bad.solx", "post doc /a --json '{}'").await;
        m.save(
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
        // guarded `entity-save-action` built-in the same way `wasm` can.
        exec_builtin(
            &m,
            "entity-save-action",
            serde_json::json!({
                "path": "/tools", "name": "s",
                "actionType": "script", "binName": "x.solx"
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
        m.save(
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
            .find(|a| a.name == "entity-get-document")
            .expect("entity-get-document should be seeded");
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

    // ── async actions: start/stop/poll ───────────────────────────────────────
    //
    // `#[serial]` on every test that touches `set_long_lived_host` —
    // `LONG_LIVED_HOST` is a process-wide static, and tests in this file run
    // in parallel threads within one process by default (`serial_test` is
    // already a dev-dependency for exactly this reason elsewhere).

    use serial_test::serial;

    /// Echoes a bare number immediately — valid JSON on both cmd.exe and sh
    /// without any quote-handling differences between the two, mirroring
    /// `exec_command_runs_fn_name_directly` above.
    fn quick_ok() -> &'static str {
        "echo 42"
    }

    /// A shell snippet with no natural end — internal to the shell (a `for`/
    /// `while` loop), not an external process, for the same reason
    /// `tests/command_nonblocking.rs`'s `a_hanging_command_hits_its_timeout`
    /// avoids one: `kill_on_drop` only reaps the shell we spawned, not an
    /// external descendant, which would otherwise leak and hold the test
    /// harness's pipes open.
    fn hangs_forever() -> &'static str {
        if cfg!(windows) {
            "for /L %i in (1,1,2000000000) do @rem"
        } else {
            "while :; do :; done"
        }
    }

    async fn save_command(m: &LocalActionManager, cfg: &ConfigService, name: &str, cmd: &str) {
        allow_command(cfg, name, cmd);
        m.save(
            "/t",
            name,
            ActionInput {
                action_type: Some(ActionType::Command),
                fn_name: Some(name.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn start_invocation_refuses_without_a_long_lived_host() {
        set_long_lived_host(false);
        let (_d, cfg, m) = setup_wired().await;
        save_command(&m, &cfg, "quick", quick_ok()).await;

        let err = m
            .start_invocation("/t", "quick", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("requires a long-lived host"), "{err}");
    }

    #[tokio::test]
    #[serial]
    async fn start_then_poll_reaches_ok_with_the_started_actions_result() {
        set_long_lived_host(true);
        let (_d, cfg, m) = setup_wired().await;
        save_command(&m, &cfg, "quick", quick_ok()).await;

        let started = m.start_invocation("/t", "quick", serde_json::json!({})).await.unwrap();
        let id = started["invocation_id"].as_str().unwrap().to_string();
        assert_eq!(started["action_ref"], "/t/quick");
        assert_eq!(started["console_seq_start"], 1);

        let polled = m.poll_invocation(&id, Some(10)).await.unwrap();
        assert_eq!(polled["status"], invocations::status::OK);
        assert_eq!(polled["result"], serde_json::json!(42));
        assert_eq!(polled["invocation_id"], id);
        assert!(polled["finished_at"].is_string());

        set_long_lived_host(false);
    }

    #[tokio::test]
    #[serial]
    async fn poll_invocation_of_an_unknown_id_is_not_found() {
        let (_d, _cfg, m) = setup_wired().await;
        let err = m.poll_invocation("no-such-id", None).await.unwrap_err();
        assert!(matches!(err, SolxError::NotFound(_)), "{err:?}");
    }

    #[tokio::test]
    #[serial]
    async fn stop_invocation_of_an_unknown_id_is_not_found() {
        let (_d, _cfg, m) = setup_wired().await;
        let err = m.stop_invocation("no-such-id", false, None).await.unwrap_err();
        assert!(matches!(err, SolxError::NotFound(_)), "{err:?}");
    }

    #[tokio::test]
    #[serial]
    async fn stop_force_aborts_a_hanging_command_promptly() {
        set_long_lived_host(true);
        let (_d, cfg, m) = setup_wired().await;
        save_command(&m, &cfg, "hangs", hangs_forever()).await;

        let started = m.start_invocation("/t", "hangs", serde_json::json!({})).await.unwrap();
        let id = started["invocation_id"].as_str().unwrap().to_string();

        // Give the child a moment to actually be spawned before stopping it.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let start = std::time::Instant::now();
        let stopped = m.stop_invocation(&id, true, None).await.unwrap();
        assert_eq!(stopped["status"], invocations::status::CANCELLED);
        // Force must not wait out the (effectively infinite) hang.
        assert!(start.elapsed() < Duration::from_secs(10), "{:?}", start.elapsed());

        let polled = m.poll_invocation(&id, None).await.unwrap();
        assert_eq!(polled["status"], invocations::status::CANCELLED);

        set_long_lived_host(false);
    }

    #[tokio::test]
    #[serial]
    async fn stop_cooperative_returns_cancelling_without_blocking_then_force_finishes_it() {
        set_long_lived_host(true);
        let (_d, cfg, m) = setup_wired().await;
        save_command(&m, &cfg, "hangs", hangs_forever()).await;

        let started = m.start_invocation("/t", "hangs", serde_json::json!({})).await.unwrap();
        let id = started["invocation_id"].as_str().unwrap().to_string();
        tokio::time::sleep(Duration::from_millis(200)).await;

        // A long grace period the test does not wait out — proves the
        // cooperative call itself does not block for it.
        let start = std::time::Instant::now();
        let stopped = m.stop_invocation(&id, false, Some(30)).await.unwrap();
        assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());
        assert_eq!(stopped["status"], invocations::status::CANCELLING);
        assert!(m.invocations().is_cancelled(&id).await.unwrap());

        // Clean up rather than let the 30s watcher fire on its own.
        let forced = m.stop_invocation(&id, true, None).await.unwrap();
        assert_eq!(forced["status"], invocations::status::CANCELLED);

        set_long_lived_host(false);
    }
}
