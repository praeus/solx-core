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

    /// A widget that is opened but never has its frontend connect within
    /// this many seconds is reaped. Defaults to 60.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widget_connect_ttl_secs: Option<u64>,
    /// A widget whose websocket disconnects is kept alive this many seconds
    /// to allow the frontend to reconnect (e.g. a page reload) before being
    /// reaped. Defaults to 15.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widget_reconnect_grace_secs: Option<u64>,

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
}
