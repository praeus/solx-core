//! A process-lifetime, localhost-only HTTP listener that lets a spawned
//! Command action's child process write to its own console without
//! solx-core reading or parsing the child's stderr.
//!
//! See `docs/console-implementation-plan.md` §8a for why this replaced the
//! originally-planned stderr-capture approach: reading a child's stderr
//! concurrently with feeding its stdin reintroduces a documented deadlock
//! risk (`solx-actions`' `exec::run_command`'s `feed`/`join!` structure
//! exists specifically to avoid it), and a raw byte stream needs heuristic
//! parsing to recover structure. A loopback the child calls directly needs
//! neither.
//!
//! Reuses the shape of `solx-actions`' OAuth loopback: bind `127.0.0.1`
//! only, mint a random one-shot token per registration, tear the
//! registration down when it's no longer needed. The one structural
//! difference: the OAuth loopback is started and stopped per
//! `oauth-start`/`oauth-stop` call (one browser sign-in at a time); this
//! listener is started **once** and lives for the process's lifetime — only
//! the *registrations* (one per Command invocation) are short-lived, since
//! Command actions can run at any point throughout the process and
//! starting/stopping a listener around each one would mean needless
//! bind/unbind churn and port-reuse races.
//!
//! ## URL contract
//!
//! `POST /print` with header `Authorization: Bearer <token>` and a JSON
//! body `{"level"?, "message"?, "data"?}` — the same shape as
//! `/builtin/console/print`'s params, since that's exactly what this
//! forwards to. Unknown or missing tokens get `401`.
//!
//! `GET /cancelled` with the same header returns `{"cancelled": bool}` —
//! whether `action-stop` has been called for this invocation. Added
//! alongside `print` rather than as a separate listener, since it needs the
//! exact same one-token-one-invocation resolution; see
//! `docs/async-actions-plan.md` §5a.
//!
//! ## Security
//!
//! Bound to `127.0.0.1` only. Each token is single-purpose: it resolves to
//! exactly one `(console, invocations, action_ref, invocation_id)` set at
//! registration time, and [`Registration`] deregisters it on drop, so it
//! can't be replayed after that Command invocation ends and can't be used
//! to write to, or query the cancellation state of, any invocation other
//! than the one it was minted for.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use axum::extract::{Json as JsonExtractor, State as AxumState};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::OnceCell;
use tracing::warn;

use crate::console::ConsoleStore;
use crate::invocations::InvocationStore;

// ── Registry ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct Target {
    console: Arc<ConsoleStore>,
    invocations: Arc<InvocationStore>,
    action_ref: String,
    invocation_id: String,
}

type Registry = Arc<Mutex<HashMap<String, Target>>>;

struct Loopback {
    port: u16,
    registry: Registry,
}

static LOOPBACK: OnceCell<Loopback> = OnceCell::const_new();

/// A live registration. Holding this alive is what keeps the token valid;
/// dropping it (including via an early `?` return in `run_command`) removes
/// it from the registry, so callers get "deregister on every exit path" for
/// free just by keeping this bound in scope rather than needing a separate
/// cleanup step per return site.
pub struct Registration {
    token: String,
}

impl Registration {
    pub fn url(&self) -> String {
        // Safe: a `Registration` only exists after `register` has already
        // awaited `ensure_started`, so `LOOPBACK` is populated.
        let port = LOOPBACK.get().expect("loopback started before Registration exists").port;
        format!("http://127.0.0.1:{port}/print")
    }

    /// URL for the `GET /cancelled` cancellation check — a second endpoint,
    /// deliberately, rather than having the child string-munge `/print`
    /// into `/cancelled` itself.
    pub fn control_url(&self) -> String {
        let port = LOOPBACK.get().expect("loopback started before Registration exists").port;
        format!("http://127.0.0.1:{port}/cancelled")
    }

    pub fn token(&self) -> &str {
        &self.token
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(loopback) = LOOPBACK.get() {
            if let Ok(mut registry) = loopback.registry.lock() {
                registry.remove(&self.token);
            }
        }
    }
}

/// Start the loopback if it isn't already running in this process (a no-op
/// on every call after the first), and register a fresh one-shot token for
/// `(console, action_ref, invocation_id)`.
///
/// `None` only if the listener itself failed to bind — best-effort: the
/// caller should proceed without console access for that invocation rather
/// than failing the exec over it.
pub async fn register(
    console: Arc<ConsoleStore>,
    invocations: Arc<InvocationStore>,
    action_ref: &str,
    invocation_id: &str,
) -> Option<Registration> {
    let loopback = ensure_started().await?;
    let token = generate_token();
    loopback.registry.lock().ok()?.insert(
        token.clone(),
        Target {
            console,
            invocations,
            action_ref: action_ref.to_string(),
            invocation_id: invocation_id.to_string(),
        },
    );
    Some(Registration { token })
}

async fn ensure_started() -> Option<&'static Loopback> {
    LOOPBACK
        .get_or_try_init(|| async {
            // The server runs on its own dedicated OS thread with its own
            // single-threaded runtime, deliberately *not* `tokio::spawn`ed
            // onto whichever runtime happens to call this first. A plain
            // `tokio::spawn` here ties the server's lifetime to that
            // runtime — invisible in production (the whole process has
            // exactly one runtime), but under `#[tokio::test]` each test
            // function gets its own throwaway runtime, so the first test to
            // reach this would silently kill the "process-lifetime" server
            // for every test that runs after it. A dedicated thread makes
            // the process-lifetime guarantee actually true rather than
            // incidental.
            let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<u16>>();
            let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
            let thread_registry = registry.clone();

            std::thread::Builder::new()
                .name("solx-console-loopback".into())
                .spawn(move || {
                    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                        Ok(rt) => rt,
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            return;
                        }
                    };
                    rt.block_on(async move {
                        // Bind before sending the port back, so a bind
                        // failure surfaces as an `Err` to `register`'s
                        // caller rather than only inside this thread.
                        let listener = match tokio::net::TcpListener::bind(SocketAddr::new(
                            IpAddr::V4(Ipv4Addr::LOCALHOST),
                            0,
                        ))
                        .await
                        {
                            Ok(l) => l,
                            Err(e) => {
                                let _ = tx.send(Err(e));
                                return;
                            }
                        };
                        let port = match listener.local_addr() {
                            Ok(a) => a.port(),
                            Err(e) => {
                                let _ = tx.send(Err(e));
                                return;
                            }
                        };
                        let _ = tx.send(Ok(port));

                        let app = router(thread_registry);
                        if let Err(e) = axum::serve(listener, app).await {
                            warn!("console loopback server exited: {e}");
                        }
                    });
                })
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

            // `spawn_blocking` so waiting on the dedicated thread's std
            // channel doesn't stall a worker thread on *this* runtime.
            let port = tokio::task::spawn_blocking(move || rx.recv())
                .await
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))??;

            Ok::<_, std::io::Error>(Loopback { port, registry })
        })
        .await
        .ok()
}

fn generate_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut s = String::with_capacity(32);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ── Route handler ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct PrintBody {
    #[serde(default)]
    level: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    data: Option<Value>,
}

async fn print_handler(
    AxumState(registry): AxumState<Registry>,
    headers: HeaderMap,
    JsonExtractor(body): JsonExtractor<PrintBody>,
) -> StatusCode {
    let Some(token) = bearer_token(&headers) else {
        return StatusCode::UNAUTHORIZED;
    };
    let Some(target) = registry.lock().ok().and_then(|r| r.get(token).cloned()) else {
        return StatusCode::UNAUTHORIZED;
    };

    let level = body.level.unwrap_or_else(|| "info".to_string());
    let data = body.data.filter(|v| !v.is_null());
    match target
        .console
        .print(&target.action_ref, &target.invocation_id, None, &level, "command", body.message, data)
        .await
    {
        Ok(_) => StatusCode::OK,
        Err(e) => {
            warn!("console loopback: print failed for {}: {e}", target.action_ref);
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

async fn cancelled_handler(AxumState(registry): AxumState<Registry>, headers: HeaderMap) -> Response {
    let Some(token) = bearer_token(&headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Some(target) = registry.lock().ok().and_then(|r| r.get(token).cloned()) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };

    // Fails closed to `false` on a store error — a lookup failure must
    // never be mistaken for "yes, stop" by the caller.
    let cancelled = target
        .invocations
        .is_cancelled(&target.invocation_id)
        .await
        .unwrap_or(false);
    axum::Json(json!({ "cancelled": cancelled })).into_response()
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
}

fn router(registry: Registry) -> Router {
    Router::new()
        .route("/print", post(print_handler))
        .route("/cancelled", get(cancelled_handler))
        .with_state(registry)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::invocations::InvocationStore;
    use solx_config::ConfigService;

    async fn test_console() -> (tempfile::TempDir, Arc<ConsoleStore>, Arc<InvocationStore>) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Db::open(&dir.path().join("t.db")).await.unwrap();
        let config = Arc::new(ConfigService::open_in(dir.path()).unwrap());
        let console = Arc::new(ConsoleStore::new(db.clone(), config.clone()));
        console.ensure_schema().await.unwrap();
        let invocations = Arc::new(InvocationStore::new(db, config));
        invocations.ensure_schema().await.unwrap();
        (dir, console, invocations)
    }

    #[tokio::test]
    async fn register_returns_a_working_url_and_token() {
        let (_d, console, invocations) = test_console().await;
        let reg = register(console, invocations, "/pkg/foo", "inv-1")
            .await
            .expect("loopback should start");
        assert!(reg.url().starts_with("http://127.0.0.1:"));
        assert!(reg.control_url().starts_with("http://127.0.0.1:"));
        assert!(!reg.token().is_empty());
    }

    #[tokio::test]
    async fn print_over_http_reaches_the_right_console() {
        let (_d, console, invocations) = test_console().await;
        let reg = register(console.clone(), invocations, "/pkg/foo", "inv-1").await.unwrap();

        let client = reqwest::Client::new();
        let resp = client
            .post(reg.url())
            .bearer_auth(reg.token())
            .json(&serde_json::json!({"level": "warn", "message": "hi", "data": {"n": 1}}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);

        let read = console.read("/pkg/foo", None, 10).await.unwrap();
        assert_eq!(read.entries.len(), 1);
        assert_eq!(read.entries[0].level, "warn");
        assert_eq!(read.entries[0].message.as_deref(), Some("hi"));
        assert_eq!(read.entries[0].source, "command");
        assert_eq!(read.entries[0].invocation_id, "inv-1");
        assert_eq!(read.entries[0].data, Some(serde_json::json!({"n": 1})));
    }

    #[tokio::test]
    async fn print_defaults_level_to_info_and_tolerates_an_empty_body() {
        let (_d, console, invocations) = test_console().await;
        let reg = register(console.clone(), invocations, "/pkg/foo", "inv-1").await.unwrap();
        let client = reqwest::Client::new();
        let resp = client
            .post(reg.url())
            .bearer_auth(reg.token())
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let read = console.read("/pkg/foo", None, 10).await.unwrap();
        assert_eq!(read.entries[0].level, "info");
        assert_eq!(read.entries[0].message, None);
    }

    #[tokio::test]
    async fn missing_token_is_rejected() {
        let (_d, console, invocations) = test_console().await;
        let reg = register(console, invocations, "/pkg/foo", "inv-1").await.unwrap();
        let client = reqwest::Client::new();
        let resp = client
            .post(reg.url())
            .json(&serde_json::json!({"message": "hi"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn unknown_token_is_rejected() {
        let (_d, console, invocations) = test_console().await;
        let reg = register(console, invocations, "/pkg/foo", "inv-1").await.unwrap();
        let client = reqwest::Client::new();
        let resp = client
            .post(reg.url())
            .bearer_auth("not-a-real-token")
            .json(&serde_json::json!({"message": "hi"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn dropping_the_registration_deregisters_the_token() {
        let (_d, console, invocations) = test_console().await;
        let reg = register(console, invocations, "/pkg/foo", "inv-1").await.unwrap();
        let url = reg.url();
        let token = reg.token().to_string();
        drop(reg);

        let client = reqwest::Client::new();
        let resp = client
            .post(url)
            .bearer_auth(token)
            .json(&serde_json::json!({"message": "too late"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn two_registrations_stay_isolated() {
        let (_d, console, invocations) = test_console().await;
        let reg_a = register(console.clone(), invocations.clone(), "/pkg/a", "inv-a").await.unwrap();
        let reg_b = register(console.clone(), invocations, "/pkg/b", "inv-b").await.unwrap();

        let client = reqwest::Client::new();
        client.post(reg_a.url()).bearer_auth(reg_a.token()).json(&serde_json::json!({"message": "a"})).send().await.unwrap();
        client.post(reg_b.url()).bearer_auth(reg_b.token()).json(&serde_json::json!({"message": "b"})).send().await.unwrap();

        let a = console.read("/pkg/a", None, 10).await.unwrap();
        let b = console.read("/pkg/b", None, 10).await.unwrap();
        assert_eq!(a.entries.len(), 1);
        assert_eq!(b.entries.len(), 1);
        assert_eq!(a.entries[0].message.as_deref(), Some("a"));
        assert_eq!(b.entries[0].message.as_deref(), Some("b"));
    }

    #[tokio::test]
    async fn null_data_is_normalized_to_none() {
        let (_d, console, invocations) = test_console().await;
        let reg = register(console.clone(), invocations, "/pkg/foo", "inv-1").await.unwrap();
        let client = reqwest::Client::new();
        client
            .post(reg.url())
            .bearer_auth(reg.token())
            .json(&serde_json::json!({"message": "m", "data": null}))
            .send()
            .await
            .unwrap();
        let read = console.read("/pkg/foo", None, 10).await.unwrap();
        assert_eq!(read.entries[0].data, None);
    }

    #[tokio::test]
    async fn cancelled_reflects_no_row_as_false() {
        let (_d, console, invocations) = test_console().await;
        // No invocation row was ever created for "inv-1" (mirrors a plain
        // synchronous `exec`, which never calls `InvocationStore::create`) —
        // must fail closed to `false`, not error.
        let reg = register(console, invocations, "/pkg/foo", "inv-1").await.unwrap();
        let client = reqwest::Client::new();
        let resp = client.get(reg.control_url()).bearer_auth(reg.token()).send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body, serde_json::json!({"cancelled": false}));
    }

    #[tokio::test]
    async fn cancelled_reflects_a_requested_cancel() {
        let (_d, console, invocations) = test_console().await;
        invocations.create("inv-1", "/pkg/foo", 0).await.unwrap();
        let reg = register(console, invocations.clone(), "/pkg/foo", "inv-1").await.unwrap();

        let client = reqwest::Client::new();
        let before = client.get(reg.control_url()).bearer_auth(reg.token()).send().await.unwrap();
        assert_eq!(before.json::<serde_json::Value>().await.unwrap(), serde_json::json!({"cancelled": false}));

        invocations.request_cancel("inv-1").await.unwrap();

        let after = client.get(reg.control_url()).bearer_auth(reg.token()).send().await.unwrap();
        assert_eq!(after.json::<serde_json::Value>().await.unwrap(), serde_json::json!({"cancelled": true}));
    }

    #[tokio::test]
    async fn cancelled_endpoint_rejects_missing_and_unknown_tokens() {
        let (_d, console, invocations) = test_console().await;
        let reg = register(console, invocations, "/pkg/foo", "inv-1").await.unwrap();
        let client = reqwest::Client::new();

        let no_auth = client.get(reg.control_url()).send().await.unwrap();
        assert_eq!(no_auth.status(), reqwest::StatusCode::UNAUTHORIZED);

        let bad_auth = client.get(reg.control_url()).bearer_auth("not-a-real-token").send().await.unwrap();
        assert_eq!(bad_auth.status(), reqwest::StatusCode::UNAUTHORIZED);
    }
}
