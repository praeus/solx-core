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
    InitializeResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities,
    Tool,
};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler};
use serde_json::{Map, Value};

use solx_manager::App;
use solx_surface::managers::Solx;
use solx_surface::query::ListOptions;

use crate::{error, schema, tools};

const PAGE_SIZE: usize = 200;

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
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let Some((path, name)) = tools::decode_tool_name(&request.name) else {
            return Err(ErrorData::invalid_params(
                format!("unknown tool '{}'", request.name),
                None,
            ));
        };
        let params: Value = request
            .arguments
            .map(Value::Object)
            .unwrap_or_else(|| Value::Object(Map::new()));

        let result: CallToolResult = match self.app.actions().exec(&path, &name, params).await {
            Ok(result) => error::exec_result_to_tool_result(result),
            Err(e) => error::solx_error_to_tool_result(e),
        };
        Ok(result.into())
    }
}
