//! Open a URL in the system browser via the platform-native handler.
//!
//! This is the solx replacement for the old `open system browser` /
//! `system_open` built-in. It is used primarily by OAuth login flows that
//! need the user to visit a consent URL in a real browser.
//!
//! Cross-platform: shells out to `xdg-open` on Linux and `open` on macOS
//! (run detached; we do not wait on the child), and calls `ShellExecuteW`
//! on Windows.
//!
//! Windows used to go through `cmd /C start "" <url>`. `std::process`
//! only quotes an argument that contains whitespace, so a typical OAuth URL
//! reached `cmd` unquoted and was cut at its first `&` — the browser got
//! `...?response_type=code` and nothing after it (Facebook: "Invalid App
//! ID", since `client_id` never arrived). Quoting wouldn't fully fix it
//! either: `cmd` still expands `%NAME%` inside quotes, and percent-encoded
//! URLs can match. `ShellExecuteW` involves no shell parsing at all.

use serde_json::{json, Value};
use solx_config::ConfigService;

use super::require_str;

/// Launch the platform's default handler for `url`.
///
/// `url` must be a non-empty string. The launch runs detached and the
/// function returns immediately — the caller does **not** wait for the
/// browser to close.
pub(super) async fn open_url(params: &Value, cfg: &ConfigService) -> Result<Value, String> {
    let url = require_str(params, "url")?;
    if url.trim().is_empty() {
        return Err("open-url: 'url' must not be empty".into());
    }
    // Gated like every other outbound path. This one hands the URL to the
    // user's real browser session, so `file:`/`javascript:`/`about:` are
    // refused outright rather than prefix-matched — see `crate::net`.
    crate::net::check_outbound_url(cfg, url).map_err(|e| e.to_string())?;

    launch(url).await?;
    Ok(json!({ "opened": true, "url": url }))
}

#[cfg(windows)]
async fn launch(url: &str) -> Result<(), String> {
    let url = url.to_string();
    // ShellExecuteW can block briefly while the shell resolves the handler.
    tokio::task::spawn_blocking(move || shell_execute_open(&url))
        .await
        .map_err(|e| format!("open-url: launcher task failed: {e}"))?
}

/// Open `url` with its registered handler (the default browser). The URL
/// is passed as one UTF-16 string, never through a command line.
#[cfg(windows)]
fn shell_execute_open(url: &str) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    let verb = wide("open");
    let file = wide(url);
    // SAFETY: both strings are NUL-terminated UTF-16 buffers that outlive
    // the call; the remaining pointer arguments are documented as optional.
    let rc = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // Per the ShellExecuteW docs, values > 32 mean success; anything else is
    // an error code.
    if rc as isize > 32 {
        Ok(())
    } else {
        Err(format!("open-url: ShellExecuteW failed (code {})", rc as isize))
    }
}

#[cfg(not(windows))]
async fn launch(url: &str) -> Result<(), String> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        // Linux / BSD — `xdg-open` is the freedesktop standard, present
        // on every mainstream desktop.
        "xdg-open"
    };

    // `spawn` (not `status`) so the browser process detaches. We do not
    // capture stdout/stderr; the launcher inherits them.
    std::process::Command::new(program)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("open-url: failed to launch '{program}': {e}"))?;
    Ok(())
}
