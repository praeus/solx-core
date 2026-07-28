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
//! WASM built-in actions are planned but not implemented in this iteration;
//! [`unsupported`] returns a clear error.

use std::io::Write;
use std::process::Stdio;

use serde_json::Value;
use solx_config::ConfigService;
use solx_surface::error::{Result, SolxError};
use solx_surface::managers::ActionManager;

/// Run a `Command` action. `fn_name` is the literal command to execute.
pub fn run_command(
    cfg: &ConfigService,
    fn_name: &str,
    action_config: &Option<Value>,
    params: &Value,
) -> Result<Value> {
    let cwd = action_config
        .as_ref()
        .and_then(|c| c.get("cwd"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| cfg.appdata().to_string_lossy().into_owned());

    let params_json = serde_json::to_string(params)?;
    let (shell, flag) = if cfg!(windows) { ("cmd", "/C") } else { ("sh", "-c") };

    let mut child = std::process::Command::new(shell)
        .arg(flag)
        .arg(fn_name)
        .current_dir(&cwd)
        .env("SOLX_PARAMS", &params_json)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| SolxError::Exec(format!("spawn '{fn_name}': {e}")))?;

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(params_json.as_bytes());
    }
    let output = child
        .wait_with_output()
        .map_err(|e| SolxError::Exec(e.to_string()))?;

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
                return dispatch_oauth_token_exchange(&client, url, auth, params).await;
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
async fn dispatch_oauth_token_exchange(
    client: &reqwest::Client,
    url: &str,
    auth: &Value,
    params: &Value,
) -> Result<Value> {
    let code = params
        .get("code")
        .and_then(Value::as_str)
        .ok_or_else(|| SolxError::Exec("oauth_authorization_code requires 'code' in params".into()))?;
    let client_id = auth
        .get("client_id")
        .and_then(Value::as_str)
        .ok_or_else(|| SolxError::Exec("oauth_authorization_code requires 'client_id' in auth".into()))?;
    let client_secret = auth
        .get("client_secret")
        .and_then(Value::as_str)
        .unwrap_or("");
    let redirect_uri = auth
        .get("redirect_uri")
        .and_then(Value::as_str)
        .unwrap_or("");

    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", client_id),
    ];
    if !client_secret.is_empty() {
        form.push(("client_secret", client_secret));
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

/// Placeholder for execution backends not yet ported (WASM components, full
/// OAuth loopback). Returns a descriptive error rather than silently failing.
pub fn unsupported(kind: &str) -> SolxError {
    SolxError::Exec(format!(
        "{kind} action execution is not implemented in this iteration of solx"
    ))
}
