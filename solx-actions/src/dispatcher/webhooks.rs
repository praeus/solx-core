//! Webhook action-type handler + OAuth token exchange.
//!
//! [`dispatch_webhook`] sends an HTTP request to a webhook URL and returns
//! the response. [`dispatch_oauth_token_exchange`] handles the
//! `authorization_code` OAuth grant as a short-circuit inside
//! `dispatch_webhook`.

use std::sync::Arc;

use serde_json::Value;

use sol_core::db::DbManager;
use sol_core::entities::ActionExecResult;

// ── dispatch_webhook ─────────────────────────────────────────────────────────

/// HTTP request the action params to a webhook URL and return the response.
///
/// `action_config` supports the following keys:
/// * `headers`     — object of `string→string` extra request headers.
/// * `timeout_secs` — positive `f64`; request timeout.
/// * `method`      — one of `GET|POST|PUT|PATCH|DELETE` (default `POST`).
/// * `path_params` — object of `string→string`; each value is substituted
///                    into `{key}` placeholders in `url` before sending.
/// * `auth`        — see [`crate::webhook_auth::resolve_auth`] for the
///                    `bearer` / `oauth_refresh` / `oauth_service_account`
///                    block. The resolved token is injected as
///                    `Authorization: Bearer <token>` AFTER the user-supplied
///                    headers loop, so an explicit user header still wins.
///
/// `action_link_bases` is the list of base-URL prefixes from the
/// `ActionLink` entries attached to this action.  They are unioned with
/// the global `allowed_webhook_base_urls` allowlist when validating
/// the (substituted) URL — see [`crate::validate_webhook_url`].
pub(crate) async fn dispatch_webhook(
    db: &Arc<dyn DbManager>,
    action_name: &str,
    url: &str,
    params: &Value,
    action_config: Option<&Value>,
    action_link_bases: &[String],
) -> Result<ActionExecResult, String> {
    let cfg = action_config.cloned().unwrap_or(Value::Null);

    // ── Path-template substitution ────────────────────────────────────────
    let mut url = url.to_string();
    // First, substitute from action_config.path_params (static defaults).
    // Empty-string defaults are skipped so that call params can fill them
    // in the next pass.
    if let Some(pp) = cfg.get("path_params").and_then(|v| v.as_object()) {
        for (k, v) in pp {
            let token = format!("{{{k}}}");
            let replacement = match v {
                Value::String(s) if !s.is_empty() => s.clone(),
                Value::String(_) => continue, // skip empty defaults
                other => other.to_string(),
            };
            // We don't URL-encode here — the action author chose the
            // template and is responsible for the shape of each placeholder.
            url = url.replace(&token, &replacement);
        }
    }
    // Then, substitute from call params (per-call overrides). This lets
    // actions like post-google-doc accept documentId as a call param and
    // have it filled into the URL template dynamically.
    if let Some(pp) = params.as_object() {
        for (k, v) in pp {
            let token = format!("{{{k}}}");
            if url.contains(&token) {
                let replacement = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                url = url.replace(&token, &replacement);
            }
        }
    }
    // Apply a configurable URL suffix (e.g. ":batchUpdate") so actions
    // can target sub-paths without a separate action definition.
    if let Some(suffix) = cfg.get("url_suffix").and_then(|v| v.as_str()) {
        url.push_str(suffix);
    }

    // Validate the (possibly substituted) URL against the configured
    // allowlist (global + per-action).  Prevents the path-param
    // substitution from being used to evade the allowlist.
    crate::validate_webhook_url(&url, action_link_bases)?;

    // ── Short-circuit: oauth_authorization_code auth type ─────────────
    // The dispatcher itself performs the form-encoded POST to the token
    // endpoint and returns the parsed JSON token response as the action
    // result. The action's `fn_name` is the token URI; `params` supplies
    // the per-call values (code, redirect_uri) and `action_config.auth`
    // supplies the client_id, client_secret, scope, etc.
    //
    // The webhook_auth resolver returns `Ok(None)` for this auth type
    // (see `webhook_auth::resolve_auth`), so we don't inject a Bearer.
    if cfg
        .get("auth")
        .and_then(|a| a.get("type"))
        .and_then(Value::as_str)
        == Some("oauth_authorization_code")
    {
        return dispatch_oauth_token_exchange(
            db.as_ref(),
            action_name,
            &url,
            params,
            &cfg,
        )
        .await;
    }

    // ── HTTP method (default POST) ───────────────────────────────────────
    let method = cfg
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("POST")
        .to_ascii_uppercase();
    let method = match method.as_str() {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "PATCH" => reqwest::Method::PATCH,
        "DELETE" => reqwest::Method::DELETE,
        other => {
            return Err(format!(
                "unsupported webhook method '{other}'; \
                 expected GET, POST, PUT, PATCH, or DELETE"
            ))
        }
    };

    // ── Build the reqwest client ─────────────────────────────────────────
    let timeout_secs = cfg
        .get("timeout_secs")
        .and_then(|v| v.as_f64())
        .filter(|&s| s > 0.0);
    let client = if let Some(secs) = timeout_secs {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs_f64(secs))
            .build()
            .map_err(|e| format!("failed to build webhook client: {e}"))?
    } else {
        reqwest::Client::new()
    };

    // ── Apply user-supplied headers (string values only) ────────────────
    let mut req = client.request(method.clone(), &url).header("User-Agent", "sol-webhook/1.0");
    if let Some(headers) = cfg.get("headers").and_then(|v| v.as_object()) {
        for (key, val) in headers {
            if let Some(v) = val.as_str() {
                req = req.header(key.as_str(), v);
            }
        }
    }

    // ── Apply body / query params per method ─────────────────────────────
    // GET / DELETE put params on the query string; everything else sends
    // a JSON body (matching the previous POST-only behavior).
    //
    // When `action_config.body_path` is set (e.g. `"body"`), the dispatcher
    // extracts that field from `params` and sends it as the JSON body
    // instead of the entire params object. This lets actions expose a
    // richer param schema (with control fields like `method`, `documentId`)
    // while only sending the relevant sub-object to the API.
    let body_path = cfg.get("body_path").and_then(|v| v.as_str());
    let body_value = match body_path {
        Some(path) => params.get(path).cloned().unwrap_or(Value::Null),
        None => params.clone(),
    };

    req = match method {
        reqwest::Method::GET | reqwest::Method::DELETE => {
            // Drop nulls so `params: {"documentId": null, "other": "x"}`
            // doesn't render as `?documentId=&other=x`.
            let cleaned: Value = params
                .as_object()
                .map(|m| {
                    Value::Object(
                        m.iter()
                            .filter(|(_, v)| !v.is_null())
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect(),
                    )
                })
                .unwrap_or_else(|| params.clone());
            req.query(&cleaned)
        }
        _ => req.json(&body_value),
    };

    // ── Apply auth block (Bearer header) AFTER user headers ─────────────
    let auth_header = crate::webhook_auth::resolve_auth(db.as_ref(), action_name, &cfg).await?;
    if let Some(bearer_value) = auth_header {
        req = req.header("Authorization", bearer_value);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("webhook request failed: {e}"))?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    Ok(ActionExecResult {
        action_name: action_name.to_string(),
        result: body,
        success: status.is_success(),
        message: if status.is_success() {
            None
        } else {
            Some(format!("webhook returned HTTP {status}"))
        },
        trace: vec![],
    })
}

// ── OAuth token exchange ─────────────────────────────────────────────────────

/// Form-encoded POST to an OAuth 2.0 token endpoint for the
/// `authorization_code` grant (RFC 6749 §4.1.3).
///
/// The action's `fn_name` is the token URI (e.g.
/// `https://oauth2.googleapis.com/token`). The body is assembled by
/// merging:
///   - `action_config.auth` (`client_id`, `client_secret`, plus any
///     provider defaults like `scope`)
///   - `params` (per-call `code`, `redirect_uri`)
///
/// with the standard `grant_type=authorization_code`. The response JSON
/// is returned as the action result. On non-2xx, the response body and
/// status are surfaced as an error.
///
/// The action's own response carries the `refresh_token` (if the
/// provider returned one), `access_token`, `expires_in`, `scope`, and
/// `token_type`. The dispatcher persists `refresh_token` back onto the
/// matching `oauth_refresh` action's `action_config.auth.refresh_token`
/// (or its keyring account, or its scoped secret-store entry) so
/// subsequent calls can use `auth.type: "oauth_refresh"`.
async fn dispatch_oauth_token_exchange(
    db: &dyn DbManager,
    action_name: &str,
    token_uri: &str,
    params: &Value,
    action_config: &Value,
) -> Result<ActionExecResult, String> {
    let auth = action_config.get("auth").cloned().unwrap_or(Value::Null);

    // Build the form body: start with grant_type, then layer
    // action_config.auth fields, then env-var fallbacks for OAuth
    // secrets, then params (params win).
    let mut form: Vec<(String, String)> = Vec::new();
    form.push(("grant_type".to_string(), "authorization_code".to_string()));
    if let Some(obj) = auth.as_object() {
        for (k, v) in obj {
            // Skip the type field and the *_secret pointer fields — those
            // are metadata, not form values.
            if k == "type" || k.ends_with("_secret") {
                continue;
            }
            if let Some(s) = v.as_str() {
                // Skip empty values from action_config.auth — they'll be
                // filled from the secret store below.
                if !s.is_empty() {
                    form.push((k.clone(), s.to_string()));
                }
            }
        }
    }
    // Scoped secret-store fallbacks for OAuth secrets. The secret name is
    // read from the `*_secret` fields in action_config.auth (e.g.
    // `client_id_secret` = "GOOGLE_OAUTH_CLIENT_ID"), and the decryption
    // key from this action's own `action_config.secrets` map. Only add
    // when the form key isn't already present (action_config.auth or
    // params may override).
    let secret_pointers: &[(&str, &str)] = &[
        ("client_id", "client_id_secret"),
        ("client_secret", "client_secret_secret"),
        ("scope", "scope_secret"),
    ];
    for (form_key, secret_field) in secret_pointers {
        let already_present = form.iter().any(|(k, _)| k == form_key);
        if !already_present {
            if let Some(name) = auth.get(secret_field).and_then(Value::as_str) {
                if !name.is_empty() {
                    if let Some(key) = action_config
                        .get("secrets")
                        .and_then(|s| s.get(name))
                        .and_then(Value::as_str)
                    {
                        if let Ok(Some(val)) = crate::secrets::get_secret(name, key) {
                            if !val.is_empty() {
                                form.push((form_key.to_string(), val));
                            }
                        }
                    }
                }
            }
        }
    }
    if let Some(obj) = params.as_object() {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                form.push((k.clone(), s.to_string()));
            }
            // Non-string params are skipped: token endpoints only accept
            // application/x-www-form-urlencoded strings.
        }
    }

    let timeout_secs = action_config
        .get("timeout_secs")
        .and_then(|v| v.as_f64())
        .filter(|&s| s > 0.0);
    let client = if let Some(secs) = timeout_secs {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs_f64(secs))
            .build()
            .map_err(|e| format!("failed to build oauth token client: {e}"))?
    } else {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| format!("failed to build oauth token client: {e}"))?
    };

    let resp = client
        .post(token_uri)
        .header("Accept", "application/json")
        .header(
            "User-Agent",
            "sol-webhook/1.0 (oauth_authorization_code)",
        )
        .form(&form)
        .send()
        .await
        .map_err(|e| format!("oauth token exchange POST failed: {e}"))?;

    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);

    // If the provider returned a refresh token, fan out to a sibling
    // `oauth_refresh` action whose name is `{this}--refresh` (or
    // whatever the caller puts in params.persist_to).  Failures are
    // logged at warn but don't fail the auth-code exchange (the
    // `result` still carries the new tokens for callers to handle
    // manually).
    if status.is_success() {
        if let Some(new_rt) = body.get("refresh_token").and_then(Value::as_str) {
            if !new_rt.is_empty() {
                // Resolve target once and own the String so the
                // borrow into the helper outlives the format!().
                let target_owned: String = params
                    .get("persist_to")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{action_name}--refresh"));
                let target: &str = target_owned.as_str();
                persist_refresh_token_for_action(db, target, new_rt).await;
            }
        }
    }

    Ok(ActionExecResult {
        action_name: action_name.to_string(),
        result: body,
        success: status.is_success(),
        message: if status.is_success() {
            None
        } else {
            Some(format!(
                "oauth token endpoint returned HTTP {status}; \
                 check client_id/client_secret/code/redirect_uri"
            ))
        },
        trace: vec![],
    })
}

/// Persist a `refresh_token` returned by an `oauth_authorization_code`
/// exchange onto `target_action`. Best-effort; logs failures and
/// returns without erroring. Mirrors the storage-shape detection used
/// inside `webhook_auth::persist_rotated_refresh_token` so the two
/// paths behave the same way.
///
/// Honors an override via `params.persist_storage`:
/// * `Some("inline")` — force inline `auth.refresh_token = "..."`
/// * `Some("keyring:<service>:<account>")` — force keyring write
async fn persist_refresh_token_for_action(
    db: &dyn DbManager,
    target_action: &str,
    new_refresh_token: &str,
) {
    let action = match db.get_action(target_action).await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(
                "cannot persist refresh_token: target action '{target_action}' not found: {e}"
            );
            return;
        }
    };
    let mut new_cfg = action
        .action_config
        .clone()
        .unwrap_or_else(|| serde_json::json!({}));
    if !new_cfg.is_object() {
        tracing::warn!(
            "action '{target_action}' action_config is not an object; cannot persist refresh_token"
        );
        return;
    }
    if new_cfg.get("auth").is_none() {
        new_cfg
            .as_object_mut()
            .unwrap()
            .insert("auth".into(), serde_json::json!({}));
    }
    // Decide storage, in the same order as
    // `webhook_auth::RefreshStorage::from_auth`: scoped secret-store
    // pointer, then keyring reference, then inline. Computed against
    // `new_cfg` before taking the mutable `auth_obj` borrow below.
    let secret_target = new_cfg
        .get("auth")
        .and_then(|a| a.get("refresh_token_secret"))
        .and_then(Value::as_str)
        .and_then(|name| {
            new_cfg
                .get("secrets")
                .and_then(|s| s.get(name))
                .and_then(Value::as_str)
                .map(|key| (name.to_string(), key.to_string()))
        });

    let auth_obj = new_cfg
        .get_mut("auth")
        .and_then(|v| v.as_object_mut())
        .unwrap();

    let existing_is_keyring = auth_obj
        .get("refresh_token")
        .and_then(Value::as_object)
        .map(|o| {
            o.contains_key("keyring_service") && o.contains_key("keyring_account")
        })
        .unwrap_or(false);

    if let Some((name, key)) = secret_target {
        if let Err(e) = crate::secrets::set_secret(&name, new_refresh_token, &key) {
            tracing::warn!(
                "failed to persist refresh_token to secret '{name}' for action '{target_action}': {e}"
            );
        }
    } else if existing_is_keyring {
        let service = auth_obj
            .get("refresh_token")
            .and_then(|v| v.get("keyring_service"))
            .and_then(Value::as_str)
            .unwrap_or("sol")
            .to_string();
        let account = auth_obj
            .get("refresh_token")
            .and_then(|v| v.get("keyring_account"))
            .and_then(Value::as_str)
            .unwrap_or(target_action)
            .to_string();
        match keyring::Entry::new(&service, &account) {
            Ok(entry) => {
                if let Err(e) = entry.set_password(new_refresh_token) {
                    tracing::warn!(
                        "keyring '{service}/{account}' set_password failed: {e}"
                    );
                }
            }
            Err(e) => tracing::warn!(
                "keyring '{service}/{account}' open failed: {e}"
            ),
        }
    } else {
        auth_obj.insert(
            "refresh_token".into(),
            Value::String(new_refresh_token.to_string()),
        );
        use sol_core::entities::ActionInput;
        let input = ActionInput {
            name: action.name.clone(),
            caption: None,
            description: None,
            capabilities: None,
            parameters_in: None,
            parameters_out: None,
            phrases: None,
            category: None,
            parameter_type_name: None,
            result_type_name: None,
            artifact_names: None,
            bin_name: None,
            fn_name: None,
            permission_name: None,
            allowed_permission_names: None,
            trusted: None,
            action_type: None,
            action_config: Some(new_cfg),
        };
        if let Err(e) = db.update_action(&action.name, &input).await {
            tracing::warn!(
                "failed to persist refresh_token on action '{target_action}': {e}"
            );
        }
    }
}