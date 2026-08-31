//! The MCP `ServerHandler` implementation.
//!
//! `list_tools` exposes a single router meta-tool (`explore_tools`) rather
//! than the full action catalogue; `call_tool` either handles that router
//! (searching/listing the live actions DB) or dispatches a real action tool.
//! Implemented by hand rather than via rmcp's `#[tool_router]`/`#[tool]`
//! macros: those assume a fixed, compile-time-known tool set, but the tools
//! the router discovers come from a live `actions.list()`/`actions.search()`
//! query.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData,
    Implementation, InitializeResult, JsonObject, ListToolsResult, PaginatedRequestParams,
    ProgressNotificationParam, ProgressToken, ProtocolVersion, ServerCapabilities, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{Peer, RoleServer, ServerHandler};
use serde_json::{Map, Value};

use solx_config::ToolPolicy;
use solx_manager::App;
use solx_surface::entities::{Action, ActionExecResult};
use solx_surface::error::SolxError;
use solx_surface::managers::Solx;
use solx_surface::path::full_ref;
use solx_surface::query::{ActionSearchQuery, ListOptions};

use crate::{error, tools};

const PAGE_SIZE: usize = 200;

/// The single router meta-tool's name. Plain (not `act__`-prefixed) so it
/// can never collide with an encoded action tool name.
const ROUTER_TOOL_NAME: &str = "explore_tools";

/// Max candidates returned per router page (both `search` and `list_all`).
const ROUTER_PAGE_SIZE: usize = 10;

/// Long-poll interval passed to `console/tail` between progress
/// notifications — mirrors `solx-cli`'s `TAIL_WAIT_SECS`, which this whole
/// mechanism is a port of (stderr rendering there, `notifications/progress`
/// here).
const TAIL_WAIT_SECS: i64 = 2;

pub struct SolxMcpServer {
    app: Arc<App>,
    /// Restrict the exposed tool catalogue to this path (and everything
    /// under it), e.g. `/builtin` or `/packages/solx-google`. `None`
    /// exposes every registered action, the pre-existing behavior.
    path_prefix: Option<String>,
}

impl SolxMcpServer {
    pub fn new(app: Arc<App>, path_prefix: Option<String>) -> Self {
        SolxMcpServer { app, path_prefix }
    }

    /// The single router meta-tool exposed by `list_tools`.
    fn router_tool() -> Tool {
        let schema: JsonObject = serde_json::json!({
            "type": "object",
            "properties": {
                "discovery_mode": {
                    "type": "string",
                    "enum": ["search", "list_all"],
                    "description": "Choose 'search' to filter tools by keywords, or 'list_all' to step through every available tool."
                },
                "search_query": {
                    "type": "string",
                    "description": "Required when discovery_mode is 'search'. Keywords matching your intent (e.g. 'git log', 'database')."
                },
                "path_prefix": {
                    "type": "string",
                    "description": "Optional. Restrict discovery to actions under this path (e.g. '/builtin')."
                },
                "cursor": {
                    "type": "string",
                    "description": "For 'list_all' mode: pass the next_cursor returned by the previous page to get the next page."
                }
            },
            "required": ["discovery_mode"]
        })
        .as_object()
        .unwrap()
        .clone();
        Tool::new(
            ROUTER_TOOL_NAME,
            "Discover available tools. Use 'search' to find tools for a specific task, or 'list_all' to browse the complete registry. Returns tool names you can then invoke directly.",
            Arc::new(schema),
        )
    }

    /// Resolve the hidden/destructive policy once, to apply across a whole
    /// catalogue page.
    ///
    /// Built per request rather than cached on the server so an edit to a
    /// row's capabilities, or to `solx-config.json`, takes effect on the next
    /// call instead of at the next restart.
    fn policy(&self) -> ToolPolicy {
        self.app.config.tool_policy()
    }

    /// The effective path scope for a router call: the caller's `path_prefix`
    /// argument when present, else the server's own `path_prefix`.
    fn effective_prefix(&self, args: &Map<String, Value>) -> Option<String> {
        args.get("path_prefix")
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| self.path_prefix.clone())
    }

    /// Fetch every non-excluded action in the effective scope, in the default
    /// `path,name` order. Used by the router's `list_all` mode.
    async fn all_actions(&self, path_prefix: Option<String>) -> Result<Vec<Action>, ErrorData> {
        let actions = self.app.actions();
        let policy = self.policy();
        let mut out = Vec::new();
        let mut offset = 0usize;
        loop {
            let page = actions
                .list(ListOptions {
                    path_prefix: path_prefix.clone(),
                    limit: Some(PAGE_SIZE),
                    offset: Some(offset),
                    ..Default::default()
                })
                .await
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
            let n = page.items.len();
            for a in page.items {
                if !policy.is_hidden(&a) {
                    out.push(a);
                }
            }
            offset += n;
            if n == 0 || offset >= page.total {
                break;
            }
        }
        Ok(out)
    }

    /// Format one action as a text candidate the model can read and then
    /// invoke by its encoded tool name.
    ///
    /// A destructive action is labelled rather than withheld: this server has
    /// no approval step of its own, so the most it can do is tell the client
    /// — which is generally driving a human who can be asked.
    async fn format_candidate(&self, a: &Action, policy: &ToolPolicy) -> String {
        let tool_name = tools::encode_tool_name(&a.path, &a.name);
        let description = a
            .description
            .clone()
            .or_else(|| a.caption.clone())
            .unwrap_or_else(|| format!("Execute the '{}{}' action.", a.path, a.name));
        let schema_str = match &a.param_type_ref {
            Some(type_ref) => match self.app.types().resolve(type_ref).await {
                Ok(ty) => ty.schema.to_string(),
                Err(_) => "{}".to_string(),
            },
            None => "{}".to_string(),
        };
        let warning = if policy.is_destructive(a) {
            "\nDestructive: yes - this action changes or removes data. Confirm with the user before invoking it."
        } else {
            ""
        };
        format!("Tool: {tool_name}\nDescription: {description}{warning}\nSchema: {schema_str}")
    }

    /// Handle a `call_tool` invocation of the router meta-tool.
    async fn handle_explore_tools(
        &self,
        args: &Map<String, Value>,
    ) -> Result<CallToolResult, ErrorData> {
        match args.get("discovery_mode").and_then(Value::as_str) {
            Some("search") => self.explore_search(args).await,
            Some("list_all") => self.explore_list_all(args).await,
            _ => Ok(CallToolResult::error(vec![ContentBlock::text(
                "invalid 'discovery_mode': must be 'search' or 'list_all'".to_string(),
            )])),
        }
    }

    async fn explore_search(&self, args: &Map<String, Value>) -> Result<CallToolResult, ErrorData> {
        let query = args
            .get("search_query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if query.is_empty() {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "'search' mode requires a non-empty 'search_query'".to_string(),
            )]));
        }
        let page = self
            .app
            .actions()
            .search(ActionSearchQuery {
                list: ListOptions {
                    path_prefix: self.effective_prefix(args),
                    limit: Some(50),
                    ..Default::default()
                },
                q: Some(query.clone()),
                // The manager applies the same ToolPolicy this server would,
                // and over-fetches to keep the page full — so the loop below
                // only has to trim to the router's page size.
                exclude_hidden: true,
            })
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        // `policy` is still needed to mark destructive candidates; hidden
        // ones never arrive.
        let policy = self.policy();
        let mut lines = Vec::new();
        for a in &page.items {
            if lines.len() >= ROUTER_PAGE_SIZE {
                break;
            }
            lines.push(self.format_candidate(a, &policy).await);
        }
        if lines.is_empty() {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "no tools matched '{query}'"
            ))]));
        }
        let mut text = format!(
            "Found {} tool(s) matching '{query}'. Invoke one by its tool name:\n\n",
            lines.len()
        );
        text.push_str(&lines.join("\n\n"));
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    async fn explore_list_all(&self, args: &Map<String, Value>) -> Result<CallToolResult, ErrorData> {
        let all = self.all_actions(self.effective_prefix(args)).await?;
        let start = args
            .get("cursor")
            .and_then(Value::as_str)
            .and_then(|c| c.parse::<usize>().ok())
            .unwrap_or(0);
        if start >= all.len() {
            return Ok(CallToolResult::success(vec![ContentBlock::text(
                "No more tools available.".to_string(),
            )]));
        }
        let end = (start + ROUTER_PAGE_SIZE).min(all.len());
        let policy = self.policy();
        let mut lines = Vec::new();
        for a in &all[start..end] {
            lines.push(self.format_candidate(a, &policy).await);
        }
        let mut text = format!(
            "Displaying tools {} to {} of {}.\n\n",
            start + 1,
            end,
            all.len()
        );
        text.push_str(&lines.join("\n\n"));
        if end < all.len() {
            text.push_str(&format!("\n\nnext_cursor: {end}"));
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
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
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(vec![Self::router_tool()]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        // The router meta-tool is handled here, before `decode_tool_name`
        // (which only understands `act__`-prefixed action tool names).
        if request.name == ROUTER_TOOL_NAME {
            let args = request.arguments.clone().unwrap_or_default();
            return Ok(self.handle_explore_tools(&args).await?.into());
        }
        let Some((path, name)) = tools::decode_tool_name(&request.name) else {
            return Err(ErrorData::invalid_params(
                format!("unknown tool '{}'", request.name),
                None,
            ));
        };
        // Defense in depth: a client that already knows a hidden tool's name
        // still can't invoke it — hidden tools are rejected here, not just
        // omitted from discovery.
        //
        // This costs one `get` per call, which the old path-only check did
        // not: `solx:hidden` lives on the row, so there is nothing to test
        // without reading it. A row that has gone missing falls through to
        // `exec`, which does its own `get` and reports NotFound in-band —
        // failing open here would be wrong, but so would inventing a
        // different error for a race `exec` already handles.
        if let Ok(action) = self.app.actions().get(&path, &name).await {
            if self.policy().is_hidden(&action) {
                return Err(ErrorData::invalid_params(
                    format!("unknown tool '{}'", request.name),
                    None,
                ));
            }
        }
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
