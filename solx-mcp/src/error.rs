//! `SolxError`/`ActionExecResult` -> MCP `CallToolResult` mapping.
//!
//! Per MCP convention: domain/execution failures are reported *in-band* via
//! `CallToolResult{is_error:true}`, not as JSON-RPC protocol errors, so a
//! model can see and react to them in the conversation. Hard protocol
//! errors (`ErrorData`) are reserved for cases where no plausible request
//! could even be identified (see `tools::decode_tool_name`).

use rmcp::model::{CallToolResult, ContentBlock};
use solx_surface::entities::ActionExecResult;
use solx_surface::error::SolxError;

fn variant_tag(e: &SolxError) -> &'static str {
    match e {
        SolxError::NotFound(_) => "not_found",
        SolxError::Invalid(_) => "invalid",
        SolxError::Validation(_) => "validation",
        SolxError::Conflict(_) => "conflict",
        SolxError::Io(_) => "io",
        SolxError::Db(_) => "db",
        SolxError::Exec(_) => "exec",
        SolxError::Config(_) => "config",
        SolxError::Other(_) => "other",
    }
}

pub fn solx_error_to_tool_result(e: SolxError) -> CallToolResult {
    let text = format!("[{}] {e}", variant_tag(&e));
    CallToolResult::error(vec![ContentBlock::text(text)])
}

/// Map the result of `ActionManager::exec()` to a tool result. `success:false`
/// (a WASM/internal action reporting a handled failure without a hard
/// `Err`) still gets the `result` payload attached (as `structured_content`)
/// so partial output isn't lost.
pub fn exec_result_to_tool_result(r: ActionExecResult) -> CallToolResult {
    if r.success {
        let mut result = CallToolResult::structured(r.result);
        if let Some(msg) = r.message {
            result.content.push(ContentBlock::text(msg));
        }
        result
    } else {
        let mut result = CallToolResult::structured_error(r.result);
        let text = r.message.unwrap_or_else(|| format!("action '{}' failed", r.action));
        result.content.insert(0, ContentBlock::text(text));
        result
    }
}
