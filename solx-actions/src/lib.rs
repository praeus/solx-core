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
mod dispatch;
mod exec;
pub mod internal;
mod invocation;
pub mod loopback;
mod mask;
pub mod net;
mod row;
mod schema;
pub mod script;
mod seed;
pub mod secrets;
mod store;
#[cfg(test)]
mod test_support;
pub mod wasm;

pub use invocation::set_long_lived_host;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use async_trait::async_trait;
use serde_json::Value;
use solx_config::ConfigService;
use solx_console::{ConsoleStore, InvocationStore};
use solx_surface::error::{Result, SolxError};
use solx_surface::internal_actions::{ActionExecutor, InternalActionRegistry};
use solx_surface::managers::{DocManager, FileStore, TypeManager};
use tokio::task::AbortHandle;

use db::Db;

/// libsql-backed [`ActionManager`](solx_surface::managers::ActionManager) with command/webhook/internal/WASM/script
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
    /// Built lazily in [`Self::set_self_ref`] rather than [`Self::open`] —
    /// see that method's doc for why.
    internal_registry: OnceLock<Arc<InternalActionRegistry>>,
    /// Abort handles for in-flight detached (`action-start`) tasks, keyed by
    /// `invocation_id`. A field, not a process global — unlike the
    /// loopback's registry (see `solx_console::loopback`'s doc on why *that* one
    /// has to be a `OnceCell`), nothing here needs to survive across
    /// `LocalActionManager` instances, so keeping it per-manager is simpler
    /// and keeps tests isolated from one another.
    running: Arc<Mutex<HashMap<String, AbortHandle>>>,
    /// This manager's own handle, set once right after construction (see
    /// [`Self::set_self_ref`]) so WASM guests can recursively call back into
    /// `action-exec` (see `wasm`). `&self` methods can't hand out
    /// `Arc<Self>` on their own, hence the `OnceLock`.
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
    /// Open the actions database, seed the built-in action catalogue, and
    /// prepare execution. `docs`/`types`/`files` are the sibling stores WASM
    /// host functions call into; call [`Self::set_self_ref`] once after
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
        schema::migrate(&conn).await?;
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
            internal_registry: OnceLock::new(),
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
    pub(crate) fn internal_registry(&self) -> Arc<InternalActionRegistry> {
        self.internal_registry.get().cloned().unwrap_or_default()
    }

    /// Provide this manager's own handle for recursive WASM `action-exec`
    /// calls, and build [`Self::internal_registry`] (which needs the same
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
            let _ = self.internal_registry.set(Arc::new(plugin));
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
}
