//! Typed view of `solx-config.json`. The writer path edits the file as a raw
//! `serde_json::Value` so unknown fields survive; this struct is only used to
//! read a convenient typed snapshot.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

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
    /// those paths) that the MCP server hides from its tool catalogue. A
    /// rule's `path` is a glob pattern matched against the action's full
    /// path (leading slash); `*` matches any characters including `/`, `?`
    /// matches a single character. When `actions` is unset or empty, every
    /// action under the matched path is excluded; otherwise only the named
    /// actions are.
    ///
    /// ```json
    /// "mcp_exclude": [
    ///   { "path": "*/_internal/*" },
    ///   { "path": "/packages/solx-google", "actions": ["search"] }
    /// ]
    /// ```
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_exclude: Option<Vec<McpExcludeRule>>,
}

/// A single MCP tool-catalogue exclusion rule. See
/// [`SolxConfig::mcp_exclude`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpExcludeRule {
    /// Glob pattern matched against the action's full path (leading slash).
    pub path: String,
    /// When set and non-empty, only these action names at the matched path
    /// are excluded; otherwise every action under the path is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<String>>,
}

impl McpExcludeRule {
    /// Whether this rule excludes the action at `path`/`name`.
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
        assert!(glob_matches("*/_internal/*", "/a/_internal/b"));
        assert!(glob_matches("*/_internal/*", "/_internal/b"));
        assert!(glob_matches("*/_internal/*", "/x/y/_internal/z"));
        assert!(!glob_matches("*/_internal/*", "/a/internal/b"));
        assert!(!glob_matches("*/_internal/*", "/a/_internal"));
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

    #[test]
    fn rule_path_only_excludes_everything_under_path() {
        let rule = McpExcludeRule { path: "*/_internal/*".into(), actions: None };
        assert!(rule.matches("/a/_internal/b", "anything"));
        assert!(!rule.matches("/a/public/b", "anything"));
    }

    #[test]
    fn rule_with_actions_excludes_only_named() {
        let rule = McpExcludeRule {
            path: "/packages/solx-google".into(),
            actions: Some(vec!["search".into()]),
        };
        assert!(rule.matches("/packages/solx-google", "search"));
        assert!(!rule.matches("/packages/solx-google", "list"));
        assert!(!rule.matches("/packages/solx-other", "search"));
    }

    #[test]
    fn rule_with_empty_actions_behaves_like_path_only() {
        let rule = McpExcludeRule { path: "/x".into(), actions: Some(vec![]) };
        assert!(rule.matches("/x", "anything"));
    }
}
