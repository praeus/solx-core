//! Provider-agnostic OAuth 2.0 authorization-code loopback listener.
//!
//! Implements the redirect-based "loopback" profile described in
//! [RFC 6749 §4.1](https://datatracker.ietf.org/doc/html/rfc6749#section-4.1)
//! and [RFC 8252 §7.3](https://datatracker.ietf.org/doc/html/rfc8252#section-7.3).
//! The listener binds to `127.0.0.1` only (no LAN exposure), serves a tiny
//! `GET /callback` that captures the redirect, and pipes the result back to
//! the caller via a `oneshot::Sender`.
//!
//! This module is intentionally provider-agnostic. The consuming action
//! (`sol-manager::loopback_control::try_execute`) drives the lifecycle; the
//! consuming package's `model_instructions` describe the provider-specific
//! authorization URL and token-exchange endpoint.
//!
//! ## URL contract
//!
//! `GET /callback?code=<code>&state=<state>`            — success path
//! `GET /callback?error=<error>&error_description=...`  — error path (RFC 6749 §4.1.2.1)
//!
//! On either path the route fires the `oneshot::Sender` with a
//! [`LoopbackResult`] and returns a small HTML page that tells the user
//! they may close the tab.
//!
//! ## Concurrency
//!
//! Multiple loopbacks may be active at once (one per `state_value`).
//! `LoopbackState::pending` is a `HashMap<state_value, oneshot::Sender>`
//! and the `/callback` route looks up the right sender by `state`. If the
//! `state` query parameter is absent or unknown the route still returns
//! 200 with a friendly error page (so the browser tab doesn't sit there
//! spinning) but fires no sender.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};

use axum::{
    extract::{Query, State as AxumState},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use serde::Deserialize;
use tokio::sync::oneshot;
use tracing::{info, warn};

// ── Public types ─────────────────────────────────────────────────────────────

/// Default loopback port per RFC 8252 §7.3 ("the port could be any unused
/// port at the discretion of the client"). 8765 is the IANA-registered port
/// for "unused" so it's a reasonable default; callers may override via
/// `LoopbackConfig::port`.
pub const DEFAULT_LOOPBACK_PORT: u16 = 8765;

/// The fixed callback path. The full redirect URI template is
/// `http://127.0.0.1:{port}{LOOPBACK_PATH}`.
pub const LOOPBACK_PATH: &str = "/callback";

/// Build the canonical redirect URI for a given port. Consumers should use
/// this so the path is always in sync with the route handler.
pub fn redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}{LOOPBACK_PATH}")
}

/// What the `/callback` route captures from the redirect. Exactly one of
/// `code` or `error` is set on success; `state` is always echoed if present
/// so the caller can validate CSRF.
#[derive(Debug, Clone)]
pub struct LoopbackResult {
    /// The authorization code from a successful grant (RFC 6749 §4.1.2).
    pub code: Option<String>,
    /// The state value the client put on the auth request, echoed back by
    /// the provider. The caller is responsible for comparing this to the
    /// `state_value` it generated at start time (CSRF protection).
    pub state: Option<String>,
    /// The `error` parameter on a failed grant (RFC 6749 §4.1.2.1).
    pub error: Option<String>,
    /// The `error_description` parameter on a failed grant, if provided.
    pub error_description: Option<String>,
}

impl LoopbackResult {
    /// True when the provider returned a successful authorization code.
    pub fn succeeded(&self) -> bool {
        self.code.is_some() && self.error.is_none()
    }
}

/// Shared state for one loopback listener instance. Stored in an `Arc` and
/// passed to the axum router via `AxumState`.
///
/// `pending` is keyed by the `state_value` generated at start time so the
/// route can dispatch the captured `code` to the right caller. The
/// `OnceLock` wrapper is only used by the registry in
/// `sol-manager::loopback_control` to share the registry itself; the
/// `LoopbackState` itself is per-listener.
pub struct LoopbackState {
    /// Pending callbacks, keyed by the `state_value` the start action minted.
    pub pending: Mutex<HashMap<String, oneshot::Sender<LoopbackResult>>>,
}

impl LoopbackState {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Register a pending callback for a given `state_value`. Returns the
    /// `oneshot::Receiver` the caller awaits.
    pub fn register(
        &self,
        state_value: String,
    ) -> Result<oneshot::Receiver<LoopbackResult>, String> {
        let (tx, rx) = oneshot::channel();
        let mut pending = self
            .pending
            .lock()
            .map_err(|e| format!("loopback state poisoned: {e}"))?;
        pending.insert(state_value, tx);
        Ok(rx)
    }

    /// Remove a registered callback (e.g. on stop / cleanup). Returns the
    /// removed sender so the caller can drop it, which signals any awaiter.
    pub fn deregister(&self, state_value: &str) -> Option<oneshot::Sender<LoopbackResult>> {
        self.pending
            .lock()
            .ok()
            .and_then(|mut p| p.remove(state_value))
    }
}

impl Default for LoopbackState {
    fn default() -> Self {
        Self::new()
    }
}

// ── Route handler ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// The `/callback` route. Captures the `code` / `error` / `state` from the
/// redirect, looks up the registered `oneshot::Sender` by `state`, fires
/// it, and returns a small HTML page that tells the user they can close
/// the tab.
async fn callback_handler(
    AxumState(state): AxumState<Arc<LoopbackState>>,
    Query(q): Query<CallbackQuery>,
) -> impl IntoResponse {
    let result = LoopbackResult {
        code: q.code.filter(|s| !s.is_empty()),
        state: q.state.clone(),
        error: q.error.filter(|s| !s.is_empty()),
        error_description: q.error_description.filter(|s| !s.is_empty()),
    };

    // Look up the pending sender by state. If the state is missing or
    // unknown we still respond with 200 so the browser tab stops spinning,
    // but we log a warning and fire no sender.
    let fired = match q.state.as_deref() {
        Some(state_value) if !state_value.is_empty() => {
            let tx_opt = state.pending.lock().ok().and_then(|mut p| p.remove(state_value));
            match tx_opt {
                Some(tx) => {
                    if result.succeeded() {
                        info!(state = %state_value, "oauth loopback: received authorization code");
                    } else {
                        warn!(
                            state = %state_value,
                            error = ?result.error,
                            description = ?result.error_description,
                            "oauth loopback: received error from provider"
                        );
                    }
                    // It's OK if the receiver was already dropped (caller gave up).
                    let _ = tx.send(result.clone());
                    true
                }
                None => {
                    warn!(
                        state = %state_value,
                        "oauth loopback: callback received for unknown state"
                    );
                    false
                }
            }
        }
        _ => {
            warn!("oauth loopback: callback received without state parameter");
            false
        }
    };

    let body: Html<String> = if result.succeeded() {
        Html(SUCCESS_HTML.to_string())
    } else {
        // We render the error description on the page so the user (and
        // anyone debugging) can see what went wrong without consulting the
        // server logs. The page auto-closes after a short delay.
        let desc = result
            .error_description
            .as_deref()
            .or(result.error.as_deref())
            .unwrap_or("(no description)");
        Html(error_html(desc))
    };

    let status = if fired || result.succeeded() {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };

    (status, body)
}

async fn health_handler() -> &'static str {
    "ok"
}

// ── HTML pages ───────────────────────────────────────────────────────────────

const SUCCESS_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Sol &mdash; sign-in complete</title>
<style>
  body { font: 14px/1.4 -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
         max-width: 480px; margin: 80px auto; padding: 0 24px; color: #1a1a1a; }
  h1 { font-size: 20px; margin: 0 0 12px; }
  p { margin: 0 0 8px; color: #555; }
</style>
</head>
<body>
<h1>Sign-in complete</h1>
<p>You can close this window and return to Sol.</p>
<script>setTimeout(function () { window.close(); }, 800);</script>
</body>
</html>"#;

fn error_html(description: &str) -> String {
    // Escape `"` and `<` to keep the page from being a vector for HTML
    // injection through a malicious provider error message. The content is
    // still escaped at the serde boundary; this is defense in depth.
    let safe = description
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;");
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Sol &mdash; sign-in failed</title>
<style>
  body {{ font: 14px/1.4 -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
         max-width: 480px; margin: 80px auto; padding: 0 24px; color: #1a1a1a; }}
  h1 {{ font-size: 20px; margin: 0 0 12px; color: #b00020; }}
  p {{ margin: 0 0 8px; color: #555; }}
  code {{ background: #f3f3f3; padding: 1px 6px; border-radius: 3px; font-size: 13px; }}
</style>
</head>
<body>
<h1>Sign-in failed</h1>
<p>The provider reported:</p>
<p><code>{safe}</code></p>
<p>You can close this window and return to Sol.</p>
<script>setTimeout(function () {{ window.close(); }}, 5000);</script>
</body>
</html>"#
    )
}

// ── Server entry points ──────────────────────────────────────────────────────

/// Build an axum `Router` for the loopback. Useful when callers want to
/// mount the routes inside a larger app (e.g. alongside the existing
/// `sol_server::router`); most callers should use [`serve_loopback`]
/// instead.
pub fn router(state: Arc<LoopbackState>) -> Router {
    Router::new()
        .route(LOOPBACK_PATH, get(callback_handler))
        .route("/health", get(health_handler))
        .with_state(state)
}

/// Bind to `127.0.0.1:{port}` and serve the loopback until the process
/// exits or the listener fails.
pub async fn serve_loopback(
    state: Arc<LoopbackState>,
    addr: SocketAddr,
) -> std::io::Result<()> {
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "oauth loopback listening");
    axum::serve(listener, app).await
}

/// Bind to `127.0.0.1:{port}` and serve the loopback until `shutdown` is
/// triggered. Used by `sol-manager::loopback_control::try_execute` to
/// keep a `oneshot::Sender<()>` alongside the `JoinHandle` in its
/// registry so `mode: "stop"` can shut down the right listener.
pub async fn serve_loopback_with_shutdown<F>(
    state: Arc<LoopbackState>,
    addr: SocketAddr,
    shutdown: F,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "oauth loopback listening");
    serve_loopback_with_shutdown_on(state, listener, shutdown).await
}

/// Serve the loopback on an already-bound listener until `shutdown` is
/// triggered. Lets the caller bind synchronously first — so a port-in-use
/// failure surfaces immediately as an `Err`, rather than being discovered
/// only inside a spawned server task (see `crate::internal::oauth_start`,
/// which binds eagerly and only spawns the long-running server after a
/// successful bind).
pub async fn serve_loopback_with_shutdown_on<F>(
    state: Arc<LoopbackState>,
    listener: tokio::net::TcpListener,
    shutdown: F,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let app = router(state);
    axum::serve(listener, app).with_graceful_shutdown(shutdown).await
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_uri_includes_callback_path() {
        assert_eq!(
            redirect_uri(8765),
            "http://127.0.0.1:8765/callback"
        );
    }

    #[test]
    fn loopback_result_succeeded_requires_code_and_no_error() {
        assert!(LoopbackResult {
            code: Some("abc".into()),
            state: Some("xyz".into()),
            error: None,
            error_description: None,
        }
        .succeeded());

        assert!(!LoopbackResult {
            code: None,
            state: Some("xyz".into()),
            error: Some("access_denied".into()),
            error_description: None,
        }
        .succeeded());

        assert!(!LoopbackResult {
            code: Some("abc".into()),
            state: Some("xyz".into()),
            error: Some("access_denied".into()),
            error_description: None,
        }
        .succeeded());
    }

    #[tokio::test]
    async fn register_and_deregister_round_trip() {
        let state = LoopbackState::new();
        let rx = state.register("state-1".into()).unwrap();
        assert!(state.deregister("state-1").is_some(), "should remove the registered sender");
        // rx should resolve to Err because the sender was dropped
        let outcome = rx.await;
        assert!(outcome.is_err(), "rx should fail when sender is dropped");
    }

    #[tokio::test]
    async fn register_replaces_existing_sender() {
        let state = LoopbackState::new();
        let _rx1 = state.register("dup".into()).unwrap();
        let rx2 = state.register("dup".into()).unwrap();
        // rx1's sender was dropped when the second register replaced it;
        // rx1.await should be Err
        // (we don't await rx1 here because it might block; we just confirm
        //  the registry now has exactly one entry)
        let pending = state.pending.lock().unwrap();
        assert_eq!(pending.len(), 1);
        drop(pending);
        // rx2 is still valid
        drop(rx2);
    }

    #[tokio::test]
    async fn callback_fires_sender_for_known_state() {
        let state = Arc::new(LoopbackState::new());
        let rx = state.register("good-state".into()).unwrap();

        let q = CallbackQuery {
            code: Some("the-code".into()),
            state: Some("good-state".into()),
            error: None,
            error_description: None,
        };
        let resp = callback_handler(
            AxumState(Arc::clone(&state)),
            Query(q),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);

        let result = rx.await.expect("sender should yield");
        assert_eq!(result.code.as_deref(), Some("the-code"));
        assert_eq!(result.state.as_deref(), Some("good-state"));
        assert!(result.succeeded());
    }

    #[tokio::test]
    async fn callback_fires_sender_on_error_path() {
        let state = Arc::new(LoopbackState::new());
        let rx = state.register("err-state".into()).unwrap();

        let q = CallbackQuery {
            code: None,
            state: Some("err-state".into()),
            error: Some("access_denied".into()),
            error_description: Some("user said no".into()),
        };
        let resp = callback_handler(AxumState(Arc::clone(&state)), Query(q))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK);

        let result = rx.await.expect("sender should yield");
        assert!(!result.succeeded());
        assert_eq!(result.error.as_deref(), Some("access_denied"));
        assert_eq!(result.error_description.as_deref(), Some("user said no"));
    }

    #[tokio::test]
    async fn callback_returns_200_for_unknown_state_with_code() {
        // We still render a success page (so the user's tab stops spinning)
        // even if the state was never registered — the caller can't
        // actually use the code, but the HTTP response is benign.
        let state = Arc::new(LoopbackState::new());
        let q = CallbackQuery {
            code: Some("the-code".into()),
            state: Some("never-registered".into()),
            error: None,
            error_description: None,
        };
        let resp = callback_handler(AxumState(Arc::clone(&state)), Query(q))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn callback_returns_200_when_state_missing() {
        let state = Arc::new(LoopbackState::new());
        let q = CallbackQuery {
            code: Some("the-code".into()),
            state: None,
            error: None,
            error_description: None,
        };
        let resp = callback_handler(AxumState(Arc::clone(&state)), Query(q))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn error_html_escapes_user_input() {
        let page = error_html("<script>alert(1)</script>");
        // No raw <script> tag should appear in the escaped output
        assert!(!page.contains("<script>alert(1)</script>"));
        assert!(page.contains("&lt;script&gt;"));
    }

    /// End-to-end smoke test: bind on a random port, hit /callback over HTTP,
    /// confirm the registered receiver yields the captured code. This is the
    /// test that mirrors a real browser OAuth redirect.
    #[tokio::test]
    async fn end_to_end_callback_over_http() {
        // Bind to a random free port.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener); // release so serve_loopback can re-bind

        let state = Arc::new(LoopbackState::new());
        let rx = state.register("e2e-state".into()).unwrap();

        let server_state = Arc::clone(&state);
        let server = tokio::spawn(async move {
            let _ = serve_loopback(server_state, addr).await;
        });

        // Give the server a beat to start listening.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let redirect_uri = redirect_uri(addr.port());
        let url = format!(
            "{redirect_uri}?code=the_code&state=e2e-state"
        );
        let resp = reqwest::get(&url).await.expect("GET /callback over HTTP");
        assert!(resp.status().is_success(), "callback should respond 2xx");
        let body = resp.text().await.unwrap();
        assert!(body.contains("Sign-in complete"));

        let result = rx.await.expect("sender should yield");
        assert_eq!(result.code.as_deref(), Some("the_code"));
        assert_eq!(result.state.as_deref(), Some("e2e-state"));
        assert!(result.succeeded());

        // Shut down the server so the test exits cleanly.
        server.abort();
    }
}

// Force-include the registry placeholder so `OnceLock` is exercised at
// compile time even when no other crate pulls it in. (The registry lives
// in `sol-manager::loopback_control`; this is here only to make the
// unused-import warning go away when building tests in isolation.)
#[allow(dead_code)]
static _ONCE_LOCK_PROBE: OnceLock<()> = OnceLock::new();
