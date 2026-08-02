//! Redaction of secret material in `action_config` on the way out, and
//! restoration of it on the way back in.
//!
//! `action_config` holds two kinds of credential: the base64 AES-256 keys
//! under `secrets` (see [`crate::secrets`]) and the inline OAuth/bearer
//! material under `auth`. Both used to be returned verbatim by `get`/`list`,
//! and therefore over HTTP and to any MCP client.
//!
//! ## The two halves must stay symmetric
//!
//! [`mask_action_config`] replaces protected **string leaves** with
//! [`MASK`]; [`unmask_merge`] walks an incoming config and restores the
//! stored value wherever it finds that same sentinel. That symmetry is what
//! makes the natural `get` → edit → `post` round trip safe: an agent can
//! change `cwd` and write the whole config back without silently destroying
//! the keys it was never shown. Neither side needs a table of protected
//! paths — masking decides what to hide, unmasking just restores whatever
//! comes back marked.
//!
//! Execution never sees a masked config: `LocalActionManager::exec_as` reads
//! through `get_unmasked`, and `post` merges against the raw stored row.

use serde_json::{Map, Value};

/// Sentinel written in place of a redacted value.
pub const MASK: &str = "***";

/// Keys under `action_config.auth` whose string values are *not* secret and
/// stay readable — endpoint/flow metadata, plus the
/// `keyring_service`/`keyring_account` pointer pair. Hiding these would make
/// `solx get action …` useless for checking how an action is wired without
/// protecting anything.
///
/// Everything under `auth` that isn't on this list is masked. That is
/// deliberately allowlist-shaped: a new credential field added to `auth`
/// later is redacted by default rather than leaking until someone remembers
/// to list it.
const AUTH_VISIBLE: &[&str] = &[
    "type",
    "auth_url",
    "token_url",
    "redirect_uri",
    "scope",
    "keyring_service",
    "keyring_account",
];

fn auth_key_is_visible(key: &str) -> bool {
    // `*_env` holds the *name* of an environment variable, never a value.
    //
    // There is no matching exemption for the `*_secret` pointer convention
    // (`client_id_secret` names a scoped secret rather than holding one),
    // because the suffix cannot distinguish it from `client_secret` — which
    // ends the same way and *is* the credential. Masking both is the safe
    // side of that ambiguity; the cost is only that you can't read back
    // which named secret an action points at.
    AUTH_VISIBLE.contains(&key) || key.ends_with("_env")
}

/// Redact secret material in an `action_config` for outbound reads.
///
/// * every string under `secrets` — those are always AES keys
/// * every string under `auth` except the pointer/metadata keys in
///   [`AUTH_VISIBLE`]
///
/// Anything else (`cwd`, `headers`, custom fields) is left alone.
pub fn mask_action_config(cfg: &mut Value) {
    let Some(obj) = cfg.as_object_mut() else { return };
    if let Some(secrets) = obj.get_mut("secrets") {
        mask_all_strings(secrets);
    }
    if let Some(auth) = obj.get_mut("auth") {
        mask_auth(auth);
    }
}

/// Convenience for the `Option<Value>` shape [`solx_surface::entities::Action`]
/// actually carries.
pub fn mask_action_config_opt(cfg: &mut Option<Value>) {
    if let Some(v) = cfg {
        mask_action_config(v);
    }
}

fn mask_all_strings(v: &mut Value) {
    match v {
        Value::String(s) => *s = MASK.to_string(),
        Value::Object(map) => map.values_mut().for_each(mask_all_strings),
        Value::Array(items) => items.iter_mut().for_each(mask_all_strings),
        _ => {}
    }
}

fn mask_auth(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                if auth_key_is_visible(k) {
                    continue;
                }
                match val {
                    Value::String(s) => *s = MASK.to_string(),
                    other => mask_auth(other),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(mask_auth),
        _ => {}
    }
}

/// Merge an incoming `action_config` over the stored one, restoring any
/// value the caller echoed back as [`MASK`].
///
/// Only the sentinel is special: every other leaf replaces the stored value,
/// so explicitly removing a key still removes it.
///
/// Returns `Err` when the sentinel appears at a path that has no stored
/// value — that means the caller typed `"***"` themselves rather than
/// echoing back a redacted read, and persisting it literally would surface
/// much later as a confusing decrypt failure.
pub fn unmask_merge(incoming: Value, existing: Option<&Value>) -> Result<Value, String> {
    let mut path = Vec::new();
    walk(incoming, existing, &mut path)
}

fn walk(incoming: Value, existing: Option<&Value>, path: &mut Vec<String>) -> Result<Value, String> {
    match incoming {
        Value::String(ref s) if s == MASK => existing.cloned().ok_or_else(|| {
            format!(
                "action_config{} is \"{MASK}\" but there is no stored value to restore; \
                 reads are redacted, so set this field explicitly",
                render_path(path)
            )
        }),
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (k, v) in map {
                let nested = existing.and_then(|e| e.get(&k));
                path.push(format!(".{k}"));
                let merged = walk(v, nested, path);
                path.pop();
                out.insert(k, merged?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, v) in items.into_iter().enumerate() {
                let nested = existing.and_then(|e| e.get(i));
                path.push(format!("[{i}]"));
                let merged = walk(v, nested, path);
                path.pop();
                out.push(merged?);
            }
            Ok(Value::Array(out))
        }
        other => Ok(other),
    }
}

fn render_path(path: &[String]) -> String {
    path.concat()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn masks_every_secret_key() {
        let mut cfg = json!({
            "cwd": "/tmp/work",
            "secrets": { "API_TOKEN": "aGVsbG8=", "OTHER": "d29ybGQ=" }
        });
        mask_action_config(&mut cfg);
        assert_eq!(cfg["secrets"]["API_TOKEN"], json!(MASK));
        assert_eq!(cfg["secrets"]["OTHER"], json!(MASK));
        // Non-secret fields are untouched.
        assert_eq!(cfg["cwd"], json!("/tmp/work"));
    }

    #[test]
    fn masks_auth_values_but_keeps_pointers_readable() {
        let mut cfg = json!({
            "auth": {
                "type": "oauth_refresh",
                "client_id": "1234.apps.example.com",
                "client_secret": "super-secret",
                "refresh_token": "rt-abc",
                "scope": "read write",
                "client_secret_secret": "MY_SECRET_NAME",
                "token_env": "TOKEN_VAR",
                "private_key": { "keyring_service": "sol", "keyring_account": "google" }
            }
        });
        mask_action_config(&mut cfg);
        let auth = &cfg["auth"];
        assert_eq!(auth["type"], json!("oauth_refresh"));
        assert_eq!(auth["scope"], json!("read write"));
        assert_eq!(auth["token_env"], json!("TOKEN_VAR"));
        assert_eq!(auth["private_key"]["keyring_service"], json!("sol"));
        assert_eq!(auth["private_key"]["keyring_account"], json!("google"));
        // Actual credentials are gone.
        assert_eq!(auth["client_id"], json!(MASK));
        assert_eq!(auth["client_secret"], json!(MASK));
        assert_eq!(auth["refresh_token"], json!(MASK));
    }

    /// `client_secret` is a credential and `client_id_secret` is a pointer,
    /// yet both end in `_secret` — so the suffix can't be used to exempt
    /// pointers. Both are masked; the alternative leaked `client_secret`.
    #[test]
    fn secret_suffix_is_not_treated_as_a_pointer_exemption() {
        let mut cfg = json!({
            "auth": { "client_secret": "leak-me", "client_id_secret": "A_NAME" }
        });
        mask_action_config(&mut cfg);
        assert_eq!(cfg["auth"]["client_secret"], json!(MASK));
        assert_eq!(cfg["auth"]["client_id_secret"], json!(MASK));
    }

    #[test]
    fn non_object_config_is_left_alone() {
        let mut cfg = json!("just a string");
        mask_action_config(&mut cfg);
        assert_eq!(cfg, json!("just a string"));
    }

    /// The round trip this whole module exists for: read a masked config,
    /// change something unrelated, write it back, keep the real keys.
    #[test]
    fn round_trip_preserves_secrets() {
        let stored = json!({
            "cwd": "/old",
            "secrets": { "API_TOKEN": "aGVsbG8=" },
            "auth": { "type": "bearer", "token": "real-token" }
        });

        let mut fetched = stored.clone();
        mask_action_config(&mut fetched);
        // Caller edits only the non-secret field and posts the whole thing back.
        fetched["cwd"] = json!("/new");

        let merged = unmask_merge(fetched, Some(&stored)).unwrap();
        assert_eq!(merged["cwd"], json!("/new"));
        assert_eq!(merged["secrets"]["API_TOKEN"], json!("aGVsbG8="));
        assert_eq!(merged["auth"]["token"], json!("real-token"));
    }

    #[test]
    fn explicit_new_value_overwrites_stored() {
        let stored = json!({ "secrets": { "API_TOKEN": "old-key" } });
        let incoming = json!({ "secrets": { "API_TOKEN": "new-key" } });
        let merged = unmask_merge(incoming, Some(&stored)).unwrap();
        assert_eq!(merged["secrets"]["API_TOKEN"], json!("new-key"));
    }

    #[test]
    fn removing_a_key_still_removes_it() {
        let stored = json!({ "cwd": "/old", "secrets": { "A": "k" } });
        let incoming = json!({ "secrets": { "A": MASK } });
        let merged = unmask_merge(incoming, Some(&stored)).unwrap();
        assert!(merged.get("cwd").is_none(), "dropped key should not be resurrected");
        assert_eq!(merged["secrets"]["A"], json!("k"));
    }

    #[test]
    fn sentinel_without_stored_value_errors() {
        let stored = json!({ "secrets": { "A": "k" } });
        let incoming = json!({ "secrets": { "B": MASK } });
        let err = unmask_merge(incoming, Some(&stored)).unwrap_err();
        assert!(err.contains(".secrets.B"), "{err}");
        assert!(err.contains("no stored value to restore"), "{err}");
    }

    #[test]
    fn sentinel_with_no_existing_config_at_all_errors() {
        let incoming = json!({ "secrets": { "A": MASK } });
        let err = unmask_merge(incoming, None).unwrap_err();
        assert!(err.contains(".secrets.A"), "{err}");
    }

    #[test]
    fn restores_inside_arrays() {
        let stored = json!({ "list": ["a", "b"] });
        let incoming = json!({ "list": [MASK, "c"] });
        let merged = unmask_merge(incoming, Some(&stored)).unwrap();
        assert_eq!(merged["list"], json!(["a", "c"]));
    }
}
