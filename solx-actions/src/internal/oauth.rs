//! OAuth 2.0 authorization-code loopback — see the module doc on
//! `super::mod` for the split rationale.
//!
//! Three modes drive the loopback:
//!
//! * `oauth_start` — binds `127.0.0.1:{port}` (default `8765`), generates a
//!   random `state_value` (CSRF token), registers a pending
//!   `oneshot::Receiver` for the callback. Returns the `port`,
//!   `redirect_uri`, and `state_value`.
//! * `oauth_await` — blocks until the registered loopback for
//!   `state_value` receives the provider's redirect (or until the loopback
//!   is stopped). Returns the captured `code` / `error`.
//! * `oauth_stop` — triggers graceful shutdown of the loopback for
//!   `state_value` and drops the inbox receiver.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{json, Value};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::oauth_loopback::{self, LoopbackResult, LoopbackState};

use super::require_str;

// ── Registry ─────────────────────────────────────────────────────────────────

/// Per-listener handle held in the registry.
struct LoopbackHandle {
    shutdown_tx: oneshot::Sender<()>,
    #[allow(dead_code)]
    server_handle: JoinHandle<()>,
}

/// Registry of active loopback listeners, keyed by their `state_value`.
static REGISTRY: OnceLock<Mutex<HashMap<String, LoopbackHandle>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<String, LoopbackHandle>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

// ── Receiver inbox ───────────────────────────────────────────────────────────

type InboxMap = HashMap<String, oneshot::Receiver<LoopbackResult>>;
static INBOX: OnceLock<Mutex<InboxMap>> = OnceLock::new();

fn inbox() -> &'static Mutex<InboxMap> {
    INBOX.get_or_init(|| Mutex::new(HashMap::new()))
}

fn inbox_put(state_value: String, rx: oneshot::Receiver<LoopbackResult>) {
    if let Ok(mut m) = inbox().lock() {
        m.insert(state_value, rx);
    }
}

fn inbox_drop(state_value: &str) {
    if let Ok(mut m) = inbox().lock() {
        m.remove(state_value);
    }
}

/// Test-only: pub(super) so the test module in `mod.rs` can drive
/// callback / receiver reordering without going through an actual
/// `oauth_start`. Not exposed outside the crate.
#[cfg(test)]
pub(super) fn test_inbox_put(state_value: String, rx: oneshot::Receiver<LoopbackResult>) {
    inbox_put(state_value, rx);
}

// ── CSRF state ───────────────────────────────────────────────────────────────

pub(super) fn generate_state_value() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut s = String::with_capacity(32);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Test-only: mirrors `generate_state_value` so the test module can
/// assert uniqueness without depending on internal visibility.
#[cfg(test)]
pub(super) fn test_generate_state_value() -> String {
    generate_state_value()
}

// ── oauth_start ──────────────────────────────────────────────────────────────

pub(super) async fn oauth_start(params: &Value) -> Result<Value, String> {
    let port = params
        .get("port")
        .and_then(Value::as_u64)
        .map(|p| p as u16)
        .unwrap_or(oauth_loopback::DEFAULT_LOOPBACK_PORT);
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);

    // Bind eagerly, before registering any state, so a port-in-use failure
    // (e.g. a second `oauth_start` on the default port before the first is
    // stopped) surfaces immediately as an `Err` here — rather than being
    // silently swallowed inside the spawned server task, which would leave
    // behind a `"started": true` state_value that can never actually
    // receive a callback.
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("failed to bind oauth loopback on {addr}: {e}"))?;

    let state_value = generate_state_value();
    let state = Arc::new(LoopbackState::new());
    let receiver = state.register(state_value.clone())?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server_state = Arc::clone(&state);
    let server_handle = tokio::spawn(async move {
        let _ = oauth_loopback::serve_loopback_with_shutdown_on(
            server_state,
            listener,
            async move {
                let _ = shutdown_rx.await;
            },
        )
        .await;
    });

    inbox_put(state_value.clone(), receiver);

    if let Ok(mut reg) = registry().lock() {
        reg.insert(
            state_value.clone(),
            LoopbackHandle {
                shutdown_tx,
                server_handle,
            },
        );
    }

    let redirect_uri = oauth_loopback::redirect_uri(port);
    let started_at = chrono::Utc::now().to_rfc3339();

    Ok(json!({
        "started": true,
        "port": port,
        "redirect_uri": redirect_uri,
        "state_value": state_value,
        "started_at": started_at,
    }))
}

// ── oauth_await ──────────────────────────────────────────────────────────────

pub(super) async fn oauth_await(params: &Value) -> Result<Value, String> {
    let state_value = params
        .get("state_value")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing required param: state_value".to_string())?
        .to_string();

    let timeout_secs = params.get("timeout_secs").and_then(Value::as_u64);

    // Take the receiver out of the inbox up front. On timeout we put it
    // back (see below) so a subsequent `oauth_await` call for the same
    // state_value can still succeed if the callback arrives later — the
    // underlying loopback listener keeps running independently of this
    // call.
    let mut rx = inbox()
        .lock()
        .ok()
        .and_then(|mut m| m.remove(&state_value))
        .ok_or_else(|| format!("no loopback registered for state_value '{state_value}'"))?;

    let loopback = match timeout_secs {
        Some(secs) => {
            // `&mut rx` (rather than `rx.await`) keeps `rx` owned by this
            // stack frame — tokio's oneshot Receiver is cancel-safe when
            // polled this way, so if the sleep branch fires first, `rx`
            // is still valid afterward and can be reinserted rather than
            // being dropped along with a cancelled `tokio::time::timeout`
            // future (which would lose it permanently).
            tokio::select! {
                result = &mut rx => {
                    match result {
                        Ok(res) => res,
                        Err(_) => return Err(format!(
                            "loopback for state '{state_value}' was stopped before the callback arrived"
                        )),
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(secs)) => {
                    inbox_put(state_value.clone(), rx);
                    return Err(format!(
                        "oauth callback timed out after {secs}s for state_value \
                         '{state_value}'; the loopback is still running and a later \
                         oauth_await call for the same state_value may still succeed"
                    ));
                }
            }
        }
        None => match rx.await {
            Ok(res) => res,
            Err(_) => {
                return Err(format!(
                    "loopback for state '{state_value}' was stopped before the callback arrived"
                ))
            }
        },
    };

    let succeeded = loopback.succeeded();
    let mut obj = serde_json::Map::new();
    obj.insert("state_value".into(), Value::String(state_value));
    if let Some(code) = &loopback.code {
        obj.insert("code".into(), Value::String(code.clone()));
    }
    if let Some(state) = &loopback.state {
        obj.insert("state".into(), Value::String(state.clone()));
    }
    if let Some(error) = &loopback.error {
        obj.insert("error".into(), Value::String(error.clone()));
    }
    if let Some(error_description) = &loopback.error_description {
        obj.insert(
            "error_description".into(),
            Value::String(error_description.clone()),
        );
    }
    obj.insert("succeeded".into(), Value::Bool(succeeded));

    Ok(Value::Object(obj))
}

// ── oauth_stop ───────────────────────────────────────────────────────────────

pub(super) async fn oauth_stop(params: &Value) -> Result<Value, String> {
    let state_value = require_str(params, "state_value")?.to_string();

    let handle = registry()
        .lock()
        .map_err(|e| format!("loopback registry poisoned: {e}"))?
        .remove(&state_value);

    inbox_drop(&state_value);

    match handle {
        Some(LoopbackHandle { shutdown_tx, .. }) => {
            let _ = shutdown_tx.send(());
            Ok(json!({ "stopped": true, "state_value": state_value }))
        }
        None => Ok(json!({
            "stopped": false,
            "state_value": state_value,
            "error": format!("no loopback registered for state_value '{state_value}'"),
        })),
    }
}
