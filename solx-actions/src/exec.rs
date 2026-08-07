//! Action execution backends.
//!
//! * **Command** — `fn_name` is the literal shell command to run; any fixed
//!   settings (e.g. `cwd`) come from the action's own `action_config`.
//! * **Webhook** — `fn_name` is the literal URL to POST to; auth/headers come
//!   from `action_config`. OAuth token exchange (bearer, refresh_token,
//!   service_account, authorization_code) is resolved via
//!   [`crate::webhook_auth::resolve_auth`].
//!
//! Actions are trusted by virtue of being `post`ed into the actions
//! database — there is no separate config-level allowlist for either kind.
//!
//! `Wasm`-typed actions are executed by [`crate::wasm_host`], not here —
//! see [`crate::LocalActionManager::exec`]'s `Wasm` arm.

use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use solx_config::ConfigService;
use solx_surface::error::{Result, SolxError};
use solx_surface::managers::ActionManager;
use tokio::io::AsyncWriteExt;

/// Wall-clock ceiling on a command, overridable per action via
/// `action_config.timeout_secs`. Also reused by `crate::script` as the
/// default ceiling on a whole `Script` action's execution.
pub(crate) const DEFAULT_TIMEOUT_SECS: u64 = 300;

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

    let mut child = tokio::process::Command::new(shell)
        .arg(flag)
        .arg(fn_name)
        .current_dir(&cwd)
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
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| SolxError::Exec(format!("spawn '{fn_name}': {e}")))?;

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
/// injected. For all other auth types, [`crate::webhook_auth::resolve_auth`]
/// resolves the `Authorization` header.
pub async fn run_webhook(
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
        if let Some(auth_header) = crate::webhook_auth::resolve_auth(actions, path, name, cfg)
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
    let client_id = crate::webhook_auth::secret_field(
        action_config,
        auth,
        "client_id",
        Some("client_id_secret"),
        "oauth_authorization_code",
    )
    .await
    .map_err(SolxError::Exec)?;
    let client_secret = crate::webhook_auth::optional_secret_field(
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
