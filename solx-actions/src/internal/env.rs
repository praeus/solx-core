//! The in-process environment store.
//!
//! `get_env` / `set_env` read and write a namespaced `RwLock<HashMap>`,
//! seeded at startup from `SolxConfig.env_mappings` via
//! [`init_env_mappings`] and from `SolxConfig.env_vars` via
//! [`init_persisted_env`].

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock, RwLock};

use serde_json::{json, Value};
use solx_config::ConfigService;

// ── in-process environment store ─────────────────────────────────────────────
//
// A generic process-lifetime keyed scratch store, partitioned into namespaces
// so unrelated callers can't collide on a bare key. Entries come from three
// places: the `env_mappings` allowlist over the *system* environment (seeded
// once at startup), the persisted `env_vars` config map (also seeded at
// startup), and anything subsequently written by `set_env`. No relation to
// the real process environment — this used to back only WASM guests; it's a
// plain Internal action now, reachable by anyone (including WASM guests, via
// `action-exec`).
//
// Writes are memory-only by default. `set_env` with `persist: true` also
// writes through to `SolxConfig.env_vars`, so the variable survives a
// restart. Persistence is *sticky*: it is a property of the variable, not of
// the individual write, so a later `set_env` on an already-persisted key
// keeps updating the config even without the flag. That keeps something like
// a resume cursor from silently degrading to memory-only after one careless
// write. Removing a variable means deleting it from `solx-config.json`.

/// `namespace -> key -> value`.
type Namespaced = HashMap<String, HashMap<String, String>>;

/// Namespace used when a caller doesn't name one. `env_mappings` seeds here
/// too, so pre-namespace callers keep resolving exactly as before.
pub const DEFAULT_NAMESPACE: &str = "default";

static ENV_STORE: OnceLock<RwLock<Namespaced>> = OnceLock::new();
/// `namespace\0key` for every variable known to be persisted — seeded from
/// config at startup and extended by each `persist: true` write. This is what
/// makes persistence sticky without re-reading config on every `set_env`.
static PERSISTED: OnceLock<RwLock<HashSet<String>>> = OnceLock::new();

fn env_store() -> &'static RwLock<Namespaced> {
    ENV_STORE.get_or_init(|| RwLock::new(HashMap::new()))
}

fn persisted() -> &'static RwLock<HashSet<String>> {
    PERSISTED.get_or_init(|| RwLock::new(HashSet::new()))
}

fn persist_id(namespace: &str, key: &str) -> String {
    format!("{namespace}\u{0}{key}")
}

fn namespace_of(params: &Value) -> String {
    params
        .get("namespace")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_NAMESPACE)
        .to_string()
}

/// Seed the environment store from the `env_mappings` allowlist:
/// `key -> system_env_var_name`. Reads each system env var once, at call
/// time, and stores it under `key` in the default namespace. Call once at
/// startup.
///
/// Re-exported as `crate::internal::init_env_mappings` from `mod.rs` so
/// existing callers (`solx-manager`) keep working unchanged.
pub fn init_env_mappings(mappings: HashMap<String, String>) {
    let mut resolved = HashMap::new();
    for (key, sys_key) in mappings {
        if let Ok(v) = std::env::var(&sys_key) {
            resolved.insert(key, v);
        }
    }
    if let Ok(mut store) = env_store().write() {
        store.entry(DEFAULT_NAMESPACE.to_string()).or_default().extend(resolved);
    }
}

/// Load persisted variables from `SolxConfig.env_vars` into the store and
/// mark them persistent, so later writes keep writing through. Call once at
/// startup, after [`init_env_mappings`].
pub fn init_persisted_env(vars: Namespaced) {
    if let (Ok(mut store), Ok(mut marks)) = (env_store().write(), persisted().write()) {
        for (namespace, entries) in vars {
            for (key, value) in entries {
                marks.insert(persist_id(&namespace, &key));
                store
                    .entry(namespace.clone())
                    .or_default()
                    .insert(key, value);
            }
        }
    }
}

pub(super) fn get_env(params: &Value) -> Value {
    let namespace = namespace_of(params);
    let key = params.get("key").and_then(Value::as_str).unwrap_or("");
    let value = env_store()
        .read()
        .ok()
        .and_then(|s| s.get(&namespace).and_then(|m| m.get(key)).cloned());
    json!({ "namespace": namespace, "value": value })
}

pub(super) fn set_env(params: &Value, config: &Arc<ConfigService>) -> Result<Value, String> {
    let namespace = namespace_of(params);
    let key = params.get("key").and_then(Value::as_str).unwrap_or("").to_string();
    let value = params.get("value").and_then(Value::as_str).unwrap_or("").to_string();

    let id = persist_id(&namespace, &key);
    let already = persisted().read().map(|m| m.contains(&id)).unwrap_or(false);
    let persist = params.get("persist").and_then(Value::as_bool).unwrap_or(false) || already;

    // Persist first: if the config write fails the caller gets an error rather
    // than an in-memory value it wrongly believes will outlive the process.
    if persist {
        config
            .set_env_var(&namespace, &key, &value)
            .map_err(|e| format!("persist env var '{namespace}/{key}': {e}"))?;
        if let Ok(mut marks) = persisted().write() {
            marks.insert(id);
        }
    }
    if let Ok(mut store) = env_store().write() {
        store.entry(namespace.clone()).or_default().insert(key, value);
    }
    Ok(json!({ "set": true, "namespace": namespace, "persisted": persist }))
}
