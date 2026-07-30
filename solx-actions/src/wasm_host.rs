//! Wasmtime component-model host for *custom* (third-party) WASM action
//! components.
//!
//! Only one WIT world exists now (`solx-wasm/wit/custom-action.wit`):
//! `custom-action`, importing `action-exec` (recursively invoke any other
//! action by full reference — including every built-in, which are all
//! native `Internal` actions now, see `crate::internal`), `artifact-read`
//! (read-only, unrestricted file-store access), and `logger`.
//!
//! The WASM-hosted `backend-action` world (trusted, direct
//! `document-ops`/`entity-ops`/`system-ops`/`secrets` host access) and its
//! `solx-builtin-actions` component were retired: everything they provided
//! is now a native `Internal` action, reachable from custom WASM guests the
//! same way anyone else reaches it — via `action-exec`. Every WASM action
//! is therefore equally sandboxed by construction now; `Action.trusted` no
//! longer affects WASM execution (kept on the entity as an inert legacy
//! field to avoid a DB migration).

use std::sync::{Arc, OnceLock};

use serde_json::Value;
use solx_surface::entities::ActionExecResult;
use solx_surface::error::{Result, SolxError};
use solx_surface::managers::{ActionManager, FileStore};
use solx_surface::path::split_ref;
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

// ── WIT bindings ─────────────────────────────────────────────────────────────

wasmtime::component::bindgen!({
    world: "custom-action",
    path: "../solx-wasm/wit",
});

// ── Engine singleton ─────────────────────────────────────────────────────────

static ENGINE: OnceLock<Engine> = OnceLock::new();

fn engine() -> &'static Engine {
    ENGINE.get_or_init(|| {
        let mut cfg = Config::new();
        cfg.wasm_component_model(true);
        Engine::new(&cfg).expect("failed to create wasmtime engine")
    })
}

// ── Host state ───────────────────────────────────────────────────────────────

struct HostState {
    actions: Arc<dyn ActionManager>,
    files: Arc<dyn FileStore>,
    /// Blocking-context handle for bridging sync WIT host functions to the
    /// async managers. Only ever constructed inside `exec_sync`, which
    /// itself always runs inside `spawn_blocking` — so blocking this
    /// thread on an async call is safe (it isn't an async worker thread).
    rt: tokio::runtime::Handle,
    wasi: WasiCtx,
    table: ResourceTable,
}

/// Bridge a sync WIT host function to an async manager call. Only valid
/// when called from a thread that isn't itself driving the tokio runtime's
/// async workers (see `HostState::rt`'s doc comment).
macro_rules! block_on {
    ($state:expr, $fut:expr) => {
        $state.rt.clone().block_on($fut)
    };
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

// ── Host trait impls ─────────────────────────────────────────────────────────

impl sol::actions::types::Host for HostState {}

impl sol::actions::logger::Host for HostState {
    fn log(&mut self, message: String) {
        tracing::info!(target: "solx_wasm_guest", "{message}");
    }
}

impl sol::actions::action_exec::Host for HostState {
    fn exec(
        &mut self,
        action_name: String,
        payload: String,
    ) -> std::result::Result<sol::actions::types::ActionResult, String> {
        let (path, name) = split_ref(&action_name).map_err(|e| e.to_string())?;
        let params: Value = serde_json::from_str(&payload).map_err(|e| format!("invalid params: {e}"))?;
        let result: ActionExecResult =
            block_on!(self, self.actions.exec(&path, &name, params)).map_err(|e| e.to_string())?;
        Ok(sol::actions::types::ActionResult {
            success: result.success,
            message: result.message,
            output: Some(result.result.to_string()),
        })
    }
}

impl sol::actions::artifact_read::Host for HostState {
    fn read(&mut self, name: String) -> std::result::Result<Vec<u8>, String> {
        block_on!(self, self.files.get(&name)).map_err(|e| e.to_string())
    }
}

// ── Public entry points ────────────────────────────────────────────────────--

/// Execute a custom WASM action component. `action_name` (the full
/// reference of the *calling* action) is used only to label the returned
/// `ActionExecResult.action`; `fn_name` is passed through to the guest's
/// `run` export.
pub async fn exec(
    actions: Arc<dyn ActionManager>,
    files: Arc<dyn FileStore>,
    wasm_bytes: &[u8],
    fn_name: Option<&str>,
    params: &Value,
    action_name: Option<&str>,
) -> Result<ActionExecResult> {
    let params_json = serde_json::to_string(params).map_err(|e| SolxError::Exec(e.to_string()))?;
    let bytes = wasm_bytes.to_vec();
    let fn_name_owned = fn_name.map(str::to_string);
    let action_name_owned = action_name.map(str::to_string);
    let rt = tokio::runtime::Handle::current();

    tokio::task::spawn_blocking(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            exec_sync(
                actions,
                files,
                &bytes,
                fn_name_owned.as_deref(),
                &params_json,
                action_name_owned.as_deref(),
                rt,
            )
        }))
        .unwrap_or_else(|e| {
            let msg = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown panic in WASM execution".to_string());
            Err(SolxError::Exec(format!(
                "panic in WASM execution (fn_name={fn_name_owned:?}): {msg}"
            )))
        })
    })
    .await
    .map_err(|e| SolxError::Exec(format!("wasmtime blocking task panicked: {e}")))?
}

fn exec_sync(
    actions: Arc<dyn ActionManager>,
    files: Arc<dyn FileStore>,
    wasm_bytes: &[u8],
    fn_name: Option<&str>,
    params_json: &str,
    action_name: Option<&str>,
    rt: tokio::runtime::Handle,
) -> Result<ActionExecResult> {
    let eng = engine();
    let component = Component::from_binary(eng, wasm_bytes)
        .map_err(|e| SolxError::Exec(format!("invalid WASM component: {e:#}")))?;

    let wasi = WasiCtxBuilder::new().build();
    let state = HostState { actions, files, rt, wasi, table: ResourceTable::new() };
    let mut store = Store::new(eng, state);

    let mut linker = Linker::<HostState>::new(eng);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
        .map_err(|e| SolxError::Exec(format!("failed to link WASI: {e:#}")))?;
    CustomAction::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s: &mut HostState| s)
        .map_err(|e| SolxError::Exec(format!("failed to link custom-action host: {e:#}")))?;
    let instance = CustomAction::instantiate(&mut store, &component, &linker)
        .map_err(|e| SolxError::Exec(format!("failed to instantiate component: {e:#}")))?;
    let (success, message, output) = match instance.sol_actions_runner().call_run(&mut store, fn_name, params_json) {
        Ok(Ok(ar)) => (ar.success, ar.message, ar.output),
        Ok(Err(msg)) => (false, Some(msg), None),
        Err(e) => {
            tracing::error!("wasm trap: {e:#}");
            (false, Some(format!("{e:#}")), None)
        }
    };

    let result = output
        .as_deref()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .unwrap_or(Value::Null);

    Ok(ActionExecResult {
        action: action_name.unwrap_or("(anonymous)").to_string(),
        result,
        success,
        message,
    })
}

// ── Tests ────────────────────────────────────────────────────────────────────
//
// The host trait impls are thin sync wrappers over `ActionManager`/
// `FileStore` calls that are already covered by `solx-actions`' own tests
// (`LocalActionManager::exec`, `LocalFileStore`); a full end-to-end run
// through a real compiled custom component is a manual verification step,
// not a unit test here (no compiled `.wasm` fixture is checked into this
// repo).
