//! The seam that lets a crate outside `solx-actions` contribute internal
//! (`fn_name`-dispatched) actions to its dispatcher, without `solx-actions`
//! needing to depend on that crate's concrete types, and without that crate
//! needing to depend on `solx-actions` (which would be circular for a crate
//! `solx-actions` itself also depends on, like `solx-console`).
//!
//! Lives here — the one crate with no internal dependencies — rather than in
//! `solx-actions`, so both `solx-actions` (the dispatcher) and any plugin
//! crate (a handler provider) can depend on it without depending on each
//! other.
//!
//! [`InternalActionHandler`] is deliberately narrower than the dispatcher's
//! own full context: it gets the four manager trait objects already defined
//! in [`crate::managers`] plus the calling [`Caller`], nothing crate-specific
//! (config services, the concrete action manager, task-spawn machinery). A
//! handler that needs something beyond that (e.g. `solx-console`'s
//! `action_start`/`stop`/`poll`, which need to spawn and abort detached
//! tasks) takes it via constructor injection instead — see
//! [`ActionExecutor`].

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use crate::error::Result;
use crate::managers::{ActionManager, DocManager, FileStore, TypeManager};

// ── Caller ───────────────────────────────────────────────────────────────────

/// Identity of the action that initiated an execution.
///
/// Only one caller exists today: an action invoking another action. That
/// happens in exactly one place — a WASM guest calling `action-exec`, which
/// is the sole re-entrant path into execution (internal actions do entity
/// CRUD only, never `exec`). Everything else — the CLI, the MCP server, the
/// HTTP route, `solx-client` — enters through `ActionManager::exec` and has
/// no action caller, i.e. `None`.
///
/// That containment is deliberate. A `Caller` is built by the host from a
/// row it just read, never parsed from a request, never serialized, and
/// never crosses the process boundary — so unlike a client-declared
/// identity it cannot be spoofed.
///
/// ## What it carries, and what it must not
///
/// Only the calling action's `action_config.secrets` map, behind a private
/// field. The caller's `cwd`, `auth`, and `headers` are never in scope, so
/// there is no way for a downstream handler to reach them even by accident.
///
/// **Invariant:** a `Caller` is read-only input to secret-key resolution.
/// No internal action may return it, or any key inside it, in its result —
/// `get_secret` returns the decrypted *value*, never the key that unlocked
/// it.
#[derive(Debug, Clone)]
pub struct Caller {
    action_ref: String,
    /// Minted fresh every time this frame is built — i.e. once per
    /// `exec_as` dispatch to a `Wasm`/`Script` action, since those are the
    /// only two call sites that construct a `Caller`. Stamped onto every
    /// console entry this invocation writes, so two concurrent runs of the
    /// same action (same console) stay separable even though they share a
    /// console.
    invocation_id: String,
    /// `action_config.secrets` of the calling action: secret name -> base64
    /// AES key. Typed as a string map rather than a `Value` so it is a
    /// type-level fact that nothing else can ride along.
    secrets: BTreeMap<String, String>,
}

impl Caller {
    /// Build a caller frame from an action row. `action_config` is the raw
    /// (unmasked) config — `exec_as` reads rows through `get_unmasked`.
    pub fn from_action(action_ref: impl Into<String>, action_config: Option<&Value>) -> Self {
        let secrets = action_config
            .and_then(|c| c.get("secrets"))
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        Caller {
            action_ref: action_ref.into(),
            invocation_id: Uuid::new_v4().to_string(),
            secrets,
        }
    }

    /// Like [`Self::from_action`], but stamped with a caller-chosen
    /// `invocation_id` rather than a freshly minted one — used by a
    /// detached `action_start` run, where the id has to be known *before*
    /// execution begins so `action_stop`/`action_poll` have something to
    /// address.
    pub fn with_invocation(
        action_ref: impl Into<String>,
        action_config: Option<&Value>,
        invocation_id: impl Into<String>,
    ) -> Self {
        let mut caller = Self::from_action(action_ref, action_config);
        caller.invocation_id = invocation_id.into();
        caller
    }

    /// Full reference of the calling action, e.g. `/pkg/summarize`.
    pub fn action_ref(&self) -> &str {
        &self.action_ref
    }

    /// Identifies this one run of `action_ref`, distinct from any other
    /// concurrent or subsequent run.
    pub fn invocation_id(&self) -> &str {
        &self.invocation_id
    }

    /// The AES key this action has configured for `name`, if any.
    pub fn secret_key(&self, name: &str) -> Option<&str> {
        self.secrets.get(name).map(String::as_str)
    }

    /// Strips the secrets map, keeping only what a pluggable (non-hard-coded)
    /// internal-action handler is allowed to see — see [`CallerInfo`].
    pub fn info(&self) -> CallerInfo {
        CallerInfo {
            action_ref: self.action_ref.clone(),
            invocation_id: self.invocation_id.clone(),
        }
    }
}

/// URI form, for logs and error messages: `action://pkg/summarize`.
impl fmt::Display for Caller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "action://{}", self.action_ref.trim_start_matches('/'))
    }
}

/// Non-secret identity of the calling action — everything a pluggable
/// [`InternalActionHandler`] is allowed to see about who invoked it. Unlike
/// [`Caller`], this type has no `secret_key` method and no secrets field to
/// add one to later by accident: the exclusion is a compile-time fact, not
/// just a documented convention.
#[derive(Debug, Clone)]
pub struct CallerInfo {
    action_ref: String,
    invocation_id: String,
}

impl CallerInfo {
    /// Full reference of the calling action, e.g. `/pkg/summarize`.
    pub fn action_ref(&self) -> &str {
        &self.action_ref
    }

    /// Identifies this one run of `action_ref`, distinct from any other
    /// concurrent or subsequent run.
    pub fn invocation_id(&self) -> &str {
        &self.invocation_id
    }
}

/// URI form, for logs and error messages: `action://pkg/summarize`.
impl fmt::Display for CallerInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "action://{}", self.action_ref.trim_start_matches('/'))
    }
}

// ── Seed catalogue entries ───────────────────────────────────────────────────

/// One built-in action to seed — same shape `solx-actions`' own catalogue
/// uses, so a plugin's entries merge into the same `INSERT OR IGNORE` loop.
#[derive(Clone)]
pub struct SeedAction {
    pub path: &'static str,
    /// The entity name. `fn_name` (below) is the dispatch key the internal
    /// dispatcher matches on — for a flat `/builtin` entry the two are
    /// identical, but a nested entry (e.g. `console/print`) needs a `name`
    /// too generic to double as a global dispatch key, hence the split.
    pub name: &'static str,
    pub fn_name: &'static str,
    pub description: &'static str,
    /// Name under the builtin types namespace giving this action's
    /// `param_type_ref` a real JSON Schema, if one is seeded.
    pub param_type: Option<&'static str>,
}

// ── Handler context and trait ────────────────────────────────────────────────

/// What a pluggable internal-action handler can reach — deliberately a
/// subset of the dispatcher's own full context (no config service, no
/// concrete action-manager handle, no task-spawn machinery). A handler that
/// needs more than this takes it via constructor injection instead.
pub struct InternalCallCtx {
    pub docs: Arc<dyn DocManager>,
    pub types: Arc<dyn TypeManager>,
    pub actions: Arc<dyn ActionManager>,
    pub files: Arc<dyn FileStore>,
    /// The invoking action's non-secret identity only — see [`CallerInfo`].
    /// A pluggable handler can never reach a secret key through this field,
    /// even by accident: [`Caller`] (which can) never appears here.
    pub caller: Option<CallerInfo>,
}

/// One internal action's implementation. Handlers are constructed with
/// whatever crate-specific state they need already baked in (dependency
/// injection) — `params`/`ctx` carry only what's genuinely per-call.
#[async_trait]
pub trait InternalActionHandler: Send + Sync {
    async fn call(&self, params: &Value, ctx: &InternalCallCtx) -> std::result::Result<Value, String>;
}

/// The seam `action_start`/`action_stop`/`action_poll` need into
/// `solx-actions`' own detached-task orchestration, without depending on its
/// concrete `LocalActionManager` type. Mirrors
/// `LocalActionManager::{start_invocation,stop_invocation,poll_invocation}`
/// 1:1 — see their doc comments in `solx-actions` for the full behavior
/// (long-lived-host requirement, cooperative-then-forced cancellation,
/// long-poll cadence).
#[async_trait]
pub trait ActionExecutor: Send + Sync {
    async fn start_invocation(&self, path: &str, name: &str, params: Value) -> Result<Value>;
    async fn stop_invocation(&self, invocation_id: &str, force: bool, grace_secs: Option<u64>) -> Result<Value>;
    async fn poll_invocation(&self, invocation_id: &str, wait_secs: Option<u64>) -> Result<Value>;
}

// ── Registry ─────────────────────────────────────────────────────────────────

/// A dispatcher's fn_name -> handler map plus the seed entries that back it,
/// assembled by merging one or more plugins' contributions. `solx-actions`
/// consults this only as a fallback after its own hard-coded built-ins — see
/// its `internal::run_internal`.
#[derive(Default)]
pub struct InternalActionRegistry {
    handlers: HashMap<String, Arc<dyn InternalActionHandler>>,
    seeds: Vec<SeedAction>,
}

impl InternalActionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one handler under every `fn_name` in `fn_names`.
    pub fn register(&mut self, fn_names: &[&'static str], handler: Arc<dyn InternalActionHandler>) {
        for name in fn_names {
            self.handlers.insert((*name).to_string(), handler.clone());
        }
    }

    /// Append `SeedAction` catalogue entries, independent of handler
    /// registration — mirrors how `solx-actions`' own `seed.rs` and
    /// `internal::run_internal` are two independent lists kept in sync by
    /// convention, not by construction.
    pub fn add_seeds(&mut self, seeds: Vec<SeedAction>) {
        self.seeds.extend(seeds);
    }

    pub fn get(&self, fn_name: &str) -> Option<&Arc<dyn InternalActionHandler>> {
        self.handlers.get(fn_name)
    }

    pub fn seed_actions(&self) -> &[SeedAction] {
        &self.seeds
    }

    /// Fold another plugin's registrations into this one — how
    /// `solx-actions` combines its own registry with one built by an
    /// external crate (e.g. `solx_console::actions::plugin(...)`).
    pub fn merge(&mut self, other: InternalActionRegistry) {
        self.handlers.extend(other.handlers);
        self.seeds.extend(other.seeds);
    }
}
