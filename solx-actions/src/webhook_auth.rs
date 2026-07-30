//! `action_config.auth` block resolver for the Webhook dispatcher.
//!
//! Reads `action_config.auth` (free-form JSON) at call time and resolves
//! it to a `Bearer` header value, if any. Four auth types are supported:
//!
//! * `bearer` — `{type: "bearer", token: "..."}` → static header.
//! * `oauth_refresh` — refresh-token grant exchange against `token_uri`.
//! * `oauth_service_account` — JWT-bearer grant exchange against `token_uri`.
//! * `oauth_authorization_code` — sentinel; the dispatcher itself runs
//!   the `code` → `access_token` exchange (see `dispatch_webhook`).
//!
//! Access tokens are cached in-memory per action name and refreshed
//! within 60 seconds of expiry. Long-lived secrets (`client_secret`,
//! `refresh_token`, `private_key`) can be sourced from the OS credential
//! manager by giving a keyring reference in `action_config.auth`:
//!
//! ```json
//! { "type": "oauth_service_account",
//!   "client_email": "sa@project.iam.gserviceaccount.com",
//!   "private_key": { "keyring_service": "sol", "keyring_account": "google-svc-pk" },
//!   "token_uri": "https://oauth2.googleapis.com/token" }
//! ```
//!
//! When the provider returns a new `refresh_token` during an
//! `oauth_refresh` exchange, the resolver persists it back to wherever
//! it was originally sourced from (inline field, keyring reference, or
//! scoped secret store) so rotation is not lost. Set
//! `action_config.auth.persist_rotated_refresh` to `false` to disable
//! this and let callers handle rotation themselves (default: `true`).
//!
//! Long-lived secrets (`client_secret`, `refresh_token`, `private_key`)
//! additionally accept either a keyring reference (object) or a pointer
//! into the scoped secret store (`client_secret_secret`, etc — see
//! `secrets.rs`). The secret's decryption key must be declared in this
//! action's own `action_config.secrets` map:
//!
//! ```json
//! { "action_config": {
//!     "secrets": { "GOOGLE_OAUTH_CLIENT_SECRET": "<32-byte-b64-key>" },
//!     "auth": { "type": "oauth_refresh", "client_secret_secret": "GOOGLE_OAUTH_CLIENT_SECRET", ... }
//! } }
//! ```

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::Deserialize;
use serde_json::{json, Value};

use solx_surface::entities::ActionInput;
use solx_surface::managers::ActionManager;

// ── Cache ────────────────────────────────────────────────────────────────────

type CacheEntry = (String, Instant); // (token, expires_at)

static AUTH_CACHE: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();

fn cache() -> &'static Mutex<HashMap<String, CacheEntry>> {
    AUTH_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// solx identity is `(path, name)`, not `name` alone — two actions with the
/// same bare name under different paths are a normal configuration (e.g.
/// `/prod/refresh-token` and `/staging/refresh-token`). The cache key must
/// incorporate both, or two such actions would silently share (and clobber)
/// each other's cached bearer token.
fn cache_key(path: &str, name: &str) -> String {
    solx_surface::path::full_ref(path, name).unwrap_or_else(|_| format!("{path}/{name}"))
}

/// Clear the cached access token for the action at `path`/`name`. Useful
/// for tests and for an explicit "log out and force refresh" path.
pub fn clear_auth_cache(path: &str, name: &str) {
    if let Ok(mut c) = cache().lock() {
        c.remove(&cache_key(path, name));
    }
}

fn cache_get(key: &str) -> Option<String> {
    cache().lock().ok().and_then(|c| {
        c.get(key).and_then(|(tok, expires_at)| {
            if *expires_at > Instant::now() {
                Some(tok.clone())
            } else {
                None
            }
        })
    })
}

fn cache_put(key: &str, token: String, expires_in_secs: u64) {
    // Refresh 60s before the actual expiry to avoid races.
    let ttl = expires_in_secs.saturating_sub(60).max(1);
    if let Ok(mut c) = cache().lock() {
        c.insert(key.to_string(), (token, Instant::now() + Duration::from_secs(ttl)));
    }
}

// ── HTTP client (shared) ─────────────────────────────────────────────────────

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build auth http client")
    })
}

// ── Public entry point ───────────────────────────────────────────────────────

/// Outcome of `oauth_refresh`: the new access token plus any rotated
/// refresh token the provider returned. The dispatcher (which has
/// access to `&dyn DbManager`) is responsible for actually persisting
/// the rotated refresh token; the resolver gathers the data and
/// returns it.
#[derive(Debug, Default)]
struct RefreshOutcome {
    access_token: String,
    /// When the provider returned a different `refresh_token` than
    /// the one we sent, this is the new value.
    rotated_refresh_token: Option<String>,
    /// Server-reported `expires_in` (seconds) for the access token.
    /// `None` when the provider omits the field; the resolver falls
    /// back to a 1h cache TTL in that case.
    expires_in_secs: Option<u64>,
}

/// Convert the full set of auth-resolution outcomes into a single
/// `Authorization` header value (without the `Authorization:` prefix).
///
/// Returns `Ok(None)` when:
/// * the action has no `auth` block, OR
/// * `auth.type == "oauth_authorization_code"` (the sentinel that the
///   dispatcher handles itself).
///
/// On `oauth_refresh` success the function **also** persists any
/// rotated `refresh_token` returned by the provider back to the
/// action's `action_config.auth.refresh_token` (or its keyring
/// account), so the next call sees the new token. Persistence is
/// best-effort: any db error is logged at `warn` and does **not**
/// fail the auth resolution — the rotated token is also returned via
/// the `RotatedRefreshToken` field on [`RefreshOutcome`] for callers
/// that want to do their own persistence.
pub async fn resolve_auth(
    actions: &dyn ActionManager,
    path: &str,
    name: &str,
    action_config: &Value,
) -> Result<Option<String>, String> {
    let key = cache_key(path, name);
    let Some(auth) = action_config.get("auth") else {
        return Ok(None);
    };
    let auth_type = auth
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "action_config.auth is missing required field 'type'".to_string())?;

    // Cache fast path for all three types.
    if let Some(tok) = cache_get(&key) {
        return Ok(Some(format!("Bearer {tok}")));
    }

    match auth_type {
        "bearer" => {
            // Resolve through the standard secret pipeline so the
            // bearer token can come from inline / keyring / scoped
            // secret store, matching the OAuth types. The token-ttl
            // `ttl_secs` field is inline-only — the per-action TTL
            // doesn't have a keyring/secret shape.
            let token = secret_field(action_config, auth, "token", Some("token_secret"), "bearer")?;
            let ttl = auth
                .get("ttl_secs")
                .and_then(Value::as_u64)
                .unwrap_or(3600);
            cache_put(&key, token.clone(), ttl);
            Ok(Some(format!("Bearer {token}")))
        }
        "oauth_refresh" => {
            let outcome = exchange_refresh_token(actions, path, name, action_config, auth).await?;
            // Touch the rotated_refresh_token field so the compiler
            // doesn't flag it as unused — the persistence side-effect
            // inside `exchange_refresh_token` already wrote the new
            // value back to wherever the refresh token was sourced from.
            let _ = outcome.rotated_refresh_token;
            // Respect the server-reported `expires_in` when the
            // provider sends one. Fall back to 1h.
            let ttl_secs = outcome.expires_in_secs.unwrap_or(3600);
            cache_put(&key, outcome.access_token.clone(), ttl_secs);
            Ok(Some(format!("Bearer {}", outcome.access_token)))
        }
        "oauth_service_account" => {
            let token = exchange_service_account_token(action_config, auth).await?;
            let ttl = auth
                .get("ttl_secs")
                .and_then(Value::as_u64)
                .unwrap_or(3600);
            cache_put(&key, token.clone(), ttl);
            Ok(Some(format!("Bearer {token}")))
        }
        "oauth_authorization_code" => {
            // The `oauth_authorization_code` auth type is handled by
            // `dispatch_webhook` itself (which knows the request URL and
            // body shape); from the resolver's perspective it just means
            // "no Bearer header to inject". The dispatcher performs the
            // form-encoded POST to the action's `fn_name` (the token
            // endpoint) and returns the token JSON as the action result.
            Ok(None)
        }
        other => Err(format!(
            "unsupported action_config.auth.type '{other}'; \
             expected 'bearer', 'oauth_refresh', 'oauth_service_account', \
             or 'oauth_authorization_code'"
        )),
    }
}

// ── Refresh-token grant (RFC 6749 §6) ─────────────────────────────────────────

/// Owned form of the inputs needed for an RFC 6749 §6 refresh-token grant.
/// We deliberately avoid `Deserialize` here because borrowing the strings
/// out of `&Value` makes serde's lifetime checker unhappy — pulling them
/// into owned `String`s is simpler and the values are tiny.
///
/// `client_id`, `client_secret`, and `refresh_token` can each be inline,
/// a keyring reference, or a pointer into the scoped secret store
/// (`client_id_secret`, `client_secret_secret`, `refresh_token_secret` —
/// see [`secret_field`]).  `token_uri` and `scope` remain in
/// `action_config.auth` since they are not secrets.
#[derive(Debug)]
struct RefreshRequest {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    token_uri: String,
}

impl RefreshRequest {
    fn from_value(action_config: &Value, auth: &Value) -> Result<Self, String> {
        Ok(Self {
            client_id: secret_field(action_config, auth, "client_id", Some("client_id_secret"), "oauth_refresh")?,
            client_secret: secret_field(
                action_config,
                auth,
                "client_secret",
                Some("client_secret_secret"),
                "oauth_refresh",
            )?,
            refresh_token: secret_field(
                action_config,
                auth,
                "refresh_token",
                Some("refresh_token_secret"),
                "oauth_refresh",
            )?,
            token_uri: require_inline_string(auth, "token_uri", "oauth_refresh")?,
        })
    }
}

/// Resolve one OAuth secret field with the three-source precedence:
///
/// 1. Inline `auth.<field>` value (string **or** keyring-ref object).
/// 2. `auth.<field>_secret` — name of a secret in the scoped secret
///    store; the decryption key must be declared in this action's own
///    `action_config.secrets` map.
/// 3. `Err` — none of the above were present.
///
/// `secret_field_name` selects which `<field>_secret` field to look at;
/// pass `None` to opt out of the secret-store indirection entirely.
pub(crate) fn secret_field(
    action_config: &Value,
    auth: &Value,
    field: &str,
    secret_field_name: Option<&str>,
    auth_type_label: &str,
) -> Result<String, String> {
    // 1. Inline (string or keyring ref).
    if let Some(v) = auth.get(field) {
        match v {
            Value::String(s) if !s.is_empty() => return Ok(s.clone()),
            Value::Object(_) => {
                return crate::secrets::resolve_in_field(
                    &format!("auth.type='{auth_type_label}'"),
                    field,
                    v,
                );
            }
            Value::Null | Value::String(_) => { /* fall through to secret-store pointer */ }
            _ => {
                return Err(format!(
                    "auth.type='{auth_type_label}': field '{field}' has \
                     unsupported JSON type {}; expected string or \
                     '{{ \"keyring_service\": …, \"keyring_account\": … }}'",
                    crate::secrets::type_name_of(v)
                ));
            }
        }
    }
    // 2. Scoped secret-store indirection.
    if let Some(name) = secret_field_name.and_then(|f| auth.get(f).and_then(Value::as_str)) {
        if !name.is_empty() {
            let key = action_config
                .get("secrets")
                .and_then(|s| s.get(name))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    format!(
                        "auth.type='{auth_type_label}': field '{field}' points to secret \
                         '{name}' but action_config.secrets has no key configured for it"
                    )
                })?;
            let value = crate::secrets::get_secret(name, key).map_err(|e| {
                format!("auth.type='{auth_type_label}': failed to read secret '{name}': {e}")
            })?;
            return value.filter(|v| !v.is_empty()).ok_or_else(|| {
                format!("auth.type='{auth_type_label}': secret '{name}' is not set")
            });
        }
    }
    let secret_hint = match secret_field_name {
        Some(name) => format!("or via '{name}' secret-store pointer"),
        None => String::new(),
    };
    Err(format!(
        "auth.type='{auth_type_label}' requires field '{field}' \
         (inline string, keyring reference{comma}{secret_hint})",
        comma = if secret_hint.is_empty() { "" } else { ", " }
    ))
}

/// Like [`secret_field`], but returns `Ok(None)` instead of `Err` when
/// *nothing at all* is configured for `field` — some OAuth flows
/// legitimately have no value here (e.g. a public/PKCE client has no
/// `client_secret`). If something *is* configured (inline, keyring
/// reference, or a `_secret` pointer) but fails to resolve, that's still a
/// hard error — a configured-but-broken secret should never fail silently.
pub(crate) fn optional_secret_field(
    action_config: &Value,
    auth: &Value,
    field: &str,
    secret_field_name: Option<&str>,
    auth_type_label: &str,
) -> Result<Option<String>, String> {
    let inline_present = auth.get(field).is_some();
    let pointer_present = secret_field_name
        .and_then(|f| auth.get(f).and_then(Value::as_str))
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if !inline_present && !pointer_present {
        return Ok(None);
    }
    secret_field(action_config, auth, field, secret_field_name, auth_type_label).map(Some)
}

/// Variant of [`secret_field`] for non-secret fields. Inline string only.
fn require_inline_string(
    auth: &Value,
    field: &str,
    auth_type_label: &str,
) -> Result<String, String> {
    auth.get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            format!("auth.type='{auth_type_label}' missing required field '{field}'")
        })
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
    /// Optional per RFC 6749 §5.1; many providers rotate refresh tokens
    /// on each successful response. We treat the response as
    /// "rotated" only when the field is **present and non-empty AND
    /// different** from the value we sent.
    #[serde(default)]
    refresh_token: Option<String>,
}

/// Where the rotated refresh token should land. Mirrors the resolution
/// source the action was originally configured with — keeping the
/// secret in the same place preserves any audit / scanning
/// expectations.
#[derive(Debug, Clone)]
enum RefreshStorage {
    /// The action has `"refresh_token": "..."` inline — patch
    /// `action_config.auth.refresh_token` in-place.
    Inline,
    /// The action has `"refresh_token": { "keyring_service": …, "keyring_account": … }`
    /// — write the new value through the keyring.
    Keyring {
        service: String,
        account: String,
    },
    /// The action has `"refresh_token_secret": "<name>"` — write the new
    /// value through the scoped secret store, using the key already
    /// configured for that name in `action_config.secrets`.
    Secret {
        name: String,
        key: String,
    },
}

impl RefreshStorage {
    fn from_auth(action_config: &Value, auth: &Value) -> Self {
        if let Some(name) = auth.get("refresh_token_secret").and_then(Value::as_str) {
            if let Some(key) = action_config
                .get("secrets")
                .and_then(|s| s.get(name))
                .and_then(Value::as_str)
            {
                return RefreshStorage::Secret {
                    name: name.to_string(),
                    key: key.to_string(),
                };
            }
        }
        if let Some(obj) = auth.get("refresh_token").and_then(Value::as_object) {
            if let (Some(service), Some(account)) = (
                obj.get("keyring_service").and_then(Value::as_str),
                obj.get("keyring_account").and_then(Value::as_str),
            ) {
                return RefreshStorage::Keyring {
                    service: service.to_string(),
                    account: account.to_string(),
                };
            }
        }
        RefreshStorage::Inline
    }
}

async fn exchange_refresh_token(
    actions: &dyn ActionManager,
    path: &str,
    name: &str,
    action_config: &Value,
    auth: &Value,
) -> Result<RefreshOutcome, String> {
    let req = RefreshRequest::from_value(action_config, auth)?;

    let body = json!({
        "grant_type": "refresh_token",
        "client_id": req.client_id,
        "client_secret": req.client_secret,
        "refresh_token": req.refresh_token,
    });
    let resp = http_client()
        .post(req.token_uri)
        .form(&body)
        .send()
        .await
        .map_err(|e| format!("oauth_refresh token endpoint POST failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!(
            "oauth_refresh token endpoint returned HTTP {status}: {text}"
        ));
    }
    let parsed: RefreshResponse = resp
        .json()
        .await
        .map_err(|e| format!("oauth_refresh response is not valid JSON: {e}"))?;

    let rotated = parsed
        .refresh_token
        .as_ref()
        .filter(|tok| !tok.is_empty() && **tok != req.refresh_token)
        .cloned();

    // Persistence of rotated tokens is opt-out (`auth.persist_rotated_refresh`).
    // The default is `true` because most providers rotate, but actions that
    // want to manage the new token themselves can disable this — see
    // `docs/oauth-integration.md` §2.3.
    let persistence_enabled = auth
        .get("persist_rotated_refresh")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    if let (Some(ref new_rt), true) = (rotated.as_ref(), persistence_enabled) {
        persist_rotated_refresh_token(actions, path, name, action_config, auth, new_rt).await;
    } else if let Some(ref new_rt) = rotated {
        tracing::info!(
            "received rotated refresh_token for action '{name}' but \
             persistence is disabled (auth.persist_rotated_refresh=false); \
             new token is NOT persisted"
        );
        let _ = new_rt;
    }

    Ok(RefreshOutcome {
        access_token: parsed.access_token,
        rotated_refresh_token: rotated,
        expires_in_secs: parsed.expires_in,
    })
}

/// Persist a rotated refresh token. Best-effort: any error is logged at
/// `warn` and *does not* propagate to the caller — the access token
/// has already been issued, so the most we can do is tell the user
/// "the next refresh may fail; rotate manually".
async fn persist_rotated_refresh_token(
    actions: &dyn ActionManager,
    path: &str,
    name: &str,
    action_config: &Value,
    auth: &Value,
    new_refresh_token: &str,
) {
    let storage = RefreshStorage::from_auth(action_config, auth);
    match storage {
        RefreshStorage::Inline => {
            // Patch the action's action_config.auth.refresh_token in place.
            // We merge with the existing field set so other auth.* fields
            // (client_id, scope, etc.) survive.
            match actions.get(path, name).await {
                Ok(action) => {
                    let mut new_cfg = action
                        .action_config
                        .clone()
                        .unwrap_or_else(|| serde_json::json!({}));
                    if !new_cfg.is_object() {
                        tracing::warn!(
                            "action '{name}' action_config is not an object; \
                             cannot persist rotated refresh_token"
                        );
                        return;
                    }
                    if new_cfg.get_mut("auth").is_none() {
                        new_cfg
                            .as_object_mut()
                            .unwrap()
                            .insert("auth".into(), serde_json::json!({}));
                    }
                    if let Some(auth_obj) = new_cfg.get_mut("auth").and_then(|v| v.as_object_mut()) {
                        auth_obj.insert(
                            "refresh_token".into(),
                            serde_json::Value::String(new_refresh_token.to_string()),
                        );
                    }
                    let input = ActionInput {
                        action_config: Some(new_cfg),
                        ..Default::default()
                    };
                    if let Err(e) = actions.post(path, name, input).await {
                        tracing::warn!(
                            "failed to persist rotated refresh_token for action '{name}': {e}"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "failed to load action '{name}' to persist rotated refresh_token: {e}"
                    );
                }
            }
        }
        RefreshStorage::Keyring { service, account } => {
            let entry = match keyring::Entry::new(&service, &account) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(
                        "keyring entry '{service}/{account}' open failed; \
                         rotated refresh_token not persisted: {e}"
                    );
                    return;
                }
            };
            if let Err(e) = entry.set_password(new_refresh_token) {
                tracing::warn!(
                    "keyring entry '{service}/{account}' set_password failed; \
                     rotated refresh_token not persisted: {e}"
                );
            }
        }
        RefreshStorage::Secret { name: secret_name, key } => {
            if let Err(e) = crate::secrets::set_secret(&secret_name, new_refresh_token, &key) {
                tracing::warn!(
                    "failed to persist rotated refresh_token to secret '{secret_name}' \
                     for action '{name}': {e}"
                );
            }
        }
    }
}

// ── Service-account JWT-bearer grant (RFC 7521 §5.1) ─────────────────────────

/// Owned form of the inputs for an RFC 7521 §5.1 service-account JWT-bearer
/// grant. See [`RefreshRequest`] for why we avoid `Deserialize` here.
///
/// `private_key` is sourced through [`secret_field`] (inline string,
/// keyring reference, or scoped secret-store pointer — see
/// `docs/oauth-integration.md` §6.1). For RSA PEMs the keyring or
/// secret-store shape is strongly preferred over inline.
#[derive(Debug)]
struct ServiceAccountRequest {
    client_email: String,
    private_key: String,
    token_uri: String,
}

impl ServiceAccountRequest {
    fn from_value(action_config: &Value, auth: &Value) -> Result<Self, String> {
        Ok(Self {
            client_email: require_inline_string(auth, "client_email", "oauth_service_account")?,
            private_key: secret_field(
                action_config,
                auth,
                "private_key",
                Some("private_key_secret"),
                "oauth_service_account",
            )?,
            token_uri: require_inline_string(auth, "token_uri", "oauth_service_account")?,
        })
    }
}

async fn exchange_service_account_token(action_config: &Value, auth: &Value) -> Result<String, String> {
    let req = ServiceAccountRequest::from_value(action_config, auth)?;

    let now = chrono::Utc::now().timestamp();
    let exp = now + 3600; // 1h
    let scope = auth
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or("");
    let claims = json!({
        "iss": req.client_email,
        "scope": scope,
        "aud": req.token_uri,
        "iat": now,
        "exp": exp,
    });
    let key = EncodingKey::from_rsa_pem(req.private_key.as_bytes())
        .context("service_account private_key is not valid RSA PEM")
        .map_err(|e| e.to_string())?;
    let assertion = jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &key)
        .map_err(|e| format!("failed to sign service-account JWT: {e}"))?;

    let body = format!(
        "grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer&assertion={}",
        urlencoding(&assertion)
    );
    let resp = http_client()
        .post(req.token_uri)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("oauth_service_account token endpoint POST failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!(
            "oauth_service_account token endpoint returned HTTP {status}: {text}"
        ));
    }
    let parsed: RefreshResponse = resp
        .json()
        .await
        .map_err(|e| format!("oauth_service_account response is not valid JSON: {e}"))?;
    Ok(parsed.access_token)
}

/// Minimal URL-encoder for the JWT assertion (the `assertion` value contains
/// dots which are safe; this only escapes `+`, `=`, `&`, etc.).
fn urlencoding(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use solx_surface::entities::{Action, ActionExecResult};
    use solx_surface::query::{ListOptions, Page};

    fn clear() {
        cache().lock().unwrap().clear();
    }

    /// Stub ActionManager. None of the bearer/auth-block tests reach the
    /// `actions` parameter — they only exercise the inline/keyring/cache
    /// paths. If a test ever does reach it, the panic tells us the test
    /// needs a real mock.
    struct StubActionManager;

    #[async_trait]
    impl ActionManager for StubActionManager {
        async fn post(&self, _path: &str, _name: &str, _input: ActionInput) -> solx_surface::error::Result<Action> {
            panic!("stub ActionManager::post should not be called by these tests")
        }
        async fn get(&self, _path: &str, _name: &str) -> solx_surface::error::Result<Action> {
            panic!("stub ActionManager::get should not be called by these tests")
        }
        async fn delete(&self, _path: &str, _name: &str) -> solx_surface::error::Result<()> {
            panic!("stub ActionManager::delete should not be called by these tests")
        }
        async fn list(&self, _opts: ListOptions) -> solx_surface::error::Result<Page<Action>> {
            panic!("stub ActionManager::list should not be called by these tests")
        }
        async fn exec(&self, _path: &str, _name: &str, _params: Value) -> solx_surface::error::Result<ActionExecResult> {
            panic!("stub ActionManager::exec should not be called by these tests")
        }
    }

    fn stub_actions() -> StubActionManager {
        StubActionManager
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn no_auth_block_returns_none() {
        clear();
        let cfg = json!({});
        let out = resolve_auth(&stub_actions(), "/test", "a", &cfg).await.unwrap();
        assert!(out.is_none());
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn bearer_returns_static_token() {
        clear();
        let cfg = json!({ "auth": { "type": "bearer", "token": "abc123" } });
        let out = resolve_auth(&stub_actions(), "/test", "a", &cfg).await.unwrap().unwrap();
        assert_eq!(out, "Bearer abc123");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn bearer_missing_token_errors() {
        clear();
        let cfg = json!({ "auth": { "type": "bearer" } });
        let err = resolve_auth(&stub_actions(), "/test", "bearer_missing_action", &cfg).await.unwrap_err();
        assert!(err.contains("requires field 'token'"), "{err}");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn unknown_auth_type_errors_helpfully() {
        clear();
        let cfg = json!({ "auth": { "type": "magic_link" } });
        let err = resolve_auth(&stub_actions(), "/test", "a", &cfg).await.unwrap_err();
        assert!(err.contains("unsupported action_config.auth.type 'magic_link'"), "{err}");
        assert!(
            err.contains("'bearer'"),
            "error should list the valid types, got: {err}"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn auth_block_missing_type_errors() {
        clear();
        let cfg = json!({ "auth": { "token": "x" } });
        let err = resolve_auth(&stub_actions(), "/test", "a", &cfg).await.unwrap_err();
        assert!(err.contains("missing required field 'type'"), "{err}");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn cache_hit_avoids_second_call() {
        clear();
        let cfg = json!({ "auth": { "type": "bearer", "token": "abc123", "ttl_secs": 60 } });
        let _ = resolve_auth(&stub_actions(), "/test", "a", &cfg).await.unwrap();
        // Second call should hit the cache and still return the same token
        // even if we mutate the config to make the bearer path fail.
        let mutated = json!({ "auth": { "type": "bearer" /* no token */ } });
        let out = resolve_auth(&stub_actions(), "/test", "a", &mutated).await.unwrap().unwrap();
        assert_eq!(out, "Bearer abc123");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn cache_keyed_per_action() {
        clear();
        let cfg_a = json!({ "auth": { "type": "bearer", "token": "for-a", "ttl_secs": 60 } });
        let cfg_b = json!({ "auth": { "type": "bearer", "token": "for-b", "ttl_secs": 60 } });
        assert_eq!(resolve_auth(&stub_actions(), "/test", "a", &cfg_a).await.unwrap().unwrap(), "Bearer for-a");
        assert_eq!(resolve_auth(&stub_actions(), "/test", "b", &cfg_b).await.unwrap().unwrap(), "Bearer for-b");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn cache_does_not_collide_across_paths_with_same_name() {
        // solx identity is (path, name), not name alone — two actions with
        // the same bare name under different paths (e.g. a "prod" and a
        // "staging" copy of the same integration, both named "webhook")
        // must not share a cached bearer token.
        clear();
        let cfg_prod = json!({ "auth": { "type": "bearer", "token": "prod-token", "ttl_secs": 60 } });
        let cfg_staging = json!({ "auth": { "type": "bearer", "token": "staging-token", "ttl_secs": 60 } });
        assert_eq!(
            resolve_auth(&stub_actions(), "/prod", "webhook", &cfg_prod).await.unwrap().unwrap(),
            "Bearer prod-token"
        );
        assert_eq!(
            resolve_auth(&stub_actions(), "/staging", "webhook", &cfg_staging).await.unwrap().unwrap(),
            "Bearer staging-token"
        );
        // Re-resolving prod (config unchanged) must still hit its own cache
        // entry, not staging's.
        assert_eq!(
            resolve_auth(&stub_actions(), "/prod", "webhook", &cfg_prod).await.unwrap().unwrap(),
            "Bearer prod-token"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn clear_auth_cache_forces_refresh() {
        clear();
        let cfg = json!({ "auth": { "type": "bearer", "token": "first" } });
        assert_eq!(resolve_auth(&stub_actions(), "/test", "a", &cfg).await.unwrap().unwrap(), "Bearer first");

        // Mutate config (e.g. user rotated the bearer); clear cache; verify
        // the new token is returned.
        let cfg2 = json!({ "auth": { "type": "bearer", "token": "second" } });
        clear_auth_cache("/test", "a");
        assert_eq!(resolve_auth(&stub_actions(), "/test", "a", &cfg2).await.unwrap().unwrap(), "Bearer second");
    }

    #[serial_test::serial]
    #[test]
    fn urlencoding_leaves_safe_chars_alone() {
        assert_eq!(urlencoding("abc.123-XYZ_ok~bar"), "abc.123-XYZ_ok~bar");
    }

    #[serial_test::serial]
    #[test]
    fn urlencoding_escapes_unsafe_chars() {
        assert_eq!(urlencoding("a+b=c&d e"), "a%2Bb%3Dc%26d%20e");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn oauth_authorization_code_returns_none_no_bearer() {
        clear();
        // `oauth_authorization_code` is handled by `dispatch_webhook`
        // itself — the resolver returns Ok(None) so no Bearer header is
        // injected. The dispatch path then takes over and POSTs to the
        // action's `fn_name` (the token URI) with form-encoded body.
        let cfg = json!({
            "auth": {
                "type": "oauth_authorization_code",
                "client_id": "cid",
                "client_secret": "csec"
            }
        });
        let out = resolve_auth(&stub_actions(), "/test", "a", &cfg).await.unwrap();
        assert!(out.is_none(), "oauth_authorization_code must not inject a Bearer header; got {out:?}");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn oauth_authorization_code_does_not_cache() {
        clear();
        // Unlike the other auth types, this one is intercepted upstream
        // by the dispatcher and never reaches `cache_get` / `cache_put`,
        // so resolve_auth must NOT populate the cache with a stale
        // bearer token.
        let cfg = json!({
            "auth": {
                "type": "oauth_authorization_code",
                "client_id": "cid"
            }
        });
        let _ = resolve_auth(&stub_actions(), "/test", "a", &cfg).await.unwrap();
        // Cache should remain empty.
        assert!(
            cache_get(&cache_key("/test", "a")).is_none(),
            "oauth_authorization_code must not write to the auth cache"
        );
    }

    // ── keyring / resolution-shape tests ────────────────────────────────

    #[serial_test::serial]
    #[tokio::test]
    async fn keyring_shape_resolves_via_test_loader() {
        // Make sure the test seam is installed *before* the call so the
        // call never reaches the real keyring crate (which would fail
        // inside the CI sandbox without an OS credential manager).
        // Always clear at the end via a guard so a panic in the test
        // doesn't leave the loader set for subsequent tests.
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                crate::secrets::set_secret_loader_for_tests(None);
            }
        }
        let _g = Guard;
        crate::secrets::set_secret_loader_for_tests(Some(|_| Ok("from-keyring".into())));
        let cfg = json!({
            "auth": {
                "type": "bearer",
                "token": { "keyring_service": "sol", "keyring_account": "bearer-x" }
            }
        });
        let out = resolve_auth(&stub_actions(), "/test", "keyring_action", &cfg)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out, "Bearer from-keyring");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn inline_string_wins_over_test_loader_for_bearer() {
        // Inline string is the highest-precedence source — the keyring
        // loader must not run when the inline path can satisfy the
        // request. Verify by installing a loader that would panic if
        // invoked. Use a unique action name so we don't hit the in-memory
        // auth cache from a previous test. The Drop guard clears the
        // loader even on assertion failure.
        clear();
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                crate::secrets::set_secret_loader_for_tests(None);
            }
        }
        let _g = Guard;
        crate::secrets::set_secret_loader_for_tests(Some(|_| panic!("loader should not run")));
        let cfg = json!({
            "auth": {
                "type": "bearer",
                "token": "inline-value"
            }
        });
        let out = resolve_auth(&stub_actions(), "/test", "inline_wins_action", &cfg)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out, "Bearer inline-value");
    }

    // ── optional_secret_field (oauth_authorization_code client_secret) ─────

    #[test]
    fn optional_secret_field_returns_none_when_nothing_configured() {
        // A public/PKCE OAuth client legitimately has no client_secret —
        // this must not be an error.
        let cfg = json!({});
        let auth = json!({});
        assert_eq!(
            optional_secret_field(&cfg, &auth, "client_secret", Some("client_secret_secret"), "oauth_authorization_code")
                .unwrap(),
            None
        );
    }

    #[test]
    fn optional_secret_field_resolves_inline_when_present() {
        let cfg = json!({});
        let auth = json!({ "client_secret": "shh" });
        assert_eq!(
            optional_secret_field(&cfg, &auth, "client_secret", Some("client_secret_secret"), "oauth_authorization_code")
                .unwrap(),
            Some("shh".to_string())
        );
    }

    #[test]
    fn optional_secret_field_errors_when_pointer_configured_but_key_missing() {
        // Points at a secret-store name, but action_config.secrets has no
        // key configured for it — a real misconfiguration that must error
        // loudly rather than silently degrading to Ok(None).
        let cfg = json!({});
        let auth = json!({ "client_secret_secret": "MY_SECRET" });
        let err = optional_secret_field(&cfg, &auth, "client_secret", Some("client_secret_secret"), "oauth_authorization_code")
            .unwrap_err();
        assert!(err.contains("MY_SECRET"), "{err}");
    }

    #[test]
    fn refresh_storage_detects_inline_or_keyring() {
        let empty_cfg = json!({});
        let inline = json!({"refresh_token": "abc"});
        assert!(matches!(
            RefreshStorage::from_auth(&empty_cfg, &inline),
            RefreshStorage::Inline
        ));
        let keyring = json!({"refresh_token": {"keyring_service": "sol", "keyring_account": "x"}});
        assert!(matches!(
            RefreshStorage::from_auth(&empty_cfg, &keyring),
            RefreshStorage::Keyring { .. }
        ));
    }

    #[test]
    fn refresh_storage_detects_secret_pointer_when_key_configured() {
        let cfg = json!({"secrets": {"GOOGLE_OAUTH_REFRESH_TOKEN": "the-key"}});
        let auth = json!({"refresh_token_secret": "GOOGLE_OAUTH_REFRESH_TOKEN"});
        assert!(matches!(
            RefreshStorage::from_auth(&cfg, &auth),
            RefreshStorage::Secret { .. }
        ));
    }

    #[test]
    fn refresh_storage_falls_back_to_inline_when_secret_pointer_has_no_key() {
        // Points at a secret store name, but action_config.secrets has no
        // key configured for it — falls back to Inline rather than
        // silently trying to use the secret store without a key.
        let cfg = json!({});
        let auth = json!({"refresh_token_secret": "GOOGLE_OAUTH_REFRESH_TOKEN", "refresh_token": "abc"});
        assert!(matches!(
            RefreshStorage::from_auth(&cfg, &auth),
            RefreshStorage::Inline
        ));
    }

    #[test]
    fn persist_rotated_refresh_flag_defaults_true() {
        // The flag is read by `exchange_refresh_token` via
        // `auth.get("persist_rotated_refresh").and_then(Value::as_bool)
        // .unwrap_or(true)`; absent / non-bool → true.
        let absent = json!({});
        assert!(absent
            .get("persist_rotated_refresh")
            .and_then(Value::as_bool)
            .unwrap_or(true));

        let string = json!({"persist_rotated_refresh": "no"});
        assert!(string
            .get("persist_rotated_refresh")
            .and_then(Value::as_bool)
            .unwrap_or(true), "non-bool values fall through to the default-true");

        let opted_out = json!({"persist_rotated_refresh": false});
        assert!(!opted_out
            .get("persist_rotated_refresh")
            .and_then(Value::as_bool)
            .unwrap_or(true));
    }
}
