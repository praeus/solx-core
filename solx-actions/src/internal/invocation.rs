//! `/builtin/action/*` — asynchronous actions: `start`/`stop`/`poll` as an
//! async alternative to `exec`, plus the caller-scoped `action_cancelled` a
//! running Wasm/Script/Internal action polls to notice a `stop` request.
//! See `docs/async-actions-plan.md` for the design.
//!
//! `cancelled` is the one handler scoped to a caller, exactly like
//! `console_print` (`internal/console.rs`) — it resolves the invocation to
//! check from `ctx.caller`, and there is no way to pass an arbitrary
//! `invocation_id` to check on someone else's behalf. `start`/`stop`/`poll`
//! are unrestricted, the same asymmetry `console_read`/`console_tail`
//! already have: `start` grants no new capability (a guest can already
//! `exec` a Command action via `action-exec`), and `stop`/`poll` taking an
//! arbitrary id is what lets an orchestrator manage a child's run.

use serde_json::{json, Value};

use crate::caller::Caller;
use crate::invocations::InvocationStore;

use super::{path_or_root, require_str, InternalCtx};

pub(super) async fn start(params: &Value, ctx: &InternalCtx) -> Result<Value, String> {
    let name = require_str(params, "name")?;
    let path = path_or_root(params);
    let action_params = params.get("params").cloned().unwrap_or_else(|| json!({}));
    ctx.local.start_invocation(path, name, action_params).await.map_err(|e| e.to_string())
}

pub(super) async fn stop(params: &Value, ctx: &InternalCtx) -> Result<Value, String> {
    let invocation_id = require_str(params, "invocation_id")?;
    let force = params.get("force").and_then(Value::as_bool).unwrap_or(false);
    let grace_secs = params.get("grace_secs").and_then(Value::as_u64);
    ctx.local
        .stop_invocation(invocation_id, force, grace_secs)
        .await
        .map_err(|e| e.to_string())
}

pub(super) async fn poll(params: &Value, ctx: &InternalCtx) -> Result<Value, String> {
    let invocation_id = require_str(params, "invocation_id")?;
    let wait_secs = params.get("wait_secs").and_then(Value::as_u64);
    ctx.local
        .poll_invocation(invocation_id, wait_secs)
        .await
        .map_err(|e| e.to_string())
}

pub(super) async fn cancelled(caller: Option<&Caller>, invocations: &InvocationStore) -> Result<Value, String> {
    let caller = caller.ok_or_else(|| {
        "action_cancelled has no action caller — it can only be called from within \
         an action's own execution"
            .to_string()
    })?;
    let cancelled = invocations
        .is_cancelled(caller.invocation_id())
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({ "cancelled": cancelled }))
}
