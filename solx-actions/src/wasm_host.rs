//! WASM Component Model host executor for sol Action dispatch.
//!
//! Uses wasmtime with the component model to load and execute action components
//! compiled from the `backend-action` WIT world defined in `sol-actions/wit/`.
//!
//! # Architecture
//!
//! ```text
//!  entity_exec (Tauri command)
//!      │  action has bin_name + fn_name?
//!      ├─YES─► wasm_host::exec()
//!      │           ├─ load Artifact bytes (base64-decode)
//!      │           ├─ Component::from_binary()
//!      │           ├─ Linker: register document-ops + logger host functions
//!      │           ├─ BackendAction::instantiate()
//!      │           └─ runner.call_run(fn_name, params_json)
//!      └─NO──► operations::action::exec() (scaffold fallback)
//! ```
//!
//! # Host function bridge (async ↔ sync)
//!
//! The WIT component model imports are synchronous from the WASM guest's
//! perspective, but sol-core's DB operations are async.  We bridge them using
//! `tokio::task::block_in_place` which is safe on a multi-thread runtime
//! (the default for Tauri).

use std::sync::{Arc, OnceLock};

use anyhow::{Context as _, Result};
use base64::Engine as _;
use serde_json::Value;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiView, ResourceTable};

use crate::browser_actions::BrowserActions;
use crate::entity_type::EntityType;
use crate::{extraction, llm_service};
use sol_core::db::DbManager;
use sol_core::search::SearchManager;
use sol_core::entities::{
    ActionInput, ActionLinkInput, ArtifactInput, ArtifactScope, DocumentInput,
    DocumentLinkInput, KnowledgeBaseInput, LinkInput, ModelInput, ModelRoleInput,
    ScheduleInput, TypeEntityInput,
    ActionExecResult, TraceEntry,
};

// ── Wasmtime engine (created once, engine is thread-safe) ────────────────────

static ENGINE: OnceLock<Engine> = OnceLock::new();

fn engine() -> &'static Engine {
    ENGINE.get_or_init(|| {
        let mut cfg = Config::new();
        cfg.wasm_component_model(true);
        Engine::new(&cfg).expect("failed to create wasmtime engine")
    })
}

// ── WASM sandbox environment store ───────────────────────────────────────────
//
// WASM actions must never read the host process's environment directly (it can
// contain credentials, API keys, etc.).  Instead, all `get-env`/`set-env`
// calls go through this in-process map.
//
// Initialised once by `init_wasm_env`, seeded from the `env_mappings`
// allowlist in `sol-config.json`.  Any subsequent `set-env` calls from WASM
// update this shared map; they never touch `std::env`.

type WasmEnvMap = std::sync::RwLock<std::collections::HashMap<String, String>>;
static WASM_ENV: OnceLock<WasmEnvMap> = OnceLock::new();

/// Seed the WASM environment store from the `env_mappings` config allowlist.
///
/// Must be called once during manager initialisation, before any WASM action
/// is executed.  Subsequent calls are no-ops (the store is write-once).
///
/// `mappings` maps the key that WASM code will use → the system env var name
/// to read the initial value from.  Only these variables become visible to
/// WASM; the rest of the host environment remains inaccessible.
///
/// Values are read from the `SOL_ENV` map first, falling back to the real
/// system environment.
pub fn init_wasm_env(mappings: &std::collections::HashMap<String, String>) {
    let mut map = std::collections::HashMap::new();
    for (wasm_key, sys_key) in mappings {
        if let Some(val) = crate::sol_env_get(sys_key) {
            map.insert(wasm_key.clone(), val);
        }
    }
    // OnceLock::set silently ignores subsequent calls, so this is safe to
    // call from multiple code paths during startup.
    let _ = WASM_ENV.set(std::sync::RwLock::new(map));
}

/// Set a single key in the WASM environment store from the host side.
///
/// Useful for manager startup injection and for tests that need to make a
/// specific key visible to WASM without going through `sol-config.json`.
pub fn set_wasm_env(key: String, value: String) {
    if let Ok(mut map) = wasm_env().write() {
        map.insert(key, value);
    }
}

/// Read a single key from the in-process WASM environment store.
///
/// Returns `None` when the key is not present. This store is for
/// non-secret config only (feature flags, non-sensitive IDs) — secrets
/// must go through `crate::secrets`/the `get secret`/`set secret`
/// actions instead, which are scoped per-action.
pub fn get_wasm_env_value(key: &str) -> Option<String> {
    let map = wasm_env().read().ok()?;
    map.get(key).cloned()
}

/// Seed the WASM env store from the `wasm_env` block of `sol-config.json`.
///
/// Called once at manager startup, **after** `init_wasm_env` has loaded the
/// `env_mappings` allowlist.  Existing keys are not overwritten, so the
/// `env_mappings` allowlist takes precedence over the persisted env table.
pub fn init_wasm_env_from_disk() {
    let persisted = crate::load_wasm_env_from_config();
    if persisted.is_empty() {
        return;
    }
    if let Ok(mut map) = wasm_env().write() {
        for (key, value) in persisted {
            // Don't overwrite allowlist-seeded values.
            map.entry(key).or_insert(value);
        }
    }
}

fn wasm_env() -> &'static WasmEnvMap {
    WASM_ENV.get_or_init(|| std::sync::RwLock::new(std::collections::HashMap::new()))
}
#[cfg(feature = "trust")]
fn load_artifact_bytes_for_write_check(artifact: &sol_core::entities::Artifact) -> Result<Vec<u8>, String> {
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

async fn verify_action_artifact_signature_on_write(
    db: &dyn DbManager,
    action_name: &str,
    bin_name: Option<&str>,
) -> Result<(), String> {
    let Some(bin_name) = bin_name.map(str::trim).filter(|name| !name.is_empty()) else {
        return Ok(());
    };

    #[cfg(feature = "trust")]
    {
        if crate::trust_bypass_dev_enabled() {
            eprintln!(
                "sol-manager: SOL_TRUST_BYPASS_DEV is enabled; skipping action write-time signature verification for '{action_name}' (artifact '{bin_name}')"
            );
            return Ok(());
        }

        let artifact = match db.get_artifact(bin_name).await {
            Ok(artifact) => artifact,
            Err(primary_err) => {
                let primary_msg = primary_err.to_string();
                let system_name = format!("system::{bin_name}");
                db.get_artifact(&system_name).await.map_err(|_| {
                    format!(
                        "cannot create/update action '{action_name}': failed to load artifact '{bin_name}' ({primary_msg})"
                    )
                })?
            }
        };

        let artifact_bytes = load_artifact_bytes_for_write_check(&artifact)?;
        let trust_store = sol_trust::TrustStore::new(crate::sol_db_home());
        trust_store
            .verify_artifact_signature(bin_name, &artifact_bytes)
            .map_err(|e| {
                format!(
                    "cannot create/update action '{action_name}': artifact signature verification failed for '{bin_name}': {e}"
                )
            })?;
    }

    #[cfg(not(feature = "trust"))]
    {
        let _ = (db, action_name, bin_name);
    }

    Ok(())
}

// ── WIT bindings (generated at compile time from sol-actions/wit/) ───────────
//
// Path is relative to this crate's Cargo.toml (sol-browser/src-tauri/).
// The generated code includes:
//   • `BackendAction` — bound component type
//   • `sol::actions::document_ops::Host` — trait for the document-ops import
//   • `sol::actions::logger::Host` — trait for the logger import
//   • `exports::sol::actions::runner::Guest` — accessor for the runner export

wasmtime::component::bindgen!({
    world: "backend-action",
    path: "../sol-actions/wit",
});

/// Bindings for user-supplied WASM components compiled against the restricted
/// `custom-action` WIT world.  Provides only permission-gated action dispatch
/// and artifact reads — no direct DB, filesystem, model, or entity access.
mod custom_world {
    wasmtime::component::bindgen!({
        world: "custom-action",
        path: "../sol-actions/wit",
    });
}

// ── Host state ────────────────────────────────────────────────────────────────

/// State threaded through `wasmtime::Store` during a single action execution.
struct HostState {
    /// DB manager used by host functions to fulfil guest import calls.
    db: Arc<dyn DbManager>,
    search_engine: Arc<dyn SearchManager>,
    /// The entity name of the action currently executing (used for permission checks).
    action_name: Option<String>,
    /// The UUID of the action currently executing (used for sandbox directory paths so
    /// they match the artifact store layout: `artifacts/actions/<id>/`).
    action_id: Option<String>,
    /// Sub-operation trace collected during this execution.
    trace: Vec<TraceEntry>,
    /// Optional browser-actions host, propagated to recursive `entity_exec`
    /// calls so user WASM can dispatch the seeded browser actions.
    browser_actions: Option<Arc<dyn BrowserActions>>,
    /// WASI context (needed to satisfy wasi:io/poll and related imports from wasm32-wasip2).
    wasi: WasiCtx,
    /// WASI resource table.
    table: ResourceTable,
    /// Per-invocation log file: `<db_home>/actions/<action_name>/<date>_<id>.log`.
    log_file: Option<std::fs::File>,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> &mut WasiCtx { &mut self.wasi }
    fn table(&mut self) -> &mut ResourceTable { &mut self.table }
}

// Helper: run an async DB operation synchronously from within wasmtime's
// synchronous execution context.  This macro is only called from exec_sync,
// which always runs inside spawn_blocking — already a blocking thread.
// block_on is valid directly here; block_in_place is not needed and can panic
// if the tokio context state is unexpected (leading to UB through FFI).
macro_rules! block_on_db {
    ($expr:expr) => {
        tokio::runtime::Handle::current().block_on($expr)
    };
}

// ── Host trait implementations ────────────────────────────────────────────────

impl sol::actions::document_ops::Host for HostState {
    fn get_document(
        &mut self,
        name: String,
    ) -> Result<sol::actions::types::DocumentInfo, String> {
        let db = Arc::clone(&self.db);
        let action_name = self.action_name.clone();
        block_on_db!(async move {
            if let Some(ref aname) = action_name {
                db.check_document_permission(aname, &name, None)
                    .await.map_err(|e| e.to_string())?;
            }
            db.get_document(&name).await
                .map(|d| sol::actions::types::DocumentInfo {
                    id: d.id.to_string(),
                    name: d.name,
                    type_name: d.type_name,
                    content: Some(d.contents.to_string()),
                })
                .map_err(|e| e.to_string())
        })
    }

    fn create_document(
        &mut self,
        type_name: String,
        name: String,
        content: String,
    ) -> Result<sol::actions::types::DocumentInfo, String> {
        let db = Arc::clone(&self.db);
        let action_name = self.action_name.clone();
        let contents: Value = serde_json::from_str(&content)
            .unwrap_or_else(|_| serde_json::json!({}));
        let input = DocumentInput {
            id: None,
            name: name.clone(),
            title: None,
            summary: None,
            type_name: type_name.clone(),
            contents,
            knowledge_base_name: None,
            artifact_names: None,
            author: None,
            pub_date: None,
            confidence: None,
        };
        block_on_db!(async move {
            if let Some(ref aname) = action_name {
                db.check_document_permission(aname, &name, Some(&type_name))
                    .await.map_err(|e| e.to_string())?;
            }
            db.create_document(&input).await
                .map(|d| sol::actions::types::DocumentInfo {
                    id: d.id.to_string(),
                    name: d.name,
                    type_name: d.type_name,
                    content: Some(d.contents.to_string()),
                })
                .map_err(|e| e.to_string())
        })
    }

    fn update_document(
        &mut self,
        name: String,
        content: String,
    ) -> Result<sol::actions::types::DocumentInfo, String> {
        let db = Arc::clone(&self.db);
        let action_name = self.action_name.clone();
        let contents: Value = serde_json::from_str(&content)
            .unwrap_or_else(|_| serde_json::json!({}));
        block_on_db!(async move {
            if let Some(ref aname) = action_name {
                db.check_document_permission(aname, &name, None)
                    .await.map_err(|e| e.to_string())?;
            }
            let existing = db.get_document(&name).await.map_err(|e| e.to_string())?;
            let input = DocumentInput {
                id: None,
                name: name.clone(),
                title: existing.title.clone(),
                summary: existing.summary.clone(),
                type_name: existing.type_name.clone().unwrap_or_default(),
                contents,
                knowledge_base_name: existing.knowledge_base_name.clone(),
                artifact_names: None,
                author: existing.author.clone(),
                pub_date: existing.pub_date.clone(),
                confidence: existing.confidence,
            };
            db.update_document(&name, &input).await
                .map(|d| sol::actions::types::DocumentInfo {
                    id: d.id.to_string(),
                    name: d.name,
                    type_name: d.type_name,
                    content: Some(d.contents.to_string()),
                })
                .map_err(|e| e.to_string())
        })
    }

    fn delete_document(&mut self, name: String) -> Result<(), String> {
        let db = Arc::clone(&self.db);
        let action_name = self.action_name.clone();
        block_on_db!(async move {
            if let Some(ref aname) = action_name {
                db.check_document_permission(aname, &name, None)
                    .await.map_err(|e| e.to_string())?;
            }
            db.delete_document(&name).await.map_err(|e| e.to_string())
        })
    }

    fn get_field(
        &mut self,
        document_name: String,
        field: String,
    ) -> Result<String, String> {
        let db = Arc::clone(&self.db);
        let action_name = self.action_name.clone();
        block_on_db!(async move {
            if let Some(ref aname) = action_name {
                db.check_document_permission(aname, &document_name, None)
                    .await.map_err(|e| e.to_string())?;
            }
            let doc = db.get_document(&document_name).await.map_err(|e| e.to_string())?;
            let val = doc.contents.get(&field).cloned().unwrap_or(Value::Null);
            Ok::<String, String>(val.to_string())
        })
    }

    fn set_field(
        &mut self,
        document_name: String,
        field: String,
        value: String,
    ) -> Result<(), String> {
        let db = Arc::clone(&self.db);
        let action_name = self.action_name.clone();
        let new_val: Value = serde_json::from_str(&value)
            .map_err(|e| format!("invalid JSON value: {e}"))?;
        block_on_db!(async move {
            if let Some(ref aname) = action_name {
                db.check_document_permission(aname, &document_name, None)
                    .await.map_err(|e| e.to_string())?;
            }
            let existing = db.get_document(&document_name).await.map_err(|e| e.to_string())?;
            let mut contents = existing.contents.clone();
            if let Some(obj) = contents.as_object_mut() {
                obj.insert(field, new_val);
            } else {
                contents = serde_json::json!({ field: new_val });
            }
            let input = DocumentInput {
                id: None,
                name: document_name.clone(),
                title: existing.title.clone(),
                summary: existing.summary.clone(),
                type_name: existing.type_name.clone().unwrap_or_default(),
                contents,
                knowledge_base_name: existing.knowledge_base_name.clone(),
                artifact_names: None,
                author: existing.author.clone(),
                pub_date: existing.pub_date.clone(),
                confidence: existing.confidence,
            };
            db.update_document(&document_name, &input).await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    }
}

impl sol::actions::logger::Host for HostState {
    fn log(&mut self, message: String) {
        eprintln!("{message}");
        if let Some(ref mut f) = self.log_file {
            use std::io::Write as _;
            let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
            let _ = writeln!(f, "[{ts}] {message}");
        }
    }
}

// `types` interface contains only type definitions — the generated Host trait
// has no methods and exists only for trait-bound purposes.
impl sol::actions::types::Host for HostState {}

// ── entity-ops Host impl ──────────────────────────────────────────────────────

impl sol::actions::entity_ops::Host for HostState {
    fn entity_new(
        &mut self,
        type_name: String,
        name: String,
        payload: String,
    ) -> Result<String, String> {
        let et = match EntityType::from_str(&type_name) {
            Some(et @ (EntityType::Action | EntityType::Artifact | EntityType::Type
                | EntityType::Model | EntityType::ModelRole | EntityType::Schedule
                | EntityType::KnowledgeBase
                | EntityType::Link | EntityType::ActionLink | EntityType::DocumentLink
                | EntityType::Document)) => et,
            _ => return Err(format!("unsupported entity type: '{type_name}'")),
        };
        let db = Arc::clone(&self.db);
        let mut v: Value =
            serde_json::from_str(&payload).map_err(|e| format!("invalid JSON payload: {e}"))?;
        if let Some(obj) = v.as_object_mut() {
            obj.entry("name").or_insert(Value::String(name.clone()));
        }
        block_on_db!(async move {
            let result: Value = match et {
                EntityType::Action => {
                    let input: ActionInput = serde_json::from_value(v)
                        .map_err(|e| e.to_string())?;
                    if input.trusted == Some(true) {
                        return Err("cannot create trusted actions via entity_new".to_string());
                    }
                    verify_action_artifact_signature_on_write(
                        db.as_ref(),
                        &input.name,
                        input.bin_name.as_deref(),
                    )
                    .await?;
                    serde_json::to_value(db.create_action(&input).await.map_err(|e| e.to_string())?)
                        .map_err(|e| e.to_string())?
                }
                EntityType::Artifact => {
                    let input: ArtifactInput = serde_json::from_value(v)
                        .map_err(|e| e.to_string())?;
                    if matches!(input.owner_type, ArtifactScope::System) {
                        return Err("cannot create system artifacts via entity_new".to_string());
                    }
                    serde_json::to_value(db.create_artifact(&input).await.map_err(|e| e.to_string())?)
                        .map_err(|e| e.to_string())?
                }
                EntityType::Type => {
                    let input: TypeEntityInput = serde_json::from_value(v)
                        .map_err(|e| e.to_string())?;
                    serde_json::to_value(db.create_type_entity(&input).await.map_err(|e| e.to_string())?)
                        .map_err(|e| e.to_string())?
                }
                EntityType::Model => {
                    let input: ModelInput = serde_json::from_value(v)
                        .map_err(|e| e.to_string())?;
                    serde_json::to_value(db.create_model(&input).await.map_err(|e| e.to_string())?)
                        .map_err(|e| e.to_string())?
                }
                EntityType::ModelRole => {
                    let input: ModelRoleInput = serde_json::from_value(v)
                        .map_err(|e| e.to_string())?;
                    serde_json::to_value(db.create_model_role(&input).await.map_err(|e| e.to_string())?)
                        .map_err(|e| e.to_string())?
                }
                EntityType::Schedule => {
                    let input: ScheduleInput = serde_json::from_value(v)
                        .map_err(|e| e.to_string())?;
                    serde_json::to_value(db.create_schedule(&input).await.map_err(|e| e.to_string())?)
                        .map_err(|e| e.to_string())?
                }
                EntityType::KnowledgeBase => {
                    let input: KnowledgeBaseInput = serde_json::from_value(v)
                        .map_err(|e| e.to_string())?;
                    serde_json::to_value(db.create_knowledge_base(&input).await.map_err(|e| e.to_string())?)
                        .map_err(|e| e.to_string())?
                }
                EntityType::Link => {
                    let input: LinkInput = serde_json::from_value(v)
                        .map_err(|e| e.to_string())?;
                    serde_json::to_value(db.create_link(&input).await.map_err(|e| e.to_string())?)
                        .map_err(|e| e.to_string())?
                }
                EntityType::ActionLink => {
                    let input: ActionLinkInput = serde_json::from_value(v)
                        .map_err(|e| e.to_string())?;
                    serde_json::to_value(db.create_action_link(&input).await.map_err(|e| e.to_string())?)
                        .map_err(|e| e.to_string())?
                }
                EntityType::DocumentLink => {
                    let input: DocumentLinkInput = serde_json::from_value(v)
                        .map_err(|e| e.to_string())?;
                    serde_json::to_value(db.create_document_link(&input).await.map_err(|e| e.to_string())?)
                        .map_err(|e| e.to_string())?
                }
                /* Document */ _ => {
                    let mut input: DocumentInput = serde_json::from_value(v)
                        .map_err(|e| e.to_string())?;
                    if input.type_name.is_empty() {
                        input.type_name = type_name.clone();
                    }
                    serde_json::to_value(db.create_document(&input).await.map_err(|e| e.to_string())?)
                        .map_err(|e| e.to_string())?
                }
            };
            Ok::<String, String>(result.to_string())
        })
        .map_err(|e| e.to_string())
    }

    fn entity_get(
        &mut self,
        type_name: String,
        name: String,
    ) -> Result<String, String> {
        let et = match EntityType::from_str(&type_name) {
            Some(et @ (EntityType::Action | EntityType::Artifact | EntityType::Type
                | EntityType::Model | EntityType::ModelRole | EntityType::Schedule
                | EntityType::Permission | EntityType::KnowledgeBase
                | EntityType::Link | EntityType::ActionLink | EntityType::DocumentLink
                | EntityType::Document)) => et,
            _ => return Err(format!("unsupported entity type: '{type_name}'")),
        };
        let db = Arc::clone(&self.db);
        block_on_db!(async move {
            // TODO - check document and artifact permissions for the caller before allowing get
            let result: Value = match et {
                EntityType::Action        => serde_json::to_value(db.get_action(&name).await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?,
                EntityType::Artifact      => serde_json::to_value(db.get_artifact(&name).await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?,
                EntityType::Type          => serde_json::to_value(db.get_type_entity(&name).await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?,
                EntityType::Model         => serde_json::to_value(db.get_model(&name).await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?,
                EntityType::ModelRole     => serde_json::to_value(db.get_model_role(&name).await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?,
                EntityType::Permission    => serde_json::to_value(db.get_permission(&name).await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?,
                EntityType::Schedule      => serde_json::to_value(db.get_schedule(&name).await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?,
                EntityType::KnowledgeBase => serde_json::to_value(db.get_knowledge_base(&name).await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?,
                EntityType::Link          => serde_json::to_value(db.get_link(&name).await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?,
                EntityType::ActionLink    => serde_json::to_value(db.get_action_link(&name).await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?,
                EntityType::DocumentLink  => serde_json::to_value(db.get_document_link(&name).await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?,
                /* Document */ _          => serde_json::to_value(db.get_document(&name).await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?,
            };
            Ok::<String, String>(result.to_string())
        })
        .map_err(|e| e.to_string())
    }

    fn entity_set(
        &mut self,
        type_name: String,
        name: String,
        payload: String,
    ) -> Result<String, String> {
        let et = match EntityType::from_str(&type_name) {
            Some(et @ (EntityType::Action | EntityType::Artifact | EntityType::Type
                | EntityType::Model | EntityType::ModelRole | EntityType::Schedule
                | EntityType::KnowledgeBase
                | EntityType::Link | EntityType::ActionLink | EntityType::DocumentLink
                | EntityType::Document)) => et,
            _ => return Err(format!("unsupported entity type: '{type_name}'")),
        };
        let db = Arc::clone(&self.db);
        let action_name = self.action_name.clone();
        let mut v: Value =
            serde_json::from_str(&payload).map_err(|e| format!("invalid JSON payload: {e}"))?;
        if let Some(obj) = v.as_object_mut() {
            obj.entry("name").or_insert(Value::String(name.clone()));
        }
        block_on_db!(async move {
            let result: Value = match et {
                EntityType::Action => {
                    let input: ActionInput = serde_json::from_value(v).map_err(|e| e.to_string())?;
                    let existing_action = db.get_action(&name).await.map_err(|e| e.to_string())?;
                    if existing_action.trusted {
                        return Err(format!("action '{name}' is trusted and cannot be modified via entity_set"));
                    }
                    verify_action_artifact_signature_on_write(
                        db.as_ref(),
                        &input.name,
                        input.bin_name.as_deref(),
                    )
                    .await?;
                    serde_json::to_value(db.update_action(&name, &input).await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?
                }
                EntityType::Artifact => {
                    // TODO - check document permissions for the artifact before allowing update
                    let input: ArtifactInput = serde_json::from_value(v).map_err(|e| e.to_string())?;
                    if matches!(input.owner_type, ArtifactScope::System) {
                        return Err("cannot set owner_type to system via entity_set".to_string());
                    }
                    let existing_artifact = db.get_artifact_by_id(&name).await.map_err(|e| e.to_string())?;
                    if matches!(existing_artifact.owner_type, ArtifactScope::System) {
                        return Err(format!("artifact '{}' is system-managed and read-only", existing_artifact.qualified_name));
                    }
                    serde_json::to_value(db.update_artifact(&name, &input).await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?
                }
                EntityType::Type => {
                    let input: TypeEntityInput = serde_json::from_value(v).map_err(|e| e.to_string())?;
                    serde_json::to_value(db.update_type_entity(&name, &input).await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?
                }
                EntityType::Model => {
                    let input: ModelInput = serde_json::from_value(v).map_err(|e| e.to_string())?;
                    serde_json::to_value(db.update_model(&name, &input).await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?
                }
                EntityType::ModelRole => {
                    // ModelRole updates are restricted to (re)assigning the
                    // `model_name` field.  Name and description are
                    // write-once at creation.  We delegate to the dedicated
                    // `assign_model_role` operation for consistent semantics.
                    let model_name = match v.get("model_name") {
                        Some(Value::Null) => None,
                        Some(Value::String(s)) => Some(s.as_str()),
                        Some(_) => return Err("model_name must be a string or null".to_string()),
                        None => None,
                    };
                    serde_json::to_value(
                        db.assign_model_role(&name, model_name).await.map_err(|e| e.to_string())?
                    ).map_err(|e| e.to_string())?
                }
                EntityType::Schedule => {
                    let input: ScheduleInput = serde_json::from_value(v).map_err(|e| e.to_string())?;
                    serde_json::to_value(db.update_schedule(&name, &input).await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?
                }
                EntityType::KnowledgeBase => {
                    let input: KnowledgeBaseInput = serde_json::from_value(v).map_err(|e| e.to_string())?;
                    serde_json::to_value(db.update_knowledge_base(&name, &input).await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?
                }
                EntityType::Link => {
                    let input: LinkInput = serde_json::from_value(v).map_err(|e| e.to_string())?;
                    serde_json::to_value(db.update_link(&name, &input).await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?
                }
                EntityType::ActionLink => {
                    let input: ActionLinkInput = serde_json::from_value(v).map_err(|e| e.to_string())?;
                    serde_json::to_value(db.update_action_link(&name, &input).await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?
                }
                EntityType::DocumentLink => {
                    let input: DocumentLinkInput = serde_json::from_value(v).map_err(|e| e.to_string())?;
                    serde_json::to_value(db.update_document_link(&name, &input).await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?
                }
                /* Document */ _ => {
                    if let Some(ref action_name) = action_name {
                        db.check_document_permission(action_name, &name, None).await.map_err(|e| e.to_string())?;
                    }
                    let input: DocumentInput = serde_json::from_value(v).map_err(|e| e.to_string())?;
                    serde_json::to_value(db.update_document(&name, &input).await.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?
                }
            };
            Ok::<String, String>(result.to_string())
        })
        .map_err(|e| e.to_string())
    }

    fn entity_delete(
        &mut self,
        type_name: String,
        name: String,
    ) -> Result<(), String> {
        let et = match EntityType::from_str(&type_name) {
            // ModelRole is intentionally absent: roles are registered at
            // startup and can only be (un)assigned, never deleted.
            Some(et @ (EntityType::Action | EntityType::Artifact | EntityType::Type
                | EntityType::Model | EntityType::Schedule
                | EntityType::KnowledgeBase
                | EntityType::Link | EntityType::ActionLink | EntityType::DocumentLink
                | EntityType::Document)) => et,
            _ => return Err(format!("unsupported entity type: '{type_name}'")),
        };
        let db = Arc::clone(&self.db);
        let action_name = self.action_name.clone();
        block_on_db!(async move {
            match et {
                EntityType::Action => {
                    let a = db.get_action(&name).await.map_err(|e| e.to_string())?;
                    if a.trusted {
                        return Err(format!("action '{name}' is trusted and cannot be deleted"));
                    }
                    db.delete_action(&name).await.map_err(|e| e.to_string())
                }
                EntityType::Artifact => {
                    // TODO - check document permissions for the artifact before allowing delete
                    if name.starts_with("system::") {
                        return Err(format!("artifact '{name}' is system-managed and cannot be deleted"));
                    }
                    db.delete_artifact(&name).await.map_err(|e| e.to_string())
                }
                EntityType::Type          => db.delete_type_entity(&name).await.map_err(|e| e.to_string()),
                EntityType::Model         => db.delete_model(&name).await.map_err(|e| e.to_string()),
                EntityType::Schedule      => db.delete_schedule(&name).await.map_err(|e| e.to_string()),
                EntityType::KnowledgeBase => db.delete_knowledge_base(&name).await.map_err(|e| e.to_string()),
                EntityType::Link          => db.delete_link(&name).await.map_err(|e| e.to_string()),
                EntityType::ActionLink    => db.delete_action_link(&name).await.map_err(|e| e.to_string()),
                EntityType::DocumentLink  => db.delete_document_link(&name).await.map_err(|e| e.to_string()),
                /* Document */ _ => {
                    if let Some(ref action_name) = action_name {
                        db.check_document_permission(action_name, &name, None).await.map_err(|e| e.to_string())?;
                    }
                    db.delete_document(&name).await.map_err(|e| e.to_string())
                }
            }
        })
        .map_err(|e| e.to_string())
    }

    fn entity_list(
        &mut self,
        type_name: String,
        params: String,
    ) -> Result<String, String> {
        let et = match EntityType::from_str(&type_name) {
            Some(et @ (EntityType::Action | EntityType::Artifact | EntityType::Type
                | EntityType::Model | EntityType::ModelRole | EntityType::Schedule
                | EntityType::Permission | EntityType::KnowledgeBase
                | EntityType::Link | EntityType::ActionLink | EntityType::DocumentLink
                | EntityType::Document)) => et,
            _ => return Err(format!("unsupported entity type: '{type_name}'")),
        };
        let db = Arc::clone(&self.db);

        // Parse optional ListOptions from the params JSON string.
        let opts: Option<sol_surface::ListOptions> = if params.trim().is_empty() || params == "{}" {
            None
        } else {
            serde_json::from_str(&params).ok()
        };
        let use_paged = opts.as_ref().map_or(false, |o| o.has_any());

        block_on_db!(async move {
            let values: Vec<Value> = match et {
                EntityType::Action if use_paged =>
                    db.list_actions_paged(opts.as_ref().unwrap()).await.map_err(|e| e.to_string())?.into_iter().map(|e| serde_json::to_value(e).unwrap()).collect(),
                EntityType::Action =>
                    db.list_actions().await.map_err(|e| e.to_string())?.into_iter().map(|e| serde_json::to_value(e).unwrap()).collect(),
                EntityType::Artifact if use_paged =>
                    db.list_artifacts_paged(opts.as_ref().unwrap()).await.map_err(|e| e.to_string())?.into_iter().map(|e| serde_json::to_value(e).unwrap()).collect(),
                EntityType::Artifact =>
                    db.list_artifacts().await.map_err(|e| e.to_string())?.into_iter().map(|e| serde_json::to_value(e).unwrap()).collect(),
                EntityType::Type if use_paged =>
                    db.list_type_entities_paged(opts.as_ref().unwrap()).await.map_err(|e| e.to_string())?.into_iter().map(|e| serde_json::to_value(e).unwrap()).collect(),
                EntityType::Type =>
                    db.list_type_entities().await.map_err(|e| e.to_string())?.into_iter().map(|e| serde_json::to_value(e).unwrap()).collect(),
                EntityType::Model if use_paged =>
                    db.list_models_paged(opts.as_ref().unwrap()).await.map_err(|e| e.to_string())?.into_iter().map(|e| serde_json::to_value(e).unwrap()).collect(),
                EntityType::Model =>
                    db.list_models().await.map_err(|e| e.to_string())?.into_iter().map(|e| serde_json::to_value(e).unwrap()).collect(),
                EntityType::ModelRole if use_paged =>
                    db.list_model_roles_paged(opts.as_ref().unwrap()).await.map_err(|e| e.to_string())?.into_iter().map(|e| serde_json::to_value(e).unwrap()).collect(),
                EntityType::ModelRole =>
                    db.list_model_roles().await.map_err(|e| e.to_string())?.into_iter().map(|e| serde_json::to_value(e).unwrap()).collect(),
                EntityType::Permission if use_paged =>
                    db.list_permissions_paged(opts.as_ref().unwrap()).await.map_err(|e| e.to_string())?.into_iter().map(|e| serde_json::to_value(e).unwrap()).collect(),
                EntityType::Permission =>
                    db.list_permissions().await.map_err(|e| e.to_string())?.into_iter().map(|e| serde_json::to_value(e).unwrap()).collect(),
                EntityType::Schedule if use_paged =>
                    db.list_schedules_paged(opts.as_ref().unwrap()).await.map_err(|e| e.to_string())?.into_iter().map(|e| serde_json::to_value(e).unwrap()).collect(),
                EntityType::Schedule =>
                    db.list_schedules().await.map_err(|e| e.to_string())?.into_iter().map(|e| serde_json::to_value(e).unwrap()).collect(),
                EntityType::KnowledgeBase if use_paged =>
                    db.list_knowledge_bases_paged(opts.as_ref().unwrap()).await.map_err(|e| e.to_string())?.into_iter().map(|e| serde_json::to_value(e).unwrap()).collect(),
                EntityType::KnowledgeBase =>
                    db.list_knowledge_bases().await.map_err(|e| e.to_string())?.into_iter().map(|e| serde_json::to_value(e).unwrap()).collect(),
                EntityType::Link if use_paged =>
                    db.list_links_paged(opts.as_ref().unwrap()).await.map_err(|e| e.to_string())?.into_iter().map(|e| serde_json::to_value(e).unwrap()).collect(),
                EntityType::Link =>
                    db.list_links().await.map_err(|e| e.to_string())?.into_iter().map(|e| serde_json::to_value(e).unwrap()).collect(),
                EntityType::ActionLink if use_paged =>
                    db.list_action_links_paged(opts.as_ref().unwrap()).await.map_err(|e| e.to_string())?.into_iter().map(|e| serde_json::to_value(e).unwrap()).collect(),
                EntityType::ActionLink =>
                    db.list_action_links().await.map_err(|e| e.to_string())?.into_iter().map(|e| serde_json::to_value(e).unwrap()).collect(),
                EntityType::DocumentLink if use_paged =>
                    db.list_document_links_paged(opts.as_ref().unwrap()).await.map_err(|e| e.to_string())?.into_iter().map(|e| serde_json::to_value(e).unwrap()).collect(),
                EntityType::DocumentLink =>
                    db.list_document_links().await.map_err(|e| e.to_string())?.into_iter().map(|e| serde_json::to_value(e).unwrap()).collect(),
                _ if use_paged =>
                    db.list_documents_paged(opts.as_ref().unwrap()).await.map_err(|e| e.to_string())?.into_iter().map(|e| serde_json::to_value(e).unwrap()).collect(),
                _ =>
                    db.list_documents().await.map_err(|e| e.to_string())?.into_iter().map(|e| serde_json::to_value(e).unwrap()).collect(),
            };
            Ok::<String, String>(
                serde_json::to_string(&values).unwrap_or_else(|_| "[]".to_string()),
            )
        })
        .map_err(|e| e.to_string())
    }

    fn entity_exec(
        &mut self,
        type_name: String,
        name: String,
        payload: String,
    ) -> Result<String, String> {
        if EntityType::from_str(&type_name) != Some(EntityType::Action) {
            return Err(format!("exec is only supported for type 'Action', got '{type_name}'"));
        }
        let start = std::time::Instant::now();
        let name_clone = name.clone();
        let db = Arc::clone(&self.db);
        let caller_action = self.action_name.clone();
        let browser_actions = self.browser_actions.clone();
        let search_engine = self.search_engine.clone();
        let message: Value = serde_json::from_str(&payload).map_err(|e| format!("invalid JSON payload: {e}"))?;
        let result = block_on_db!(async move {
            // Permission check: verify the caller is allowed to invoke the target action.
            if let Some(ref caller) = caller_action {
                db.check_action_permission(caller, &name)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            let result = crate::dispatcher::execute_action_internal(
                db,
                search_engine,
                browser_actions,
                None,
                &name,
                &message,
                true,
            )
            .await
            .map_err(|e| e.to_string())?;
            Ok::<String, String>(result.to_string())
        })
        .map_err(|e| e.to_string());
        self.trace.push(TraceEntry {
            kind: "entity_exec".to_string(),
            name: name_clone,
            duration_ms: start.elapsed().as_millis() as u64,
            success: result.is_ok(),
        });
        result
    }
}

// ── system-ops Host impl ──────────────────────────────────────────────────────
impl sol::actions::system_ops::Host for HostState {
    fn read_file(&mut self, path: String) -> Result<String, String> {
        let (host_path, _) = sandbox_resolve(&path, self.action_id.as_deref())?;
        std::fs::read_to_string(&host_path).map_err(|e| format!("read_file '{path}': {e}"))
    }

    fn write_file(&mut self, path: String, content: String) -> Result<(), String> {
        let (host_path, read_only) = sandbox_resolve(&path, self.action_id.as_deref())?;
        if read_only {
            return Err(format!("write_file '{path}': absolute paths are read-only"));
        }
        if let Some(parent) = host_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("write_file create_dir '{}': {e}", parent.display()))?;
        }
        std::fs::write(&host_path, content.as_bytes())
            .map_err(|e| format!("write_file '{path}': {e}"))
    }

    fn list_dir(&mut self, path: String) -> Result<Vec<String>, String> {
        let (host_path, _) = sandbox_resolve(&path, self.action_id.as_deref())?;
        let rd = std::fs::read_dir(&host_path)
            .map_err(|e| format!("list_dir '{path}': {e}"))?;
        let mut entries = Vec::new();
        for entry in rd {
            let entry = entry.map_err(|e| format!("list_dir entry error: {e}"))?;
            entries.push(entry.file_name().to_string_lossy().into_owned());
        }
        Ok(entries)
    }

    fn copy_file(&mut self, source_path: String, destination_path: String) -> Result<(), String> {
        let (src, _) = sandbox_resolve(&source_path, self.action_id.as_deref())?;
        let (dst, dst_read_only) = sandbox_resolve(&destination_path, self.action_id.as_deref())?;
        if dst_read_only {
            return Err(format!(
                "copy_file destination '{destination_path}': absolute paths are read-only"
            ));
        }
        if !src.is_file() {
            return Err(format!("copy_file source is not a file: {source_path}"));
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("copy_file create_dir_all '{}': {e}", parent.display()))?;
        }
        std::fs::copy(&src, &dst)
            .map(|_| ())
            .map_err(|e| format!("copy_file '{source_path}' -> '{destination_path}': {e}"))
    }

    fn copy_dir(&mut self, source_path: String, destination_path: String) -> Result<(), String> {
        fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<(), String> {
            std::fs::create_dir_all(dst)
                .map_err(|e| format!("copy_dir create_dir_all '{}': {e}", dst.display()))?;
            for entry in std::fs::read_dir(src)
                .map_err(|e| format!("copy_dir read_dir '{}': {e}", src.display()))?
            {
                let entry = entry.map_err(|e| format!("copy_dir read_dir entry: {e}"))?;
                let file_type = entry
                    .file_type()
                    .map_err(|e| format!("copy_dir file_type: {e}"))?;
                // Skip symlinks — following them could escape the sandbox.
                if file_type.is_symlink() {
                    continue;
                }
                let path = entry.path();
                let target = dst.join(entry.file_name());
                if file_type.is_dir() {
                    copy_dir_recursive(&path, &target)?;
                } else {
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| {
                            format!("copy_dir create_dir_all '{}': {e}", parent.display())
                        })?;
                    }
                    std::fs::copy(&path, &target).map_err(|e| {
                        format!(
                            "copy_dir copy '{}' -> '{}': {e}",
                            path.display(),
                            target.display()
                        )
                    })?;
                }
            }
            Ok(())
        }

        let (src, _) = sandbox_resolve(&source_path, self.action_id.as_deref())?;
        let (dst, dst_read_only) = sandbox_resolve(&destination_path, self.action_id.as_deref())?;
        if dst_read_only {
            return Err(format!(
                "copy_dir destination '{destination_path}': absolute paths are read-only"
            ));
        }
        if !src.is_dir() {
            return Err(format!("copy_dir source is not a directory: {source_path}"));
        }
        copy_dir_recursive(&src, &dst)
    }

    fn db_home(&mut self) -> Result<String, String> {
        Ok(crate::sol_db_home().to_string_lossy().to_string())
    }

    fn gen_uuid(&mut self) -> String {
        uuid::Uuid::new_v4().to_string()
    }

    fn get_env(&mut self, key: String) -> Option<String> {
        wasm_env().read().ok()?.get(&key).cloned()
    }

    fn set_env(&mut self, key: String, value: String) {
        if let Ok(mut map) = wasm_env().write() {
            map.insert(key.clone(), value.clone());
        }
        // Persist to sol-config.json so the value survives restarts.
        // Best-effort: a failure here doesn't break the in-process write.
        let _ = crate::upsert_wasm_env(&key, &value);
    }

    fn fetch_html(&mut self, url: String) -> Result<String, String> {
        block_on_db!(crate::manager::fetch_html(&url))
    }

    fn fetch_and_extract(
        &mut self,
        source_document_name: String,
        url: String,
        knowledge_base_name: Option<String>,
        selected_type_name: Option<String>,
        extraction_prompt: Option<String>,
    ) -> Result<String, String> {
        let db = Arc::clone(&self.db);
        block_on_db!(async move {
            let html = crate::manager::fetch_html(&url).await.map_err(|e| e.to_string())?;
            let result = extraction::execute_extract(
                Arc::clone(&db),
                &source_document_name,
                Some(&html),
                None,
                Some(url),
                knowledge_base_name,
                selected_type_name,
                extraction_prompt,
                None,
            )
            .await
            .map_err(|e| e.to_string())?;
            Ok::<String, String>(result.to_string())
        })
        .map_err(|e| e.to_string())
    }
}

// ── secrets Host impl ─────────────────────────────────────────────────────────
//
// Unlike get_env/set_env above (one shared map, visible to every trusted
// action), these calls are scoped: the decryption key for `name` is looked
// up in the *calling* action's own `action_config.secrets` map. An action
// with no entry for `name` there gets `None`/an error, never a value —
// access is denied by the absence of a key, not by an explicit check.

impl sol::actions::secrets::Host for HostState {
    fn get_secret(&mut self, name: String) -> Option<String> {
        let action_name = self.action_name.clone()?;
        let db = Arc::clone(&self.db);
        let key = block_on_db!(async move { db.get_action(&action_name).await.ok() })?
            .action_config?
            .get("secrets")?
            .get(&name)?
            .as_str()?
            .to_string();
        crate::secrets::get_secret(&name, &key).ok().flatten()
    }

    fn set_secret(&mut self, name: String, value: String) -> Result<(), String> {
        let action_name = self
            .action_name
            .clone()
            .ok_or_else(|| "no action context for set-secret".to_string())?;
        let db = Arc::clone(&self.db);
        let action = block_on_db!(db.get_action(&action_name)).map_err(|e| e.to_string())?;
        let key = action
            .action_config
            .as_ref()
            .and_then(|c| c.get("secrets"))
            .and_then(|s| s.get(&name))
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("action '{action_name}' is not configured with a key for secret '{name}'"))?;
        crate::secrets::set_secret(&name, &value, key)
    }
}

// ── model-ops Host impl ───────────────────────────────────────────────────────

impl sol::actions::model_ops::Host for HostState {
    fn model_chat(&mut self, model_name: String, messages: String) -> Result<String, String> {
        let start = std::time::Instant::now();
        let model_name_clone = model_name.clone();
        let db = Arc::clone(&self.db);
        let caller_action = self.action_name.clone();
        let result = block_on_db!(async move {
            // Resolve selector (model name OR model role) before the permission
            // check so role strings like "instructor" work correctly.
            if let Some(ref caller) = caller_action {
                let info = llm_service::resolve_model_info(db.as_ref(), &model_name)
                    .await
                    .map_err(|e| format!("model '{}' not found: {e}", model_name))?;
                db.check_model_permission(caller, &info.model_name)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            llm_service::chat(db.as_ref(), &model_name, &messages).await
                .map_err(|e| format!("model chat error: {e}"))
        })
        .map_err(|e| e.to_string());
        self.trace.push(TraceEntry {
            kind: "model_chat".to_string(),
            name: model_name_clone,
            duration_ms: start.elapsed().as_millis() as u64,
            success: result.is_ok(),
        });
        result
    }

    fn model_generate(&mut self, model_name: String, prompt: String) -> Result<String, String> {
        let start = std::time::Instant::now();
        let model_name_clone = model_name.clone();
        let db = Arc::clone(&self.db);
        let caller_action = self.action_name.clone();
        let result = block_on_db!(async move {
            // Resolve selector (model name OR model role) before the permission
            // check so role strings like "instructor" work correctly.
            if let Some(ref caller) = caller_action {
                let info = llm_service::resolve_model_info(db.as_ref(), &model_name)
                    .await
                    .map_err(|e| format!("model '{}' not found: {e}", model_name))?;
                db.check_model_permission(caller, &info.model_name)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            // generate_with_selector resolves name-or-role and handles mock providers.
            llm_service::generate_with_selector(db.as_ref(), &model_name, &prompt)
                .await
                .map_err(|e| format!("model generate error: {e}"))
        })
        .map_err(|e| e.to_string());
        self.trace.push(TraceEntry {
            kind: "model_generate".to_string(),
            name: model_name_clone,
            duration_ms: start.elapsed().as_millis() as u64,
            success: result.is_ok(),
        });
        result
    }

    fn model_list(&mut self) -> Result<String, String> {
        let db = Arc::clone(&self.db);
        block_on_db!(async move {
            let models = db.list_models()
                .await
                .map_err(|e| e.to_string())?;
            let names: Vec<serde_json::Value> =
                models.into_iter().map(|m| serde_json::json!(m.name)).collect();
            serde_json::to_string(&names).map_err(|e| e.to_string())
        })
        .map_err(|e| e.to_string())
    }
}

// ── python-ops Host impl ─────────────────────────────────────────────────────

impl sol::actions::python_ops::Host for HostState {
    fn eval_python(
        &mut self,
        _script: String,
        _input: String,
        _site_packages_path: Option<String>,
    ) -> Result<String, String> {
        Err("Python actions are no longer supported. Port your script to a Rust WASM component using the custom-action template (see sol-browser/src-tauri/custom-actions/rust-action).".to_string())
    }
}

// ── Public executor ───────────────────────────────────────────────────────────

/// Execute a WASM action component.
///
/// * `wasm_bytes` — raw WASM binary (the Artifact data, base64-decoded)
/// * `fn_name`    — action name forwarded to the component's `run` export as
///                  `action-name`; `None` for single-action components
/// * `params`     — JSON-encoded parameter object
// ── custom-action world Host trait implementations ────────────────────────────

impl custom_world::sol::actions::types::Host for HostState {}

impl custom_world::sol::actions::logger::Host for HostState {
    fn log(&mut self, message: String) {
        eprintln!("{message}");
        if let Some(ref mut f) = self.log_file {
            use std::io::Write as _;
            let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
            let _ = writeln!(f, "[{ts}] {message}");
        }
    }
}

impl custom_world::sol::actions::action_exec::Host for HostState {
    fn exec(
        &mut self,
        action_name: String,
        payload: String,
    ) -> Result<custom_world::sol::actions::types::ActionResult, String> {
        let db = Arc::clone(&self.db);
        let caller_action = self.action_name.clone();
        let browser_actions = self.browser_actions.clone();
        let search_engine = self.search_engine.clone();
        let name_clone = action_name.clone();
        let message: Value = serde_json::from_str(&payload)
            .map_err(|e| format!("invalid JSON payload: {e}"))?;
        let start = std::time::Instant::now();
        let result = block_on_db!(async move {
            if let Some(ref caller) = caller_action {
                db.check_action_permission(caller, &action_name)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            crate::dispatcher::execute_action_internal(
                db,
                search_engine,
                browser_actions,
                None,
                &action_name,
                &message,
                true,
            )
            .await
            .map_err(|e| e.to_string())
        });
        self.trace.push(TraceEntry {
            kind: "entity_exec".to_string(),
            name: name_clone,
            duration_ms: start.elapsed().as_millis() as u64,
            success: result.is_ok(),
        });
        let val = result?;
        Ok(custom_world::sol::actions::types::ActionResult {
            success: val["success"].as_bool().unwrap_or(false),
            message: val["message"].as_str().map(str::to_string),
            output: val.get("result").map(|v| v.to_string()),
        })
    }
}

impl custom_world::sol::actions::artifact_read::Host for HostState {
    fn read(&mut self, name: String) -> Result<Vec<u8>, String> {
        let db = Arc::clone(&self.db);
        let caller = self.action_name.clone();
        block_on_db!(async move {
            if let Some(ref caller_name) = caller {
                db.check_action_permission(caller_name, "get artifact")
                    .await
                    .map_err(|e| e.to_string())?;
            }
            let artifact = db.get_artifact(&name)
                .await
                .map_err(|e| e.to_string())?;
            if let Some(path) = artifact.path {
                std::fs::read(&path)
                    .map_err(|e| format!("artifact_read '{name}': {e}"))
            } else {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&artifact.data)
                    .unwrap_or_else(|_| artifact.data.into_bytes());
                Ok(bytes)
            }
        })
        .map_err(|e| e.to_string())
    }
}

pub async fn exec(
    db: Arc<dyn DbManager>,
    search_engine: Arc<dyn SearchManager>,
    browser_actions: Option<Arc<dyn BrowserActions>>,
    wasm_bytes: &[u8],
    fn_name: Option<&str>,
    params: &Value,
    action_name: Option<&str>,
    trusted: bool,
) -> Result<ActionExecResult> {
    let params_json = serde_json::to_string(params)
        .context("failed to serialise params")?;

    let fn_name = fn_name.map(str::to_string);
    let action_name = action_name.map(str::to_string);
    let bytes = wasm_bytes.to_vec();

    // Run the synchronous wasmtime work on a blocking thread so we don't
    // block the async executor.
    //
    // catch_unwind guards against panics inside the wasmtime JIT or host
    // function trampolines propagating through the C FFI boundary, which is
    // undefined behaviour and manifests as STATUS_ACCESS_VIOLATION on Windows.
    tokio::task::spawn_blocking(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            exec_sync(
                db,
                search_engine,
                browser_actions,
                &bytes,
                fn_name.as_deref(),
                &params_json,
                action_name.as_deref(),
                trusted,
            )
        }))
        .map_err(|e| {
            let msg = e
                .downcast_ref::<String>()
                .map(String::clone)
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown panic in WASM execution".to_string());
            anyhow::anyhow!("panic in WASM execution (action={:?}): {msg}", fn_name)
        })?
    })
    .await
    .context("wasmtime blocking task panicked")?
}

// ── Sandbox helpers ──────────────────────────────────────────────────────────

/// Sanitise an action name into a safe directory component.
/// `::` (sol namespace separator) → `--`; all other path-unsafe characters
/// (Windows forbids `< > : " / \ | ? *`) → `_`.
fn sanitize_action_name(name: &str) -> String {
    name.replace("::", "--")
        .replace(['/', '\\', '<', '>', '|', '?', '*', '"', ':'], "_")
}

/// Root of the read-only sandbox: `<db_home>/artifacts/`.
fn sandbox_artifacts_root() -> std::path::PathBuf {
    crate::artifact_store_dir()
}

/// Root of the read-write action sandbox: `<db_home>/artifacts/actions/<id>/`.
/// Uses the action UUID so the path matches the artifact store layout produced
/// by `ArtifactNameParts::storage_relative_path_with_id`.
fn sandbox_action_dir(action_id: &str) -> std::path::PathBuf {
    crate::artifact_store_dir()
        .join("actions")
        .join(sanitize_action_name(action_id))
}

/// Lexically resolve `..` and `.` in `path`, then verify the result stays
/// within `root`.  Returns an error if the path escapes.
///
/// For paths that already exist on-disk, also performs a `canonicalize` check
/// to catch symlinks that point outside the sandbox.
fn lexical_sandbox_check(
    root: &std::path::Path,
    path: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    use std::path::Component;
    let mut parts: Vec<Component<'_>> = Vec::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                // Pop only if we are above the root prefix/root-dir components;
                // if the stack is empty (or only holds prefix/rootdir) the path
                // is trying to escape, which we catch after the loop.
                let last_is_normal = parts.last().map_or(false, |c| {
                    matches!(c, Component::Normal(_))
                });
                if last_is_normal {
                    parts.pop();
                }
                // If not normal, silently absorb: the boundary check below
                // will catch any actual escape.
            }
            Component::CurDir => {}
            c => parts.push(c),
        }
    }
    let normalized: std::path::PathBuf = parts.iter().collect();
    if !normalized.starts_with(root) {
        return Err(format!(
            "path '{}' escapes the sandbox (root: '{}')",
            path.display(),
            root.display()
        ));
    }
    // If the path already exists, canonicalize to catch symlinks pointing
    // outside the sandbox.
    if normalized.exists() {
        let canonical = std::fs::canonicalize(&normalized)
            .map_err(|e| format!("canonicalize '{}': {e}", normalized.display()))?;
        let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        if !canonical.starts_with(&canonical_root) {
            return Err(format!(
                "path '{}' resolves outside the sandbox via symlink (root: '{}')",
                normalized.display(),
                root.display()
            ));
        }
    }
    Ok(normalized)
}

/// Resolve a guest-supplied `path` to a real host path and a write-permission
/// flag (`true` = read-only, `false` = read/write).
///
/// * **Absolute paths** are mapped into `<db_home>/artifacts/` — **read-only**.
/// * **Relative paths** are mapped into
///   `<db_home>/artifacts/actions/<action_id>/` — **read/write**.
///
/// Path traversal (`..`) and symlink escapes are both rejected.
fn sandbox_resolve(
    path: &str,
    action_id: Option<&str>,
) -> Result<(std::path::PathBuf, bool /* read_only */), String> {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        let root = sandbox_artifacts_root();
        // Strip leading root/prefix components so `/foo/bar` →
        // `<artifacts_root>/foo/bar` on both Unix and Windows.
        let relative: std::path::PathBuf = p
            .components()
            .filter(|c| {
                !matches!(
                    c,
                    std::path::Component::RootDir | std::path::Component::Prefix(_)
                )
            })
            .collect();
        let candidate = root.join(&relative);
        let resolved = lexical_sandbox_check(&root, &candidate)?;
        Ok((resolved, true))
    } else {
        let aid = action_id.ok_or_else(|| {
            "relative paths require a named action context (no action_id set)".to_string()
        })?;
        let root = sandbox_action_dir(aid);
        let candidate = root.join(p);
        let resolved = lexical_sandbox_check(&root, &candidate)?;
        Ok((resolved, false))
    }
}

/// Open (or create) a dated log file for `action_name` under
/// `<db_home>/actions/<sanitized_name>/`.  Returns `None` on any I/O error so
/// that a missing log directory never prevents action execution.
fn open_action_log_file(action_name: &str) -> Option<std::fs::File> {
    let dir = crate::sol_db_home()
        .join("actions")
        .join(sanitize_action_name(action_name));
    std::fs::create_dir_all(&dir).ok()?;
    let now = chrono::Local::now();
    let short_id = &uuid::Uuid::new_v4().to_string()[..8];
    let filename = format!("{}_{}.log", now.format("%Y-%m-%d_%H-%M-%S"), short_id);
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(filename))
        .ok()
}

/// Synchronous (non-async) wasmtime execution — called from `spawn_blocking`.
fn exec_sync(
    db: Arc<dyn DbManager>,
    search_engine: Arc<dyn SearchManager>,
    browser_actions: Option<Arc<dyn BrowserActions>>,
    wasm_bytes: &[u8],
    fn_name: Option<&str>,
    params_json: &str,
    action_name: Option<&str>,
    trusted: bool,
) -> Result<ActionExecResult> {
    let eng = engine();

    let component = Component::from_binary(eng, wasm_bytes)
        .context("failed to parse WASM component binary")?;

    let log_file = action_name.and_then(open_action_log_file);

    // Resolve the action UUID so sandbox paths match the artifact store layout
    // (`artifacts/actions/<uuid>/`).  Failure to resolve is non-fatal — the
    // sandbox will fall back to None and reject relative path access.
    let action_id: Option<String> = if let Some(name) = action_name {
        block_on_db!(db.get_action(name))
            .ok()
            .map(|a| a.id.to_string())
    } else {
        None
    };

    let state = HostState {
        db,
        search_engine: search_engine.clone(),
        action_name: action_name.map(str::to_string),
        action_id,
        browser_actions,
        trace: Vec::new(),
        wasi: WasiCtxBuilder::new().build(),
        table: ResourceTable::new(),
        log_file,
    };
    let mut store = Store::new(eng, state);

    let (success, message, output) = if trusted {
        let mut linker: Linker<HostState> = Linker::new(eng);
        wasmtime_wasi::add_to_linker_sync(&mut linker)
            .context("failed to register WASI host functions")?;
        BackendAction::add_to_linker(&mut linker, |s| s)
            .context("failed to register backend-action host functions")?;
        let instance = BackendAction::instantiate(&mut store, &component, &linker)
            .context("failed to instantiate backend WASM component")?;
        match instance
            .sol_actions_runner()
            .call_run(&mut store, fn_name, params_json)
        {
            Ok(Ok(ar)) => (ar.success, ar.message, ar.output),
            Ok(Err(msg)) => (false, Some(msg), None),
            Err(e) => {
                // wasmtime trap — surface the full error chain so callers
                // can see what went wrong (WIT mismatch, missing import, etc.)
                let full = format!("{e:#}");
                tracing::error!(%full, "WASM call_run trap");
                (false, Some(full), None)
            }
        }
    } else {
        let mut linker: Linker<HostState> = Linker::new(eng);
        wasmtime_wasi::add_to_linker_sync(&mut linker)
            .context("failed to register WASI host functions")?;
        custom_world::CustomAction::add_to_linker(&mut linker, |s| s)
            .context("failed to register custom-action host functions")?;
        let instance = custom_world::CustomAction::instantiate(&mut store, &component, &linker)
            .context("failed to instantiate custom WASM component")?;
        match instance
            .sol_actions_runner()
            .call_run(&mut store, fn_name, params_json)
        {
            Ok(Ok(ar)) => (ar.success, ar.message, ar.output),
            Ok(Err(msg)) => (false, Some(msg), None),
            Err(e) => {
                let full = format!("{e:#}");
                tracing::error!(%full, "WASM call_run trap (custom)");
                (false, Some(full), None)
            }
        }
    };

    let trace = std::mem::take(&mut store.data_mut().trace);

    Ok(ActionExecResult {
        action_name: fn_name.unwrap_or("(anonymous)").to_string(),
        success,
        message,
        result: match output {
            Some(s) => serde_json::from_str::<Value>(&s).unwrap_or_else(|_| Value::String(s)),
            None => Value::Null,
        },
        trace,
    })
}

// ── Tests: get_secret/set_secret per-action scoping ──────────────────────────
//
// These exercise `HostState::get_secret`/`set_secret` directly (bypassing
// wasmtime) against a real, temporary libsql-backed DbManager, so the
// `db.get_action(action_name).action_config.secrets` lookup is genuine —
// not a mock stub. The OS keyring itself is swapped for an in-memory fake
// via `secrets::set_secret_backend_for_tests` so these tests never touch
// the real credential manager.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wasm_host::sol::actions::secrets::Host as _;
    use serial_test::serial;
    use sol_core::db::{schema, DbState, TursoDbManager};
    use sol_core::entities::ActionInput;
    use sol_core::search::MockSearchManager;
    use serde_json::json;

    async fn test_setup() -> Arc<dyn DbManager> {
        let path = std::env::temp_dir().join(format!("sol-wasm-host-test-{}.db", uuid::Uuid::new_v4()));
        let db = libsql::Builder::new_local(&path).build().await.expect("create db");
        let db_state = DbState::new(db);
        let conn = db_state.connect().await.expect("connect");
        schema::apply(&conn).await.expect("apply schema");
        Arc::new(TursoDbManager::new(db_state))
    }

    fn blank_action_input(name: &str, action_config: Value) -> ActionInput {
        ActionInput {
            name: name.to_string(),
            caption: None,
            description: None,
            capabilities: None,
            parameters_in: None,
            parameters_out: None,
            phrases: None,
            category: None,
            parameter_type_name: None,
            result_type_name: None,
            artifact_names: None,
            bin_name: None,
            fn_name: None,
            permission_name: None,
            allowed_permission_names: None,
            trusted: Some(true),
            action_type: None,
            action_config: Some(action_config),
        }
    }

    fn host_state(db: Arc<dyn DbManager>, action_name: &str) -> HostState {
        HostState {
            db,
            search_engine: Arc::new(MockSearchManager),
            action_name: Some(action_name.to_string()),
            action_id: None,
            trace: Vec::new(),
            browser_actions: None,
            wasi: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            log_file: None,
        }
    }

    fn key_b64(byte: u8) -> String {
        base64::engine::general_purpose::STANDARD.encode([byte; 32])
    }

    fn install_fake_secret_backend() {
        use std::collections::HashMap;
        use std::sync::Mutex;
        static STORE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
        fn store() -> &'static Mutex<HashMap<String, String>> {
            STORE.get_or_init(|| Mutex::new(HashMap::new()))
        }
        store().lock().unwrap().clear();
        crate::secrets::set_secret_backend_for_tests(Some(crate::secrets::SecretBackendOverride {
            get: |name| Ok(store().lock().unwrap().get(name).cloned()),
            set: |name, blob| {
                store().lock().unwrap().insert(name.to_string(), blob.to_string());
                Ok(())
            },
            delete: |name| {
                store().lock().unwrap().remove(name);
                Ok(())
            },
        }));
    }

    #[tokio::test]
    #[serial(wasm_host_secrets)]
    async fn action_can_get_secret_it_set_itself() {
        install_fake_secret_backend();
        let db = test_setup().await;
        db.create_action(&blank_action_input(
            "action-a",
            json!({ "secrets": { "MY_SECRET": key_b64(1) } }),
        ))
        .await
        .unwrap();

        let mut state = host_state(Arc::clone(&db), "action-a");
        // `get_secret`/`set_secret` use `block_on_db!` internally, which
        // (per its own doc comment) is only safe to call from a blocking
        // thread, not directly inside a running async task — mirror
        // production usage (`exec_sync` runs inside `spawn_blocking`) by
        // doing the same here.
        let result = tokio::task::spawn_blocking(move || {
            state.set_secret("MY_SECRET".to_string(), "top-secret".to_string()).unwrap();
            state.get_secret("MY_SECRET".to_string())
        })
        .await
        .unwrap();
        assert_eq!(result, Some("top-secret".to_string()));

        crate::secrets::set_secret_backend_for_tests(None);
    }

    #[tokio::test]
    #[serial(wasm_host_secrets)]
    async fn action_with_different_key_cannot_read_anothers_secret() {
        install_fake_secret_backend();
        let db = test_setup().await;
        db.create_action(&blank_action_input(
            "action-a",
            json!({ "secrets": { "SHARED_NAME": key_b64(1) } }),
        ))
        .await
        .unwrap();
        db.create_action(&blank_action_input(
            "action-b",
            json!({ "secrets": { "SHARED_NAME": key_b64(2) } }),
        ))
        .await
        .unwrap();

        let mut state_a = host_state(Arc::clone(&db), "action-a");
        tokio::task::spawn_blocking(move || {
            state_a.set_secret("SHARED_NAME".to_string(), "a-owns-this".to_string()).unwrap();
        })
        .await
        .unwrap();

        let mut state_b = host_state(Arc::clone(&db), "action-b");
        // action-b has a *different* key configured for the same secret
        // name, so it cannot decrypt what action-a wrote, even though it
        // knows the name and can reach the same host function.
        let result = tokio::task::spawn_blocking(move || state_b.get_secret("SHARED_NAME".to_string()))
            .await
            .unwrap();
        assert_eq!(result, None);

        crate::secrets::set_secret_backend_for_tests(None);
    }

    #[tokio::test]
    #[serial(wasm_host_secrets)]
    async fn action_with_no_key_configured_cannot_read_or_write() {
        install_fake_secret_backend();
        let db = test_setup().await;
        db.create_action(&blank_action_input(
            "action-a",
            json!({ "secrets": { "MY_SECRET": key_b64(1) } }),
        ))
        .await
        .unwrap();
        db.create_action(&blank_action_input("action-c", json!({})))
            .await
            .unwrap();

        let mut state_a = host_state(Arc::clone(&db), "action-a");
        tokio::task::spawn_blocking(move || {
            state_a.set_secret("MY_SECRET".to_string(), "a-owns-this".to_string()).unwrap();
        })
        .await
        .unwrap();

        let mut state_c = host_state(Arc::clone(&db), "action-c");
        let (get_result, set_err) = tokio::task::spawn_blocking(move || {
            let get_result = state_c.get_secret("MY_SECRET".to_string());
            let set_err = state_c
                .set_secret("MY_SECRET".to_string(), "hijack".to_string())
                .unwrap_err();
            (get_result, set_err)
        })
        .await
        .unwrap();
        assert_eq!(get_result, None);
        assert!(set_err.contains("not configured with a key"), "{set_err}");

        crate::secrets::set_secret_backend_for_tests(None);
    }
}
