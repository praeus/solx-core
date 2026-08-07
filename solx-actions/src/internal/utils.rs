//! Small stateless utilities and the in-process environment store.
//!
//! * `now` / `uuid` / `random_int` / `random_string` — process-local
//!   generators with no I/O, no caller-scoping, and no trust model beyond
//!   "an action called this".
//! * `get_env` / `set_env` read and write a `RwLock<HashMap>` seeded once
//!   at startup from `SolxConfig.env_mappings` via [`init_env_mappings`]
//!   (re-exported from `super::super::secrets` — see the re-export note
//!   below).
//!
//! `now` returns RFC 3339 UTC, matching the `created_at`/`updated_at`
//! representation used everywhere else in the actions and docs tables.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use serde_json::{json, Value};

// ── in-process environment store ─────────────────────────────────────────────
//
// A generic process-lifetime keyed scratch store: read-only entries seeded
// once at startup from `SolxConfig.env_mappings` (via `init_env_mappings`),
// plus anything subsequently written by `set_env`. No relation to the real
// process environment — this used to back only WASM guests; it's a plain
// Internal action now, reachable by anyone (including WASM guests, via
// `action-exec`).

static ENV_STORE: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();

fn env_store() -> &'static RwLock<HashMap<String, String>> {
    ENV_STORE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Seed the environment store from the `env_mappings` allowlist:
/// `key -> system_env_var_name`. Reads each system env var once, at call
/// time, and stores it under `key`. Call once at startup.
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
    if let Ok(mut map) = env_store().write() {
        map.extend(resolved);
    }
}

pub(super) fn get_env(params: &Value) -> Value {
    let key = params.get("key").and_then(Value::as_str).unwrap_or("");
    let value = env_store().read().ok().and_then(|m| m.get(key).cloned());
    json!({ "value": value })
}

pub(super) fn set_env(params: &Value) -> Value {
    let key = params.get("key").and_then(Value::as_str).unwrap_or("").to_string();
    let value = params.get("value").and_then(Value::as_str).unwrap_or("").to_string();
    if let Ok(mut m) = env_store().write() {
        m.insert(key, value);
    }
    json!({ "set": true })
}

// ── now / uuid / random_int / random_string ─────────────────────────────────

pub(super) fn now_value() -> Value {
    json!({ "now": chrono::Utc::now().to_rfc3339() })
}

pub(super) fn uuid_value() -> Value {
    json!({ "uuid": uuid::Uuid::new_v4().to_string() })
}

pub(super) fn random_int_value(params: &Value) -> Result<Value, String> {
    use rand::Rng;
    let lo = params.get("lo").and_then(Value::as_i64).unwrap_or(0);
    let hi = params.get("hi").and_then(Value::as_i64).unwrap_or(i64::MAX);
    if lo > hi {
        return Err(format!("random_int: lo ({lo}) must be <= hi ({hi})"));
    }
    // `gen_range` on i64 is inclusive on both ends; we want the same
    // semantics for a uniform integer in [lo, hi].
    let n = rand::thread_rng().gen_range(lo..=hi);
    Ok(json!({ "value": n }))
}

const RANDOM_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

pub(super) fn random_string_value(params: &Value) -> Result<Value, String> {
    use rand::seq::SliceRandom;
    let length = params
        .get("length")
        .and_then(Value::as_u64)
        .unwrap_or(16) as usize;
    if length == 0 {
        return Ok(json!({ "value": "" }));
    }
    let mut rng = rand::thread_rng();
    let mut buf = String::with_capacity(length);
    for _ in 0..length {
        // `unwrap` is safe: the alphabet is non-empty and we draw one byte
        // per iteration, so `choose` cannot return `None`.
        let b = RANDOM_ALPHABET.choose(&mut rng).copied().unwrap();
        buf.push(b as char);
    }
    Ok(json!({ "value": buf }))
}
