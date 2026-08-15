//! Action execution backends.
//!
//! * **Command** — `fn_name` is the literal shell command to run; any fixed
//!   settings (e.g. `cwd`) come from the action's own `action_config`.
//! * **Webhook** — `fn_name` is the literal URL to POST to; auth/headers come
//!   from `action_config`. OAuth token exchange (bearer, refresh_token,
//!   service_account, authorization_code) is resolved via
//!   [`crate::auth::resolve_auth`].
//!
//! Actions are trusted by virtue of being `post`ed into the actions
//! database — there is no separate config-level allowlist for either kind.
//!
//! `Wasm`-typed actions are executed by [`crate::wasm`], not here —
//! see [`crate::LocalActionManager::exec`]'s `Wasm` arm.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use solx_config::ConfigService;
use solx_surface::error::{Result, SolxError};
use solx_surface::managers::ActionManager;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::console::ConsoleStore;
use crate::loopback::console as console_loopback;

/// Wall-clock ceiling on a command, overridable per action via
/// `action_config.timeout_secs`. Also reused by `crate::script` as the
/// default ceiling on a whole `Script` action's execution.
pub(crate) const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// Turn an action ref into a filesystem-safe path segment for
/// [`ConfigService::logs_dir`] — `/packages/solx-omniparse/process-file` ->
/// `packages_solx-omniparse_process-file`.
fn log_dir_slug(action_ref: &str) -> String {
    action_ref
        .trim_start_matches('/')
        .chars()
        .map(|c| if c == '/' { '_' } else { c })
        .collect()
}

/// Run a `Command` action. `fn_name` is the literal command to execute.
/// Params are passed as JSON on stdin only (no env var) — no payload size
/// limit, and it doesn't leak into process listings.
///
/// Fully async: the child is spawned with `tokio::process`, so an action
/// that takes minutes parks no thread. This used to be a synchronous
/// `std::process` call made directly from `exec_as`, which meant every
/// running command held an async *worker* thread — only `num_cpus` of
/// those exist, so a handful of concurrent commands could wedge the whole
/// process, HTTP routes included.

pub async fn run_command(
    cfg: &ConfigService,
    console: Arc<ConsoleStore>,
    action_ref: &str,
    fn_name: &str,
    action_config: &Option<Value>,
    params: &Value,
    timeout_secs: Option<u64>,
) -> Result<Value> {
    let cwd = action_config
        .as_ref()
        .and_then(|c| c.get("cwd"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| cfg.appdata().to_string_lossy().into_owned());

    let params_json = serde_json::to_string(params)?;
    let (shell, flag) = if cfg!(windows) { ("cmd", "/C") } else { ("sh", "-c") };

    // `SOL_LOG_DIR` is the env var `solx-omniparse`/`solx-media` already read
    // for their own file logging (a convention carried over from old sol) —
    // this is what actually turns it on. It was never set here before, so
    // that logging has been silently inert regardless of what any package
    // does on its own. One subdirectory per action ref, so two actions in
    // the same package (e.g. omniparse's process/write variants) don't
    // collide on one shared file.
    let log_dir = cfg.logs_dir().join(log_dir_slug(action_ref));

    // A one-shot credential letting this specific invocation POST to its
    // own console over loopback HTTP — see `crate::loopback::console` for
    // why this replaced reading the child's stderr. Best-effort: if the
    // loopback can't start, the command still runs, it just has no console
    // access for this run. `registration` must stay bound (not `_`) for the
    // rest of this function — dropping it deregisters the token, and we
    // want that to happen automatically on every exit path below, not just
    // the success path.
    let invocation_id = Uuid::new_v4().to_string();
    let registration = console_loopback::register(console, action_ref, &invocation_id).await;

    let mut cmd = tokio::process::Command::new(shell);
    cmd.arg(flag)
        .arg(fn_name)
        .current_dir(&cwd)
        .env("SOL_LOG_DIR", &log_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // So a timed-out or cancelled exec reaps the child instead of
        // leaking it.
        //
        // Caveat: this kills the *shell* we spawned, not its descendants.
        // Neither Windows nor POSIX terminates a process tree on request
        // without extra machinery (a Job Object / a process group), so a
        // long-running grandchild — `sh -c 'sleep 600'` spawning `sleep`,
        // or `cmd /C ping ...` spawning `ping` — outlives the timeout and
        // keeps the inherited stdout pipe open. Killing the tree properly
        // would mean putting each command in its own job/process group.
        .kill_on_drop(true);
    if let Some(reg) = &registration {
        cmd.env("SOLX_CONSOLE_URL", reg.url()).env("SOLX_CONSOLE_TOKEN", reg.token());
    }
    let mut child = cmd.spawn().map_err(|e| SolxError::Exec(format!("spawn '{fn_name}': {e}")))?;

    // Feed stdin *concurrently* with draining stdout/stderr. Writing it all
    // up front deadlocks whenever the payload exceeds the pipe buffer
    // (~64KB) and the child doesn't drain stdin before producing output:
    // the child blocks writing stdout, we block writing stdin, neither
    // moves. Dropping the handle after the write closes the pipe so the
    // child sees EOF.
    let mut stdin = child.stdin.take();
    let feed = async move {
        if let Some(mut pipe) = stdin.take() {
            let _ = pipe.write_all(params_json.as_bytes()).await;
            let _ = pipe.shutdown().await;
        }
    };

    let timeout = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
    let wait = async {
        let (_, out) = tokio::join!(feed, child.wait_with_output());
        out
    };

    let output = match tokio::time::timeout(timeout, wait).await {
        Ok(res) => res.map_err(|e| SolxError::Exec(e.to_string()))?,
        Err(_) => {
            return Err(SolxError::Exec(format!(
                "command '{fn_name}' timed out after {}s",
                timeout.as_secs()
            )))
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SolxError::Exec(format!(
            "command '{fn_name}' failed ({}): {}",
            output.status,
            stderr.trim()
        )));
    }
    Ok(serde_json::from_str::<Value>(stdout.trim()).unwrap_or(Value::String(stdout)))
}

/// Run a `Webhook` action. `url` is the endpoint (the action's `fn_name`).
///
/// When `action_config.auth.type == "oauth_authorization_code"`, this
/// performs the RFC 6749 §4.1.3 form-encoded POST to `url` (the token
/// endpoint) directly and returns the token JSON — no Bearer header is
/// injected. For all other auth types, [`crate::auth::resolve_auth`]
/// resolves the `Authorization` header.
///
/// Logs method, URL, and outcome (status/duration, or the error) to
/// `action_ref`'s console — deliberately never the body, headers, or
/// resolved auth, any of which could carry a secret straight into a log a
/// nested action can read (`console/read` is unrestricted by design, see
/// `docs/console-implementation-plan.md` §4).
#[allow(clippy::too_many_arguments)]
pub async fn run_webhook(
    actions: &dyn ActionManager,
    console: Arc<ConsoleStore>,
    action_ref: &str,
    path: &str,
    name: &str,
    url: &str,
    action_config: &Option<Value>,
    params: &Value,
) -> Result<Value> {
    let invocation_id = Uuid::new_v4().to_string();
    let started = std::time::Instant::now();
    log_webhook(&console, action_ref, &invocation_id, "info", format!("POST {url}")).await;

    let result = run_webhook_inner(actions, path, name, url, action_config, params).await;

    let elapsed_ms = started.elapsed().as_millis();
    match &result {
        Ok(_) => {
            log_webhook(
                &console,
                action_ref,
                &invocation_id,
                "info",
                format!("POST {url} succeeded ({elapsed_ms}ms)"),
            )
            .await;
        }
        Err(e) => {
            log_webhook(
                &console,
                action_ref,
                &invocation_id,
                "warn",
                format!("POST {url} failed after {elapsed_ms}ms: {e}"),
            )
            .await;
        }
    }

    result
}

async fn log_webhook(console: &ConsoleStore, action_ref: &str, invocation_id: &str, level: &str, message: String) {
    let _ = console
        .print(action_ref, invocation_id, None, level, "webhook", Some(message), None)
        .await;
}

async fn run_webhook_inner(
    actions: &dyn ActionManager,
    path: &str,
    name: &str,
    url: &str,
    action_config: &Option<Value>,
    params: &Value,
) -> Result<Value> {
    let client = reqwest::Client::new();

    // Check for oauth_authorization_code — short-circuit token exchange.
    if let Some(cfg) = action_config {
        if let Some(auth) = cfg.get("auth") {
            if auth.get("type").and_then(Value::as_str) == Some("oauth_authorization_code") {
                return dispatch_oauth_token_exchange(&client, url, cfg, auth, params).await;
            }
        }
    }

    let mut req = client.post(url).json(params);

    if let Some(cfg) = action_config {
        // Resolve auth via the full pipeline (bearer/oauth_refresh/oauth_service_account).
        if let Some(auth_header) = crate::auth::resolve_auth(actions, path, name, cfg)
            .await
            .map_err(|e| SolxError::Exec(e))?
        {
            req = req.header("Authorization", auth_header);
        }
        if let Some(headers) = cfg.get("headers").and_then(|v| v.as_object()) {
            for (k, v) in headers {
                if let Some(s) = v.as_str() {
                    req = req.header(k, s);
                }
            }
        }
    }

    let resp = req
        .send()
        .await
        .map_err(|e| SolxError::Exec(format!("webhook request failed: {e}")))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| SolxError::Exec(e.to_string()))?;
    if !status.is_success() {
        let snippet: String = text.chars().take(500).collect();
        return Err(SolxError::Exec(format!(
            "webhook returned {status}: {snippet}"
        )));
    }
    Ok(serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text)))
}

/// RFC 6749 §4.1.3 token exchange: POST `code` + credentials as
/// `application/x-www-form-urlencoded` to the token endpoint (`url`).
/// Returns the parsed JSON response (which includes `access_token`,
/// `refresh_token`, `expires_in`, etc.).
///
/// `client_id` is resolved through the same inline/keyring/scoped-secret
/// pipeline as the other auth types (it's always required per RFC 6749).
/// `client_secret` uses the "optional" variant — a public/PKCE client
/// legitimately has none — but if one *is* configured (inline, keyring, or
/// a `client_secret_secret` pointer) and fails to resolve, that's a hard
/// error rather than a silent empty-string fallback.
async fn dispatch_oauth_token_exchange(
    client: &reqwest::Client,
    url: &str,
    action_config: &Value,
    auth: &Value,
    params: &Value,
) -> Result<Value> {
    let code = params
        .get("code")
        .and_then(Value::as_str)
        .ok_or_else(|| SolxError::Exec("oauth_authorization_code requires 'code' in params".into()))?;
    let client_id = crate::auth::secret_field(
        action_config,
        auth,
        "client_id",
        Some("client_id_secret"),
        "oauth_authorization_code",
    )
    .await
    .map_err(SolxError::Exec)?;
    let client_secret = crate::auth::optional_secret_field(
        action_config,
        auth,
        "client_secret",
        Some("client_secret_secret"),
        "oauth_authorization_code",
    )
    .await
    .map_err(SolxError::Exec)?;
    let redirect_uri = auth
        .get("redirect_uri")
        .and_then(Value::as_str)
        .unwrap_or("");

    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", client_id.as_str()),
    ];
    if let Some(ref secret) = client_secret {
        form.push(("client_secret", secret.as_str()));
    }
    if !redirect_uri.is_empty() {
        form.push(("redirect_uri", redirect_uri));
    }

    let resp = client
        .post(url)
        .form(&form)
        .send()
        .await
        .map_err(|e| SolxError::Exec(format!("oauth token exchange POST failed: {e}")))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| SolxError::Exec(e.to_string()))?;
    if !status.is_success() {
        let snippet: String = text.chars().take(500).collect();
        return Err(SolxError::Exec(format!(
            "oauth token endpoint returned {status}: {snippet}"
        )));
    }
    Ok(serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text)))
}
