//! Typed view of `solx-config.json`. The writer path edits the file as a raw
//! `serde_json::Value` so unknown fields survive; this struct is only used to
//! read a convenient typed snapshot.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

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
}
