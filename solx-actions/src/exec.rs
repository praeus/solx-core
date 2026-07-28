//! Action execution backends.
//!
//! * **Command** — a shell command from the config `command_actions` allowlist.
//! * **Webhook** — an HTTP POST gated by `allowed_webhook_base_urls`, with an
//!   optional bearer token / custom headers from `action_config`.
//!
//! WASM built-in actions and the full OAuth loopback flow are planned but not
//! implemented in this iteration; [`unsupported`] returns a clear error.

use std::io::Write;
use std::process::Stdio;

use serde_json::Value;
use solx_config::ConfigService;
use solx_surface::error::{Result, SolxError};

/// Run a `Command` action. `fn_name` is the key into the config allowlist.
pub fn run_command(
    cfg: &ConfigService,
    fn_name: &str,
    action_config: &Option<Value>,
    params: &Value,
) -> Result<Value> {
    let snap = cfg.snapshot();
    let def = snap.command_actions.get(fn_name).ok_or_else(|| {
        SolxError::Exec(format!(
            "command '{fn_name}' is not in the command_actions allowlist"
        ))
    })?;

    let cwd = action_config
        .as_ref()
        .and_then(|c| c.get("cwd"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| def.cwd.clone())
        .unwrap_or_else(|| cfg.appdata().to_string_lossy().into_owned());

    let params_json = serde_json::to_string(params)?;
    let (shell, flag) = if cfg!(windows) { ("cmd", "/C") } else { ("sh", "-c") };

    let mut child = std::process::Command::new(shell)
        .arg(flag)
        .arg(&def.command)
        .current_dir(&cwd)
        .env("SOLX_PARAMS", &params_json)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| SolxError::Exec(format!("spawn '{}': {e}", def.command)))?;

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
pub async fn run_webhook(
    cfg: &ConfigService,
    url: &str,
    action_config: &Option<Value>,
    params: &Value,
) -> Result<Value> {
    let snap = cfg.snapshot();
    let allow = &snap.allowed_webhook_base_urls;
    if !allow.is_empty() && !allow.iter().any(|b| url.starts_with(b)) {
        return Err(SolxError::Exec(format!(
            "webhook URL '{url}' is not permitted by allowed_webhook_base_urls"
        )));
    }

    let client = reqwest::Client::new();
    let mut req = client.post(url).json(params);

    if let Some(obj) = action_config {
        if let Some(bearer) = obj
            .get("auth")
            .and_then(|a| a.get("bearer_token"))
            .and_then(|v| v.as_str())
        {
            req = req.bearer_auth(bearer);
        }
        if let Some(headers) = obj.get("headers").and_then(|v| v.as_object()) {
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

/// Placeholder for execution backends not yet ported (WASM components, full
/// OAuth loopback). Returns a descriptive error rather than silently failing.
pub fn unsupported(kind: &str) -> SolxError {
    SolxError::Exec(format!(
        "{kind} action execution is not implemented in this iteration of solx"
    ))
}
