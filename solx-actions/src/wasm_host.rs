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
//!
//! ## Async execution — why nesting no longer costs threads
//!
//! Guests run on wasmtime *fibers*, not on blocking-pool threads. The
//! bindings are generated with `imports`/`exports: { default: async }`, so
//! the guest export is invoked through `TypedFunc::call_async`, which is
//! `store.on_fiber(..)`: the guest gets its own stack, and whenever a host
//! import's future returns `Pending` the fiber suspends and the OS thread
//! goes back to the executor.
//!
//! That is what makes `action-exec` recursion cheap. The previous design
//! ran each guest inside `spawn_blocking` and bridged host imports back to
//! the async managers with `Handle::block_on`, so **every nesting level
//! pinned one blocking-pool thread** for the whole duration of the call
//! beneath it — a hard ceiling at tokio's `max_blocking_threads` (512 by
//! default, and that budget is shared across concurrent requests, so real
//! exhaustion arrives at depth × concurrency). Past the limit
//! `spawn_blocking` queues rather than erroring, so it presented as a hang.
//! Now a suspended level holds a fiber stack and no thread at all.
//!
//! Note this needs nothing from the guest. `call_async` runs an ordinary
//! sync-ABI component; only the *host* side is async. Native component-model
//! async (the `call_concurrent` path, reached by adding the `store` flag to
//! the bindgen config) would require guests built against the async ABI —
//! deliberately not used here, so components produced by componentize-qjs's
//! `Runtime::OptSizeSync` keep working unmodified.
//!
//! Two things follow from dropping `spawn_blocking` and are handled below:
//! compiling a component is CPU-heavy and must not land on an async worker
//! (see [`compile_component`]), and a compute-bound guest never awaits, so
//! it needs epoch-based preemption to stop it monopolizing a worker (see
//! [`engine`]).
//!
//! ## Caller attribution
//!
//! This module is the *only* re-entrant path into execution — internal
//! actions do entity CRUD, never `exec`. So it is also the only place a
//! [`crate::caller::Caller`] is minted: `exec` builds one from the row it
//! was handed and stores it on [`HostState`], and every `action-exec` call
//! the guest makes goes through [`crate::LocalActionManager::exec_as`]
//! carrying it. That is what scopes `get_secret`/`set_secret` to the
//! guest's own keys. The frame is rebuilt per nesting level, so a guest
//! invoking another guest cannot reach the outer one's secrets.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures::FutureExt;
use serde_json::Value;
use solx_surface::entities::ActionExecResult;
use solx_surface::error::{Result, SolxError};
use solx_surface::managers::FileStore;
use solx_surface::path::split_ref;
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::caller::Caller;
use crate::LocalActionManager;

// ── WIT bindings ─────────────────────────────────────────────────────────────
//
// `imports`/`exports: { default: async }` is wasmtime 47's replacement for
// the old `async: true`. Imports become `-> impl Future + Send` (so the
// impls below are plain `async fn`, no `async_trait`), and the export is
// called via `call_async`/`on_fiber`.
//
// Do NOT add the `store` flag: that switches codegen to `call_concurrent`,
// the native component-model-async path, which only accepts guests built
// against the async ABI.

wasmtime::component::bindgen!({
    world: "custom-action",
    path: "../solx-wasm/wit",
    imports: { default: async },
    exports: { default: async },
});

// ── Tuning ───────────────────────────────────────────────────────────────────

/// How often the epoch ticker advances wasmtime's epoch counter. Bounds how
/// long a compute-bound guest can run before it yields.
const EPOCH_TICK: Duration = Duration::from_millis(10);

/// Wall-clock ceiling on a single guest invocation, overridable per action
/// via `action_config.timeout_secs`. Epoch interruption makes a busy guest
/// *yield* but never terminates it, so a hard bound is still needed.
const DEFAULT_TIMEOUT_SECS: u64 = 300;

// ── Engine singleton ─────────────────────────────────────────────────────────

static ENGINE: OnceLock<Engine> = OnceLock::new();

/// The process-wide engine, configured for epoch-based preemption.
///
/// Initializing it also starts a plain OS thread that ticks the epoch
/// counter. It is deliberately not a tokio task: this initializer isn't
/// async and may run off-runtime, and a preemption clock that depends on
/// the very executor it is meant to rescue would be useless under load.
fn engine() -> &'static Engine {
    ENGINE.get_or_init(|| {
        let mut cfg = Config::new();
        cfg.wasm_component_model(true);
        // Fiber-based async needs no engine flag on wasmtime 47 —
        // `Config::async_support` is deprecated as a no-op, and sync vs.
        // async is chosen per call site (`call_async`/`instantiate_async`).
        cfg.epoch_interruption(true);
        let engine = Engine::new(&cfg).expect("failed to create wasmtime engine");

        let ticker = engine.weak();
        std::thread::Builder::new()
            .name("solx-wasm-epoch".into())
            .spawn(move || loop {
                std::thread::sleep(EPOCH_TICK);
                // Stops once the engine is dropped (process teardown).
                match ticker.upgrade() {
                    Some(e) => e.increment_epoch(),
                    None => break,
                }
            })
            .expect("failed to spawn wasm epoch ticker");

        engine
    })
}

// ── Component cache ──────────────────────────────────────────────────────────

/// Compiled components, keyed by a hash of the artifact bytes.
///
/// `Component::from_binary` is a full Cranelift compile — 100ms to seconds
/// for a real guest — and it used to run on *every single execution*. That
/// was merely wasteful while execution sat inside `spawn_blocking`; now
/// that the pipeline is async it would land squarely on an async worker,
/// so it is both cached and, on a miss, pushed onto the blocking pool.
static COMPONENT_CACHE: OnceLock<Mutex<HashMap<u64, Component>>> = OnceLock::new();

fn component_cache() -> &'static Mutex<HashMap<u64, Component>> {
    COMPONENT_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_key(bytes: &[u8]) -> u64 {
    // Not cryptographic, and doesn't need to be: the input is a local
    // artifact the host just read. Hashing a few MB costs microseconds
    // against a compile measured in hundreds of milliseconds.
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// Fetch a compiled component, compiling it off-worker on a cache miss.
async fn compile_component(bytes: Arc<Vec<u8>>) -> Result<Component> {
    let key = cache_key(&bytes);

    // Scoped so the std mutex guard is dropped before any await — holding
    // it across one would be a deadlock waiting to happen.
    if let Some(hit) = component_cache().lock().unwrap().get(&key).cloned() {
        return Ok(hit);
    }

    let component = tokio::task::spawn_blocking(move || Component::from_binary(engine(), &bytes))
        .await
        .map_err(|e| SolxError::Exec(format!("component compile task panicked: {e}")))?
        .map_err(|e| SolxError::Exec(format!("invalid WASM component: {e:#}")))?;

    // A concurrent miss on the same artifact may have compiled it too;
    // last writer wins and both handles are equivalent.
    component_cache().lock().unwrap().insert(key, component.clone());
    Ok(component)
}

// ── Host state ───────────────────────────────────────────────────────────────

struct HostState {
    /// Concrete rather than `Arc<dyn ActionManager>` so recursive calls can
    /// reach `exec_as` and carry `caller` with them.
    actions: Arc<LocalActionManager>,
    files: Arc<dyn FileStore>,
    /// The guest currently running, as seen by anything it invokes.
    caller: Caller,
    wasi: WasiCtx,
    table: ResourceTable,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

// ── Host trait impls ─────────────────────────────────────────────────────────
//
// Each of these awaits directly. When one is pending the guest's fiber
// suspends and the worker thread is released — no `block_on`, and no
// thread held for the duration of a nested call.

impl sol::actions::types::Host for HostState {}

impl sol::actions::logger::Host for HostState {
    async fn log(&mut self, message: String) {
        tracing::info!(target: "solx_wasm_guest", "{message}");
    }
}

impl sol::actions::action_exec::Host for HostState {
    async fn exec(
        &mut self,
        action_name: String,
        payload: String,
    ) -> std::result::Result<sol::actions::types::ActionResult, String> {
        let (path, name) = split_ref(&action_name).map_err(|e| e.to_string())?;
        let params: Value = serde_json::from_str(&payload).map_err(|e| format!("invalid params: {e}"))?;
        let result: ActionExecResult = self
            .actions
            .exec_as(&path, &name, params, Some(&self.caller))
            .await
            .map_err(|e| e.to_string())?;
        Ok(sol::actions::types::ActionResult {
            success: result.success,
            message: result.message,
            output: Some(result.result.to_string()),
        })
    }
}

impl sol::actions::artifact_read::Host for HostState {
    async fn read(&mut self, name: String) -> std::result::Result<Vec<u8>, String> {
        self.files.get(&name).await.map_err(|e| e.to_string())
    }
}

// ── Public entry points ────────────────────────────────────────────────────--

/// Execute a custom WASM action component. `caller` identifies the action
/// being executed — it labels the returned `ActionExecResult.action`, and
/// it is what anything this guest invokes via `action-exec` will see as
/// *its* caller. `fn_name` is passed through to the guest's `run` export.
///
/// `timeout_secs` is the wall-clock ceiling for the whole invocation,
/// including everything it recursively invokes.
pub async fn exec(
    actions: Arc<LocalActionManager>,
    files: Arc<dyn FileStore>,
    wasm_bytes: Arc<Vec<u8>>,
    fn_name: Option<&str>,
    params: &Value,
    caller: Caller,
    timeout_secs: Option<u64>,
) -> Result<ActionExecResult> {
    let params_json = serde_json::to_string(params).map_err(|e| SolxError::Exec(e.to_string()))?;
    let fn_name_owned = fn_name.map(str::to_string);
    let action_ref = caller.action_ref().to_string();
    let timeout = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));

    let run = run_guest(
        actions,
        files,
        wasm_bytes,
        fn_name_owned.clone(),
        params_json,
        caller,
    );

    // `spawn_blocking` used to absorb panics into a `JoinError`; without it
    // a wasmtime panic would unwind into whichever request happened to be
    // driving the future. `catch_unwind` keeps the old contract without
    // detaching the task, so cancellation still propagates into the guest.
    let outcome = match tokio::time::timeout(timeout, std::panic::AssertUnwindSafe(run).catch_unwind()).await {
        Ok(res) => res,
        Err(_) => {
            // Dropping the future here unwinds the fiber and tears down the
            // store; the guest does not keep running in the background.
            return Ok(ActionExecResult {
                action: action_ref,
                result: Value::Null,
                success: false,
                message: Some(format!(
                    "wasm action timed out after {}s (fn_name={fn_name_owned:?})",
                    timeout.as_secs()
                )),
            });
        }
    };

    outcome.unwrap_or_else(|e| {
        let msg = e
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "unknown panic in WASM execution".to_string());
        Err(SolxError::Exec(format!(
            "panic in WASM execution (fn_name={fn_name_owned:?}): {msg}"
        )))
    })
}

async fn run_guest(
    actions: Arc<LocalActionManager>,
    files: Arc<dyn FileStore>,
    wasm_bytes: Arc<Vec<u8>>,
    fn_name: Option<String>,
    params_json: String,
    caller: Caller,
) -> Result<ActionExecResult> {
    let eng = engine();
    let component = compile_component(wasm_bytes).await?;

    let action_ref = caller.action_ref().to_string();
    let wasi = WasiCtxBuilder::new().build();
    let state = HostState { actions, files, caller, wasi, table: ResourceTable::new() };
    let mut store = Store::new(eng, state);

    // Yield rather than trap when the epoch advances: a busy guest hands
    // the worker back to the executor and resumes later, so it can't
    // monopolize a thread. Termination is the outer timeout's job.
    store.set_epoch_deadline(1);
    store.epoch_deadline_async_yield_and_update(1);

    let mut linker = Linker::<HostState>::new(eng);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)
        .map_err(|e| SolxError::Exec(format!("failed to link WASI: {e:#}")))?;
    CustomAction::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s: &mut HostState| s)
        .map_err(|e| SolxError::Exec(format!("failed to link custom-action host: {e:#}")))?;
    let instance = CustomAction::instantiate_async(&mut store, &component, &linker)
        .await
        .map_err(|e| SolxError::Exec(format!("failed to instantiate component: {e:#}")))?;

    let called = instance
        .sol_actions_runner()
        .call_run(&mut store, fn_name.as_deref(), &params_json)
        .await;

    let (success, message, output) = match called {
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
        action: action_ref,
        result,
        success,
        message,
    })
}

// ── Tests ────────────────────────────────────────────────────────────────────
//
// End-to-end coverage — including the nesting behaviour this module's async
// model exists for — lives in `solx-actions/tests/wasm_nesting.rs`, which
// builds a real guest component against `solx-wasm/wit`.
