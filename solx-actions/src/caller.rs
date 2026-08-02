//! Identity of the action that initiated an execution.
//!
//! Only one caller exists today: an action invoking another action. That
//! happens in exactly one place — a WASM guest calling `action-exec`, which
//! is the sole re-entrant path into execution (internal actions do entity
//! CRUD only, never `exec`). Everything else — the CLI, the MCP server, the
//! HTTP route, `solx-client` — enters through `ActionManager::exec` and has
//! no action caller, i.e. `None`.
//!
//! That containment is deliberate. A `Caller` is built by the host from a
//! row it just read, never parsed from a request, never serialized, and
//! never crosses the process boundary — so unlike a client-declared
//! identity it cannot be spoofed.
//!
//! ## What it carries, and what it must not
//!
//! Only the calling action's `action_config.secrets` map, behind a private
//! field. The caller's `cwd`, `auth`, and `headers` are never in scope, so
//! there is no way for a downstream handler to reach them even by accident.
//!
//! **Invariant:** a `Caller` is read-only input to secret-key resolution.
//! No internal action may return it, or any key inside it, in its result —
//! `get_secret` returns the decrypted *value*, never the key that unlocked
//! it. See `crate::internal::InternalCtx`.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value;

/// The action that invoked the action currently executing.
#[derive(Debug, Clone)]
pub struct Caller {
    action_ref: String,
    /// `action_config.secrets` of the calling action: secret name -> base64
    /// AES key. Typed as a string map rather than a `Value` so it is a
    /// type-level fact that nothing else can ride along.
    secrets: BTreeMap<String, String>,
}

impl Caller {
    /// Build a caller frame from an action row. `action_config` is the raw
    /// (unmasked) config — `exec_as` reads rows through `get_unmasked`.
    pub fn from_action(action_ref: impl Into<String>, action_config: Option<&Value>) -> Self {
        let secrets = action_config
            .and_then(|c| c.get("secrets"))
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        Caller {
            action_ref: action_ref.into(),
            secrets,
        }
    }

    /// Full reference of the calling action, e.g. `/pkg/summarize`.
    pub fn action_ref(&self) -> &str {
        &self.action_ref
    }

    /// The AES key this action has configured for `name`, if any.
    pub fn secret_key(&self, name: &str) -> Option<&str> {
        self.secrets.get(name).map(String::as_str)
    }
}

/// URI form, for logs and error messages: `action://pkg/summarize`.
impl fmt::Display for Caller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "action://{}", self.action_ref.trim_start_matches('/'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_secret_keys() {
        let cfg = json!({
            "cwd": "/tmp",
            "secrets": { "API_TOKEN": "a2V5", "OTHER": "b3RoZXI=" }
        });
        let c = Caller::from_action("/pkg/foo", Some(&cfg));
        assert_eq!(c.secret_key("API_TOKEN"), Some("a2V5"));
        assert_eq!(c.secret_key("OTHER"), Some("b3RoZXI="));
        assert_eq!(c.secret_key("MISSING"), None);
    }

    #[test]
    fn tolerates_missing_or_malformed_secrets() {
        assert_eq!(Caller::from_action("/a/b", None).secret_key("x"), None);
        let no_secrets = json!({ "cwd": "/tmp" });
        assert_eq!(Caller::from_action("/a/b", Some(&no_secrets)).secret_key("x"), None);
        // Non-string values are skipped rather than panicking.
        let odd = json!({ "secrets": { "x": 42, "y": "b2s=" } });
        let c = Caller::from_action("/a/b", Some(&odd));
        assert_eq!(c.secret_key("x"), None);
        assert_eq!(c.secret_key("y"), Some("b2s="));
    }

    #[test]
    fn renders_as_a_uri() {
        assert_eq!(Caller::from_action("/pkg/foo", None).to_string(), "action://pkg/foo");
        assert_eq!(Caller::from_action("/foo", None).to_string(), "action://foo");
    }
}
