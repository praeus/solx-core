//! Open a URL in the system browser via the platform-native handler.
//!
//! This is the solx replacement for the old `open system browser` /
//! `system_open` built-in. It is used primarily by OAuth login flows that
//! need the user to visit a consent URL in a real browser.
//!
//! Cross-platform: shells out to `xdg-open` on Linux, `open` on macOS, and
//! `cmd /C start ""` on Windows. Run detached so the shell exits immediately
//! after launching the browser; we do not wait on the child.

use std::process::Command;

use serde_json::{json, Value};

use super::require_str;

/// Launch the platform's default handler for `url`.
///
/// `url` must be a non-empty string. The launch runs detached and the
/// function returns immediately — the caller does **not** wait for the
/// browser to close.
pub(super) async fn open_url(params: &Value) -> Result<Value, String> {
    let url = require_str(params, "url")?;
    if url.trim().is_empty() {
        return Err("open_url: 'url' must not be empty".into());
    }

    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        // The leading empty `""` is the (dummy) window title that `start`
        // requires; without it the URL is treated as the title and the
        // command fails. Run through `cmd /C` so the shell parses the
        // quoted string properly.
        ("cmd", vec!["/C", "start", "", url])
    } else {
        // Linux / BSD — `xdg-open` is the freedesktop standard, present
        // on every mainstream desktop.
        ("xdg-open", vec![url])
    };

    // `spawn` (not `status`) so the browser process detaches. We do not
    // capture stdout/stderr; the launcher inherits them.
    Command::new(program)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("open_url: failed to launch '{program}': {e}"))?;

    Ok(json!({ "opened": true, "url": url }))
}
