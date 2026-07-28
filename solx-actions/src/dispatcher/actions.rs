//! Prompt & Actions action-type handlers.
//!
//! Both `Prompt` and `Actions` action types load an artifact (via
//! `bin_name`) and dispatch on its content:
//!
//! * **Prompt** — When the message contains `prompt_file`, the artifact
//!   acts as a prompt override (JSON map or plain text). Otherwise the
//!   artifact text is executed as an instruction.
//! * **Actions** — The artifact is parsed as a JSON script and delegated
//!   to [`crate::script_executor::dispatch_actions_artifact`].

use std::sync::Arc;

use serde_json::{json, Value};

use sol_core::{
    db::DbManager,
    entities::{Action, ActionExecResult},
};
use sol_core::search::SearchManager;

use crate::browser_actions::BrowserActions;
use crate::instruction;

use super::load_artifact_bytes_from_entity;

/// Dispatch a `Prompt` or `Actions` action.
///
/// The `action_type` is matched inside this function so the caller
/// (`execute_action_once`) can use a single arm for both types.
pub(crate) async fn dispatch_prompt_or_actions(
    db: Arc<dyn DbManager>,
    search_engine: Arc<dyn SearchManager>,
    browser_actions: Option<Arc<dyn BrowserActions>>,
    event_hooks: Option<Arc<crate::EventHooksConfig>>,
    entity_name: &str,
    action: &Action,
    message: &Value,
) -> Result<ActionExecResult, String> {
    match action.action_type.as_deref().unwrap_or("") {
        "Prompt" => {
            dispatch_prompt(
                db,
                search_engine,
                browser_actions,
                event_hooks,
                entity_name,
                action,
                message,
            )
            .await
        }
        "Actions" => {
            dispatch_actions(
                db,
                search_engine,
                browser_actions,
                event_hooks,
                entity_name,
                action,
                message,
            )
            .await
        }
        other => Err(format!(
            "dispatch_prompt_or_actions called with unexpected action_type '{other}'"
        )),
    }
}

// ── Prompt ───────────────────────────────────────────────────────────────────

async fn dispatch_prompt(
    db: Arc<dyn DbManager>,
    search_engine: Arc<dyn SearchManager>,
    browser_actions: Option<Arc<dyn BrowserActions>>,
    event_hooks: Option<Arc<crate::EventHooksConfig>>,
    entity_name: &str,
    action: &Action,
    message: &Value,
) -> Result<ActionExecResult, String> {
    let bin = action.bin_name.as_deref().ok_or_else(|| {
        "Prompt action requires exec_artifact (bin_name) to be set".to_string()
    })?;
    let artifact = db.get_artifact(bin)
        .await
        .map_err(|e| e.to_string())?;
    let bytes = load_artifact_bytes_from_entity(&artifact)?;
    let artifact_text = String::from_utf8(bytes)
        .map_err(|e| format!("prompt artifact is not valid UTF-8: {e}"))?;

    // ── Prompt override mode (on_prompt_load hook) ────────────────────
    // When the message contains "prompt_file", this action acts as a
    // prompt override rather than executing an instruction. The artifact
    // can be:
    //   - A JSON object map:  { "some.prompt.txt": "custom text", ... }
    //     Only the matching entry is returned. Unmatched keys are ignored
    //     (returns no "prompt" field so the default prompt is used).
    //   - Plain text: applied as a universal override for any prompt file.
    // "prompt_file" is echoed back in the result so subsequent actions
    // in the fire_sync chain still know which file is being loaded.
    if let Some(prompt_file) = message.get("prompt_file").and_then(|v| v.as_str()) {
        let override_text =
            if let Ok(map) = serde_json::from_str::<serde_json::Map<String, Value>>(&artifact_text) {
                map.get(prompt_file)
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            } else {
                // Plain text artifact — universal override for any prompt
                Some(artifact_text)
            };
        return Ok(ActionExecResult {
            action_name: entity_name.to_string(),
            result: match override_text {
                Some(prompt) => json!({ "prompt": prompt, "prompt_file": prompt_file }),
                None => json!({ "prompt_file": prompt_file }),
            },
            success: true,
            message: None,
            trace: Vec::new(),
        });
    }

    // ── Instruction execution mode (no prompt_file in message) ────────
    let instr_msg = json!({
        "instruction": artifact_text,
        "session_name": null,
        "allow_destructive": false,
    });
    instruction::try_execute(Arc::clone(&db), search_engine, browser_actions, event_hooks, Some("instruction_execute"), &instr_msg)
        .await
        .ok_or_else(|| "instruction_execute handler is not registered".to_string())?
}

// ── Actions ──────────────────────────────────────────────────────────────────

async fn dispatch_actions(
    db: Arc<dyn DbManager>,
    search_engine: Arc<dyn SearchManager>,
    browser_actions: Option<Arc<dyn BrowserActions>>,
    event_hooks: Option<Arc<crate::EventHooksConfig>>,
    entity_name: &str,
    action: &Action,
    message: &Value,
) -> Result<ActionExecResult, String> {
    let bin = action.bin_name.as_deref().ok_or_else(|| {
        "Actions action requires exec_artifact (bin_name) to be set".to_string()
    })?;
    let artifact = db.get_artifact(bin)
        .await
        .map_err(|e| e.to_string())?;
    let bytes = load_artifact_bytes_from_entity(&artifact)?;
    let script_text = String::from_utf8(bytes)
        .map_err(|e| format!("actions artifact is not valid UTF-8: {e}"))?;
    let script_value: Value = serde_json::from_str(&script_text)
        .map_err(|e| format!("actions artifact is not valid JSON: {e}"))?;
    // Delegate to the canonical executor so the `Actions`
    // action_type gets identical loop / if / step-ref / interpolation
    // behaviour to the instruction pipeline. The caller's
    // invocation params are surfaced as `$.step_0.result.<key>`
    // inside the script so action authors can write generic
    // orchestrators (e.g. a login flow that takes the user's
    // OAuth `client_id`) without baking per-call values into
    // the script's static step parameters.
    let mut result = crate::script_executor::dispatch_actions_artifact(
        Arc::clone(&db),
        Arc::clone(&search_engine),
        browser_actions.clone(),
        event_hooks.clone(),
        script_value,
        Some(message.clone()),
    )
    .await?;
    // Attach the entity name so the result carries the same
    // `action_name` as other dispatch paths.
    result.action_name = entity_name.to_string();
    Ok(result)
}