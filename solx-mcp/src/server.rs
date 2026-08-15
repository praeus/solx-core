//! The MCP `ServerHandler` implementation.
//!
//! `list_tools`/`call_tool` are pure functions of current actions-DB state —
//! no cached tool map, no mutable server state beyond the `Arc<App>` itself.
//! Implemented by hand rather than via rmcp's `#[tool_router]`/`#[tool]`
//! macros: those assume a fixed, compile-time-known tool set, but every tool
//! here comes from a live `actions.list()` query.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ErrorData, Implementation,
    InitializeResult, ListToolsResult, PaginatedRequestParams, ProgressNotificationParam,
    ProgressToken, ProtocolVersion, ServerCapabilities, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{Peer, RoleServer, ServerHandler};
use serde_json::{Map, Value};

use solx_manager::App;
use solx_surface::entities::ActionExecResult;
use solx_surface::error::SolxError;
use solx_surface::managers::Solx;
use solx_surface::path::full_ref;
use solx_surface::query::ListOptions;

use crate::{error, schema, tools};

const PAGE_SIZE: usize = 200;

/// Long-poll interval passed to `console/tail` between progress
/// notifications — mirrors `solx-cli`'s `TAIL_WAIT_SECS`, which this whole
/// mechanism is a port of (stderr rendering there, `notifications/progress`
/// here).
const TAIL_WAIT_SECS: i64 = 2;

pub struct SolxMcpServer {
    app: Arc<App>,
}

impl SolxMcpServer {
    pub fn new(app: Arc<App>) -> Self {
        SolxMcpServer { app }
    }

    async fn list_action_tools(&self, offset: usize) -> Result<(Vec<Tool>, Option<usize>), ErrorData> {
        let actions = self.app.actions();
        let types = self.app.types();
        let page = actions
            .list(ListOptions { limit: Some(PAGE_SIZE), offset: Some(offset), ..Default::default() })
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let mut out = Vec::with_capacity(page.items.len());
        for a in &page.items {
            let input_schema = match &a.param_type_ref {
                Some(type_ref) => match types.resolve(type_ref).await {
                    Ok(ty) => schema::schema_from_type_value(&ty.schema),
                    Err(_) => schema::permissive_object_schema(),
                },
                None => schema::permissive_object_schema(),
            };
            let name = tools::encode_tool_name(&a.path, &a.name);
            let description = a
                .description
                .clone()
                .or_else(|| a.caption.clone())
                .unwrap_or_else(|| format!("Execute the '{}{}' action.", a.path, a.name));
            out.push(Tool::new(name, description, input_schema));
        }

        let next_offset = offset + page.items.len();
        let next = if next_offset < page.total { Some(next_offset) } else { None };
        Ok((out, next))
    }
}

impl ServerHandler for SolxMcpServer {
    fn get_info(&self) -> InitializeResult {
        let mut info = InitializeResult::new(ServerCapabilities::builder().enable_tools().build());
        info.protocol_version = ProtocolVersion::LATEST;
        // `Implementation::from_build_env()`'s `env!("CARGO_CRATE_NAME")` would
        // resolve to rmcp's own crate name (macros expand in the defining
        // crate), not ours — set it explicitly instead.
        info.server_info = Implementation::new("solx-mcp", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(
            "Every action in the solx actions database is surfaced here as a tool. \
             Documents, types, actions-as-data, search, and general file-store access \
             are all reached through those actions (e.g. entity_new_document, \
             search_documents, file_put) — there is no separate CRUD tool layer."
                .to_string(),
        );
        info
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let offset: usize = request
            .and_then(|r| r.cursor)
            .and_then(|c: String| c.parse::<usize>().ok())
            .unwrap_or(0);
        let (tools, next_offset) = self.list_action_tools(offset).await?;
        let mut result = ListToolsResult::with_all_items(tools);
        result.next_cursor = next_offset.map(|o| o.to_string());
        Ok(result)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let Some((path, name)) = tools::decode_tool_name(&request.name) else {
            return Err(ErrorData::invalid_params(
                format!("unknown tool '{}'", request.name),
                None,
            ));
        };
        // Not `request.progress_token()`: on the receiving side, rmcp
        // strips the wire `_meta` into the request envelope's `Extensions`
        // during deserialization rather than populating the typed params'
        // own `meta` field, then moves it into `RequestContext::meta` before
        // dispatch (see `rmcp::model::meta`'s `GetMeta` doc comment) — so
        // `ctx.meta` is the one that's actually populated here.
        let progress_token = ctx.meta.get_progress_token();
        let params: Value = request
            .arguments
            .map(Value::Object)
            .unwrap_or_else(|| Value::Object(Map::new()));

        // Only pay for the tail loop if the client actually asked for
        // progress updates (attached a `progressToken`) — most callers of
        // short builtin actions won't, and skipping this is free for them.
        let tail_handle = match (progress_token, full_ref(&path, &name)) {
            (Some(token), Ok(action_ref)) => {
                Some(self.spawn_console_tail(action_ref, token, ctx.peer.clone()).await)
            }
            _ => None,
        };

        let exec_result = self.app.actions().exec(&path, &name, params).await;
        if let Some(handle) = tail_handle {
            handle.stop_and_drain().await;
        }

        let result: CallToolResult = match exec_result {
            Ok(result) => error::exec_result_to_tool_result(result),
            Err(e) => error::solx_error_to_tool_result(e),
        };
        Ok(result.into())
    }
}

/// A live console-tailing task feeding `notifications/progress` back to the
/// client for the duration of one `call_tool` invocation. See
/// `solx-cli/src/main.rs`'s `ConsoleTailHandle` — same shape, different sink
/// (stderr there, MCP notifications here), because both poll the same
/// `/builtin/console` `tail`/`read` actions.
struct ConsoleTailHandle {
    stop: Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl ConsoleTailHandle {
    async fn stop_and_drain(self) {
        self.stop.notify_one();
        let _ = self.task.await;
    }
}

impl SolxMcpServer {
    /// The seq that will be assigned to this console's next write, or `None`
    /// if it has never been written to. Starting the tail from here (rather
    /// than 0) means a frequently-invoked action's tool call doesn't replay
    /// its entire prior history as progress notifications.
    async fn current_tip(&self, action_ref: &str) -> Option<i64> {
        let res = self
            .app
            .actions()
            .exec(
                "/builtin/console",
                "list",
                serde_json::json!({ "prefix": action_ref, "limit": 50 }),
            )
            .await
            .ok()?;
        res.result
            .get("consoles")
            .and_then(Value::as_array)?
            .iter()
            .find(|c| c.get("action_ref").and_then(Value::as_str) == Some(action_ref))
            .and_then(|c| c.get("next_seq"))
            .and_then(Value::as_i64)
    }

    async fn spawn_console_tail(
        &self,
        action_ref: String,
        progress_token: ProgressToken,
        peer: Peer<RoleServer>,
    ) -> ConsoleTailHandle {
        let start_cursor = self.current_tip(&action_ref).await.unwrap_or(0);

        let app = self.app.clone();
        let stop = Arc::new(tokio::sync::Notify::new());
        let task_stop = stop.clone();
        let task = tokio::spawn(async move {
            let actions = app.actions();
            let mut cursor = start_cursor;
            loop {
                let tail = actions.exec(
                    "/builtin/console",
                    "tail",
                    serde_json::json!({ "action_ref": action_ref, "cursor": cursor, "wait_secs": TAIL_WAIT_SECS }),
                );
                tokio::select! {
                    res = tail => {
                        cursor = send_progress(&peer, &progress_token, res, cursor).await;
                    }
                    _ = task_stop.notified() => {
                        // One last non-blocking read so nothing printed by
                        // the action right before it returned is lost to a
                        // race against this task's own poll cadence.
                        let res = actions.exec(
                            "/builtin/console",
                            "read",
                            serde_json::json!({ "action_ref": action_ref, "from_seq": cursor }),
                        ).await;
                        send_progress(&peer, &progress_token, res, cursor).await;
                        return;
                    }
                }
            }
        });

        ConsoleTailHandle { stop, task }
    }
}

/// Turn any entries in a `console/tail` or `console/read` result into
/// `notifications/progress` sends, and return the cursor to continue from.
/// Failures are swallowed on both sides — a dead/slow client must never
/// fail the action it's watching, mirroring the loopback's own
/// best-effort-delivery philosophy.
async fn send_progress(
    peer: &Peer<RoleServer>,
    token: &ProgressToken,
    res: std::result::Result<ActionExecResult, SolxError>,
    fallback_cursor: i64,
) -> i64 {
    let Ok(res) = res else { return fallback_cursor };
    if let Some(entries) = res.result.get("entries").and_then(Value::as_array) {
        for entry in entries {
            let seq = entry.get("seq").and_then(Value::as_i64).unwrap_or(0);
            let level = entry.get("level").and_then(Value::as_str).unwrap_or("info");
            let message = entry.get("message").and_then(Value::as_str).unwrap_or("");
            let mut text = format!("[{}] {message}", level.to_ascii_uppercase());
            if let Some(data) = entry.get("data").filter(|d| !d.is_null()) {
                text.push(' ');
                text.push_str(&data.to_string());
            }
            let param = ProgressNotificationParam::new(token.clone(), seq as f64).with_message(text);
            let _ = peer.notify_progress(param).await;
        }
    }
    res.result.get("next_cursor").and_then(Value::as_i64).unwrap_or(fallback_cursor)
}
