//! `/builtin/http_stream/*` — host-side streaming HTTP, for callers (chiefly
//! WASM guests) that have no sockets of their own and no state across
//! invocations. See `solx-packages/solx-ollama/docs/streaming-design.md` for
//! the motivating design — this module is that design's "three new internal
//! actions", modeled directly on `super::oauth`'s registry pattern.
//!
//! Unrestricted by caller, exactly like `oauth_await`/`oauth_stop` and
//! `console_read`/`tail`/`clear`: access is a bearer capability on the
//! unguessable `stream_id` (a UUID v4), not a caller/ownership check. This
//! keeps a single guest able to `start` under one action invocation and
//! `poll`/`close` under a different one (or a different action entirely) —
//! the shape the design doc's `.solx` script-loop example relies on — without
//! extra plumbing.
//!
//! * `http_stream_start` — sends the request, returns `{stream_id, status}`
//!   as soon as headers arrive, and spawns a task that reads the body as
//!   newline-delimited JSON into a cursor-addressable buffer.
//! * `http_stream_poll` — `{stream_id, cursor?, wait_secs?}` → drains
//!   whatever's buffered since `cursor`, optionally long-polling for more.
//! * `http_stream_close` — stops the reader task and drops the buffer.
//!
//! Two safety valves, both driven by config (`solx_config::ConfigService`):
//! a max-buffered-bytes cap (oldest chunks are dropped on overflow, counted
//! in `dropped`) and an idle-TTL reaper (a stream nobody polls for a while
//! self-terminates) — `oauth.rs` has the same abandoned-resource shape today
//! but relies on a human calling `oauth_stop`; nothing would do that for an
//! abandoned generation.

use std::collections::VecDeque;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use futures::StreamExt;
use serde_json::{json, Value};
use solx_config::ConfigService;
use tokio::task::AbortHandle;

use crate::console::{MAX_TAIL_WAIT_SECS, TAIL_POLL_INTERVAL};

use super::require_str;

// ── Registry ─────────────────────────────────────────────────────────────────

struct StreamState {
    status: Option<u16>,
    /// Index of `chunks[0]`, so `cursor` addresses survive front-eviction.
    base_index: u64,
    chunks: VecDeque<Value>,
    buffered_bytes: usize,
    dropped: u64,
    done: bool,
    error: Option<String>,
    last_polled: Instant,
    abort: Option<AbortHandle>,
}

impl StreamState {
    fn next_index(&self) -> u64 {
        self.base_index + self.chunks.len() as u64
    }
}

type Registry = HashMap<String, Arc<Mutex<StreamState>>>;

static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

fn registry() -> &'static Mutex<Registry> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

// ── http_stream_start ───────────────────────────────────────────────────────

pub(super) async fn start(params: &Value, config: &Arc<ConfigService>) -> Result<Value, String> {
    let url = require_str(params, "url")?.to_string();
    let method = params
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("GET")
        .to_ascii_uppercase();
    let timeout_secs = params
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .unwrap_or(30);
    if timeout_secs == 0 {
        return Err("timeout_secs must be > 0".to_string());
    }

    let body_str = params.get("body").and_then(Value::as_str);
    let body_encoding = params
        .get("body_encoding")
        .and_then(Value::as_str)
        .unwrap_or("utf8");
    let body_bytes: Option<Vec<u8>> = match body_str {
        None => None,
        Some(s) => Some(match body_encoding {
            "utf8" => s.as_bytes().to_vec(),
            "base64" => base64::engine::general_purpose::STANDARD
                .decode(s)
                .map_err(|e| format!("invalid base64 body: {e}"))?,
            other => {
                return Err(format!(
                    "unknown body_encoding '{other}'; expected \"utf8\" or \"base64\""
                ));
            }
        }),
    };

    // No overall `.timeout()`: a stream can legitimately stay open for
    // minutes (a long chat/generate response, a large model pull). It's
    // bounded instead by the idle-TTL reaper below and by the caller's own
    // outer timeout. `timeout_secs` only governs the initial connect.
    let mut req = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| format!("failed to build http client: {e}"))?
        .request(
            method.parse().map_err(|e| format!("invalid method: {e}"))?,
            &url,
        );

    if let Some(headers_val) = params.get("headers") {
        let headers_obj = headers_val
            .as_object()
            .ok_or_else(|| "headers must be an object of string -> string".to_string())?;
        let mut h = reqwest::header::HeaderMap::new();
        for (k, v) in headers_obj {
            let v_str = v
                .as_str()
                .ok_or_else(|| format!("header '{k}' must be a string"))?;
            let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| format!("invalid header name '{k}': {e}"))?;
            let value = reqwest::header::HeaderValue::from_str(v_str)
                .map_err(|e| format!("invalid header value for '{k}': {e}"))?;
            h.insert(name, value);
        }
        req = req.headers(h);
    }

    if let Some(b) = body_bytes {
        req = req.body(b);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("http request failed: {e}"))?;
    let status = resp.status().as_u16();

    let stream_id = uuid::Uuid::new_v4().to_string();
    let state = Arc::new(Mutex::new(StreamState {
        status: Some(status),
        base_index: 0,
        chunks: VecDeque::new(),
        buffered_bytes: 0,
        dropped: 0,
        done: false,
        error: None,
        last_polled: Instant::now(),
        abort: None,
    }));

    let max_buffer_bytes = config.http_stream_max_buffer_bytes() as usize;
    let idle_ttl = Duration::from_secs(config.http_stream_idle_ttl_secs());
    let task_state = state.clone();
    let handle = tokio::spawn(async move {
        read_stream(resp, task_state, max_buffer_bytes, idle_ttl).await;
    });
    if let Ok(mut s) = state.lock() {
        s.abort = Some(handle.abort_handle());
    }

    if let Ok(mut reg) = registry().lock() {
        reg.insert(stream_id.clone(), state);
    }

    Ok(json!({ "stream_id": stream_id, "status": status }))
}

/// How often the reader loop wakes up to re-check idleness even when no
/// bytes have arrived. A stalled connection sits inside `stream.next()`
/// indefinitely otherwise — there'd be no other point to notice the idle
/// deadline has passed.
const IDLE_CHECK_INTERVAL: Duration = Duration::from_millis(200);

/// Reader task body: splits the response body on `\n`, parses each complete
/// line as JSON (falling back to `{"raw": line}` so a malformed or non-JSON
/// line is still visible rather than silently dropped), and appends to the
/// registry buffer until the body ends, a transport error occurs, or the
/// stream has gone unpolled past `idle_ttl`.
async fn read_stream(
    resp: reqwest::Response,
    state: Arc<Mutex<StreamState>>,
    max_buffer_bytes: usize,
    idle_ttl: Duration,
) {
    let mut stream = resp.bytes_stream();
    let mut pending = Vec::new();

    loop {
        if is_idle(&state, idle_ttl) {
            finish(&state, Some("idle: no poller, stream aborted".to_string()));
            return;
        }

        let next = match tokio::time::timeout(IDLE_CHECK_INTERVAL, stream.next()).await {
            Ok(next) => next,
            // No bytes within this tick — loop back around to the idle
            // check above rather than blocking indefinitely.
            Err(_) => continue,
        };
        match next {
            Some(Ok(bytes)) => {
                pending.extend_from_slice(&bytes);
                while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = pending.drain(..=pos).collect();
                    let line = &line[..line.len() - 1]; // drop the trailing '\n'
                    push_line(&state, line, max_buffer_bytes);
                }
            }
            Some(Err(e)) => {
                if !pending.is_empty() {
                    push_line(&state, &pending, max_buffer_bytes);
                }
                finish(&state, Some(format!("stream read failed: {e}")));
                return;
            }
            None => {
                if !pending.is_empty() {
                    push_line(&state, &pending, max_buffer_bytes);
                }
                finish(&state, None);
                return;
            }
        }
    }
}

fn is_idle(state: &Arc<Mutex<StreamState>>, idle_ttl: Duration) -> bool {
    state
        .lock()
        .map(|s| s.last_polled.elapsed() > idle_ttl)
        .unwrap_or(false)
}

fn finish(state: &Arc<Mutex<StreamState>>, error: Option<String>) {
    if let Ok(mut s) = state.lock() {
        s.done = true;
        s.error = error;
    }
}

fn push_line(state: &Arc<Mutex<StreamState>>, line: &[u8], max_buffer_bytes: usize) {
    if line.is_empty() {
        return;
    }
    let chunk = match std::str::from_utf8(line) {
        Ok(text) if text.trim().is_empty() => return,
        Ok(text) => serde_json::from_str::<Value>(text).unwrap_or_else(|_| json!({ "raw": text })),
        Err(_) => json!({
            "raw_base64": base64::engine::general_purpose::STANDARD.encode(line),
        }),
    };
    let Ok(mut s) = state.lock() else { return };
    let size = line.len();
    s.chunks.push_back(chunk);
    s.buffered_bytes += size;
    while s.buffered_bytes > max_buffer_bytes && s.chunks.len() > 1 {
        if let Some(evicted) = s.chunks.pop_front() {
            s.buffered_bytes = s
                .buffered_bytes
                .saturating_sub(serde_json::to_string(&evicted).map(|s| s.len()).unwrap_or(0));
            s.base_index += 1;
            s.dropped += 1;
        }
    }
}

// ── http_stream_poll ─────────────────────────────────────────────────────────

pub(super) async fn poll(params: &Value) -> Result<Value, String> {
    let stream_id = require_str(params, "stream_id")?.to_string();
    // Missing, negative, or unparsable all mean "from the start" — covers a
    // `.solx` script's first loop iteration, where an unresolved cursor
    // variable passes through as its own literal token rather than a number.
    let cursor = params
        .get("cursor")
        .and_then(Value::as_i64)
        .filter(|c| *c >= 0)
        .unwrap_or(0) as u64;
    let wait_secs = params.get("wait_secs").and_then(Value::as_u64);

    let wait = wait_secs.map(|s| Duration::from_secs(s.min(MAX_TAIL_WAIT_SECS)));
    let deadline = wait.map(|w| Instant::now() + w);

    loop {
        let state = registry()
            .lock()
            .map_err(|e| format!("http_stream registry poisoned: {e}"))?
            .get(&stream_id)
            .cloned()
            .ok_or_else(|| format!("no stream registered for stream_id '{stream_id}'"))?;

        let result = {
            let mut s = state.lock().map_err(|e| format!("stream state poisoned: {e}"))?;
            s.last_polled = Instant::now();
            // Running total of chunks evicted over the stream's whole
            // lifetime, matching `ConsoleStore::ReadResult::dropped` —  not
            // scoped to this particular cursor, so a late poller can tell it
            // missed data even if every chunk it *can* still see is fresh.
            let dropped = s.dropped;
            let start = cursor.max(s.base_index).saturating_sub(s.base_index) as usize;
            let chunks: Vec<Value> = s.chunks.iter().skip(start).cloned().collect();
            let has_new = !chunks.is_empty();
            (
                json!({
                    "chunks": chunks,
                    "next_cursor": s.next_index(),
                    "done": s.done,
                    "dropped": dropped,
                    "status": s.status,
                    "error": s.error,
                }),
                has_new,
                s.done,
            )
        };
        let (value, has_new, done) = result;
        if has_new || done {
            return Ok(value);
        }
        match deadline {
            Some(d) => {
                let now = Instant::now();
                if now >= d {
                    return Ok(value);
                }
                tokio::time::sleep(TAIL_POLL_INTERVAL.min(d - now)).await;
            }
            None => return Ok(value),
        }
    }
}

// ── http_stream_close ────────────────────────────────────────────────────────

pub(super) async fn close(params: &Value) -> Result<Value, String> {
    let stream_id = require_str(params, "stream_id")?.to_string();
    let removed = registry()
        .lock()
        .map_err(|e| format!("http_stream registry poisoned: {e}"))?
        .remove(&stream_id);
    match removed {
        Some(state) => {
            if let Ok(s) = state.lock() {
                if let Some(abort) = &s.abort {
                    abort.abort();
                }
            }
            Ok(json!({ "closed": true, "stream_id": stream_id }))
        }
        None => Ok(json!({ "closed": false, "stream_id": stream_id })),
    }
}
