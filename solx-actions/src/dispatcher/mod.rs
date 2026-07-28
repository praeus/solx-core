//! Action dispatch chain — extracted from `manager.rs`.
//!
//! This module owns the entire action-execution pipeline:
//!
//! * [`execute_action_internal`] — public entry point; wraps
//!   [`execute_action_once`] and fires `on_action_completed` /
//!   `on_file_upload_completed` schedule events.
//! * [`execute_action_once`] — core dispatch by `action_type`
//!   (`Prompt` / `Actions` / `Webhook` / `Command`) with fallthrough to
//!   extraction / inquiry / instruction / browser-handler / WASM.
//! * [`actions`] — Prompt & Actions action-type handlers.
//! * [`commands`] — Command action-type handler + logging helpers.
//! * [`webhooks`] — Webhook action-type handler + OAuth token exchange.
//! * [`process_due_schedules`] / [`process_due_schedule_invocations`] /
//!   [`trigger_schedule_event`] — schedule-driven action execution.
//! * [`load_artifact_bytes_from_entity`] — shared artifact loader.
//!
//! The `LocalSolManager` in `manager.rs` and other modules (`wasm_host`,
//! `script_executor`, `event_hooks`) call into [`execute_action_internal`]
//! via `crate::dispatcher::execute_action_internal`.

pub(crate) mod actions;
pub(crate) mod commands;
pub(crate) mod webhooks;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use base64::Engine as _;
use serde_json::{json, Value};

use sol_core::{
    db::DbManager,
    entities::*,
    scheduler,
};
use sol_core::search::SearchManager;

use crate::browser_actions::BrowserActions;
use crate::{
    browser_handler,
    extraction,
    inquiry,
    instruction,
    loopback_control,
    system_browser,
    wasm_host,
};

// ── Native handler registry ──────────────────────────────────────────────────
//
// Every action mod (extraction, inquiry, instruction, browser_handler,
// system_browser, loopback_control) registers a `NativeHandler` closure
// into this registry at startup. The dispatcher's `try_execute` iterates
// the registry instead of hard-coding a 6-call chain, so adding a new
// action mod only requires implementing one `register()` function.
//
// Two flavours of handler coexist:
//
// * `push_native` — for mods whose `try_execute` takes owned
//   `Arc<dyn DbManager>` / `Arc<dyn SearchManager>` /
//   `Option<Arc<dyn BrowserActions>>` / `Option<Arc<EventHooksConfig>>`.
//   The closure clones the Arcs into the async block (cheap).
//
// * `push_borrowing` — for mods whose `try_execute` takes `&dyn DbManager`
//   and `Option<&Arc<dyn BrowserActions>>` plus an `Option<String>` for
//   the caller action. The closure holds the Arcs and borrows them
//   inside the async block; the borrow lifetime is the future's, so
//   everything stays Send + 'static.

type NativeHandlerFut = Pin<Box<dyn Future<Output = Option<Result<ActionExecResult, String>>> + Send>>;

/// Handler flavour. Native handlers take owned Arcs; borrowing handlers
/// hold the Arcs and pass borrows to their inner `try_execute`. Both
/// take `fn_name` and `caller_action` as owned `String` so the closures
/// can be `Send + 'static` without forcing `'static` borrows at the
/// call site.
type NativeHandler = Arc<
    dyn Fn(
            Arc<dyn DbManager>,
            Arc<dyn SearchManager>,
            Option<Arc<dyn BrowserActions>>,
            Option<Arc<crate::EventHooksConfig>>,
            Option<String>,
            Value,
        ) -> NativeHandlerFut
        + Send
        + Sync,
>;

type BorrowingHandler = Arc<
    dyn Fn(
            Arc<dyn DbManager>,
            Arc<dyn SearchManager>,
            Option<Arc<dyn BrowserActions>>,
            Option<Arc<crate::EventHooksConfig>>,
            Option<String>,
            Value,
            Option<String>,
        ) -> NativeHandlerFut
        + Send
        + Sync,
>;

/// Owning entry in the dispatch registry.
enum HandlerEntry {
    Native { handle: NativeHandler },
    Borrowing { handle: BorrowingHandler },
}

/// Thread-safe registry of action mod dispatchers. The dispatcher holds
/// one of these as a `OnceLock`; each mod's `register(&mut registry)`
/// function pushes an entry. Iteration stops at the first handler that
/// returns `Some(_)`. Public so action mods can implement
/// `pub fn register(registry: &mut HandlerRegistry)`.
pub struct HandlerRegistry {
    entries: Vec<HandlerEntry>,
}

impl HandlerRegistry {
    pub fn new() -> Self {
        Self { entries: Vec::new() }
    }

    /// Push a handler that takes owned Arcs.
    pub fn push_native(
        &mut self,
        handle: impl Fn(
            Arc<dyn DbManager>,
            Arc<dyn SearchManager>,
            Option<Arc<dyn BrowserActions>>,
            Option<Arc<crate::EventHooksConfig>>,
            Option<String>,
            Value,
        ) -> NativeHandlerFut
        + Send
        + Sync
        + 'static,
    ) {
        self.entries.push(HandlerEntry::Native { handle: Arc::new(handle) });
    }

    /// Push a handler that takes owned Arcs but internally borrows them
    /// to call a `try_execute` with the `&dyn DbManager` /
    /// `Option<&Arc<…>>` signature. Also forwards the caller action name
    /// as an owned `Option<String>` (so it can survive a `Pin<Box>`).
    pub fn push_borrowing(
        &mut self,
        handle: impl Fn(
            Arc<dyn DbManager>,
            Arc<dyn SearchManager>,
            Option<Arc<dyn BrowserActions>>,
            Option<Arc<crate::EventHooksConfig>>,
            Option<String>,
            Value,
            Option<String>,
        ) -> NativeHandlerFut
        + Send
        + Sync
        + 'static,
    ) {
        self.entries.push(HandlerEntry::Borrowing { handle: Arc::new(handle) });
    }

    /// Iterate every registered handler in order. Returns the first
    /// `Some(_)` result (preserving the original chain's fallthrough
    /// semantics). Stops on the first `Some(_)`.
    pub(crate) async fn try_execute(
        &self,
        db: Arc<dyn DbManager>,
        search_engine: Arc<dyn SearchManager>,
        browser_actions: Option<Arc<dyn BrowserActions>>,
        event_hooks: Option<Arc<crate::EventHooksConfig>>,
        fn_name: Option<String>,
        message: Value,
        caller_action: Option<String>,
    ) -> Option<Result<ActionExecResult, String>> {
        for entry in &self.entries {
            let outcome = match entry {
                HandlerEntry::Native { handle } => {
                    handle(
                        Arc::clone(&db),
                        Arc::clone(&search_engine),
                        browser_actions.clone(),
                        event_hooks.clone(),
                        fn_name.clone(),
                        message.clone(),
                    )
                    .await
                }
                HandlerEntry::Borrowing { handle } => {
                    handle(
                        Arc::clone(&db),
                        Arc::clone(&search_engine),
                        browser_actions.clone(),
                        event_hooks.clone(),
                        fn_name.clone(),
                        message.clone(),
                        caller_action.clone(),
                    )
                    .await
                }
            };
            if outcome.is_some() {
                return outcome;
            }
        }
        None
    }
}

/// Build the global handler registry. Called once at first dispatch;
/// the result is cached in a `OnceLock` and reused for every subsequent
/// call.
pub(crate) fn handler_registry() -> &'static HandlerRegistry {
    static REGISTRY: OnceLock<HandlerRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut reg = HandlerRegistry::new();
        extraction::builtins::register(&mut reg);
        inquiry::register(&mut reg);
        instruction::builtins::register(&mut reg);
        browser_handler::register(&mut reg);
        system_browser::register(&mut reg);
        loopback_control::register(&mut reg);
        reg
    })
}

// ── Public entry point ───────────────────────────────────────────────────────

pub(crate) async fn execute_action_internal(
    db: Arc<dyn DbManager>,
    search_engine: Arc<dyn SearchManager>,
    browser_actions: Option<Arc<dyn BrowserActions>>,
    event_hooks: Option<Arc<crate::EventHooksConfig>>,
    entity_name: &str,
    message: &Value,
    emit_events: bool,
) -> Result<Value, String> {
    let action = db.get_action(entity_name)
        .await
        .map_err(Into::<String>::into)?;
    let execution = execute_action_once(Arc::clone(&db), search_engine.clone(), browser_actions.clone(), event_hooks, entity_name, message)
        .await?;
    let execution_json = serde_json::to_value(&execution).unwrap_or(Value::Null);

    if emit_events && execution.success {
        let generic_event = json!( {
            "action_name": entity_name,
            "fn_name": action.fn_name.clone(),
            "params": message,
            "result": execution_json.clone(),
        });
        trigger_schedule_event(Arc::clone(&db), search_engine.clone(), browser_actions.clone(), "on_action_completed", &generic_event).await?;

        if action.fn_name.as_deref() == Some("artifact_upload_file") {
            let artifact_name = execution
                .result
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let specific_event = json!( {
                "action_name": entity_name,
                "artifact_name": artifact_name,
                "result": execution_json.clone(),
            });
            trigger_schedule_event(Arc::clone(&db), search_engine.clone(), browser_actions.clone(), "on_file_upload_completed", &specific_event).await?;
        }
    }

    Ok(execution_json)
}

// ── Core dispatch ────────────────────────────────────────────────────────────

async fn execute_action_once(
    db: Arc<dyn DbManager>,
    search_engine: Arc<dyn SearchManager>,
    browser_actions: Option<Arc<dyn BrowserActions>>,
    event_hooks: Option<Arc<crate::EventHooksConfig>>,
    entity_name: &str,
    message: &Value,
) -> Result<ActionExecResult, String> {
    let action = db.get_action(entity_name)
        .await
        .map_err(Into::<String>::into)?;

    if let Some(ref type_name) = action.parameter_type_name {
        match db.get_type_entity(type_name).await {
            Ok(te) => {
                sol_core::validation::validate_against_schema(message, &te.schema)
                    .map_err(|e| e.to_string())?;
            }
            Err(e) => {
                return Err(format!(
                    "could not load parameter schema for type '{type_name}': {e}"
                ));
            }
        }
    }

    // ── Typed dispatch based on action_type ──────────────────────────────────
    match action.action_type.as_deref().unwrap_or("") {
        "Prompt" | "Actions" => {
            return actions::dispatch_prompt_or_actions(
                Arc::clone(&db),
                Arc::clone(&search_engine),
                browser_actions.clone(),
                event_hooks,
                entity_name,
                &action,
                message,
            )
            .await;
        }
        "Webhook" => {
            let url = action.fn_name.as_deref().ok_or_else(|| {
                "Webhook action requires invoke_resource (fn_name) to be a URL".to_string()
            })?;
            // Fetch the action's ActionLink grants so its own allowlist
            // supplements the global `allowed_webhook_base_urls`.  Each
            // ActionLink points to a Link entity whose `link` field holds
            // the URL — we resolve them here.  Failures are non-fatal
            // (we fall back to the global allowlist only).
            let action_link_bases = collect_action_link_bases(&db, entity_name).await;
            return webhooks::dispatch_webhook(
                &Arc::clone(&db),
                entity_name,
                url,
                message,
                action.action_config.as_ref(),
                &action_link_bases,
            )
            .await;
        }
        "Command" => {
            let key = action.fn_name.as_deref().ok_or_else(|| {
                "Command action requires invoke_resource (fn_name) to be a command key".to_string()
            })?;
            let def = crate::resolve_command_key(key)?;
            // action_config.cwd overrides the registry cwd when present
            let config_cwd = action
                .action_config
                .as_ref()
                .and_then(|c| c.get("cwd"))
                .and_then(|v| v.as_str())
                .map(|s| crate::path_from_str(s));
            let cwd: Option<std::path::PathBuf> = config_cwd.or_else(|| {
                if let Some(ref dir) = def.cwd {
                    Some(crate::path_from_str(dir))
                } else {
                    Some(crate::sol_appdata_dir())
                }
            });
            return commands::dispatch_command(entity_name, &def.command, message, cwd.as_deref()).await;
        }
        _ => {} // fall through to WASM / fn_name dispatch below
    }

    if let Some(ref bin_name) = action.bin_name {
        // ── WASM component dispatch ─────────────────────────────────────
        let artifact_result = match db.get_artifact(bin_name).await {
            Ok(artifact) => Ok(artifact),
            Err(primary_err) => {
                let primary_msg = primary_err.to_string();
                let system_name = format!("system::{bin_name}");
                match db.get_artifact(&system_name).await {
                    Ok(artifact) => Ok(artifact),
                    Err(system_err) => {
                        let action_owned_name = format!("actions::{}::{}", entity_name, bin_name);
                        match db.get_artifact(&action_owned_name).await {
                            Ok(artifact) => Ok(artifact),
                            Err(action_err) => Err(sol_core::SolError::InvalidOperation(format!(
                                "failed to load WASM artifact '{bin_name}' ({primary_msg}; system fallback: {system_err}; action fallback: {action_err})"
                            ))),
                        }
                    }
                }
            }
        };

        match artifact_result {
            Ok(artifact) => {
                let wasm_bytes = if !artifact.data.trim().is_empty() {
                    base64::engine::general_purpose::STANDARD
                        .decode(&artifact.data)
                        .map_err(|e| format!("artifact data is not valid base64: {e}"))?
                } else if let Some(path) = artifact.path.as_deref() {
                    std::fs::read(path)
                        .map_err(|e| format!("failed to read artifact path '{path}': {e}"))?
                } else {
                    return Err("artifact has neither data nor path".to_string());
                };

                return wasm_host::exec(
                    Arc::clone(&db),
                    search_engine.clone(),
                    browser_actions.clone(),
                    &wasm_bytes,
                    action.fn_name.as_deref(),
                    message,
                    Some(entity_name),
                    action.trusted,
                )
                .await
                // Alternate format keeps the anyhow cause chain — a bare
                // to_string() reduces e.g. an instantiation failure to its
                // outermost context line, hiding which import mismatched.
                .map_err(|e| format!("{e:#}"));
            }
            Err(e) => {
                return Err(e.to_string());
            }
        }
    } else if let Some(value) = try_execute(&action, &db, &search_engine, &browser_actions, event_hooks, entity_name, message).await {
        return value;
    }

    Err(format!(
        "action '{}' has no registered handler (bin_name={:?}, fn_name={:?})",
        action.name, action.bin_name, action.fn_name,
    ))
}

async fn try_execute(
    action: &Action,
    db: &Arc<dyn DbManager>,
    search_engine: &Arc<dyn SearchManager>,
    browser_actions: &Option<Arc<dyn BrowserActions>>,
    event_hooks: Option<Arc<crate::EventHooksConfig>>,
    entity_name: &str,
    message: &Value,
) -> Option<Result<ActionExecResult, String>> {
    // Dispatch via the global handler registry. Each action mod has
    // registered a closure at startup; the registry iterates them in
    // order and returns the first `Some(_)`. The WASM-dispatched
    // actions are handled by the bin_name branch in `execute_action_once`
    // and never reach this fallthrough.
    handler_registry()
        .try_execute(
            Arc::clone(db),
            Arc::clone(search_engine),
            browser_actions.clone(),
            event_hooks,
            action.fn_name.clone(),
            message.clone(),
            Some(entity_name.to_string()),
        )
        .await
}

// ── Shared helpers ───────────────────────────────────────────────────────────

/// Collect the base-URL prefixes from all `Link` entities that are
/// attached to `action_name` via `ActionLink` records.
///
/// Returns an empty `Vec` on any failure (DB error, missing link, etc.)
/// so callers can fall back to the global allowlist without surfacing
/// transient lookup errors to the user.
///
/// Each returned entry is the full URL stored on the underlying `Link`
/// entity (e.g. `https://docs.googleapis.com/v1/documents/`).  The
/// dispatcher will accept any request URL that starts with one of these
/// prefixes, in addition to any prefix in the global
/// `allowed_webhook_base_urls` allowlist.
async fn collect_action_link_bases(
    db: &Arc<dyn DbManager>,
    action_name: &str,
) -> Vec<String> {
    let action_links = match db.list_action_links_for(action_name).await {
        Ok(links) => links,
        Err(_) => return Vec::new(),
    };
    let mut bases = Vec::with_capacity(action_links.len());
    for al in action_links {
        let Some(link_name) = al.link_name.as_deref() else {
            continue;
        };
        // Resolve the underlying Link entity to get its URL.
        match db.get_link(link_name).await {
            Ok(link) => bases.push(link.link),
            Err(_) => continue,
        }
    }
    bases
}

/// Decode base64 `artifact.data` or read `artifact.path` into raw bytes.
pub(crate) fn load_artifact_bytes_from_entity(artifact: &Artifact) -> Result<Vec<u8>, String> {
    if !artifact.data.trim().is_empty() {
        return base64::engine::general_purpose::STANDARD
            .decode(&artifact.data)
            .map_err(|e| format!("artifact data is not valid base64: {e}"));
    }
    if let Some(path) = artifact.path.as_deref() {
        return std::fs::read(path)
            .map_err(|e| format!("failed to read artifact path '{path}': {e}"));
    }
    Err("artifact has neither data nor path".to_string())
}

// ── Schedule-driven action execution ─────────────────────────────────────────

pub(crate) async fn process_due_schedule_invocations(
    db: Arc<dyn DbManager>,
    search_engine: Arc<dyn SearchManager>,
    browser_actions: Option<Arc<dyn BrowserActions>>,
    invocations: Vec<scheduler::ScheduleInvocation>,
) -> Result<(), String> {
    for invocation in invocations {
        match execute_action_once(
            Arc::clone(&db),
            search_engine.clone(),
            browser_actions.clone(),
            None,
            &invocation.action_name,
            &invocation.payload,
        )
        .await
        {
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    "scheduled action '{}' (schedule '{}') failed: {e}",
                    invocation.action_name, invocation.schedule_name
                );
                let _ = db
                    .set_schedule_status(&invocation.schedule_name, "failed", Some(&e))
                    .await;
            }
        }
    }
    Ok(())
}

pub(crate) async fn process_due_schedules(
    db: Arc<dyn DbManager>,
    search_engine: Arc<dyn SearchManager>,
    browser_actions: Option<Arc<dyn BrowserActions>>,
) -> Result<(), String> {
    let now = chrono::Utc::now();
    let schedules = db.list_schedules()
        .await
        .map_err(Into::<String>::into)?;
    let mut invocations = Vec::new();
    for sch in &schedules {
        if !sch.enabled || sch.event_name != "timer" {
            continue;
        }
        if let Some(max_runs) = sch.max_runs {
            if sch.run_count >= max_runs {
                continue;
            }
        }
        let due = sch.next_run_at.or(sch.trigger_at).map(|dt| dt <= now).unwrap_or(false);
        if due {
            let _ = db.mark_schedule_fired(sch, now).await;
            invocations.push(scheduler::ScheduleInvocation {
                schedule_name: sch.name.clone(),
                action_name: sch.action_name.clone(),
                event_name: "timer".to_string(),
                payload: json!( {
                    "schedule": sch.name,
                    "event": {"name": "timer", "payload": null},
                    "payload": sch.payload,
                }),
            });
        }
    }
    process_due_schedule_invocations(Arc::clone(&db), search_engine, browser_actions, invocations).await
}

async fn trigger_schedule_event(
    db: Arc<dyn DbManager>,
    search_engine: Arc<dyn SearchManager>,
    browser_actions: Option<Arc<dyn BrowserActions>>,
    event_name: &str,
    event_payload: &Value,
) -> Result<(), String> {
    let now = chrono::Utc::now();
    let schedules = db.list_schedules()
        .await
        .map_err(Into::<String>::into)?;
    let mut invocations = Vec::new();
    for sch in &schedules {
        if !sch.enabled || sch.event_name != event_name {
            continue;
        }
        if let Some(max_runs) = sch.max_runs {
            if sch.run_count >= max_runs {
                continue;
            }
        }
        let fired_names: Vec<String> = if let Some(n) = event_payload.get("artifact_name").and_then(|v| v.as_str()) {
            vec![n.to_string()]
        } else {
            event_payload.get("artifact_names").and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default()
        };
        let matches = sch.artifact_names.is_empty()
            || fired_names.iter().any(|n| sch.artifact_names.iter().any(|m| m == n));
        if matches {
            let _ = db.mark_schedule_fired(sch, now).await;
            invocations.push(scheduler::ScheduleInvocation {
                schedule_name: sch.name.clone(),
                action_name: sch.action_name.clone(),
                event_name: event_name.to_string(),
                payload: json!( {
                    "schedule": sch.name,
                    "event": {"name": event_name, "payload": event_payload},
                    "payload": sch.payload,
                }),
            });
        }
    }
    process_due_schedule_invocations(Arc::clone(&db), search_engine, browser_actions, invocations).await
}