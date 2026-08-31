//! Typed view of `solx-config.json`. The writer path edits the file as a raw
//! `serde_json::Value` so unknown fields survive; this struct is only used to
//! read a convenient typed snapshot.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use solx_surface::entities::{Action, ActionType};

/// A single allowed shell command for `Command`-type actions. A Command
/// action's `fn_name` is never the literal command to run — it's a key into
/// `SolxConfig.command_actions`, resolved here at execution time. See
/// `docs/next-steps.md` §1.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommandDef {
    /// The literal shell command string that actually runs.
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Working directory override. Takes priority over the action's own
    /// `action_config.cwd` when both are set — the allowlist author, not the
    /// action, decides where an approved command runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// A record of an installed package.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPackage {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub installed_at: String,
    /// `command_actions` keys this package's manifest declared and had
    /// granted into the global allowlist at install time. `#[serde(default)]`
    /// so a record from before this field existed just reads as empty —
    /// reinstalling repopulates it. `uninstall_package` uses this to know
    /// exactly what to revoke, rather than re-reading (possibly stale)
    /// `package.json` off disk. See `docs/next-steps.md` §1.
    #[serde(default)]
    pub granted_commands: Vec<String>,
    /// `allowed_webhook_base_urls` prefixes this package's manifest declared
    /// and had granted at install time. Same defaulting/rationale as
    /// `granted_commands`.
    #[serde(default)]
    pub granted_webhook_prefixes: Vec<String>,
}

/// Typed snapshot of the config. All fields are optional so a partial or
/// hand-edited file still parses; defaults are supplied by the accessors on
/// [`crate::ConfigService`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SolxConfig {
    /// Root directory for the entity databases. Defaults to `<appdata>/db`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_directory: Option<String>,
    /// Root directory for stored files. Defaults to `<appdata>/files`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_directory: Option<String>,
    /// Directory for model files. Defaults to `<appdata>/models`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_directory: Option<String>,
    /// Directory for the Tantivy search indexes. Defaults to `<appdata>/search_index`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_index_dir: Option<String>,

    /// Filename for the documents DB (within `data_directory`). Default `solx-docs.db`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs_db: Option<String>,
    /// Filename for the actions DB. Default `solx-actions.db`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actions_db: Option<String>,
    /// Filename for the types DB. Default `solx-types.db`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub types_db: Option<String>,

    /// Installed package registry.
    #[serde(default)]
    pub installed_packages: Vec<InstalledPackage>,

    /// Environment-store allowlist: guest key -> system/process env var
    /// name to read. Only keys listed here are ever visible via the
    /// `get_env` built-in action — callers never see the raw process
    /// environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_mappings: Option<HashMap<String, String>>,

    /// Persisted environment-store variables, `namespace -> key -> value`.
    /// Written by the `set_env` built-in when called with `persist: true`,
    /// and loaded back into the store at startup, so a variable survives a
    /// restart. Distinct from `env_mappings`, which is a read-only allowlist
    /// over the *system* environment and is never written here.
    ///
    /// **Plaintext.** Anything sensitive belongs in `get_secret`/`set_secret`,
    /// which are encrypted and held in the OS credential manager. Delete a
    /// variable by removing it from this map.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_vars: Option<HashMap<String, HashMap<String, String>>>,

    /// Base URL of a remote `solx-server` to proxy all manager calls to
    /// (e.g. `"http://127.0.0.1:8766"`). `None` (default) means local mode
    /// — every existing user's exact current behavior, no server needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_url: Option<String>,
    /// Shared bearer token for `server_url`. Generated once by `solx-server`
    /// on first run and persisted here, so a server and any client pointed
    /// at the same appdata dir pick it up automatically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_token: Option<String>,
    /// Port `solx-server` binds on `127.0.0.1`. Server-side only —
    /// independent of `server_url`, since the machine running `solx-server`
    /// won't normally have its own config's `server_url` set at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_port: Option<u16>,

    /// Ring-buffer cap on entries retained per action console. Oldest
    /// entries are evicted once a console exceeds this. Defaults to 5000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub console_max_entries: Option<i64>,
    /// A console whose most recent write is older than this many days is
    /// dropped entirely by the startup sweep. Defaults to 7.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub console_ttl_days: Option<i64>,

    /// Wall-clock ceiling for a detached (`action_start`) invocation with no
    /// `action_config.timeout_secs` of its own. Defaults to 86400 (24h) —
    /// deliberately much larger than `exec`'s 300s default, since a detached
    /// run is expected to outlive any one caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_timeout_secs: Option<u64>,
    /// How long `action_stop` waits for cooperative exit (the running
    /// action noticing `cancel_requested` and returning on its own) before
    /// force-aborting the task. Defaults to 10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_grace_secs: Option<u64>,
    /// A terminal invocation row older than this many days is dropped by
    /// the startup sweep. Defaults to 7.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation_ttl_days: Option<i64>,

    /// A streaming HTTP request (`http_stream/start`) whose buffer has gone
    /// this many seconds without a `http_stream/poll` self-terminates, so an
    /// abandoned generation doesn't pin a connection open forever. Defaults
    /// to 120.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_stream_idle_ttl_secs: Option<u64>,
    /// Max bytes buffered per stream before the oldest chunks are dropped to
    /// make room (counted in the poll response's `dropped`). Defaults to
    /// 8388608 (8 MiB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_stream_max_buffer_bytes: Option<u64>,

    /// Allowlist of shell commands `Command`-type actions may run, keyed by
    /// an opaque name. A Command action's `fn_name` is that key, resolved
    /// against this map — never a literal command string. **Deny-by-default**:
    /// unset or empty means no Command action can execute at all.
    ///
    /// ```json
    /// "command_actions": {
    ///   "compress-pdf": {
    ///     "command": "gs -dBATCH -dNOPAUSE -sDEVICE=pdfwrite -sOutputFile=out.pdf in.pdf",
    ///     "description": "Compress a PDF file",
    ///     "cwd": "~/scripts"
    ///   }
    /// }
    /// ```
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_actions: Option<HashMap<String, CommandDef>>,

    /// Allowlist of URL prefixes `Webhook`-type actions may POST to. A
    /// request URL must start with at least one listed prefix.
    /// **Deny-by-default**: unset or empty means no Webhook action can
    /// execute at all.
    ///
    /// ```json
    /// "allowed_webhook_base_urls": ["https://hooks.example.com"]
    /// ```
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_webhook_base_urls: Option<Vec<String>>,

    /// Denylist of action paths (and optionally specific action names at
    /// those paths) hidden from the tool catalogue a model sees. A rule's
    /// `path` is a glob pattern matched against the action's full path
    /// (leading slash); `*` matches any characters including `/`, `?`
    /// matches a single character. When `actions` is unset or empty, every
    /// action under the matched path is hidden; otherwise only the named
    /// actions are.
    ///
    /// ```json
    /// "tool_exclude": [
    ///   { "path": "*/_private/*" },
    ///   { "path": "/packages/solx-google", "actions": ["search"] }
    /// ]
    /// ```
    ///
    /// One of *three* inputs to the hidden decision — see [`ToolPolicy`],
    /// which unions this with [`SolxConfig::mcp_exclude`] and the
    /// [`CAP_HIDDEN`] tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_exclude: Option<Vec<ToolRule>>,

    /// Former name of [`SolxConfig::tool_exclude`], still read so an existing
    /// `solx-config.json` keeps working untouched. The rules are unioned, not
    /// overridden, so a file carrying both keys behaves as the sum of the two.
    ///
    /// Deliberately a second field rather than `#[serde(alias)]` on
    /// `tool_exclude`: serde rejects a struct that carries *both* names as a
    /// duplicate field, and [`crate::ConfigService::snapshot`] swallows a
    /// deserialization error into `SolxConfig::default()` — so adding the new
    /// key beside an existing old one would silently blank the entire config,
    /// command allowlists and all, rather than merging the two lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_exclude: Option<Vec<ToolRule>>,

    /// Action paths flagged as destructive: a model may call them, but a
    /// caller that implements an approval step should stop and ask first.
    /// Same rule shape as [`SolxConfig::tool_exclude`].
    ///
    /// ```json
    /// "tool_destructive": [
    ///   { "path": "/builtin/document", "actions": ["entity_delete_document"] }
    /// ]
    /// ```
    ///
    /// Also one of several inputs — see [`ToolPolicy`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_destructive: Option<Vec<ToolRule>>,
}

/// Reserved capability tag marking an action hidden from model-facing tool
/// catalogues. Namespaced so a typo is visible as an unrecognized tag rather
/// than silently unhiding the action, and so it cannot collide with a
/// descriptive capability.
pub const CAP_HIDDEN: &str = "solx:hidden";

/// Reserved capability tag marking an action destructive. See [`CAP_HIDDEN`]
/// for why it is namespaced.
pub const CAP_DESTRUCTIVE: &str = "solx:destructive";

/// A single path/name rule, used by [`SolxConfig::tool_exclude`] and
/// [`SolxConfig::tool_destructive`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolRule {
    /// Glob pattern matched against the action's full path (leading slash).
    pub path: String,
    /// When set and non-empty, only these action names at the matched path
    /// match; otherwise every action under the path does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<String>>,
}

/// Former name of [`ToolRule`]. The struct now backs `tool_exclude` and
/// `tool_destructive`, so the `McpExclude` name no longer describes it.
pub type McpExcludeRule = ToolRule;

impl ToolRule {
    /// Whether this rule matches the action at `path`/`name`.
    pub fn matches(&self, path: &str, name: &str) -> bool {
        if !glob_matches(&self.path, path) {
            return false;
        }
        match &self.actions {
            Some(names) if !names.is_empty() => names.iter().any(|n| n == name),
            _ => true,
        }
    }
}

/// Resolved hidden/destructive policy, built once from a config snapshot and
/// then applied to many actions.
///
/// Two reasons this is a struct rather than a pair of `ConfigService`
/// methods. It is the single place both decisions are made, so a caller
/// cannot consult one input and forget the other; and `ConfigService::snapshot`
/// deserializes the whole config on every call, which a per-action check
/// inside a catalogue loop would pay hundreds of times.
///
/// **Both decisions union their inputs, and nothing subtracts.** Config rules
/// are a floor that a row cannot lower: anything that can call
/// `entity_save_action` can rewrite a row's `capabilities` (the executable-
/// action guard protects Command and Webhook *rows*, not the tags on a Script
/// or Wasm one), so a tag alone would be a flag the flagged party can remove.
#[derive(Debug, Clone, Default)]
pub struct ToolPolicy {
    hidden: Vec<ToolRule>,
    destructive: Vec<ToolRule>,
}

impl ToolPolicy {
    pub fn new(hidden: Vec<ToolRule>, destructive: Vec<ToolRule>) -> Self {
        ToolPolicy { hidden, destructive }
    }

    /// Hidden = config rules ∪ the [`CAP_HIDDEN`] tag.
    ///
    /// A hidden action is omitted from the catalogue *and* refused when
    /// called by name — hiding a tool a client has already learned the name
    /// of is not, on its own, a control.
    pub fn is_hidden(&self, action: &Action) -> bool {
        has_tag(action, CAP_HIDDEN) || self.hidden.iter().any(|r| r.matches(&action.path, &action.name))
    }

    /// Destructive = config rules ∪ the [`CAP_DESTRUCTIVE`] tag ∪ every
    /// Command and Webhook row, unconditionally.
    ///
    /// That last clause is the one to keep even if the rest changes: a shell
    /// action is destructive by construction, and must never depend on
    /// someone having remembered to tag it.
    pub fn is_destructive(&self, action: &Action) -> bool {
        matches!(action.action_type, Some(ActionType::Command) | Some(ActionType::Webhook))
            || has_tag(action, CAP_DESTRUCTIVE)
            || self.destructive.iter().any(|r| r.matches(&action.path, &action.name))
    }
}

/// Prefix owned by solx for capability tags that carry meaning to the
/// runtime rather than describing what an action does.
pub const RESERVED_CAP_PREFIX: &str = "solx:";

/// Every reserved capability tag the runtime understands.
pub const RESERVED_CAPS: &[&str] = &[CAP_HIDDEN, CAP_DESTRUCTIVE];

/// Reject a `solx:`-prefixed capability that isn't one the runtime knows.
///
/// This is what makes the namespace worth having. Without it, `solx:destructiv`
/// is merely an unrecognized tag and the action silently stays ungated — the
/// exact failure a typo in a bare `destructive` would cause. Unprefixed tags
/// are the free-form descriptive vocabulary and are never checked.
pub fn validate_capabilities(capabilities: &[String]) -> Result<(), String> {
    for cap in capabilities {
        if cap.starts_with(RESERVED_CAP_PREFIX) && !RESERVED_CAPS.contains(&cap.as_str()) {
            return Err(format!(
                "unknown reserved capability '{cap}': the '{RESERVED_CAP_PREFIX}' prefix is \
                 reserved, and the recognized tags are {}",
                RESERVED_CAPS.join(", ")
            ));
        }
    }
    Ok(())
}

/// Exact match on one capability entry.
///
/// Deliberately not the SQL `LIKE` filter `capabilities` supports: a
/// substring test would make `non-destructive` match `solx:destructive`.
fn has_tag(action: &Action, tag: &str) -> bool {
    action.capabilities.iter().any(|c| c == tag)
}

/// Match a glob pattern against a path. `*` matches any sequence of
/// characters (including `/`), `?` matches any single character; every other
/// character matches literally. No external glob/regex dependency — this is
/// the classic two-pointer glob algorithm.
pub fn glob_matches(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_p, mut star_t) = (usize::MAX, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_p = pi;
            star_t = ti;
            pi += 1;
        } else if star_p != usize::MAX {
            pi = star_p + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_star_matches_across_slashes() {
        assert!(glob_matches("*/_private/*", "/a/_private/b"));
        assert!(glob_matches("*/_private/*", "/_private/b"));
        assert!(glob_matches("*/_private/*", "/x/y/_private/z"));
        assert!(!glob_matches("*/_private/*", "/a/internal/b"));
        assert!(!glob_matches("*/_private/*", "/a/_private"));
    }

    #[test]
    fn glob_question_matches_single_char() {
        assert!(glob_matches("/packages/solx-?oogle", "/packages/solx-google"));
        assert!(!glob_matches("/packages/solx-?oogle", "/packages/solx-gooogle"));
    }

    #[test]
    fn glob_literal_and_anchoring() {
        assert!(glob_matches("/builtin", "/builtin"));
        assert!(!glob_matches("/builtin", "/builtin/console"));
        assert!(glob_matches("/builtin/*", "/builtin/console"));
        assert!(glob_matches("*", "/anything/at/all"));
    }

    /// Built through serde rather than a struct literal: `Action` has no
    /// `Default`, and this way the helper survives the struct gaining fields.
    fn action(path: &str, name: &str, caps: &[&str], ty: Option<ActionType>) -> Action {
        serde_json::from_value(serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000000",
            "path": path,
            "name": name,
            "capabilities": caps,
            "actionType": ty,
        }))
        .expect("test action must deserialize")
    }

    #[test]
    fn tag_hides_an_action_no_config_rule_covers() {
        let policy = ToolPolicy::default();
        assert!(policy.is_hidden(&action("/packages/x", "y", &[CAP_HIDDEN], None)));
        assert!(!policy.is_hidden(&action("/packages/x", "y", &["hidden"], None)));
    }

    #[test]
    fn config_rule_hides_an_untagged_action() {
        let policy = ToolPolicy::new(vec![ToolRule { path: "/packages/x".into(), actions: None }], vec![]);
        assert!(policy.is_hidden(&action("/packages/x", "y", &[], None)));
        assert!(!policy.is_hidden(&action("/packages/z", "y", &[], None)));
    }

    #[test]
    fn removing_the_tag_cannot_unhide_what_config_hid() {
        // The whole reason config is a floor rather than the only input:
        // a row's capabilities are writable by anything that can call
        // entity_save_action.
        let policy = ToolPolicy::new(vec![ToolRule { path: "/packages/x".into(), actions: None }], vec![]);
        assert!(policy.is_hidden(&action("/packages/x", "y", &["totally-safe"], None)));
    }

    #[test]
    fn command_and_webhook_are_destructive_untagged() {
        let policy = ToolPolicy::default();
        assert!(policy.is_destructive(&action("/p", "a", &[], Some(ActionType::Command))));
        assert!(policy.is_destructive(&action("/p", "a", &[], Some(ActionType::Webhook))));
        assert!(!policy.is_destructive(&action("/p", "a", &[], Some(ActionType::Wasm))));
        assert!(!policy.is_destructive(&action("/p", "a", &[], None)));
    }

    #[test]
    fn destructive_unions_tag_and_config() {
        let policy = ToolPolicy::new(
            vec![],
            vec![ToolRule { path: "/builtin/document".into(), actions: Some(vec!["entity_delete_document".into()]) }],
        );
        assert!(policy.is_destructive(&action("/builtin/document", "entity_delete_document", &[], Some(ActionType::Internal))));
        assert!(!policy.is_destructive(&action("/builtin/document", "entity_get_document", &[], Some(ActionType::Internal))));
        assert!(policy.is_destructive(&action("/packages/x", "wipe", &[CAP_DESTRUCTIVE], Some(ActionType::Wasm))));
    }

    #[test]
    fn reserved_prefix_rejects_a_typo_but_allows_free_form_tags() {
        assert!(validate_capabilities(&["document".into(), CAP_HIDDEN.into()]).is_ok());
        // The whole point of the namespace: this is an error, not a silently
        // ungated action.
        let err = validate_capabilities(&["solx:destructiv".into()]).unwrap_err();
        assert!(err.contains("solx:destructiv"), "{err}");
        assert!(err.contains(CAP_DESTRUCTIVE), "{err}");
        // Unprefixed tags are free-form and never checked.
        assert!(validate_capabilities(&["destructive".into(), "hidden".into()]).is_ok());
    }

    #[test]
    fn a_tag_is_matched_exactly_not_as_a_substring() {
        let policy = ToolPolicy::default();
        assert!(!policy.is_destructive(&action("/p", "a", &["non-solx:destructive"], Some(ActionType::Wasm))));
        assert!(!policy.is_hidden(&action("/p", "a", &["solx:hidden-ish"], None)));
    }

    #[test]
    fn hidden_and_destructive_are_independent() {
        let policy = ToolPolicy::default();
        let a = action("/p", "a", &[CAP_DESTRUCTIVE], Some(ActionType::Wasm));
        assert!(policy.is_destructive(&a));
        assert!(!policy.is_hidden(&a), "destructive must stay callable, just gated");
    }

    #[test]
    fn rule_path_only_excludes_everything_under_path() {
        let rule = ToolRule { path: "*/_private/*".into(), actions: None };
        assert!(rule.matches("/a/_private/b", "anything"));
        assert!(!rule.matches("/a/public/b", "anything"));
    }

    #[test]
    fn rule_with_actions_excludes_only_named() {
        let rule = ToolRule {
            path: "/packages/solx-google".into(),
            actions: Some(vec!["search".into()]),
        };
        assert!(rule.matches("/packages/solx-google", "search"));
        assert!(!rule.matches("/packages/solx-google", "list"));
        assert!(!rule.matches("/packages/solx-other", "search"));
    }

    #[test]
    fn rule_with_empty_actions_behaves_like_path_only() {
        let rule = ToolRule { path: "/x".into(), actions: Some(vec![]) };
        assert!(rule.matches("/x", "anything"));
    }
}
