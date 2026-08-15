//! Wasmtime engine, component cache, WIT bindings, and the per-invocation
//! [`HostState`] that implements the guest's imports.
//!
//! ## Caller attribution
//!
//! `crate::wasm` is the *only* re-entrant path into execution — internal
//! actions do entity CRUD, never `exec`. So it is also the only place a
//! [`crate::caller::Caller`] is minted: `super::actions::exec` builds one
//! from the row it was handed and passes it to [`HostState::new`], and
//! every `action-exec` call the guest makes goes through
//! [`crate::LocalActionManager::exec_as`] carrying it. That is what scopes
//! `get_secret`/`set_secret` to the guest's own keys. The frame is rebuilt
//! per nesting level, so a guest invoking another guest cannot reach the
//! outer one's secrets.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::Value;
use solx_surface::error::{Result, SolxError};
use solx_surface::managers::FileStore;
use solx_surface::path::split_ref;
use wasmtime::component::Component;
use wasmtime::{Config, Engine};
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

// ── Engine singleton ─────────────────────────────────────────────────────────

static ENGINE: OnceLock<Engine> = OnceLock::new();

/// The process-wide engine, configured for epoch-based preemption.
///
/// Initializing it also starts a plain OS thread that ticks the epoch
/// counter. It is deliberately not a tokio task: this initializer isn't
/// async and may run off-runtime, and a preemption clock that depends on
/// the very executor it is meant to rescue would be useless under load.
pub(super) fn engine() -> &'static Engine {
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
pub(super) async fn compile_component(bytes: Arc<Vec<u8>>) -> Result<Component> {
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

pub(super) struct HostState {
    /// Concrete rather than `Arc<dyn ActionManager>` so recursive calls can
    /// reach `exec_as` and carry `caller` with them.
    actions: Arc<LocalActionManager>,
    files: Arc<dyn FileStore>,
    /// The guest currently running, as seen by anything it invokes.
    caller: Caller,
    wasi: WasiCtx,
    table: ResourceTable,
}

impl HostState {
    pub(super) fn new(actions: Arc<LocalActionManager>, files: Arc<dyn FileStore>, caller: Caller) -> Self {
        HostState {
            actions,
            files,
            caller,
            wasi: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
        }
    }
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
        // Direct call into the console store rather than a synthetic
        // action-exec("/builtin/console/print", ...) round trip — this
        // guest already *is* the caller `console_print` would resolve to,
        // so there's nothing the extra hop would add. Best-effort: a
        // console write failing must never fail the guest's log call.
        if let Err(e) = self
            .actions
            .console()
            .print(
                self.caller.action_ref(),
                self.caller.invocation_id(),
                None,
                "info",
                "guest",
                Some(message.clone()),
                None,
            )
            .await
        {
            tracing::warn!("console print failed for {}: {e}", self.caller.action_ref());
        }
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
        let result: solx_surface::entities::ActionExecResult = self
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
