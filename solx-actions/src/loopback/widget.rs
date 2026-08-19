//! A process-lifetime, localhost-only listener that serves a widget's JS
//! bundle and relays messages between the host (the WASM guest that opened
//! it, or an internal-action caller driving it directly) and the widget's
//! frontend, over a websocket. See `docs/widget-actions.md` §4 for the full
//! design this implements.
//!
//! Modeled on `loopback::console`'s process-lifetime shape — see that
//! module's doc comment for why the listener runs on its own dedicated OS
//! thread with its own single-threaded runtime rather than being
//! `tokio::spawn`ed onto whichever runtime happens to call `open` first
//! (`#[tokio::test]`'s per-test throwaway runtime would otherwise silently
//! kill it), and why a `spawn_blocking`-wrapped `std::mpsc` handshake is used
//! to surface a bind failure to the caller instead of only inside the
//! thread.
//!
//! The structural difference from `console`: a widget's lifetime is manual,
//! not tied to the scope of the call that opened it — `open` returns the
//! descriptor and the *opening* invocation then returns, while the widget
//! itself keeps running until an explicit `close` (or the reaper below
//! decides it's abandoned). So there is no `Registration` guard to `Drop`;
//! instead, each registration carries its own connect-window TTL and
//! reconnect grace period, and a periodic sweep on the loopback's own
//! runtime reaps anything that's overstayed either one. See the doc comment
//! on [`reap_once`] for the exact rule.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State as AxumState};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get as get_route;
use axum::Router;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use solx_surface::entities::WidgetDescriptor;
use solx_surface::managers::FileStore;
use tokio::sync::{mpsc, OnceCell};
use tokio::time::interval;
use tracing::warn;

// ── Registry ─────────────────────────────────────────────────────────────────

struct WidgetState {
    /// The single source of truth both sides read/write.
    fields: Value,
    visible: bool,
    /// Resolved once at `open`, so `/bundle` is a pure in-memory read.
    bundle: Vec<u8>,
    /// Not read anywhere today — kept alongside `fields` for a future
    /// introspection op (e.g. a `widget_list`), mirroring the design doc's
    /// `WidgetState` shape rather than trimming it to only what this round
    /// consumes.
    #[allow(dead_code)]
    tag_name: String,
    /// One-shot loopback token, gates both `/bundle` and `/ws`. Not
    /// consumed on connect — it stays valid for the widget's whole
    /// lifetime, so a reconnect within the grace period can reuse it.
    token: String,
    /// The connected widget's sender, if any. Host-side `set`/`show`/
    /// `hide`/`exec` are fire-and-forget when this is `None`: the state
    /// still updates, so a later `get` or a later connect sees the truth.
    ws_tx: Option<mpsc::UnboundedSender<Message>>,
    /// Attribution only (per `docs/widget-actions.md` §4.2) — not read
    /// anywhere today, kept for the same future-introspection reason as
    /// `tag_name` above.
    #[allow(dead_code)]
    action_ref: String,
    #[allow(dead_code)]
    invocation_id: String,
    opened_at: Instant,
    /// Set the instant the websocket disconnects, cleared on (re)connect.
    /// `None` while never-yet-connected *or* currently connected —
    /// `ws_tx.is_some()` disambiguates those two for the reaper.
    disconnected_at: Option<Instant>,
    connect_ttl: Duration,
    reconnect_grace: Duration,
}

/// Keyed by `widget_id` — the handle `open`'s caller gets back and every
/// other widget op (`close`/`show`/`hide`/`get`/`set`/`exec`) addresses.
type Registry = Arc<Mutex<HashMap<String, WidgetState>>>;

struct Loopback {
    port: u16,
    registry: Registry,
}

static LOOPBACK: OnceCell<Loopback> = OnceCell::const_new();

// ── Public API (called by the WASM host and the internal actions) ──────────────

/// Open a widget: reads the bundle bytes once, mints a `widget_id` + token,
/// and registers it. Returns the descriptor the caller surfaces to the
/// frontend. `connect_ttl`/`reconnect_grace` are captured per-registration
/// (from config at the caller's own call site) rather than read from a
/// shared config handle on every reap sweep, so a config change mid-process
/// only affects widgets opened after it.
pub async fn open(
    files: &Arc<dyn FileStore>,
    bin_name: &str,
    tag_name: &str,
    fields: Value,
    action_ref: &str,
    invocation_id: &str,
    connect_ttl: Duration,
    reconnect_grace: Duration,
) -> Result<WidgetDescriptor, String> {
    let loopback = ensure_started()
        .await
        .ok_or_else(|| "widget loopback failed to start".to_string())?;
    let bundle = files.get(bin_name).await.map_err(|e| e.to_string())?;

    let widget_id = uuid::Uuid::new_v4().to_string();
    let token = generate_token();
    let state = WidgetState {
        fields: fields.clone(),
        visible: false,
        bundle,
        tag_name: tag_name.to_string(),
        token: token.clone(),
        ws_tx: None,
        action_ref: action_ref.to_string(),
        invocation_id: invocation_id.to_string(),
        opened_at: Instant::now(),
        disconnected_at: None,
        connect_ttl,
        reconnect_grace,
    };
    loopback
        .registry
        .lock()
        .map_err(|_| "widget registry poisoned".to_string())?
        .insert(widget_id.clone(), state);

    Ok(WidgetDescriptor {
        widget_id: widget_id.clone(),
        tag_name: tag_name.to_string(),
        entry_url: format!("http://127.0.0.1:{}/bundle?token={token}", loopback.port),
        ws_url: format!("ws://127.0.0.1:{}/ws?token={token}", loopback.port),
        token,
        fields,
    })
}

/// Close a widget: deregisters its token (so `/bundle`/`/ws` can't be
/// replayed) and, if connected, tells the frontend to close before dropping
/// its sender.
pub async fn close(widget_id: &str) -> Result<(), String> {
    let Some(loopback) = LOOPBACK.get() else { return Ok(()) };
    let removed = loopback
        .registry
        .lock()
        .map_err(|_| "widget registry poisoned".to_string())?
        .remove(widget_id);
    if let Some(state) = removed {
        if let Some(tx) = state.ws_tx {
            let _ = tx.send(Message::Text(json!({ "op": "close" }).to_string()));
        }
    }
    Ok(())
}

pub async fn show(widget_id: &str) -> Result<(), String> {
    push_visibility(widget_id, true).await
}

pub async fn hide(widget_id: &str) -> Result<(), String> {
    push_visibility(widget_id, false).await
}

async fn push_visibility(widget_id: &str, visible: bool) -> Result<(), String> {
    let loopback = LOOPBACK.get().ok_or_else(|| "widget loopback not started".to_string())?;
    let tx = {
        let mut reg = loopback.registry.lock().map_err(|_| "widget registry poisoned".to_string())?;
        let state = reg
            .get_mut(widget_id)
            .ok_or_else(|| format!("no widget registered for widget_id '{widget_id}'"))?;
        state.visible = visible;
        state.ws_tx.clone()
    };
    if let Some(tx) = tx {
        let op = if visible { "show" } else { "hide" };
        let _ = tx.send(Message::Text(json!({ "op": op }).to_string()));
    }
    Ok(())
}

/// Read one field (JSON-encoded) or, if `field` is empty, the whole fields
/// object.
pub async fn get(widget_id: &str, field: &str) -> Result<Value, String> {
    let loopback = LOOPBACK.get().ok_or_else(|| "widget loopback not started".to_string())?;
    let reg = loopback.registry.lock().map_err(|_| "widget registry poisoned".to_string())?;
    let state = reg
        .get(widget_id)
        .ok_or_else(|| format!("no widget registered for widget_id '{widget_id}'"))?;
    Ok(if field.is_empty() {
        state.fields.clone()
    } else {
        state.fields.get(field).cloned().unwrap_or(Value::Null)
    })
}

/// Set one field. Updates state and, if connected, pushes the write to the
/// widget so its UI stays in sync.
pub async fn set(widget_id: &str, field: &str, value: Value) -> Result<(), String> {
    let loopback = LOOPBACK.get().ok_or_else(|| "widget loopback not started".to_string())?;
    let tx = {
        let mut reg = loopback.registry.lock().map_err(|_| "widget registry poisoned".to_string())?;
        let state = reg
            .get_mut(widget_id)
            .ok_or_else(|| format!("no widget registered for widget_id '{widget_id}'"))?;
        set_field(&mut state.fields, field, value.clone());
        state.ws_tx.clone()
    };
    if let Some(tx) = tx {
        let _ = tx.send(Message::Text(json!({ "op": "set", "field": field, "value": value }).to_string()));
    }
    Ok(())
}

/// Dispatch an event to the widget's frontend code. Fire-and-forget when
/// nothing is connected — there is no state for an event to update, unlike
/// `set`.
pub async fn exec(widget_id: &str, event: &str, payload: Value) -> Result<(), String> {
    let loopback = LOOPBACK.get().ok_or_else(|| "widget loopback not started".to_string())?;
    let tx = {
        let reg = loopback.registry.lock().map_err(|_| "widget registry poisoned".to_string())?;
        let state = reg
            .get(widget_id)
            .ok_or_else(|| format!("no widget registered for widget_id '{widget_id}'"))?;
        state.ws_tx.clone()
    };
    if let Some(tx) = tx {
        let _ = tx.send(Message::Text(json!({ "op": "exec", "event": event, "payload": payload }).to_string()));
    }
    Ok(())
}

fn set_field(fields: &mut Value, field: &str, value: Value) {
    if !fields.is_object() {
        *fields = json!({});
    }
    if let Some(obj) = fields.as_object_mut() {
        obj.insert(field.to_string(), value);
    }
}

// ── Listener lifecycle ───────────────────────────────────────────────────────

async fn ensure_started() -> Option<&'static Loopback> {
    LOOPBACK
        .get_or_try_init(|| async {
            // See the module doc comment / `console.rs` for why this is a
            // dedicated OS thread + single-threaded runtime rather than a
            // plain `tokio::spawn`.
            let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<u16>>();
            let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
            let thread_registry = registry.clone();

            std::thread::Builder::new()
                .name("solx-widget-loopback".into())
                .spawn(move || {
                    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                        Ok(rt) => rt,
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            return;
                        }
                    };
                    rt.block_on(async move {
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

                        spawn_reaper(thread_registry.clone());

                        let app = router(thread_registry);
                        if let Err(e) = axum::serve(listener, app).await {
                            warn!("widget loopback server exited: {e}");
                        }
                    });
                })
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

            let port = tokio::task::spawn_blocking(move || rx.recv())
                .await
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))??;

            Ok::<_, std::io::Error>(Loopback { port, registry })
        })
        .await
        .ok()
}

/// Trigger a reap sweep synchronously, bypassing the real
/// [`REAP_INTERVAL`] wait — lets reap tests assert on TTL/grace-period
/// expiry deterministically instead of sleeping for several real seconds.
#[cfg(test)]
async fn reap_now() {
    if let Some(loopback) = ensure_started().await {
        reap_once(&loopback.registry);
    }
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

// ── Reaper ───────────────────────────────────────────────────────────────────

const REAP_INTERVAL: Duration = Duration::from_secs(5);

fn spawn_reaper(registry: Registry) {
    tokio::spawn(async move {
        let mut ticker = interval(REAP_INTERVAL);
        loop {
            ticker.tick().await;
            reap_once(&registry);
        }
    });
}

/// A widget is reaped when it is not currently connected (`ws_tx.is_none()`)
/// and either:
/// - it has never connected and `opened_at` is older than `connect_ttl`
///   (the frontend never showed up), or
/// - it was connected once and `disconnected_at` is older than
///   `reconnect_grace` (the frontend disconnected and didn't come back).
///
/// A currently-connected widget is never reaped by this sweep, regardless of
/// age — see the module doc comment on why idle-but-connected reaping isn't
/// implemented here.
fn reap_once(registry: &Registry) {
    let Ok(mut reg) = registry.lock() else { return };
    let now = Instant::now();
    reg.retain(|_, s| match (s.ws_tx.is_some(), s.disconnected_at) {
        (true, _) => true,
        (false, None) => now.duration_since(s.opened_at) <= s.connect_ttl,
        (false, Some(at)) => now.duration_since(at) <= s.reconnect_grace,
    });
}

// ── Routes ───────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TokenQuery {
    token: String,
}

fn find_by_token(registry: &Registry, token: &str) -> Option<String> {
    registry
        .lock()
        .ok()?
        .iter()
        .find(|(_, s)| s.token == token)
        .map(|(id, _)| id.clone())
}

async fn bundle_handler(AxumState(registry): AxumState<Registry>, Query(q): Query<TokenQuery>) -> Response {
    let Some(widget_id) = find_by_token(&registry, &q.token) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let bundle = registry.lock().ok().and_then(|r| r.get(&widget_id).map(|s| s.bundle.clone()));
    match bundle {
        Some(bytes) => {
            ([(header::CONTENT_TYPE, "application/javascript; charset=utf-8")], bytes).into_response()
        }
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

async fn ws_handler(
    AxumState(registry): AxumState<Registry>,
    Query(q): Query<TokenQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(widget_id) = find_by_token(&registry, &q.token) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    ws.on_upgrade(move |socket| handle_socket(socket, registry, widget_id))
}

/// How often the server originates a ping while a widget is connected. The
/// WebSocket protocol's own ping/pong control frames (RFC 6455 §5.5.2) are
/// the keepalive: a client library answers a ping with a pong automatically,
/// so this loop's own send failing is what surfaces a dead connection (the
/// TCP write fails) without any application-level ping/pong message
/// polluting the JSON envelope. This can't distinguish "connected but idle"
/// from "connected and dead-but-still-ack'ing at the OS level" — see the
/// module doc comment / `docs/widget-actions.md` §4.1 for why that's judged
/// out of scope for now.
const WS_PING_INTERVAL: Duration = Duration::from_secs(30);

async fn handle_socket(socket: WebSocket, registry: Registry, widget_id: String) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    {
        let Ok(mut reg) = registry.lock() else { return };
        let Some(state) = reg.get_mut(&widget_id) else {
            // Reaped or closed between the upgrade and here.
            return;
        };
        state.ws_tx = Some(tx.clone());
        state.disconnected_at = None;
    }

    let send_task = tokio::spawn(async move {
        let mut ping_ticker = interval(WS_PING_INTERVAL);
        // `interval`'s first tick completes immediately rather than after
        // one full period — consume it up front so the first real ping
        // fires after `WS_PING_INTERVAL`, not the instant the connection
        // opens (which would otherwise race ahead of any early push).
        ping_ticker.tick().await;
        loop {
            tokio::select! {
                msg = rx.recv() => {
                    match msg {
                        Some(msg) => {
                            if sink.send(msg).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                _ = ping_ticker.tick() => {
                    if sink.send(Message::Ping(Vec::new())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Text(text) => handle_widget_message(&registry, &widget_id, &text, &tx).await,
            Message::Close(_) => break,
            _ => {}
        }
    }

    send_task.abort();
    if let Ok(mut reg) = registry.lock() {
        if let Some(state) = reg.get_mut(&widget_id) {
            // Only clear/mark-disconnected if this is still the connection
            // that's finishing — a reconnect may have already installed a
            // fresh sender before this cleanup ran.
            if state.ws_tx.as_ref().map(|t| t.same_channel(&tx)).unwrap_or(false) {
                state.ws_tx = None;
                state.disconnected_at = Some(Instant::now());
            }
        }
    }
}

/// Widget → host messages: `get`/`set`/`show`/`hide` (see
/// `docs/widget-actions.md` §4.5). A `get` gets a reply on the same socket;
/// `set`/`show`/`hide` update state with no reply — the widget already
/// knows what it wrote.
async fn handle_widget_message(registry: &Registry, widget_id: &str, text: &str, tx: &mpsc::UnboundedSender<Message>) {
    let Ok(envelope) = serde_json::from_str::<Value>(text) else { return };
    let Some(op) = envelope.get("op").and_then(Value::as_str) else { return };
    match op {
        "get" => {
            let field = envelope.get("field").and_then(Value::as_str).unwrap_or("");
            let value = registry
                .lock()
                .ok()
                .and_then(|r| {
                    r.get(widget_id).map(|s| {
                        if field.is_empty() {
                            s.fields.clone()
                        } else {
                            s.fields.get(field).cloned().unwrap_or(Value::Null)
                        }
                    })
                })
                .unwrap_or(Value::Null);
            let reply = json!({ "op": "get", "field": field, "value": value });
            let _ = tx.send(Message::Text(reply.to_string()));
        }
        "set" => {
            let Some(field) = envelope.get("field").and_then(Value::as_str) else { return };
            let value = envelope.get("value").cloned().unwrap_or(Value::Null);
            if let Ok(mut reg) = registry.lock() {
                if let Some(state) = reg.get_mut(widget_id) {
                    set_field(&mut state.fields, field, value);
                }
            }
        }
        "show" => set_visible_locked(registry, widget_id, true),
        "hide" => set_visible_locked(registry, widget_id, false),
        _ => {}
    }
}

fn set_visible_locked(registry: &Registry, widget_id: &str, visible: bool) {
    if let Ok(mut reg) = registry.lock() {
        if let Some(state) = reg.get_mut(widget_id) {
            state.visible = visible;
        }
    }
}

fn router(registry: Registry) -> Router {
    Router::new()
        .route("/bundle", get_route(bundle_handler))
        .route("/ws", get_route(ws_handler))
        .with_state(registry)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use solx_files::LocalFileStore;
    use tokio_tungstenite::tungstenite::Message as TMessage;

    const TEST_CONNECT_TTL: Duration = Duration::from_secs(60);
    const TEST_RECONNECT_GRACE: Duration = Duration::from_secs(15);

    async fn test_files() -> (tempfile::TempDir, Arc<dyn FileStore>) {
        let dir = tempfile::tempdir().unwrap();
        let files: Arc<dyn FileStore> = Arc::new(LocalFileStore::new(dir.path().join("files")));
        files.put("widget.js", b"console.log('hi');".to_vec()).await.unwrap();
        (dir, files)
    }

    #[tokio::test]
    async fn open_returns_a_descriptor_with_a_reachable_bundle() {
        let (_d, files) = test_files().await;
        let descriptor = open(
            &files,
            "widget.js",
            "my-widget",
            json!({"title": "hi"}),
            "/pkg/foo",
            "inv-1",
            TEST_CONNECT_TTL,
            TEST_RECONNECT_GRACE,
        )
        .await
        .unwrap();

        assert!(descriptor.entry_url.starts_with("http://127.0.0.1:"));
        assert!(descriptor.ws_url.starts_with("ws://127.0.0.1:"));
        assert_eq!(descriptor.tag_name, "my-widget");
        assert_eq!(descriptor.fields, json!({"title": "hi"}));

        let resp = reqwest::get(&descriptor.entry_url).await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), "console.log('hi');");
    }

    #[tokio::test]
    async fn bundle_rejects_unknown_token() {
        let (_d, files) = test_files().await;
        let descriptor = open(
            &files, "widget.js", "my-widget", Value::Null, "/pkg/foo", "inv-1",
            TEST_CONNECT_TTL, TEST_RECONNECT_GRACE,
        )
        .await
        .unwrap();
        let base = descriptor.entry_url.split("?token=").next().unwrap();
        let resp = reqwest::get(format!("{base}?token=not-a-real-token")).await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn get_set_round_trip_through_the_registry() {
        let (_d, files) = test_files().await;
        let descriptor = open(
            &files, "widget.js", "my-widget", json!({"a": 1}), "/pkg/foo", "inv-1",
            TEST_CONNECT_TTL, TEST_RECONNECT_GRACE,
        )
        .await
        .unwrap();

        let a = get(&descriptor.widget_id, "a").await.unwrap();
        assert_eq!(a, json!(1));

        set(&descriptor.widget_id, "b", json!("two")).await.unwrap();
        let b = get(&descriptor.widget_id, "b").await.unwrap();
        assert_eq!(b, json!("two"));

        let whole = get(&descriptor.widget_id, "").await.unwrap();
        assert_eq!(whole, json!({"a": 1, "b": "two"}));
    }

    #[tokio::test]
    async fn close_deregisters_the_token() {
        let (_d, files) = test_files().await;
        let descriptor = open(
            &files, "widget.js", "my-widget", Value::Null, "/pkg/foo", "inv-1",
            TEST_CONNECT_TTL, TEST_RECONNECT_GRACE,
        )
        .await
        .unwrap();

        close(&descriptor.widget_id).await.unwrap();

        let resp = reqwest::get(&descriptor.entry_url).await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert!(get(&descriptor.widget_id, "").await.is_err());
    }

    #[tokio::test]
    async fn websocket_connect_and_widget_side_set_updates_fields() {
        let (_d, files) = test_files().await;
        let descriptor = open(
            &files, "widget.js", "my-widget", json!({}), "/pkg/foo", "inv-1",
            TEST_CONNECT_TTL, TEST_RECONNECT_GRACE,
        )
        .await
        .unwrap();

        let (mut ws, _) = tokio_tungstenite::connect_async(&descriptor.ws_url).await.unwrap();
        ws.send(TMessage::Text(json!({"op": "set", "field": "title", "value": "hi"}).to_string()))
            .await
            .unwrap();

        // No reply for a widget-side `set` — give the server a moment to
        // apply it, then read through the host-facing API.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let title = get(&descriptor.widget_id, "title").await.unwrap();
        assert_eq!(title, json!("hi"));

        ws.send(TMessage::Text(json!({"op": "get", "field": "title"}).to_string())).await.unwrap();
        let reply = ws.next().await.unwrap().unwrap();
        let reply: Value = serde_json::from_str(reply.to_text().unwrap()).unwrap();
        assert_eq!(reply, json!({"op": "get", "field": "title", "value": "hi"}));
    }

    #[tokio::test]
    async fn host_side_set_is_pushed_to_a_connected_widget() {
        let (_d, files) = test_files().await;
        let descriptor = open(
            &files, "widget.js", "my-widget", json!({}), "/pkg/foo", "inv-1",
            TEST_CONNECT_TTL, TEST_RECONNECT_GRACE,
        )
        .await
        .unwrap();

        let (mut ws, _) = tokio_tungstenite::connect_async(&descriptor.ws_url).await.unwrap();
        // Let the server-side handshake finish installing ws_tx before the
        // host-side set fires, so it isn't fire-and-forgotten to nobody.
        tokio::time::sleep(Duration::from_millis(100)).await;

        set(&descriptor.widget_id, "title", json!("pushed")).await.unwrap();

        let msg = ws.next().await.unwrap().unwrap();
        let msg: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        assert_eq!(msg, json!({"op": "set", "field": "title", "value": "pushed"}));
    }

    #[tokio::test]
    async fn never_connected_widget_is_reaped_after_connect_ttl() {
        let (_d, files) = test_files().await;
        let descriptor = open(
            &files, "widget.js", "my-widget", Value::Null, "/pkg/foo", "inv-1",
            Duration::from_millis(1), TEST_RECONNECT_GRACE,
        )
        .await
        .unwrap();

        tokio::time::sleep(Duration::from_millis(20)).await;
        reap_now().await;

        let err = get(&descriptor.widget_id, "").await.unwrap_err();
        assert!(err.contains("no widget registered"), "{err}");
    }

    #[tokio::test]
    async fn connected_widget_is_never_reaped_by_connect_ttl() {
        let (_d, files) = test_files().await;
        // Long enough that the WS handshake below reliably completes before
        // it expires (tests run in parallel against the same shared
        // registry, so too tight a TTL here risks the connection racing an
        // unrelated test's `reap_now()` sweep).
        let connect_ttl = Duration::from_millis(300);
        let descriptor = open(
            &files, "widget.js", "my-widget", Value::Null, "/pkg/foo", "inv-1",
            connect_ttl, TEST_RECONNECT_GRACE,
        )
        .await
        .unwrap();

        let (_ws, _) = tokio_tungstenite::connect_async(&descriptor.ws_url).await.unwrap();
        tokio::time::sleep(connect_ttl + Duration::from_millis(100)).await; // now past connect_ttl
        reap_now().await;

        // Still connected, so the connect-window TTL must not apply to it —
        // only a disconnect starts the reconnect-grace clock.
        assert!(get(&descriptor.widget_id, "").await.is_ok());
    }

    #[tokio::test]
    async fn disconnected_widget_is_reaped_after_reconnect_grace_elapses() {
        let (_d, files) = test_files().await;
        let descriptor = open(
            &files, "widget.js", "my-widget", Value::Null, "/pkg/foo", "inv-1",
            TEST_CONNECT_TTL, Duration::from_millis(1),
        )
        .await
        .unwrap();

        let (ws, _) = tokio_tungstenite::connect_async(&descriptor.ws_url).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await; // let the server register ws_tx
        drop(ws); // simulate a tab close / network drop
        tokio::time::sleep(Duration::from_millis(200)).await; // give the server time to notice the disconnect

        reap_now().await;

        let err = get(&descriptor.widget_id, "").await.unwrap_err();
        assert!(err.contains("no widget registered"), "{err}");
    }

    #[tokio::test]
    async fn disconnected_widget_survives_within_the_reconnect_grace_period() {
        let (_d, files) = test_files().await;
        let descriptor = open(
            &files, "widget.js", "my-widget", Value::Null, "/pkg/foo", "inv-1",
            TEST_CONNECT_TTL, Duration::from_secs(60),
        )
        .await
        .unwrap();

        let (ws, _) = tokio_tungstenite::connect_async(&descriptor.ws_url).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(ws);
        tokio::time::sleep(Duration::from_millis(100)).await;

        reap_now().await;

        // Disconnected, but well within the 60s grace period.
        assert!(get(&descriptor.widget_id, "").await.is_ok());
    }
}
