//! OAuth 2.0 authorization-code loopback — see the module doc on
//! `super::mod` for the split rationale.
//!
//! Three modes drive the loopback:
//!
//! * `oauth-start` — binds `127.0.0.1:{port}` (default `8765`), generates a
//!   random `state_value` (CSRF token), registers a pending
//!   `oneshot::Receiver` for the callback. Returns the `port`,
//!   `redirect_uri`, and `state_value`.
//! * `oauth-await` — blocks until the registered loopback for
//!   `state_value` receives the provider's redirect (or until the loopback
//!   is stopped). Returns the captured `code` / `error`.
//! * `oauth-stop` — triggers graceful shutdown of the loopback for
//!   `state_value` and drops the inbox receiver.
//!
//! ## Listener lifetime
//!
//! A listener is a detached server task, so nothing ties it to the script
//! (or WASM guest) that started it: a caller that is stopped, errors out,
//! or is force-aborted between `oauth-start` and `oauth-stop` would
//! otherwise leave the port bound until the process exits. Four things
//! bound that:
//!
//! * `oauth-await` stops the listener itself once the callback arrives
//!   (it can only ever deliver one), when the caller's invocation is
//!   cancelled (it polls the `action-stop` flag), and when its own future
//!   is dropped mid-wait (force-abort, client disconnect). It leaves the
//!   listener running only on its own timeout, so a later `oauth-await`
//!   can still pick the callback up.
//! * Every listener shuts itself down after `max_lifetime_secs` (default
//!   [`DEFAULT_MAX_LIFETIME_SECS`]) no matter what.
//! * `oauth-start` on a port still held by one of *our* listeners stops
//!   that listener and takes the port over — a new login flow supersedes
//!   an abandoned one. A port held by some other process still fails.
//! * The server task removes its own registry/inbox entries on exit.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

// Aliased: this file is itself `crate::internal::oauth`, so importing the
// loopback module under its own bare name (`oauth`) would read ambiguously
// against this file's own identity at every call site below.
use crate::loopback::oauth as oauth_loopback;
use crate::loopback::oauth::{LoopbackResult, LoopbackState};

use super::{require_str, InternalCtx};

/// How long a listener may live before it shuts itself down, when
/// `oauth-start` isn't given `max_lifetime_secs`. Comfortably longer than
/// a human consent screen; short enough that a leaked listener doesn't
/// block the port for long.
pub(super) const DEFAULT_MAX_LIFETIME_SECS: u64 = 900;
/// Floor for `max_lifetime_secs`, so a listener can't expire before the
/// browser has any realistic chance of reaching it.
const MIN_LIFETIME_SECS: u64 = 30;
/// How often a blocked `oauth-await` checks whether its caller was stopped.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// How long `oauth-start` waits for a superseded listener to release its
/// port before trying to bind anyway.
const RECLAIM_WAIT: Duration = Duration::from_secs(2);

// ── Registry ─────────────────────────────────────────────────────────────────

/// Per-listener handle held in the registry.
struct LoopbackHandle {
    port: u16,
    shutdown_tx: oneshot::Sender<()>,
    server_handle: JoinHandle<()>,
}

/// Registry of active loopback listeners, keyed by their `state_value`.
static REGISTRY: OnceLock<Mutex<HashMap<String, LoopbackHandle>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<String, LoopbackHandle>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Remove `state_value`'s listener and inbox entry and signal the server
/// to shut down. Synchronous (no `.await`) so it can run from `Drop`.
/// Returns the server task's handle if a listener was registered.
fn stop_listener(state_value: &str) -> Option<JoinHandle<()>> {
    let handle = registry().lock().ok().and_then(|mut reg| reg.remove(state_value));
    inbox_drop(state_value);
    handle.map(|LoopbackHandle { shutdown_tx, server_handle, .. }| {
        let _ = shutdown_tx.send(());
        server_handle
    })
}

/// Stop every listener of ours bound to `port` and wait (bounded) for
/// their server tasks to exit, which drops their `TcpListener`s.
async fn reclaim_port(port: u16) {
    let stale: Vec<String> = registry()
        .lock()
        .map(|reg| {
            reg.iter()
                .filter(|(_, h)| h.port == port)
                .map(|(state, _)| state.clone())
                .collect()
        })
        .unwrap_or_default();
    for state_value in stale {
        tracing::info!(port, "oauth-start: superseding abandoned loopback listener on port {port}");
        if let Some(server) = stop_listener(&state_value) {
            let _ = tokio::time::timeout(RECLAIM_WAIT, server).await;
        }
    }
}

/// Stops the listener when dropped, unless disarmed. Held across the wait
/// in `oauth-await`, so a future dropped mid-wait (force-abort, client
/// disconnect) still releases the port.
struct StopOnDrop {
    state_value: String,
    armed: bool,
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if self.armed {
            stop_listener(&self.state_value);
        }
    }
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
/// `oauth-start`. Not exposed outside the crate.
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

// ── oauth-start ──────────────────────────────────────────────────────────────

pub(super) async fn oauth_start(params: &Value) -> Result<Value, String> {
    let port = params
        .get("port")
        .and_then(Value::as_u64)
        .map(|p| p as u16)
        .unwrap_or(oauth_loopback::DEFAULT_LOOPBACK_PORT);
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let max_lifetime_secs = params
        .get("max_lifetime_secs")
        .and_then(Value::as_u64)
        .filter(|&s| s > 0)
        .unwrap_or(DEFAULT_MAX_LIFETIME_SECS)
        .max(MIN_LIFETIME_SECS);

    reclaim_port(port).await;

    // Bind eagerly, before registering any state, so a port-in-use failure
    // (e.g. a second `oauth-start` on the default port before the first is
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
    let task_state_value = state_value.clone();
    let server_handle = tokio::spawn(async move {
        let _ = oauth_loopback::serve_loopback_with_shutdown_on(
            server_state,
            listener,
            async move {
                tokio::select! {
                    _ = shutdown_rx => {}
                    _ = tokio::time::sleep(Duration::from_secs(max_lifetime_secs)) => {
                        tracing::info!(
                            "oauth loopback on port {port} reached max_lifetime_secs={max_lifetime_secs}; shutting down"
                        );
                    }
                }
            },
        )
        .await;
        // Whichever way it ended, drop our bookkeeping. A no-op if a stop
        // already removed it.
        stop_listener(&task_state_value);
    });

    inbox_put(state_value.clone(), receiver);

    if let Ok(mut reg) = registry().lock() {
        reg.insert(
            state_value.clone(),
            LoopbackHandle {
                port,
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
        "max_lifetime_secs": max_lifetime_secs,
    }))
}

// ── oauth-await ──────────────────────────────────────────────────────────────

pub(super) async fn oauth_await(params: &Value, ctx: &InternalCtx) -> Result<Value, String> {
    let state_value = params
        .get("state_value")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing required param: state_value".to_string())?
        .to_string();

    let timeout_secs = params.get("timeout_secs").and_then(Value::as_u64);

    // Take the receiver out of the inbox up front. On timeout we put it
    // back (see below) so a subsequent `oauth-await` call for the same
    // state_value can still succeed if the callback arrives later — the
    // underlying loopback listener keeps running independently of this
    // call.
    let mut rx = inbox()
        .lock()
        .ok()
        .and_then(|mut m| m.remove(&state_value))
        .ok_or_else(|| format!("no loopback registered for state_value '{state_value}'"))?;

    // From here on, every way out of this function except our own timeout
    // releases the listener — including this future being dropped.
    let mut guard = StopOnDrop { state_value: state_value.clone(), armed: true };

    // `None` means "no deadline": a sleep long enough never to fire in
    // practice (the listener's own max lifetime ends the wait long before).
    const NO_DEADLINE_SECS: u64 = 365 * 24 * 60 * 60;
    let deadline = tokio::time::sleep(Duration::from_secs(timeout_secs.unwrap_or(NO_DEADLINE_SECS)));
    tokio::pin!(deadline);
    let mut cancel_poll = tokio::time::interval(CANCEL_POLL_INTERVAL);
    cancel_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let loopback = loop {
        // `&mut rx` (rather than `rx.await`) keeps `rx` owned by this stack
        // frame — tokio's oneshot Receiver is cancel-safe when polled this
        // way, so if another branch fires first, `rx` is still valid
        // afterward and can be reinserted on timeout.
        tokio::select! {
            result = &mut rx => {
                break match result {
                    Ok(res) => res,
                    Err(_) => return Err(format!(
                        "loopback for state '{state_value}' was stopped before the callback arrived"
                    )),
                };
            }
            _ = &mut deadline => {
                guard.armed = false;
                inbox_put(state_value.clone(), rx);
                return Err(format!(
                    "oauth callback timed out after {}s for state_value \
                     '{state_value}'; the loopback is still running and a later \
                     oauth-await call for the same state_value may still succeed",
                    timeout_secs.unwrap_or_default()
                ));
            }
            _ = cancel_poll.tick() => {
                if caller_cancelled(ctx).await {
                    return Err(format!(
                        "oauth-await cancelled: the calling action was stopped; \
                         loopback for state '{state_value}' released"
                    ));
                }
            }
        }
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

    // `guard` drops here and stops the listener: it has delivered its one
    // callback and has nothing left to receive. A follow-up `oauth-stop`
    // is harmless (it reports `stopped: false`).
    Ok(Value::Object(obj))
}

/// Whether `action-stop` was requested for the invocation that called us.
/// Fails closed: no caller (CLI/HTTP/MCP), or a lookup error, is "not
/// cancelled" — a missing signal must never abort a real login.
async fn caller_cancelled(ctx: &InternalCtx) -> bool {
    let Some(caller) = &ctx.caller else {
        return false;
    };
    ctx.local
        .invocations()
        .is_cancelled(caller.invocation_id())
        .await
        .unwrap_or(false)
}

// ── oauth-stop ───────────────────────────────────────────────────────────────

pub(super) async fn oauth_stop(params: &Value) -> Result<Value, String> {
    let state_value = require_str(params, "state_value")?.to_string();

    match stop_listener(&state_value) {
        Some(_) => Ok(json!({ "stopped": true, "state_value": state_value })),
        None => Ok(json!({
            "stopped": false,
            "state_value": state_value,
            "error": format!("no loopback registered for state_value '{state_value}'"),
        })),
    }
}
