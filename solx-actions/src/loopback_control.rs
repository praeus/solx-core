//! Dispatcher-level native handler for the `oauth-loopback` action.
//!
//! Provider-agnostic OAuth 2.0 authorization-code loopback controller.
//! Drives the lifecycle of one or more loopback HTTP listeners
//! (defined in `sol_server::oauth_loopback`) that capture the redirect
//! from any RFC 6749 / RFC 8252 provider.
//!
//! Three modes drive the action from the dispatcher:
//!
//! * `mode: "start"` — binds `127.0.0.1:{port}` (default `8765`), generates a
//!   random `state_value` (CSRF token), registers a pending
//!   `oneshot::Receiver` for the callback. Returns the `port`,
//!   `redirect_uri`, and `state_value` synchronously.
//! * `mode: "await_callback"` — blocks until the registered loopback for
//!   `state_value` receives the provider's redirect (or until the loopback
//!   is stopped). Returns the captured `code` / `error`. This is what
//!   makes the full OAuth choreography reachable through the dispatcher
//!   without any Tauri event bus.
//! * `mode: "stop"` — triggers graceful shutdown of the loopback for
//!   `state_value` and drops the inbox receiver.
//!
//! The listener itself runs in the `sol-manager` process (same one that
//! will use the OAuth tokens), so no IPC boundary is needed. No Tauri
//! events are emitted — every consumer of this action goes through the
//! dispatcher, and the three modes above cover the entire OAuth dance.
//!
//! See [`crate::dispatcher::execute_action_once`] for the fallthrough
//! that chains this handler into the dispatch pipeline.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{json, Value};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use sol_core::db::DbManager;
use sol_core::entities::ActionExecResult;
use sol_server::oauth_loopback::{self, LoopbackResult, LoopbackState};

use crate::browser_actions::BrowserActions;

const FN_NAME: &str = "oauth-loopback";

/// Per-listener handle held in the registry. The `shutdown_tx` triggers
/// the axum server's `with_graceful_shutdown`; the `server_handle` lets
/// us await the task (mostly to keep tests deterministic — production
/// servers are expected to terminate quickly after shutdown).
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

/// Generate a 32-hex-char CSRF state value from a CSPRNG.
///
/// Uses [`rand::random`] (the `rand` crate's thread-local
/// [`rand::rngs::ThreadRng`], which on supported targets is
/// ChaCha12-based) and emits 16 random bytes as 32 lower-hex
/// characters. The token is bound to a localhost redirect URI; CSPRNG
/// strength means even a peer on the same host cannot guess the token.
fn generate_state_value() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    // Thread-local CSPRNG; no fall-through to a hash-based scheme.
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut s = String::with_capacity(32);
    for b in bytes {
        // Two hex digits per byte: align with how OAuth libraries
        // conventionally format CSRF state.
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Returns `Some(Ok/Err)` when `fn_name == "oauth-loopback"`, `None`
/// otherwise so the dispatcher can fall through.
pub async fn try_execute(
    _db: &dyn DbManager,
    _browser_actions: Option<&Arc<dyn BrowserActions>>,
    fn_name: Option<&str>,
    params: &Value,
) -> Option<Result<ActionExecResult, String>> {
    if fn_name != Some(FN_NAME) {
        return None;
    }

    let mode = params
        .get("mode")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            "missing required param: mode \
             (\"start\" | \"await_callback\" | \"stop\")"
                .to_string()
        });

    let mode = match mode {
        Ok(m) => m,
        Err(e) => return Some(Err(e)),
    };

    match mode {
        "start" => Some(Ok(start_loopback(params).await)),
        "await_callback" => Some(Ok(await_callback_action(params).await)),
        "stop" => Some(stop_loopback(params).await),
        other => Some(Err(format!(
            "invalid oauth-loopback mode '{other}'; \
             expected \"start\", \"await_callback\", or \"stop\""
        ))),
    }
}

// ── start ────────────────────────────────────────────────────────────────────

async fn start_loopback(params: &Value) -> ActionExecResult {
    let port = params
        .get("port")
        .and_then(Value::as_u64)
        .map(|p| p as u16)
        .unwrap_or(oauth_loopback::DEFAULT_LOOPBACK_PORT);
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);

    let state_value = generate_state_value();
    let state = Arc::new(LoopbackState::new());
    let receiver = match state.register(state_value.clone()) {
        Ok(rx) => rx,
        Err(e) => {
            return ActionExecResult {
                action_name: FN_NAME.to_string(),
                result: json!({ "started": false, "error": e }),
                success: false,
                message: Some(e),
                trace: Vec::new(),
            };
        }
    };

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server_state = Arc::clone(&state);
    let server_handle = tokio::spawn(async move {
        let _ =
            oauth_loopback::serve_loopback_with_shutdown(server_state, addr, async move {
                let _ = shutdown_rx.await;
            })
            .await;
    });

    // Hand the receiver back to the caller via a small "inbox" so the
    // `await_callback` action can pick it up by `state_value` later. We
    // use a separate OnceLock<Mutex<HashMap<state_value, Receiver>>> so
    // the user's next call can `await_callback(state_value)` without
    // needing to keep the start action's return value around.
    inbox_put(state_value.clone(), receiver);

    // Register the shutdown signal + server handle so `stop` can clean up.
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

    ActionExecResult {
        action_name: FN_NAME.to_string(),
        result: json!({
            "started": true,
            "port": port,
            "redirect_uri": redirect_uri,
            "state_value": state_value,
            "started_at": started_at,
        }),
        success: true,
        message: Some(format!(
            "OAuth loopback listening on {addr} (state {state_value})"
        )),
        trace: Vec::new(),
    }
}

// ── await_callback ───────────────────────────────────────────────────────────

async fn await_callback_action(params: &Value) -> ActionExecResult {
    let state_value = match params.get("state_value").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => {
            return ActionExecResult {
                action_name: FN_NAME.to_string(),
                result: json!({"error": "missing required param: state_value"}),
                success: false,
                message: Some("missing required param: state_value".to_string()),
                trace: Vec::new(),
            };
        }
    };

    let timeout_secs = params
        .get("timeout_secs")
        .and_then(Value::as_u64);

    let await_future = await_callback(&state_value);
    let result = match timeout_secs {
        Some(secs) => match tokio::time::timeout(
            std::time::Duration::from_secs(secs),
            await_future,
        )
        .await
        {
            Ok(Some(res)) => res,
            Ok(None) => Err(format!(
                "no loopback registered for state_value '{state_value}'"
            )),
            Err(_) => Err(format!(
                "oauth callback timed out after {secs}s for state_value '{state_value}'"
            )),
        },
        None => match await_future.await {
            Some(res) => res,
            None => Err(format!(
                "no loopback registered for state_value '{state_value}'"
            )),
        },
    };

    match result {
        Ok(loopback) => {
            let succeeded = loopback.succeeded();
            // Mirror the provider's result shape so callers can pass the
            // action's `result` straight into the next action's params
            // (the instruction pipeline already does this).
            let mut obj = serde_json::Map::new();
            obj.insert("state_value".into(), Value::String(state_value.clone()));
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

            let error_message = if !succeeded {
                loopback
                    .error_description
                    .clone()
                    .or_else(|| loopback.error.clone())
            } else {
                None
            };

            ActionExecResult {
                action_name: FN_NAME.to_string(),
                result: Value::Object(obj),
                success: succeeded,
                message: if succeeded {
                    Some("OAuth callback captured".to_string())
                } else {
                    error_message
                },
                trace: Vec::new(),
            }
        }
        Err(e) => ActionExecResult {
            action_name: FN_NAME.to_string(),
            result: json!({ "error": e, "state_value": state_value }),
            success: false,
            message: Some(e),
            trace: Vec::new(),
        },
    }
}

// ── stop ─────────────────────────────────────────────────────────────────────

async fn stop_loopback(params: &Value) -> Result<ActionExecResult, String> {
    let state_value = params
        .get("state_value")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing required param: state_value".to_string())?
        .to_string();

    let handle = registry()
        .lock()
        .map_err(|e| format!("loopback registry poisoned: {e}"))?
        .remove(&state_value);

    // Drop the receiver so any in-flight callback send becomes a no-op.
    inbox_drop(&state_value);

    match handle {
        Some(LoopbackHandle { shutdown_tx, .. }) => {
            // Best-effort: ignore send errors (the receiver may already
            // have been dropped if the server task exited for another reason).
            let _ = shutdown_tx.send(());

            Ok(ActionExecResult {
                action_name: FN_NAME.to_string(),
                result: json!({ "stopped": true, "state_value": state_value }),
                success: true,
                message: Some(format!("OAuth loopback for state {state_value} stopped")),
                trace: Vec::new(),
            })
        }
        None => Ok(ActionExecResult {
            action_name: FN_NAME.to_string(),
            result: json!({
                "stopped": false,
                "state_value": state_value,
                "error": format!("no loopback registered for state_value '{state_value}'"),
            }),
            success: false,
            message: Some(format!(
                "no loopback registered for state_value '{state_value}'"
            )),
            trace: Vec::new(),
        }),
    }
}

// ── Receiver inbox ───────────────────────────────────────────────────────────
//
// The `start` action stores the oneshot::Receiver here, keyed by
// `state_value`. The `await_callback` action picks it up (and removes it
// from the inbox so each callback fires exactly once). Nothing else
// needs to read this map directly; the public entry point is the
// `try_execute` action dispatch.

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
pub async fn await_callback(state_value: &str) -> Option<Result<LoopbackResult, String>> {
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

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use sol_core::db::DbManager;

    /// Stub DbManager. The loopback_control handler never calls into the db,
    /// so we just use the shared `MockDbManager` from `sol_core::db::mock`.
    fn empty_db() -> std::sync::Arc<dyn DbManager> {
        std::sync::Arc::new(sol_core::db::mock::MockDbManager::new())
    }

    fn host() -> Arc<dyn BrowserActions> {
        Arc::new(NoopBrowserActions)
    }
    struct NoopBrowserActions;
    #[async_trait]
    impl BrowserActions for NoopBrowserActions {
        async fn dom_exec(
            &self,
            _op: &str,
            _p: &Value,
        ) -> Result<crate::browser_actions::BrowserDomActionResult, String> {
            Err("not used".into())
        }
        async fn navigate(&self, _url: &str) -> Result<(), String> {
            Ok(())
        }
        async fn listen(&self, _e: &str, _a: &str) -> Result<String, String> {
            Ok("k".into())
        }
        async fn emit(&self, _e: &str, _p: &str) -> Result<(), String> {
            Ok(())
        }
        async fn unlisten(&self, _id: &str) -> Result<(), String> {
            Ok(())
        }
    }

    /// Build an action params Value from a state_value and a `loopback_control`
    /// mode plus any extra JSON the caller wants merged in.
    fn params_for_mode(mode: &str, state_value: Option<&str>, extra: Value) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("mode".into(), Value::String(mode.into()));
        if let Some(s) = state_value {
            m.insert("state_value".into(), Value::String(s.into()));
        }
        if let Value::Object(extra_obj) = extra {
            for (k, v) in extra_obj {
                m.insert(k, v);
            }
        }
        Value::Object(m)
    }

    fn dispatch(fn_name: Option<&str>, params: Value) -> Option<Result<ActionExecResult, String>> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(try_execute(empty_db().as_ref(), Some(&host()), fn_name, &params))
    }

    #[test]
    fn unknown_fn_name_returns_none() {
        assert!(dispatch(Some("not-oauth-loopback"), json!({})).is_none());
    }

    #[test]
    fn missing_mode_param_errors() {
        let err = dispatch(Some(FN_NAME), json!({})).unwrap().unwrap_err();
        assert!(err.contains("missing required param: mode"), "{err}");
    }

    #[test]
    fn invalid_mode_errors() {
        let err = dispatch(
            Some(FN_NAME),
            params_for_mode("dance", None, json!({})),
        )
        .unwrap()
        .unwrap_err();
        assert!(err.contains("invalid oauth-loopback mode 'dance'"), "{err}");
        assert!(err.contains("await_callback"), "{err}");
    }

    #[test]
    fn stop_with_missing_state_value_errors() {
        let err = dispatch(
            Some(FN_NAME),
            params_for_mode("stop", None, json!({})),
        )
        .unwrap()
        .unwrap_err();
        assert!(err.contains("missing required param: state_value"), "{err}");
    }

    #[test]
    fn stop_unknown_state_value_succeeds_with_message() {
        let ar = dispatch(
            Some(FN_NAME),
            params_for_mode("stop", Some("never-registered"), json!({})),
        )
        .unwrap()
        .unwrap();
        assert!(!ar.success);
        let msg = ar.message.unwrap();
        assert!(msg.contains("no loopback registered"), "{msg}");
        assert_eq!(
            ar.result.get("stopped").and_then(Value::as_bool),
            Some(false)
        );
    }

    #[test]
    fn generate_state_value_is_unique_and_hex() {
        let a = generate_state_value();
        let b = generate_state_value();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ── await_callback action tests ────────────────────────────────────────

    /// Drive a manual callback for `state_value` without spinning up a
    /// real axum server — directly installs a `oneshot::Receiver` in the
    /// inbox that resolves to `result` when awaited.
    fn install_test_callback(state_value: &str, result: LoopbackResult) {
        let (tx, rx) = oneshot::channel();
        inbox_put(state_value.into(), rx);
        let _ = tx.send(result);
    }

    #[test]
    fn await_callback_unknown_state_value_errors() {
        let ar = dispatch(
            Some(FN_NAME),
            params_for_mode("await_callback", Some("nope"), json!({})),
        )
        .unwrap()
        .unwrap();
        assert!(!ar.success);
        assert!(ar
            .message
            .as_deref()
            .unwrap_or("")
            .contains("no loopback registered"));
    }

    #[test]
    fn await_callback_captures_success() {
        install_test_callback(
            "test-state",
            LoopbackResult {
                code: Some("the-code".into()),
                state: Some("test-state".into()),
                error: None,
                error_description: None,
            },
        );

        let ar = dispatch(
            Some(FN_NAME),
            params_for_mode("await_callback", Some("test-state"), json!({})),
        )
        .unwrap()
        .unwrap();

        assert!(ar.success);
        assert_eq!(
            ar.result.get("code").and_then(Value::as_str),
            Some("the-code")
        );
        assert_eq!(
            ar.result.get("state_value").and_then(Value::as_str),
            Some("test-state")
        );
        assert_eq!(
            ar.result.get("succeeded").and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn await_callback_captures_error() {
        install_test_callback(
            "denied",
            LoopbackResult {
                code: None,
                state: Some("denied".into()),
                error: Some("access_denied".into()),
                error_description: Some("user said no".into()),
            },
        );

        let ar = dispatch(
            Some(FN_NAME),
            params_for_mode("await_callback", Some("denied"), json!({})),
        )
        .unwrap()
        .unwrap();

        assert!(!ar.success);
        assert_eq!(
            ar.result.get("error").and_then(Value::as_str),
            Some("access_denied")
        );
        assert_eq!(
            ar.result.get("error_description").and_then(Value::as_str),
            Some("user said no")
        );
        assert_eq!(
            ar.result.get("succeeded").and_then(Value::as_bool),
            Some(false)
        );
    }

    #[test]
    fn await_callback_with_timeout_fires_after_no_callback() {
        // No callback installed; await with a short timeout. The future
        // should time out and the action should return success=false.
        let ar = dispatch(
            Some(FN_NAME),
            params_for_mode(
                "await_callback",
                Some("never-arrives"),
                json!({ "timeout_secs": 1 }),
            ),
        )
        .unwrap()
        .unwrap();

        assert!(!ar.success);
        let msg = ar.message.unwrap_or_default();
        assert!(msg.contains("timed out") || msg.contains("no loopback registered"), "{msg}");
    }
}

// ── Built-in action metadata ─────────────────────────────────────────────────

/// Loopback-control action metadata. Single action: drives the OAuth
/// 2.0 authorization-code loopback listener lifecycle (start, await,
/// stop).
pub const BUILTIN_ACTIONS: &[sol_core::builtin_actions::BuiltinAction] = &[
    sol_core::builtin_actions::BuiltinAction {
        name: "oauth loopback",
        fn_name: Some("oauth-loopback"),
        bin_name: None,
        phrases: &[
            "oauth loopback",
            "start oauth loopback",
            "stop oauth loopback",
            "await oauth callback",
            "oauth callback listener",
            "oauth loopback server",
            "oauth sign in callback",
        ],
        description: "Provider-agnostic OAuth 2.0 authorization-code loopback. Three modes drive the action end-to-end through the dispatcher (no Tauri event bus required): `mode: \"start\"` binds 127.0.0.1:{port} (default 8765) and returns a CSRF `state_value` plus `redirect_uri`; `mode: \"await_callback\"` blocks until the user finishes the sign-in flow and returns `{code, state, error, succeeded}`; `mode: \"stop\"` releases the port. Works with any RFC 6749 / RFC 8252 provider (Google, Microsoft, GitHub, Slack, etc.).",
        model_instructions: Some(
            "Generic OAuth 2.0 authorization-code with loopback redirect (RFC 6749 §4.1, RFC 8252 §7.3). To complete a sign-in flow with any OAuth provider: (1) call `oauth loopback` with `{\"mode\": \"start\"}` (optionally override `port` if the default 8765 is taken). The result contains `port`, `redirect_uri` (`http://127.0.0.1:{port}/callback`), and a random `state_value` (CSRF token). (2) build the provider's authorization URL with `redirect_uri=<redirect_uri>`, `state=<state_value>`, and the provider's required scopes. Add `access_type=offline&prompt=consent` if you need a refresh token. (3) call `open system browser` with `{\"url\": <auth_url>}`. (4) call `oauth loopback` with `{\"mode\": \"await_callback\", \"state_value\": <state_value>}` (optionally add `timeout_secs` to bound the wait — defaults to no timeout). The action blocks until the provider redirects to the loopback; result is `{code, state, error, error_description, succeeded, state_value}`. **Validate `state == state_value` before proceeding** (CSRF protection). (5) POST the code to the provider's token endpoint (with `code_verifier` if PKCE) to exchange for `access_token` / `refresh_token`. (6) call `oauth loopback` with `{\"mode\": \"stop\", \"state_value\": <state_value>}` to release the port. Store the tokens on the consuming action's `action_config.auth` block. Error path: if the provider redirects with `?error=access_denied&error_description=...`, the await_callback action returns `success: false` and the result contains the error fields so the caller can surface it instead of timing out.",
        ),
        parameter_type_name: None,
        result_type_name: None,
        destructive: None,
    },
];

pub struct LoopbackControlActionsProvider;

impl sol_core::builtin_actions::BuiltinActionsProvider for LoopbackControlActionsProvider {
    fn source_name(&self) -> &'static str {
        "loopback_control"
    }

    fn actions(&self) -> Vec<sol_core::builtin_actions::BuiltinAction> {
        BUILTIN_ACTIONS.to_vec()
    }
}

/// Register the loopback-control handler with the dispatcher.
pub fn register(registry: &mut crate::dispatcher::HandlerRegistry) {
    registry.push_borrowing(
        |db, _search_engine, browser_actions, _event_hooks, fn_name, params, _caller_action| {
            Box::pin(async move {
                let ba = browser_actions.as_ref();
                let fn_name_ref = fn_name.as_deref();
                try_execute(&*db, ba, fn_name_ref, &params).await
            })
        },
    );
}
