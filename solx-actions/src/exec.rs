//! Action execution backends.
//!
//! * **Command** — `fn_name` is never the literal shell command. It's a key
//!   resolved against `ConfigService::command_actions` (the
//!   `command_actions` allowlist in `solx-config.json`); the matching
//!   [`solx_config::CommandDef`]'s `command` field is what actually runs.
//!   **Deny-by-default**: an unregistered key is a hard error, not a
//!   fallback to running the key text itself.
//! * **Webhook** — `fn_name` is the literal URL to POST to, but the URL must
//!   start with one of the prefixes in `ConfigService::allowed_webhook_base_urls`
//!   (also `solx-config.json`) or dispatch is refused before any network
//!   call. Auth/headers come from `action_config`; OAuth token exchange
//!   (bearer, refresh_token, service_account, authorization_code) is
//!   resolved via [`crate::auth::resolve_auth`].
//!
//! Both allowlists are deny-by-default: an unset or empty allowlist rejects
//! every Command/Webhook action, not the old behavior of permitting
//! everything until configured. See `docs/next-steps.md` §1 for why (ported
//! from old `sol`'s `command_actions` key-indirection and
//! `allowed_webhook_base_urls` prefix allowlist, with the default flipped).
//!
//! `Wasm`-typed actions are executed by [`crate::wasm`], not here —
//! see [`crate::LocalActionManager::exec`]'s `Wasm` arm.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use serde_json::{json, Value};
use solx_config::ConfigService;
use solx_surface::error::{Result, SolxError};
use solx_surface::managers::ActionManager;
use tokio::io::AsyncWriteExt;

use solx_console::loopback as console_loopback;
use solx_console::{ConsoleStore, InvocationStore};

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

/// Run a `Command` action. `fn_name` is a key resolved against the
/// `command_actions` allowlist (see the module doc); the matching
/// `CommandDef.command` is what actually executes. Params are passed as
/// JSON on stdin only (no env var) — no payload size limit, and it doesn't
/// leak into process listings.
///
/// Fully async: the child is spawned with `tokio::process`, so an action
/// that takes minutes parks no thread. This used to be a synchronous
/// `std::process` call made directly from `exec_as`, which meant every
/// running command held an async *worker* thread — only `num_cpus` of
/// those exist, so a handful of concurrent commands could wedge the whole
/// process, HTTP routes included.
///
/// `invocation_id` is minted by the caller (`exec_as_with`), not here — a
/// detached `action_start` run needs the id fixed *before* execution begins
/// so `action_stop`/`action_poll` have something to address.
#[allow(clippy::too_many_arguments)]
pub async fn run_command(
    cfg: &ConfigService,
    console: Arc<ConsoleStore>,
    invocations: Arc<InvocationStore>,
    action_ref: &str,
    invocation_id: &str,
    fn_name: &str,
    action_config: &Option<Value>,
    params: &Value,
    timeout_secs: Option<u64>,
) -> Result<Value> {
    // `fn_name` is a key, not a command — resolve it against the
    // `command_actions` allowlist before anything else runs. Deny-by-default:
    // an unregistered key (including every key when the allowlist itself is
    // unset) is a hard error naming the fix, not a fallback to running the
    // key text as a shell command.
    let def = cfg.command_actions().remove(fn_name).ok_or_else(|| {
        SolxError::Exec(format!(
            "command key '{fn_name}' is not registered in solx-config.json's \
             'command_actions' allowlist; add an entry there to allow this \
             command to run"
        ))
    })?;

    // The allowlist's own `cwd` wins over the action's `action_config.cwd`
    // when both are set — the allowlist author decides where an approved
    // command runs, not the (potentially untrusted) action that invokes it.
    let cwd = def
        .cwd
        .clone()
        .or_else(|| {
            action_config
                .as_ref()
                .and_then(|c| c.get("cwd"))
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| cfg.appdata().to_string_lossy().into_owned());
    let command = def.command.as_str();

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
    // own console over loopback HTTP — see `solx_console::loopback` for
    // why this replaced reading the child's stderr. Best-effort: if the
    // loopback can't start, the command still runs, it just has no console
    // (or cancellation-check) access for this run. `registration` must stay
    // bound (not `_`) for the rest of this function — dropping it
    // deregisters the token, and we want that to happen automatically on
    // every exit path below, not just the success path.
    let registration = console_loopback::register(console, invocations, action_ref, invocation_id).await;

    let mut cmd = tokio::process::Command::new(shell);
    cmd.arg(flag)
        .arg(command)
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
        cmd.env("SOLX_CONSOLE_URL", reg.url())
            .env("SOLX_CONSOLE_TOKEN", reg.token())
            .env("SOLX_CONTROL_URL", reg.control_url());
    }

    // Forward any `action_config.env` map to the spawned child, so
    // packages like `solx-media` that read SOLX_SERVER_URL/SOLX_SERVER_TOKEN
    // from their own env don't fail with "config error: ... is required".
    // String values only — non-string entries are skipped with a warning
    // rather than silently dropped, since silently dropping a secret
    // would be the worse failure mode.
    if let Some(cfg) = action_config {
        if let Some(env) = cfg.get("env").and_then(|v| v.as_object()) {
            for (k, v) in env {
                match v.as_str() {
                    Some(s) => { cmd.env(k, s); }
                    None => tracing::warn!(
                        action_ref = %action_ref,
                        env_key = %k,
                        "action_config.env entry is not a string; skipping",
                    ),
                }
            }
        }
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| SolxError::Exec(format!("spawn command key '{fn_name}' ('{command}'): {e}")))?;

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
                "command key '{fn_name}' timed out after {}s",
                timeout.as_secs()
            )))
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SolxError::Exec(format!(
            "command key '{fn_name}' failed ({}): {}",
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
    cfg: &ConfigService,
    actions: &dyn ActionManager,
    console: Arc<ConsoleStore>,
    action_ref: &str,
    invocation_id: &str,
    path: &str,
    name: &str,
    url: &str,
    action_config: &Option<Value>,
    params: &Value,
) -> Result<Value> {
    // `action_config.path_params` names params that should be substituted
    // into `{name}` placeholders in the URL template rather than sent in
    // the JSON body — e.g. `fn_name: ".../documents/{documentId}"` with
    // `path_params: {"documentId": ""}` and a call-time
    // `params.documentId`. Substitution must happen before the allowlist
    // check below so a substituted URL can't be used to evade the gate.
    let (url, params_owned) = substitute_path_params(url, action_config, params);
    let url = url.as_str();
    let params = &params_owned;

    // Deny-by-default, checked before anything else — including the console
    // log line below, so a denied webhook leaves no trace of the attempt
    // beyond the returned error. The URL must start with one of the
    // configured prefixes; an unset or empty allowlist rejects every URL.
    let allowlist = cfg.allowed_webhook_base_urls();
    if !allowlist.iter().any(|base| url.starts_with(base.as_str())) {
        return Err(SolxError::Exec(format!(
            "webhook URL '{url}' does not match any prefix in solx-config.json's \
             'allowed_webhook_base_urls' allowlist; add a matching prefix there to \
             allow this webhook to run"
        )));
    }

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

/// Substitutes `{key}` placeholders in `url` with the matching string
/// value from `params`, for every key named in
/// `action_config.path_params`. The substituted keys are removed from the
/// returned params so they aren't also sent in the JSON/multipart body.
/// No URL-encoding is applied — the action author chose the template and
/// owns how it's encoded (path vs. query vs. header differ).
fn substitute_path_params(url: &str, action_config: &Option<Value>, params: &Value) -> (String, Value) {
    let Some(path_params) = action_config
        .as_ref()
        .and_then(|c| c.get("path_params"))
        .and_then(|v| v.as_object())
    else {
        return (url.to_string(), params.clone());
    };

    let mut substituted = url.to_string();
    let mut remaining = params.clone();
    for key in path_params.keys() {
        if let Some(val) = params.get(key).and_then(Value::as_str) {
            substituted = substituted.replace(&format!("{{{key}}}"), val);
        }
        if let Value::Object(map) = &mut remaining {
            map.remove(key);
        }
    }
    (substituted, remaining)
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

    let is_multipart_related = action_config
        .as_ref()
        .and_then(|c| c.get("body_mode"))
        .and_then(Value::as_str)
        == Some("multipart_related");

    let mut req = if is_multipart_related {
        let body = build_multipart_related_body(params)?;
        client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, body.content_type)
            .body(body.bytes)
    } else {
        client.post(url).json(params)
    };

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

#[derive(Debug)]
struct MultipartRelatedBody {
    content_type: String,
    bytes: Vec<u8>,
}

/// Builds a raw `multipart/related` body (Google's upload-with-metadata
/// convention, e.g. Drive `files.create?uploadType=multipart`) from
/// `params.metadata` (a JSON object) + `params.media_base64` +
/// `params.media_content_type`. `reqwest::multipart::Form` only produces
/// `multipart/form-data`, which Drive's upload endpoint doesn't accept —
/// this constructs the two-part `multipart/related` body by hand instead.
fn build_multipart_related_body(params: &Value) -> Result<MultipartRelatedBody> {
    let metadata = params.get("metadata").cloned().unwrap_or_else(|| json!({}));
    let media_b64 = params
        .get("media_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| SolxError::Exec("body_mode 'multipart_related' requires params.media_base64".into()))?;
    let media_bytes = base64::engine::general_purpose::STANDARD
        .decode(media_b64)
        .map_err(|e| SolxError::Exec(format!("invalid media_base64: {e}")))?;
    let media_content_type = params
        .get("media_content_type")
        .and_then(Value::as_str)
        .unwrap_or("application/octet-stream");

    let boundary = format!(
        "solxmpb{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );

    let mut bytes = Vec::new();
    bytes.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{metadata}\r\n--{boundary}\r\nContent-Type: {media_content_type}\r\n\r\n"
        )
        .as_bytes(),
    );
    bytes.extend_from_slice(&media_bytes);
    bytes.extend_from_slice(format!("\r\n--{boundary}--").as_bytes());

    Ok(MultipartRelatedBody {
        content_type: format!("multipart/related; boundary={boundary}"),
        bytes,
    })
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
    // Unlike `client_id`/`client_secret` (static per action, so they belong
    // in config), `redirect_uri` varies per call — it's the loopback's
    // actual bound address, which callers only know at request time (see
    // the doc comment on this fn: "it needs an already-obtained code +
    // redirect_uri"). So `params` takes priority here, the same way `code`
    // is read from `params` above; `auth.redirect_uri` remains as a
    // fallback for integrations that always redirect to one fixed URI.
    let redirect_uri = params
        .get("redirect_uri")
        .and_then(Value::as_str)
        .or_else(|| auth.get("redirect_uri").and_then(Value::as_str))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitute_path_params_replaces_placeholder_and_strips_body_key() {
        let action_config = Some(json!({ "path_params": { "fileId": "" } }));
        let params = json!({ "fileId": "abc123", "type": "anyone", "role": "reader" });

        let (url, remaining) = substitute_path_params(
            "https://www.googleapis.com/drive/v3/files/{fileId}/permissions",
            &action_config,
            &params,
        );

        assert_eq!(url, "https://www.googleapis.com/drive/v3/files/abc123/permissions");
        assert_eq!(remaining, json!({ "type": "anyone", "role": "reader" }));
    }

    #[test]
    fn substitute_path_params_is_a_no_op_without_config() {
        let params = json!({ "documentId": "xyz" });
        let (url, remaining) = substitute_path_params("https://docs.googleapis.com/v1/documents/{documentId}", &None, &params);
        assert_eq!(url, "https://docs.googleapis.com/v1/documents/{documentId}");
        assert_eq!(remaining, params);
    }

    #[test]
    fn substitute_path_params_leaves_placeholder_when_param_missing() {
        let action_config = Some(json!({ "path_params": { "documentId": "" } }));
        let params = json!({});
        let (url, remaining) = substitute_path_params(
            "https://docs.googleapis.com/v1/documents/{documentId}",
            &action_config,
            &params,
        );
        assert_eq!(url, "https://docs.googleapis.com/v1/documents/{documentId}");
        assert_eq!(remaining, json!({}));
    }

    #[test]
    fn multipart_related_body_contains_metadata_and_media_parts() {
        let params = json!({
            "metadata": { "name": "icon.png" },
            "media_base64": base64::engine::general_purpose::STANDARD.encode(b"fake-png-bytes"),
            "media_content_type": "image/png",
        });

        let body = build_multipart_related_body(&params).expect("body should build");
        assert!(body.content_type.starts_with("multipart/related; boundary=solxmpb"));

        let text = String::from_utf8_lossy(&body.bytes);
        assert!(text.contains("Content-Type: application/json; charset=UTF-8"));
        assert!(text.contains("\"name\":\"icon.png\""));
        assert!(text.contains("Content-Type: image/png"));
        assert!(body.bytes.windows(b"fake-png-bytes".len()).any(|w| w == b"fake-png-bytes"));
        assert!(text.trim_end().ends_with("--"));
    }

    #[test]
    fn multipart_related_body_requires_media_base64() {
        let params = json!({ "metadata": {} });
        let err = build_multipart_related_body(&params).unwrap_err();
        assert!(err.to_string().contains("media_base64"));
    }

    // ── dispatch_oauth_token_exchange: redirect_uri resolution ─────────────

    /// Binds a one-shot HTTP server that captures the request body of the
    /// single request it receives and replies with `response_body`. Returns
    /// the base URL plus a `JoinHandle` yielding the captured body.
    async fn start_token_endpoint(response_body: String) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}/token");
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let body = req.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
            body
        });
        (base, handle)
    }

    /// A per-call `redirect_uri` (the loopback's actual bound address) must
    /// win over a static `auth.redirect_uri` — regression test for the bug
    /// where the token exchange always used the (usually absent) static
    /// config value and Google's endpoint rejected the request with
    /// "Missing parameter: redirect_uri".
    #[tokio::test]
    async fn oauth_authorization_code_prefers_params_redirect_uri_over_auth() {
        let (url, handle) = start_token_endpoint(r#"{"access_token":"at","expires_in":3600}"#.into()).await;
        let client = reqwest::Client::new();
        let action_config = json!({});
        let auth = json!({
            "type": "oauth_authorization_code",
            "client_id": "cid",
            "redirect_uri": "http://static-fallback/callback"
        });
        let params = json!({
            "code": "the-code",
            "redirect_uri": "http://127.0.0.1:8765/callback"
        });

        let result = dispatch_oauth_token_exchange(&client, &url, &action_config, &auth, &params).await;
        assert!(result.is_ok(), "{:?}", result.err());

        let body = handle.await.unwrap();
        assert!(
            body.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A8765%2Fcallback"),
            "expected the params redirect_uri in the POST body, got: {body}"
        );
        assert!(
            !body.contains("static-fallback"),
            "params.redirect_uri should win over auth.redirect_uri, got: {body}"
        );
    }

    /// When the caller doesn't supply `redirect_uri` in params, the static
    /// `auth.redirect_uri` (for integrations that always use one fixed
    /// callback) is still honored.
    #[tokio::test]
    async fn oauth_authorization_code_falls_back_to_auth_redirect_uri() {
        let (url, handle) = start_token_endpoint(r#"{"access_token":"at","expires_in":3600}"#.into()).await;
        let client = reqwest::Client::new();
        let action_config = json!({});
        let auth = json!({
            "type": "oauth_authorization_code",
            "client_id": "cid",
            "redirect_uri": "http://static-fallback/callback"
        });
        let params = json!({ "code": "the-code" });

        let result = dispatch_oauth_token_exchange(&client, &url, &action_config, &auth, &params).await;
        assert!(result.is_ok(), "{:?}", result.err());

        let body = handle.await.unwrap();
        assert!(
            body.contains("redirect_uri=http%3A%2F%2Fstatic-fallback%2Fcallback"),
            "expected the auth.redirect_uri fallback in the POST body, got: {body}"
        );
    }
}
