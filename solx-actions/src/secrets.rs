//! Secret retrieval layer.
//!
//! Centralises three ways a value referenced from `action_config.auth`
//! can be sourced at call time:
//!
//! | Shape                                      | Source                                          |
//! |--------------------------------------------|--------------------------------------------------|
//! | Plain `String`                              | Inline in `action_config.auth` (existing)       |
//! | `{"keyring_service": …, "keyring_account": …}` | OS credential manager via the `keyring` crate |
//! | String env-var *name* (`*_env` field)       | In-process WASM env store                       |
//!
//! ## Why a separate module?
//!
//! The dispatcher needs to read the same kind of secret in three places
//! (`exchange_refresh_token`, `exchange_service_account_token`, and
//! `dispatch_oauth_token_exchange`). Centralising the resolution keeps
//! the precedence rules and the keyring-failure semantics in one spot,
//! and gives us a place to add caching if we ever need it.
//!
//! ## Failure model
//!
//! The `resolve` function returns `Err(String)` with a human-readable
//! message on any failure. The error is propagated all the way back
//! through `dispatch_webhook` into the action result; callers see the
//! provider-agnostic message rather than the raw `keyring::Error`.
//!
//! ## Test seams
//!
//! Tests inject a fake loader via [`set_secret_loader_for_tests`] so
//! they do not require a real OS credential manager (and so they can
//! cover the keyring-failure path deterministically). The default
//! loader is the real `keyring::Entry::get_password`.

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::Value;

/// Alternate shape accepted by [`resolve_str`] and friends.
///
/// `keyring_service` is a per-app namespace (conventionally
/// `"sol-manager"`); `keyring_account` is the unique identifier within
/// that namespace (typically the action name, or some random ID for a
/// one-off secret).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct KeyringRef {
    pub keyring_service: String,
    pub keyring_account: String,
}

/// Inspect a JSON value and decide how to resolve it.
///
/// * `Value::String(s)`        → inline value (return immediately)
/// * `Value::Object({..})`     → expect `keyring_service` + `keyring_account`
/// * anything else              → treat as `Err`
///
/// The `*_env` indirection is handled *outside* this module by the
/// caller; `resolve` itself is the final hop.
pub fn resolve(value: &Value) -> Result<String, String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Object(_) => {
            let r: KeyringRef = serde_json::from_value(value.clone()).map_err(|e| {
                format!(
                    "keyring reference must have 'keyring_service' and \
                     'keyring_account' string fields; got: {e}"
                )
            })?;
            load_keyring_with_test_override(&r)
        }
        Value::Null => Err("secret value is null; provide an inline string, a keyring reference, or a *-env pointer".to_string()),
        other => Err(format!(
            "secret value has unsupported JSON type {}; expected string or object",
            type_name(other)
        )),
    }
}

fn type_name(v: &Value) -> &'static str {
    type_name_of(v)
}

/// Public re-export of [`type_name`] so callers (e.g. `webhook_auth`)
/// can describe the shape of a malformed auth field without depending
/// on the private helper.
pub fn type_name_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Resolve a string-or-keyring-ref to a `String`. Convenience wrapper
/// around [`resolve`] for the common "read one field out of auth"
/// case.
///
/// `field_name` is used to build a helpful error message ("auth's
/// 'private_key' is …") so callers don't need to interpolate.
pub fn resolve_in_field(parent: &str, field_name: &str, value: &Value) -> Result<String, String> {
    resolve(value).map_err(|e| format!("{parent}'s '{field_name}' {e}"))
}

/// Load a secret from the OS credential manager via the `keyring`
/// crate. Errors are converted to human-readable strings here so the
/// dispatcher doesn't need to know about `keyring::Error`.
///
/// On platforms where `keyring::Entry::get_password` returns
/// `NoEntry`, the error is "no entry under service/account"; on
/// permission-denied it's "access denied by credential manager".
pub fn load_keyring(r: &KeyringRef) -> Result<String, String> {
    if r.keyring_service.is_empty() || r.keyring_account.is_empty() {
        return Err(format!(
            "keyring reference requires non-empty 'keyring_service' \
             and 'keyring_account' (got service='{}', account='{}')",
            r.keyring_service, r.keyring_account
        ));
    }
    let entry = keyring::Entry::new(&r.keyring_service, &r.keyring_account)
        .map_err(|e| format!("keyring entry '{}/{}' open failed: {e}", r.keyring_service, r.keyring_account))?;
    entry.get_password().map_err(|e| {
        format!(
            "keyring lookup '{}/{}' failed: {e} \
             (this commonly means the entry was never stored; see \
             `docs/oauth-integration.md` for the setup commands)",
            r.keyring_service, r.keyring_account
        )
    })
}

// ── Test seam ────────────────────────────────────────────────────────────────

/// Type of the test-only secret loader. Receives the same `KeyringRef`
/// the real loader would receive, returns whatever the test wants.
pub type SecretLoaderFn = fn(&KeyringRef) -> Result<String, String>;

static TEST_LOADER: std::sync::OnceLock<std::sync::Mutex<Option<SecretLoaderFn>>> =
    std::sync::OnceLock::new();

fn test_loader_slot() -> &'static std::sync::Mutex<Option<SecretLoaderFn>> {
    TEST_LOADER.get_or_init(|| std::sync::Mutex::new(None))
}

/// Install a fake [`SecretLoaderFn`] for tests. Pass `None` to restore
/// the real `keyring`-backed loader.
pub fn set_secret_loader_for_tests(loader: Option<SecretLoaderFn>) {
    if let Ok(mut g) = test_loader_slot().lock() {
        *g = loader;
    }
}

/// Try the test loader first; fall back to the real implementation.
pub fn load_keyring_with_test_override(r: &KeyringRef) -> Result<String, String> {
    if let Ok(g) = test_loader_slot().lock() {
        if let Some(loader) = *g {
            return loader(r);
        }
    }
    load_keyring(r)
}

// ── Scoped secret store ──────────────────────────────────────────────────────
//
// Unlike `resolve`/`KeyringRef` above (which serve the existing
// inline/keyring-ref `auth` fields), this store backs the `get
// secret`/`set secret` built-in actions and the `*_secret` auth
// fields. Every value is additionally encrypted with a caller-supplied
// 32-byte key before being written to the OS credential manager, and
// that key lives in the calling action's own `action_config.secrets`
// map (see `wasm_host::get_secret`/`set_secret`) — so reading a secret
// back out requires both a keyring hit *and* possession of the
// matching key. The host never hands a key to a guest itself; it only
// ever looks one up from the config of the action that is currently
// calling in.

const SECRETS_KEYRING_SERVICE: &str = "sol-secrets";
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;

/// Decode a base64 key and enforce it is exactly 32 bytes (AES-256).
/// Keys are meant to be produced only by the `solx random` command, so
/// rejecting anything else keeps someone from wiring up a weak
/// hand-typed key.
fn decode_key(key_b64: &str) -> Result<[u8; KEY_LEN], String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(key_b64)
        .map_err(|e| format!("secret key is not valid base64: {e}"))?;
    bytes
        .try_into()
        .map_err(|v: Vec<u8>| format!("secret key must decode to {KEY_LEN} bytes, got {}", v.len()))
}

fn encrypt(value: &str, key_b64: &str) -> Result<String, String> {
    let key_bytes = decode_key(key_b64)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, value.as_bytes())
        .map_err(|e| format!("secret encryption failed: {e}"))?;
    let mut blob = nonce.to_vec();
    blob.extend_from_slice(&ciphertext);
    Ok(base64::engine::general_purpose::STANDARD.encode(blob))
}

fn decrypt(blob_b64: &str, key_b64: &str) -> Result<String, String> {
    let key_bytes = decode_key(key_b64)?;
    let blob = base64::engine::general_purpose::STANDARD
        .decode(blob_b64)
        .map_err(|e| format!("stored secret is not valid base64: {e}"))?;
    if blob.len() < NONCE_LEN {
        return Err("stored secret is too short to contain a nonce".to_string());
    }
    let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));
    let plaintext = cipher
        .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
        .map_err(|_| "secret decryption failed (wrong key or corrupted entry)".to_string())?;
    String::from_utf8(plaintext).map_err(|e| format!("decrypted secret is not valid UTF-8: {e}"))
}

/// Encrypt `value` with `key_b64` and write it to the OS credential
/// manager under `name`. `key_b64` must be a 32-byte, base64-encoded
/// value (as produced by `solx random`).
pub fn set_secret(name: &str, value: &str, key_b64: &str) -> Result<(), String> {
    let blob = encrypt(value, key_b64)?;
    raw_set(name, &blob)
}

/// Read the value stored under `name`, decrypting with `key_b64`.
/// Returns `Ok(None)` if no entry exists under that name. Returns
/// `Err` if an entry exists but `key_b64` cannot decrypt it (wrong
/// key) or the entry is corrupted.
pub fn get_secret(name: &str, key_b64: &str) -> Result<Option<String>, String> {
    match raw_get(name)? {
        Some(blob) => decrypt(&blob, key_b64).map(Some),
        None => Ok(None),
    }
}

/// Remove the entry stored under `name`, if any. Used by package
/// uninstall flows so they don't leave orphaned keyring entries.
pub fn delete_secret(name: &str) -> Result<(), String> {
    raw_delete(name)
}

// ── Raw keyring I/O (test-overridable) ───────────────────────────────────────

fn keyring_entry(name: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SECRETS_KEYRING_SERVICE, name)
        .map_err(|e| format!("keyring entry '{SECRETS_KEYRING_SERVICE}/{name}' open failed: {e}"))
}

fn real_raw_get(name: &str) -> Result<Option<String>, String> {
    match keyring_entry(name)?.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!(
            "keyring lookup '{SECRETS_KEYRING_SERVICE}/{name}' failed: {e}"
        )),
    }
}

fn real_raw_set(name: &str, blob: &str) -> Result<(), String> {
    keyring_entry(name)?
        .set_password(blob)
        .map_err(|e| format!("keyring write '{SECRETS_KEYRING_SERVICE}/{name}' failed: {e}"))
}

fn real_raw_delete(name: &str) -> Result<(), String> {
    match keyring_entry(name)?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!(
            "keyring delete '{SECRETS_KEYRING_SERVICE}/{name}' failed: {e}"
        )),
    }
}

/// Test-only override for the raw keyring backend, so tests can cover
/// the encrypt/decrypt and scoping logic without touching the real OS
/// credential manager. Mirrors the `TEST_LOADER` seam above, but for
/// the name/value store rather than `KeyringRef`.
pub struct SecretBackendOverride {
    pub get: fn(&str) -> Result<Option<String>, String>,
    pub set: fn(&str, &str) -> Result<(), String>,
    pub delete: fn(&str) -> Result<(), String>,
}

static TEST_BACKEND: std::sync::OnceLock<std::sync::Mutex<Option<SecretBackendOverride>>> =
    std::sync::OnceLock::new();

fn test_backend_slot() -> &'static std::sync::Mutex<Option<SecretBackendOverride>> {
    TEST_BACKEND.get_or_init(|| std::sync::Mutex::new(None))
}

/// Install a fake backend for tests. Pass `None` to restore the real
/// keyring-backed implementation.
pub fn set_secret_backend_for_tests(backend: Option<SecretBackendOverride>) {
    if let Ok(mut g) = test_backend_slot().lock() {
        *g = backend;
    }
}

fn raw_get(name: &str) -> Result<Option<String>, String> {
    if let Ok(g) = test_backend_slot().lock() {
        if let Some(b) = g.as_ref() {
            return (b.get)(name);
        }
    }
    real_raw_get(name)
}

fn raw_set(name: &str, blob: &str) -> Result<(), String> {
    if let Ok(g) = test_backend_slot().lock() {
        if let Some(b) = g.as_ref() {
            return (b.set)(name, blob);
        }
    }
    real_raw_set(name, blob)
}

fn raw_delete(name: &str) -> Result<(), String> {
    if let Ok(g) = test_backend_slot().lock() {
        if let Some(b) = g.as_ref() {
            return (b.delete)(name);
        }
    }
    real_raw_delete(name)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use serial_test::serial;

    #[test]
    fn inline_string_resolves_directly() {
        assert_eq!(resolve(&json!("abc")).unwrap(), "abc");
    }

    #[test]
    fn null_value_errors_with_helpful_message() {
        let err = resolve(&json!(null)).unwrap_err();
        assert!(err.contains("secret value is null"), "{err}");
        assert!(err.contains("*-env"), "{err}");
    }

    #[test]
    fn number_value_errors_with_type_name() {
        let err = resolve(&json!(42)).unwrap_err();
        assert!(err.contains("unsupported JSON type number"), "{err}");
    }

    #[test]
    fn keyring_shape_round_trips() {
        // The shape must serialise to two non-empty strings.
        let v = json!({"keyring_service": "sol", "keyring_account": "google"});
        let r: KeyringRef = serde_json::from_value(v).unwrap();
        assert_eq!(r.keyring_service, "sol");
        assert_eq!(r.keyring_account, "google");
    }

    #[test]
    fn malformed_keyring_shape_errors_without_calling_loader() {
        // Single-field object — should not match the KeyringRef shape.
        let err = resolve(&json!({"keyring_service": "sol"})).unwrap_err();
        assert!(err.contains("missing field"), "{err}");
    }

    #[test]
    fn empty_keyring_fields_rejected_before_loader_runs() {
        let r = KeyringRef {
            keyring_service: "".into(),
            keyring_account: "x".into(),
        };
        let err = load_keyring(&r).unwrap_err();
        assert!(err.contains("non-empty"), "{err}");
    }

    #[test]
    #[serial(secrets_loader)]
    fn loader_test_seam_overrides_real_implementation() {
        set_secret_loader_for_tests(Some(|_| Ok("from-test-loader".into())));
        let v = json!({"keyring_service": "sol", "keyring_account": "any"});
        let r = KeyringRef {
            keyring_service: "sol".into(),
            keyring_account: "any".into(),
        };
        assert_eq!(load_keyring_with_test_override(&r).unwrap(), "from-test-loader");
        // Also covers the resolve path.
        assert_eq!(resolve(&v).unwrap(), "from-test-loader");
        // Restore.
        set_secret_loader_for_tests(None);
    }

    #[test]
    #[serial(secrets_loader)]
    fn loader_test_seam_error_surfaces_in_resolve() {
        set_secret_loader_for_tests(Some(|r| Err(format!("synthetic: {}/{}", r.keyring_service, r.keyring_account))));
        let v = json!({"keyring_service": "sol", "keyring_account": "missing"});
        let err = resolve(&v).unwrap_err();
        assert!(err.contains("synthetic: sol/missing"), "{err}");
        set_secret_loader_for_tests(None);
    }

    // ── Scoped secret store ──────────────────────────────────────────────────

    /// In-memory fake keyring for the `SecretBackendOverride` seam. Fn
    /// pointers can't capture state, so the map lives in a static.
    static FAKE_STORE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, String>>> =
        std::sync::OnceLock::new();

    fn fake_store() -> &'static std::sync::Mutex<std::collections::HashMap<String, String>> {
        FAKE_STORE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
    }

    fn install_fake_backend() {
        fake_store().lock().unwrap().clear();
        set_secret_backend_for_tests(Some(SecretBackendOverride {
            get: |name| Ok(fake_store().lock().unwrap().get(name).cloned()),
            set: |name, blob| {
                fake_store().lock().unwrap().insert(name.to_string(), blob.to_string());
                Ok(())
            },
            delete: |name| {
                fake_store().lock().unwrap().remove(name);
                Ok(())
            },
        }));
    }

    fn key_a() -> String {
        base64::engine::general_purpose::STANDARD.encode([1u8; KEY_LEN])
    }

    fn key_b() -> String {
        base64::engine::general_purpose::STANDARD.encode([2u8; KEY_LEN])
    }

    #[test]
    #[serial(secrets_backend)]
    fn set_then_get_secret_round_trips_with_the_right_key() {
        install_fake_backend();
        set_secret("MY_SECRET", "top-secret-value", &key_a()).unwrap();
        assert_eq!(
            get_secret("MY_SECRET", &key_a()).unwrap(),
            Some("top-secret-value".to_string())
        );
        set_secret_backend_for_tests(None);
    }

    #[test]
    #[serial(secrets_backend)]
    fn get_secret_with_wrong_key_fails_to_decrypt() {
        install_fake_backend();
        set_secret("MY_SECRET", "top-secret-value", &key_a()).unwrap();
        let err = get_secret("MY_SECRET", &key_b()).unwrap_err();
        assert!(err.contains("decryption failed"), "{err}");
        set_secret_backend_for_tests(None);
    }

    #[test]
    #[serial(secrets_backend)]
    fn get_secret_missing_entry_returns_none() {
        install_fake_backend();
        assert_eq!(get_secret("NOT_STORED", &key_a()).unwrap(), None);
        set_secret_backend_for_tests(None);
    }

    #[test]
    #[serial(secrets_backend)]
    fn delete_secret_removes_the_entry() {
        install_fake_backend();
        set_secret("MY_SECRET", "value", &key_a()).unwrap();
        delete_secret("MY_SECRET").unwrap();
        assert_eq!(get_secret("MY_SECRET", &key_a()).unwrap(), None);
        set_secret_backend_for_tests(None);
    }

    #[test]
    #[serial(secrets_backend)]
    fn short_key_is_rejected() {
        install_fake_backend();
        let short_key = base64::engine::general_purpose::STANDARD.encode([1u8; 16]);
        let err = set_secret("MY_SECRET", "value", &short_key).unwrap_err();
        assert!(err.contains("must decode to 32 bytes"), "{err}");
        set_secret_backend_for_tests(None);
    }
}
