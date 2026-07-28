//! Command action-type handler + logging helpers.
//!
//! [`dispatch_command`] executes a shell command, injecting the action
//! params as the `SOL_PARAMS` environment variable. The CWD is set to
//! the action's artifact directory when it exists.

use serde_json::{json, Value};

use sol_core::entities::ActionExecResult;

// ── dispatch_command ─────────────────────────────────────────────────────────

/// Execute a shell command, injecting params as `SOL_PARAMS` env var.
/// CWD is set to the action's artifact directory when it exists.
pub(crate) async fn dispatch_command(
    action_name: &str,
    command: &str,
    params: &Value,
    cwd: Option<&std::path::Path>,
) -> Result<ActionExecResult, String> {
    let params_json =
        serde_json::to_string(params).unwrap_or_else(|_| "{}".to_string());

    #[cfg(windows)]
    let (shell, flag) = ("cmd.exe", "/C");
    #[cfg(not(windows))]
    let (shell, flag) = ("sh", "-c");

    // Per-package log directory: <sol_db_home>/logs/commands/<action_name>/.
    // The action name is the entity name (e.g. "extractous-preprocess") and is
    // a safe-enough path segment after `safe_log_segment`.
    let package_name = crate::safe_log_segment(action_name);
    let log_dir = crate::command_log_dir(&package_name);
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        tracing::debug!(
            "command log directory create failed at '{}': {e}",
            log_dir.display()
        );
    }
    let log_dir_str = log_dir.to_string_lossy().to_string();
    let master_log = crate::command_master_log_path(&package_name);

    let mut cmd = tokio::process::Command::new(shell);
    cmd.arg(flag).arg(command);
    cmd.env("SOL_PARAMS", &params_json);
    // Hand the child a writable directory it can use for its own verbose
    // logging. Packages that want package-level detail should append to
    // <SOL_LOG_DIR>/<command_key>.log. See sol-extractous for an example.
    cmd.env("SOL_LOG_DIR", &log_dir_str);

    if let Some(dir) = cwd {
        if dir.exists() {
            cmd.current_dir(dir);
        }
    }

    let started = std::time::Instant::now();
    let output = cmd
        .output()
        .await
        .map_err(|e| {
            append_command_log(
                &master_log,
                &format!(
                    "ts={} action={} command={:?} status=spawn_failed duration_ms={} error={}",
                    event_timestamp(),
                    action_name,
                    command,
                    started.elapsed().as_millis(),
                    e,
                ),
            );
            format!("failed to execute command: {e}")
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let success = output.status.success();
    let exit_code = output.status.code();
    let duration_ms = started.elapsed().as_millis();

    let result = serde_json::from_str::<Value>(&stdout).unwrap_or_else(|_| {
        json!({
            "stdout": stdout,
            "stderr": stderr,
            "exit_code": exit_code,
        })
    });

    let parsed_error_message = result
        .get("error")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .or_else(|| {
            result
                .get("message")
                .and_then(Value::as_str)
                .map(|s| s.to_string())
        })
        .or_else(|| {
            result
                .get("response")
                .and_then(|response| response.get("error"))
                .and_then(Value::as_str)
                .map(|s| s.to_string())
        })
        .or_else(|| {
            result
                .get("response")
                .and_then(|response| response.get("message"))
                .and_then(Value::as_str)
                .map(|s| s.to_string())
        });

    // Master dispatch log line: one entry per command execution. The child's
    // own verbose trace (if any) is in SOL_LOG_DIR. Truncate stdout/stderr in
    // the log to keep the master log compact.
    let stdout_summary: String = stdout.chars().take(200).collect();
    let stderr_summary: String = stderr.chars().take(200).collect();
    let status_label = if success { "ok" } else { "failed" };
    let log_line = format!(
        "ts={} action={} status={} duration_ms={} exit_code={:?} stdout_preview={:?} stderr_preview={:?}",
        event_timestamp(),
        action_name,
        status_label,
        duration_ms,
        exit_code,
        stdout_summary,
        stderr_summary,
    );
    append_command_log(&master_log, &log_line);

    Ok(ActionExecResult {
        action_name: action_name.to_string(),
        result,
        success,
        message: if success {
            None
        } else if !stderr.is_empty() {
            Some(stderr)
        } else if let Some(msg) = parsed_error_message {
            Some(msg)
        } else {
            Some(format!("command exited with code {:?}", output.status.code()))
        },
        trace: vec![],
    })
}

// ── Private helpers ──────────────────────────────────────────────────────────

fn event_timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_millis())
        .unwrap_or(0);
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, ms * 1_000_000)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        .unwrap_or_else(|| format!("{secs}.{ms:03}"))
}

fn append_command_log(path: &std::path::Path, line: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let open = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path);
    if let Ok(mut file) = open {
        use std::io::Write;
        let _ = writeln!(file, "{line}");
    }
}