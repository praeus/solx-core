//! `/builtin/console/*` and `/builtin/action/{start,stop,poll,cancelled}` —
//! this crate's own internal-action handlers, registered into
//! `solx-actions`' dispatcher via [`plugin`] rather than hard-coded there.
//! This is the reference consumer of `solx_surface::internal_actions`'
//! plugin API.
//!
//! `print` and `cancelled` are two of the handlers scoped to a caller: they
//! always act on *the calling action's own* console/invocation, resolved
//! from `ctx.caller` exactly the way `get_secret`/`set_secret` resolve
//! their scope — there is no way to pass an arbitrary `action_ref`/
//! `invocation_id` in. `read`/`tail`/`clear`/`list`/`start`/`stop`/`poll`
//! take an explicit target and are unrestricted, which is what lets an
//! orchestrating action watch (or manage) a child's console/invocation.
//!
//! `copy` is scoped on one side only: its *source* (`from_action_ref`) is an
//! explicit, unrestricted target, no more sensitive than `read`/`tail`
//! already being unrestricted on the same console — but its *destination* is
//! always the calling action's own, resolved from `ctx.caller` like `print`.
//! Without that, any action could inject fabricated entries into any other
//! action's console just by naming it, the exact spoofing `print`'s own
//! scoping already exists to prevent.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use solx_surface::internal_actions::{
    ActionExecutor, InternalActionHandler, InternalActionRegistry, InternalCallCtx, SeedAction,
};

use crate::console::{ConsoleStore, CopyResult, ReadResult};
use crate::invocations::InvocationStore;

const CONSOLE_PATH: &str = "/builtin/console";
const ACTION_PATH: &str = "/builtin/action";

fn require_str<'a>(params: &'a Value, field: &str) -> std::result::Result<&'a str, String> {
    params
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing required param: {field}"))
}

fn path_or_root(params: &Value) -> &str {
    params.get("path").and_then(Value::as_str).unwrap_or("/")
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

// ── Console handlers ─────────────────────────────────────────────────────────

struct ConsolePrint {
    store: Arc<ConsoleStore>,
}

#[async_trait]
impl InternalActionHandler for ConsolePrint {
    async fn call(&self, params: &Value, ctx: &InternalCallCtx) -> std::result::Result<Value, String> {
        let caller = ctx.caller.as_ref().ok_or_else(|| {
            "console_print has no action caller — it can only be called from within \
             an action's own execution"
                .to_string()
        })?;

        let level = params.get("level").and_then(Value::as_str).unwrap_or("info").to_string();
        let message = params.get("message").and_then(Value::as_str).map(str::to_string);
        let data = params.get("data").cloned().filter(|v| !v.is_null());

        let seq = self
            .store
            .print(
                caller.action_ref(),
                caller.invocation_id(),
                None, // run_id: reserved, unwired
                &level,
                "guest",
                message,
                data,
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(json!({ "seq": seq }))
    }
}

struct ConsoleCopy {
    store: Arc<ConsoleStore>,
}

#[async_trait]
impl InternalActionHandler for ConsoleCopy {
    async fn call(&self, params: &Value, ctx: &InternalCallCtx) -> std::result::Result<Value, String> {
        let caller = ctx.caller.as_ref().ok_or_else(|| {
            "console_copy has no action caller — it can only be called from within \
             an action's own execution"
                .to_string()
        })?;

        let from_action_ref = require_str(params, "from_action_ref")?;
        let invocation_id = require_str(params, "invocation_id")?;
        let cursor = params.get("cursor").and_then(Value::as_i64);
        let limit = params.get("limit").and_then(Value::as_i64).unwrap_or(200);
        let label = params.get("label").and_then(Value::as_str);

        let result = self
            .store
            .copy(from_action_ref, caller.action_ref(), invocation_id, cursor, limit, label)
            .await
            .map_err(|e| e.to_string())?;
        Ok(copy_result_to_value(result))
    }
}

fn copy_result_to_value(r: CopyResult) -> Value {
    json!({ "copied": r.copied, "next_cursor": r.next_cursor })
}

struct ConsoleRead {
    store: Arc<ConsoleStore>,
}

#[async_trait]
impl InternalActionHandler for ConsoleRead {
    async fn call(&self, params: &Value, _ctx: &InternalCallCtx) -> std::result::Result<Value, String> {
        let action_ref = require_str(params, "action_ref")?;
        let from_seq = params.get("from_seq").and_then(Value::as_i64);
        let limit = params.get("limit").and_then(Value::as_i64).unwrap_or(200);
        let result = self.store.read(action_ref, from_seq, limit).await.map_err(|e| e.to_string())?;
        Ok(result_to_value(result))
    }
}

struct ConsoleTail {
    store: Arc<ConsoleStore>,
}

#[async_trait]
impl InternalActionHandler for ConsoleTail {
    async fn call(&self, params: &Value, _ctx: &InternalCallCtx) -> std::result::Result<Value, String> {
        let action_ref = require_str(params, "action_ref")?;
        let cursor = params.get("cursor").and_then(Value::as_i64);
        let limit = params.get("limit").and_then(Value::as_i64).unwrap_or(200);
        let wait_secs = params.get("wait_secs").and_then(Value::as_u64);
        let result = self
            .store
            .tail(action_ref, cursor, limit, wait_secs)
            .await
            .map_err(|e| e.to_string())?;
        Ok(result_to_value(result))
    }
}

struct ConsoleClear {
    store: Arc<ConsoleStore>,
}

#[async_trait]
impl InternalActionHandler for ConsoleClear {
    async fn call(&self, params: &Value, _ctx: &InternalCallCtx) -> std::result::Result<Value, String> {
        let action_ref = require_str(params, "action_ref")?;
        let before_seq = params.get("before_seq").and_then(Value::as_i64);
        let removed = self.store.clear(action_ref, before_seq).await.map_err(|e| e.to_string())?;
        Ok(json!({ "removed": removed }))
    }
}

struct ConsoleList {
    store: Arc<ConsoleStore>,
}

#[async_trait]
impl InternalActionHandler for ConsoleList {
    async fn call(&self, params: &Value, _ctx: &InternalCallCtx) -> std::result::Result<Value, String> {
        let prefix = params.get("prefix").and_then(Value::as_str);
        let limit = params.get("limit").and_then(Value::as_i64).unwrap_or(100);
        let consoles = self.store.list(prefix, limit).await.map_err(|e| e.to_string())?;
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
}

// ── Action (invocation) handlers ─────────────────────────────────────────────

struct ActionStart {
    executor: Arc<dyn ActionExecutor>,
}

#[async_trait]
impl InternalActionHandler for ActionStart {
    async fn call(&self, params: &Value, _ctx: &InternalCallCtx) -> std::result::Result<Value, String> {
        let name = require_str(params, "name")?;
        let path = path_or_root(params);
        let action_params = params.get("params").cloned().unwrap_or_else(|| json!({}));
        self.executor
            .start_invocation(path, name, action_params)
            .await
            .map_err(|e| e.to_string())
    }
}

struct ActionStop {
    executor: Arc<dyn ActionExecutor>,
}

#[async_trait]
impl InternalActionHandler for ActionStop {
    async fn call(&self, params: &Value, _ctx: &InternalCallCtx) -> std::result::Result<Value, String> {
        let invocation_id = require_str(params, "invocation_id")?;
        let force = params.get("force").and_then(Value::as_bool).unwrap_or(false);
        let grace_secs = params.get("grace_secs").and_then(Value::as_u64);
        self.executor
            .stop_invocation(invocation_id, force, grace_secs)
            .await
            .map_err(|e| e.to_string())
    }
}

struct ActionPoll {
    executor: Arc<dyn ActionExecutor>,
}

#[async_trait]
impl InternalActionHandler for ActionPoll {
    async fn call(&self, params: &Value, _ctx: &InternalCallCtx) -> std::result::Result<Value, String> {
        let invocation_id = require_str(params, "invocation_id")?;
        let wait_secs = params.get("wait_secs").and_then(Value::as_u64);
        self.executor
            .poll_invocation(invocation_id, wait_secs)
            .await
            .map_err(|e| e.to_string())
    }
}

struct ActionCancelled {
    store: Arc<InvocationStore>,
}

#[async_trait]
impl InternalActionHandler for ActionCancelled {
    async fn call(&self, _params: &Value, ctx: &InternalCallCtx) -> std::result::Result<Value, String> {
        let caller = ctx.caller.as_ref().ok_or_else(|| {
            "action_cancelled has no action caller — it can only be called from within \
             an action's own execution"
                .to_string()
        })?;
        let cancelled = self
            .store
            .is_cancelled(caller.invocation_id())
            .await
            .map_err(|e| e.to_string())?;
        Ok(json!({ "cancelled": cancelled }))
    }
}

// ── Plugin assembly ──────────────────────────────────────────────────────────

/// This crate's built-in catalogue entries — a standalone function (not tied
/// to [`plugin`]'s handler construction) so `solx-actions` can seed them into
/// the `actions` table at `LocalActionManager::open()` time, before an
/// `Arc<Self>` exists to build the full registry (which `action_start`/
/// `stop`/`poll`'s handlers need, via [`ActionExecutor`]). See
/// `LocalActionManager::set_self_ref`, which builds the full registry once
/// that `Arc` is available.
pub fn seed_actions() -> Vec<SeedAction> {
    vec![
        SeedAction {
            path: CONSOLE_PATH,
            name: "print",
            fn_name: "console_print",
            description: "Write one entry to the calling action's own console. Requires an action caller — this cannot be called directly from the CLI, MCP, or HTTP.",
            param_type: Some("ConsolePrintParams"),
        },
        SeedAction {
            path: CONSOLE_PATH,
            name: "copy",
            fn_name: "console_copy",
            description: "Copy one invocation's entries from another console into the calling action's own, renumbered into its sequence and optionally message-prefixed with a label. Requires an action caller — this cannot be called directly from the CLI, MCP, or HTTP. Built for an orchestrator draining a child invocation's console without one print call per entry.",
            param_type: Some("ConsoleCopyParams"),
        },
        SeedAction {
            path: CONSOLE_PATH,
            name: "read",
            fn_name: "console_read",
            description: "Read entries from an action's console, oldest first, starting at from_seq.",
            param_type: Some("ConsoleReadParams"),
        },
        SeedAction {
            path: CONSOLE_PATH,
            name: "tail",
            fn_name: "console_tail",
            description: "Like read, but if nothing new is available yet, long-polls up to wait_secs before returning.",
            param_type: Some("ConsoleTailParams"),
        },
        SeedAction {
            path: CONSOLE_PATH,
            name: "clear",
            fn_name: "console_clear",
            description: "Drop entries from the front of an action's console, freeing retention.",
            param_type: Some("ConsoleClearParams"),
        },
        SeedAction {
            path: CONSOLE_PATH,
            name: "list",
            fn_name: "console_list",
            description: "List known consoles, most recently written first.",
            param_type: Some("ConsoleListParams"),
        },
        SeedAction {
            path: ACTION_PATH,
            name: "start",
            fn_name: "action_start",
            description: "Start an action detached: returns an invocation_id immediately while it runs in the background. Requires a long-lived host (solx-server/solx-mcp), not the CLI.",
            param_type: Some("ActionStartParams"),
        },
        SeedAction {
            path: ACTION_PATH,
            name: "stop",
            fn_name: "action_stop",
            description: "Request that a detached invocation stop. Cooperative first (the running action notices and exits on its own); force-aborted after a grace period.",
            param_type: Some("ActionStopParams"),
        },
        SeedAction {
            path: ACTION_PATH,
            name: "poll",
            fn_name: "action_poll",
            description: "Check a detached invocation's status, optionally long-polling until it finishes.",
            param_type: Some("ActionPollParams"),
        },
        SeedAction {
            path: ACTION_PATH,
            name: "cancelled",
            fn_name: "action_cancelled",
            description: "Check whether the calling action's own invocation has had a stop requested. Requires an action caller.",
            param_type: Some("EmptyParams"),
        },
    ]
}

/// Build the registry of every console/invocation internal action this
/// crate provides, ready to be merged into `solx-actions`' own registry.
pub fn plugin(
    console: Arc<ConsoleStore>,
    invocations: Arc<InvocationStore>,
    executor: Arc<dyn ActionExecutor>,
) -> InternalActionRegistry {
    let mut registry = InternalActionRegistry::new();

    registry.register(&["console_print"], Arc::new(ConsolePrint { store: console.clone() }));
    registry.register(&["console_copy"], Arc::new(ConsoleCopy { store: console.clone() }));
    registry.register(&["console_read"], Arc::new(ConsoleRead { store: console.clone() }));
    registry.register(&["console_tail"], Arc::new(ConsoleTail { store: console.clone() }));
    registry.register(&["console_clear"], Arc::new(ConsoleClear { store: console.clone() }));
    registry.register(&["console_list"], Arc::new(ConsoleList { store: console }));

    registry.register(&["action_start"], Arc::new(ActionStart { executor: executor.clone() }));
    registry.register(&["action_stop"], Arc::new(ActionStop { executor: executor.clone() }));
    registry.register(&["action_poll"], Arc::new(ActionPoll { executor }));
    registry.register(&["action_cancelled"], Arc::new(ActionCancelled { store: invocations }));

    registry.add_seeds(seed_actions());
    registry
}
