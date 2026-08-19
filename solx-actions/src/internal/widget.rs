//! `/builtin/widget/*` — thin wrappers over `crate::loopback::widget`, so a
//! `.solx` script or any other action can drive a widget via `action-exec`
//! without being a WASM guest. See `docs/widget-actions.md` §6.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use solx_config::ConfigService;
use solx_surface::managers::FileStore;

use crate::caller::Caller;
use crate::loopback::widget;

use super::require_str;

pub(super) async fn open(
    params: &Value,
    files: &Arc<dyn FileStore>,
    config: &Arc<ConfigService>,
    caller: Option<&Caller>,
) -> Result<Value, String> {
    let bin_name = require_str(params, "bin_name")?;
    let tag_name = require_str(params, "tag_name")?;
    let fields = params.get("fields").cloned().unwrap_or(Value::Null);

    // Attribution only (see `loopback::widget::WidgetState`) — the CLI, MCP,
    // and HTTP routes have no action caller, so `action_ref` is empty in
    // that case rather than required.
    let (action_ref, invocation_id) = match caller {
        Some(c) => (c.action_ref().to_string(), c.invocation_id().to_string()),
        None => (String::new(), uuid::Uuid::new_v4().to_string()),
    };

    let descriptor = widget::open(
        files,
        bin_name,
        tag_name,
        fields,
        &action_ref,
        &invocation_id,
        Duration::from_secs(config.widget_connect_ttl_secs()),
        Duration::from_secs(config.widget_reconnect_grace_secs()),
    )
    .await?;
    serde_json::to_value(descriptor).map_err(|e| format!("failed to serialize descriptor: {e}"))
}

pub(super) async fn close(params: &Value) -> Result<Value, String> {
    let widget_id = require_str(params, "widget_id")?;
    widget::close(widget_id).await?;
    Ok(json!({ "closed": true }))
}

pub(super) async fn show(params: &Value) -> Result<Value, String> {
    let widget_id = require_str(params, "widget_id")?;
    widget::show(widget_id).await?;
    Ok(json!({ "shown": true }))
}

pub(super) async fn hide(params: &Value) -> Result<Value, String> {
    let widget_id = require_str(params, "widget_id")?;
    widget::hide(widget_id).await?;
    Ok(json!({ "hidden": true }))
}

pub(super) async fn get(params: &Value) -> Result<Value, String> {
    let widget_id = require_str(params, "widget_id")?;
    let field = params.get("field").and_then(Value::as_str).unwrap_or("");
    widget::get(widget_id, field).await
}

pub(super) async fn set(params: &Value) -> Result<Value, String> {
    let widget_id = require_str(params, "widget_id")?;
    let field = require_str(params, "field")?;
    let value = params.get("value").cloned().unwrap_or(Value::Null);
    widget::set(widget_id, field, value).await?;
    Ok(json!({ "set": true }))
}

pub(super) async fn exec(params: &Value) -> Result<Value, String> {
    let widget_id = require_str(params, "widget_id")?;
    let event = require_str(params, "event")?;
    let payload = params.get("payload").cloned().unwrap_or(Value::Null);
    widget::exec(widget_id, event, payload).await?;
    Ok(json!({ "executed": true }))
}
