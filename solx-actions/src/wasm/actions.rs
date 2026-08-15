//! Drives one guest invocation: builds a `HostState` from [`super::host`],
//! runs it under a wall-clock timeout with panic-catching, and reports the
//! result. This is the only public surface of `crate::wasm` — everything
//! wasmtime-specific stays in `host`.
//!
//! End-to-end coverage — including the nesting behaviour the fiber-based
//! async model in `crate::wasm`'s module doc exists for — lives in
//! `solx-actions/tests/wasm_nesting.rs`, which builds a real guest component
//! against `solx-wasm/wit`.

use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use serde_json::Value;
use solx_surface::entities::ActionExecResult;
use solx_surface::error::{Result, SolxError};
use solx_surface::managers::FileStore;
use wasmtime::component::{HasSelf, Linker};
use wasmtime::Store;

use crate::caller::Caller;
use crate::LocalActionManager;

use super::host::{compile_component, engine, CustomAction, HostState};

/// Wall-clock ceiling on a single guest invocation, overridable per action
/// via `action_config.timeout_secs`. Epoch interruption makes a busy guest
/// *yield* but never terminates it, so a hard bound is still needed.
const DEFAULT_TIMEOUT_SECS: u64 = 300;

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
    let state = HostState::new(actions, files, caller);
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
