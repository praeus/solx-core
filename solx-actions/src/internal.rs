//! Dispatcher-level native handler for internal actions.
//!
//! Internal actions are dispatched by `fn_name` — no WASM, no shell, no HTTP.
//! The first (and currently only) handler is the OAuth 2.0 authorization-code
//! loopback controller, which drives the lifecycle of one or more loopback HTTP
//! listeners (defined in [`crate::oauth_loopback`]) that capture the redirect
//! from any RFC 6749 / RFC 8252 provider.
//!
//! Three modes drive the OAuth loopback:
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

/// Await the callback for the given `state_value`. Returns `None` if no
/// loopback is registered for that state.
async fn await_callback(state_value: &str) -> Option<Result<LoopbackResult, String>> {
    let rx = inbox()
        .lock()
        .ok()
        .and_then(|mut m| m.remove(state_value))?;
    match rx.await {
        Ok(r) => Some(Ok(r)),
        Err(_) => Some(Err(format!(
            "loopback for state '{state_value}' was stopped before the callback arrived"
        ))),
    }
}

// ── CSRF state ───────────────────────────────────────────────────────────────

fn generate_state_value() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut s = String::with_capacity(32);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ── Public entry point ───────────────────────────────────────────────────────

/// Dispatch an internal action by `fn_name`. Returns the JSON result value
/// (the caller wraps it in `ActionExecResult`).
pub async fn run_internal(fn_name: &str, params: &Value) -> Result<Value, String> {
    match fn_name {
        "oauth_start" => oauth_start(params).await,
        "oauth_await" => oauth_await(params).await,
        "oauth_stop" => oauth_stop(params).await,
        other => Err(format!(
            "unknown internal fn_name '{other}'; \
             expected \"oauth_start\", \"oauth_await\", or \"oauth_stop\""
        )),
    }
}

// ── oauth_start ──────────────────────────────────────────────────────────────

async fn oauth_start(params: &Value) -> Result<Value, String> {
    let port = params
        .get("port")
        .and_then(Value::as_u64)
        .map(|p| p as u16)
        .unwrap_or(oauth_loopback::DEFAULT_LOOPBACK_PORT);
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);

    let state_value = generate_state_value();
    let state = Arc::new(LoopbackState::new());
    let receiver = state.register(state_value.clone())?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server_state = Arc::clone(&state);
    let server_handle = tokio::spawn(async move {
        let _ =
            oauth_loopback::serve_loopback_with_shutdown(server_state, addr, async move {
                let _ = shutdown_rx.await;
            })
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

async fn oauth_await(params: &Value) -> Result<Value, String> {
    let state_value = params
        .get("state_value")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing required param: state_value".to_string())?
        .to_string();

    let timeout_secs = params.get("timeout_secs").and_then(Value::as_u64);

    let await_future = await_callback(&state_value);
    let loopback = match timeout_secs {
        Some(secs) => match tokio::time::timeout(
            std::time::Duration::from_secs(secs),
            await_future,
        )
        .await
        {
            Ok(Some(Ok(res))) => res,
            Ok(Some(Err(e))) => return Err(e),
            Ok(None) => {
                return Err(format!(
                    "no loopback registered for state_value '{state_value}'"
                ))
            }
            Err(_) => {
                return Err(format!(
                    "oauth callback timed out after {secs}s for state_value '{state_value}'"
                ))
            }
        },
        None => match await_future.await {
            Some(Ok(res)) => res,
            Some(Err(e)) => return Err(e),
            None => {
                return Err(format!(
                    "no loopback registered for state_value '{state_value}'"
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

async fn oauth_stop(params: &Value) -> Result<Value, String> {
    let state_value = params
        .get("state_value")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing required param: state_value".to_string())?
        .to_string();

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

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a manual callback for `state_value` without spinning up a
    /// real axum server — directly installs a `oneshot::Receiver` in the
    /// inbox that resolves to `result` when awaited.
    fn install_test_callback(state_value: &str, result: LoopbackResult) {
        let (tx, rx) = oneshot::channel();
        inbox_put(state_value.into(), rx);
        let _ = tx.send(result);
    }

    #[tokio::test]
    async fn unknown_fn_name_errors() {
        let err = run_internal("bogus", &json!({})).await.unwrap_err();
        assert!(err.contains("unknown internal fn_name 'bogus'"), "{err}");
    }

    #[tokio::test]
    async fn oauth_await_missing_state_value_errors() {
        let err = run_internal("oauth_await", &json!({})).await.unwrap_err();
        assert!(err.contains("missing required param: state_value"), "{err}");
    }

    #[tokio::test]
    async fn oauth_await_unknown_state_value_errors() {
        let err = run_internal("oauth_await", &json!({"state_value": "nope"}))
            .await
            .unwrap_err();
        assert!(err.contains("no loopback registered"), "{err}");
    }

    #[tokio::test]
    async fn oauth_await_captures_success() {
        install_test_callback(
            "test-state",
            LoopbackResult {
                code: Some("the-code".into()),
                state: Some("test-state".into()),
                error: None,
                error_description: None,
            },
        );

        let v = run_internal(
            "oauth_await",
            &json!({"state_value": "test-state"}),
        )
        .await
        .unwrap();

        assert_eq!(v.get("code").and_then(Value::as_str), Some("the-code"));
        assert_eq!(
            v.get("state_value").and_then(Value::as_str),
            Some("test-state")
        );
        assert_eq!(v.get("succeeded").and_then(Value::as_bool), Some(true));
    }

    #[tokio::test]
    async fn oauth_await_captures_error() {
        install_test_callback(
            "denied",
            LoopbackResult {
                code: None,
                state: Some("denied".into()),
                error: Some("access_denied".into()),
                error_description: Some("user said no".into()),
            },
        );

        let v = run_internal("oauth_await", &json!({"state_value": "denied"}))
            .await
            .unwrap();

        assert_eq!(
            v.get("error").and_then(Value::as_str),
            Some("access_denied")
        );
        assert_eq!(
            v.get("error_description").and_then(Value::as_str),
            Some("user said no")
        );
        assert_eq!(v.get("succeeded").and_then(Value::as_bool), Some(false));
    }

    #[tokio::test]
    async fn oauth_stop_missing_state_value_errors() {
        let err = run_internal("oauth_stop", &json!({})).await.unwrap_err();
        assert!(err.contains("missing required param: state_value"), "{err}");
    }

    #[tokio::test]
    async fn oauth_stop_unknown_state_value_succeeds_with_message() {
        let v = run_internal("oauth_stop", &json!({"state_value": "never-registered"}))
            .await
            .unwrap();
        assert_eq!(v.get("stopped").and_then(Value::as_bool), Some(false));
    }

    #[test]
    fn generate_state_value_is_unique_and_hex() {
        let a = generate_state_value();
        let b = generate_state_value();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
