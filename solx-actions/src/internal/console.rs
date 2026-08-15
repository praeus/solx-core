//! `/builtin/console/*` — see [`crate::console`] for the storage layer and
//! the design rationale (console identity, `invocation_id`, retention).
//!
//! `print` is the one handler scoped to a caller: it always writes to *the
//! calling action's own* console, resolved from `ctx.caller` exactly the
//! way `get_secret`/`set_secret` resolve their scope — there is no way to
//! pass an arbitrary `action_ref` to write into. `read`/`tail`/`clear`/
//! `list` take an explicit `action_ref` and are unrestricted, which is what
//! lets an orchestrating action watch (or clear) a child's console.

use serde_json::{json, Value};

use crate::caller::Caller;
use crate::console::{ConsoleStore, ReadResult};

use super::require_str;

pub(super) async fn print(
    params: &Value,
    caller: Option<&Caller>,
    store: &ConsoleStore,
) -> Result<Value, String> {
    let caller = caller.ok_or_else(|| {
        "console_print has no action caller — it can only be called from within \
         an action's own execution"
            .to_string()
    })?;

    let level = params
        .get("level")
        .and_then(Value::as_str)
        .unwrap_or("info")
        .to_string();
    let message = params.get("message").and_then(Value::as_str).map(str::to_string);
    let data = params.get("data").cloned().filter(|v| !v.is_null());

    let seq = store
        .print(
            caller.action_ref(),
            caller.invocation_id(),
            None, // run_id: reserved, unwired in phase 1
            &level,
            "guest",
            message,
            data,
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({ "seq": seq }))
}

pub(super) async fn read(params: &Value, store: &ConsoleStore) -> Result<Value, String> {
    let action_ref = require_str(params, "action_ref")?;
    let from_seq = params.get("from_seq").and_then(Value::as_i64);
    let limit = params.get("limit").and_then(Value::as_i64).unwrap_or(200);
    let result = store.read(action_ref, from_seq, limit).await.map_err(|e| e.to_string())?;
    Ok(result_to_value(result))
}

pub(super) async fn tail(params: &Value, store: &ConsoleStore) -> Result<Value, String> {
    let action_ref = require_str(params, "action_ref")?;
    let cursor = params.get("cursor").and_then(Value::as_i64);
    let limit = params.get("limit").and_then(Value::as_i64).unwrap_or(200);
    let wait_secs = params.get("wait_secs").and_then(Value::as_u64);
    let result = store
        .tail(action_ref, cursor, limit, wait_secs)
        .await
        .map_err(|e| e.to_string())?;
    Ok(result_to_value(result))
}

pub(super) async fn clear(params: &Value, store: &ConsoleStore) -> Result<Value, String> {
    let action_ref = require_str(params, "action_ref")?;
    let before_seq = params.get("before_seq").and_then(Value::as_i64);
    let removed = store.clear(action_ref, before_seq).await.map_err(|e| e.to_string())?;
    Ok(json!({ "removed": removed }))
}

pub(super) async fn list(params: &Value, store: &ConsoleStore) -> Result<Value, String> {
    let prefix = params.get("prefix").and_then(Value::as_str);
    let limit = params.get("limit").and_then(Value::as_i64).unwrap_or(100);
    let consoles = store.list(prefix, limit).await.map_err(|e| e.to_string())?;
    Ok(json!({
        "consoles": consoles.into_iter().map(|c| json!({
            "action_ref": c.action_ref,
            "created_at": c.created_at,
            "last_write": c.last_write,
            "entry_count": c.entry_count,
            "dropped": c.dropped,
            "next_seq": c.next_seq,
        })).collect::<Vec<_>>(),
    }))
}

fn result_to_value(r: ReadResult) -> Value {
    json!({
        "entries": r.entries.into_iter().map(|e| json!({
            "seq": e.seq,
            "ts": e.ts,
            "level": e.level,
            "invocation_id": e.invocation_id,
            "run_id": e.run_id,
            "source": e.source,
            "message": e.message,
            "data": e.data,
        })).collect::<Vec<_>>(),
        "next_cursor": r.next_cursor,
        "first_seq": r.first_seq,
        "dropped": r.dropped,
    })
}
