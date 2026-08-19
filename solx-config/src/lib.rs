//! `solx-config` — the config service backing `solx-config.json`.
//!
//! Concurrency model (the explicit cross-process requirement):
//! * **Reads** are lock-free. The in-memory cache is refreshed by comparing the
//!   file mtime; if another process wrote the file, the next read reloads it.
//! * **Writes** take an OS advisory exclusive lock (`fs2`) over the config file
//!   for the whole read-modify-write, serializing writers across processes.
//! * The write path edits the file as a raw [`serde_json::Value`] so unknown
//!   fields written by other tools/versions survive.

mod types;

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::SystemTime;

use base64::Engine as _;
use fs2::FileExt;
use serde_json::{Map, Value};
use solx_surface::error::{Result, SolxError};

pub use types::{CommandDef, InstalledPackage, SolxConfig};

const CONFIG_FILE: &str = "solx-config.json";

/// Default port `solx-server` binds on `127.0.0.1` when `server_port` isn't
/// configured. Distinct from the OAuth loopback's own default port (8765,
/// see `solx-actions::oauth_loopback::DEFAULT_LOOPBACK_PORT`).
pub const DEFAULT_SERVER_PORT: u16 = 8766;

/// Resolve the solx appdata directory:
/// `SOLX_APPDATA_DIR` env override → `%APPDATA%/praeus/solx` (Windows) →
/// `$HOME/.praeus/solx` → temp dir fallback.
pub fn appdata_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("SOLX_APPDATA_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            if !appdata.trim().is_empty() {
                return PathBuf::from(appdata).join("praeus").join("solx");
            }
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home).join(".praeus").join("solx");
        }
    }
    std::env::temp_dir().join("praeus").join("solx")
}

struct Cached {
    value: Value,
    mtime: Option<SystemTime>,
}

/// The config service. Cheap to share via `Arc`; holds an mtime-guarded cache.
pub struct ConfigService {
    appdata: PathBuf,
    path: PathBuf,
    cache: RwLock<Cached>,
}

impl ConfigService {
    /// Open (or lazily create) the config service rooted at the default appdata
    /// directory.
    pub fn open() -> Result<Self> {
        Self::open_in(appdata_dir())
    }

    /// Open the config service with an explicit appdata directory (used in tests).
    pub fn open_in(appdata: impl Into<PathBuf>) -> Result<Self> {
        let appdata = appdata.into();
        fs::create_dir_all(&appdata)?;
        let path = appdata.join(CONFIG_FILE);
        let (value, mtime) = read_file(&path)?;
        Ok(ConfigService {
            appdata,
            path,
            cache: RwLock::new(Cached { value, mtime }),
        })
    }

    /// The appdata directory this service is rooted at.
    pub fn appdata(&self) -> &Path {
        &self.appdata
    }

    /// Path to the config file.
    pub fn config_path(&self) -> &Path {
        &self.path
    }

    /// Current file mtime on disk (if the file exists).
    fn disk_mtime(&self) -> Option<SystemTime> {
        fs::metadata(&self.path).ok().and_then(|m| m.modified().ok())
    }

    /// Return a clone of the current config value, reloading from disk first if
    /// another process changed it since we last read.
    pub fn raw_snapshot(&self) -> Value {
        let disk_mtime = self.disk_mtime();
        {
            let cache = self.cache.read().unwrap();
            if cache.mtime == disk_mtime {
                return cache.value.clone();
            }
        }
        // Stale — reload under a write lock.
        let mut cache = self.cache.write().unwrap();
        if let Ok((value, mtime)) = read_file(&self.path) {
            cache.value = value;
            cache.mtime = mtime;
        }
        cache.value.clone()
    }

    /// Typed snapshot of the config.
    pub fn snapshot(&self) -> SolxConfig {
        serde_json::from_value(self.raw_snapshot()).unwrap_or_default()
    }

    /// Read a top-level key.
    pub fn get(&self, key: &str) -> Option<Value> {
        self.raw_snapshot().get(key).cloned()
    }

    /// Read-modify-write the config under a cross-process exclusive lock. The
    /// closure receives the authoritative on-disk object (fresh under the lock)
    /// as a mutable [`serde_json::Map`], preserving any unknown keys.
    pub fn mutate<F>(&self, f: F) -> Result<()>
    where
        F: FnOnce(&mut Map<String, Value>) -> Result<()>,
    {
        // Hold the cache write lock for the whole RMW so in-process writers are
        // serialized too and the cache stays consistent with the file.
        let mut cache = self.cache.write().unwrap();

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&self.path)?;
        file.lock_exclusive()
            .map_err(|e| SolxError::Config(format!("lock config file: {e}")))?;

        let result = (|| -> Result<Value> {
            // Read authoritative current contents under the lock.
            let mut buf = String::new();
            file.seek(SeekFrom::Start(0))?;
            file.read_to_string(&mut buf)?;
            let mut obj = if buf.trim().is_empty() {
                Map::new()
            } else {
                match serde_json::from_str::<Value>(&buf)? {
                    Value::Object(m) => m,
                    other => {
                        return Err(SolxError::Config(format!(
                            "config root must be a JSON object, found {}",
                            kind_of(&other)
                        )));
                    }
                }
            };

            f(&mut obj)?;

            let value = Value::Object(obj);
            let serialized = serde_json::to_string_pretty(&value)?;
            file.seek(SeekFrom::Start(0))?;
            file.set_len(0)?;
            file.write_all(serialized.as_bytes())?;
            file.flush()?;
            Ok(value)
        })();

        // Best-effort unlock (also released on drop).
        let _ = FileExt::unlock(&file);

        let value = result?;
        cache.value = value;
        cache.mtime = file.metadata().ok().and_then(|m| m.modified().ok());
        Ok(())
    }

    /// Set a single top-level key.
    pub fn set(&self, key: &str, value: Value) -> Result<()> {
        self.mutate(|obj| {
            obj.insert(key.to_string(), value);
            Ok(())
        })
    }

    /// Shallow-merge a JSON object into the top level (overwriting on collision).
    pub fn patch(&self, patch: Value) -> Result<()> {
        let patch = match patch {
            Value::Object(m) => m,
            other => {
                return Err(SolxError::Config(format!(
                    "patch must be a JSON object, found {}",
                    kind_of(&other)
                )));
            }
        };
        self.mutate(|obj| {
            for (k, v) in patch {
                obj.insert(k, v);
            }
            Ok(())
        })
    }

    // ── Derived path accessors ───────────────────────────────────────────────

    pub fn db_dir(&self) -> PathBuf {
        match self.snapshot().data_directory {
            Some(d) if !d.trim().is_empty() => PathBuf::from(d),
            _ => self.appdata.join("db"),
        }
    }

    pub fn docs_db_path(&self) -> PathBuf {
        self.db_dir()
            .join(self.snapshot().docs_db.unwrap_or_else(|| "solx-docs.db".into()))
    }

    pub fn actions_db_path(&self) -> PathBuf {
        self.db_dir()
            .join(self.snapshot().actions_db.unwrap_or_else(|| "solx-actions.db".into()))
    }

    pub fn types_db_path(&self) -> PathBuf {
        self.db_dir()
            .join(self.snapshot().types_db.unwrap_or_else(|| "solx-types.db".into()))
    }

    pub fn files_dir(&self) -> PathBuf {
        match self.snapshot().files_directory {
            Some(d) if !d.trim().is_empty() => PathBuf::from(d),
            _ => self.appdata.join("files"),
        }
    }

    pub fn search_index_dir(&self) -> PathBuf {
        match self.snapshot().search_index_dir {
            Some(d) if !d.trim().is_empty() => PathBuf::from(d),
            _ => self.appdata.join("search_index"),
        }
    }

    /// Ring-buffer cap on entries retained per action console. Defaults to
    /// 5000 when unset or non-positive.
    pub fn console_max_entries(&self) -> i64 {
        match self.snapshot().console_max_entries {
            Some(n) if n > 0 => n,
            _ => 5000,
        }
    }

    /// TTL (in days) before an idle console is swept away entirely. Defaults
    /// to 7 when unset or non-positive.
    pub fn console_ttl_days(&self) -> i64 {
        match self.snapshot().console_ttl_days {
            Some(n) if n > 0 => n,
            _ => 7,
        }
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.appdata.join("logs")
    }

    /// Wall-clock ceiling for a detached invocation with no
    /// `action_config.timeout_secs` of its own. Defaults to 86400 (24h)
    /// when unset or non-positive.
    pub fn background_timeout_secs(&self) -> u64 {
        match self.snapshot().background_timeout_secs {
            Some(n) if n > 0 => n,
            _ => 86400,
        }
    }

    /// Grace period `action_stop` waits for cooperative exit before
    /// force-aborting. Defaults to 10 when unset or non-positive.
    pub fn stop_grace_secs(&self) -> u64 {
        match self.snapshot().stop_grace_secs {
            Some(n) if n > 0 => n,
            _ => 10,
        }
    }

    /// TTL (in days) before a terminal invocation row is swept away.
    /// Defaults to 7 when unset or non-positive.
    pub fn invocation_ttl_days(&self) -> i64 {
        match self.snapshot().invocation_ttl_days {
            Some(n) if n > 0 => n,
            _ => 7,
        }
    }

    /// Idle TTL (in seconds) before an unpolled http_stream self-terminates.
    /// Defaults to 120 when unset or non-positive.
    pub fn http_stream_idle_ttl_secs(&self) -> u64 {
        match self.snapshot().http_stream_idle_ttl_secs {
            Some(n) if n > 0 => n,
            _ => 120,
        }
    }

    /// Max bytes buffered per http_stream before oldest chunks are dropped.
    /// Defaults to 8 MiB when unset or non-positive.
    pub fn http_stream_max_buffer_bytes(&self) -> u64 {
        match self.snapshot().http_stream_max_buffer_bytes {
            Some(n) if n > 0 => n,
            _ => 8 * 1024 * 1024,
        }
    }

    /// Connect window (in seconds) before an opened-but-never-connected
    /// widget is reaped. Defaults to 60 when unset or non-positive.
    pub fn widget_connect_ttl_secs(&self) -> u64 {
        match self.snapshot().widget_connect_ttl_secs {
            Some(n) if n > 0 => n,
            _ => 60,
        }
    }

    /// Grace period (in seconds) a widget survives after its websocket
    /// disconnects, before being reaped. Defaults to 15 when unset or
    /// non-positive.
    pub fn widget_reconnect_grace_secs(&self) -> u64 {
        match self.snapshot().widget_reconnect_grace_secs {
            Some(n) if n > 0 => n,
            _ => 15,
        }
    }

    // ── Package registry ──────────────────────────────────────────────────────

    pub fn list_packages(&self) -> Vec<InstalledPackage> {
        self.snapshot().installed_packages
    }

    pub fn register_package(&self, pkg: InstalledPackage) -> Result<()> {
        self.mutate(|obj| {
            let mut list: Vec<InstalledPackage> = obj
                .get("installed_packages")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            list.retain(|p| p.name != pkg.name);
            list.push(pkg);
            obj.insert("installed_packages".into(), serde_json::to_value(list)?);
            Ok(())
        })
    }

    pub fn unregister_package(&self, name: &str) -> Result<()> {
        self.mutate(|obj| {
            let mut list: Vec<InstalledPackage> = obj
                .get("installed_packages")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            list.retain(|p| p.name != name);
            obj.insert("installed_packages".into(), serde_json::to_value(list)?);
            Ok(())
        })
    }

    // ── persisted environment variables ──────────────────────────────────────

    /// Every persisted variable, `namespace -> key -> value`. Loaded into the
    /// in-process environment store at startup.
    pub fn env_vars(&self) -> HashMap<String, HashMap<String, String>> {
        self.snapshot().env_vars.unwrap_or_default()
    }

    /// Persist `value` under `namespace`/`key`, creating the namespace if
    /// needed. Goes through the same cross-process `mutate()` lock as every
    /// other config write, so concurrent `set_env` calls can't lose an entry
    /// by read-modify-writing a stale snapshot.
    pub fn set_env_var(&self, namespace: &str, key: &str, value: &str) -> Result<()> {
        self.mutate(|obj| {
            let mut all: HashMap<String, HashMap<String, String>> = obj
                .get("env_vars")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            all.entry(namespace.to_string())
                .or_default()
                .insert(key.to_string(), value.to_string());
            obj.insert("env_vars".into(), serde_json::to_value(all)?);
            Ok(())
        })
    }

    // ── Command / Webhook allowlists ─────────────────────────────────────────

    /// The full `command_actions` allowlist: opaque key -> command
    /// definition. A `Command` action's `fn_name` is looked up here, never
    /// run as a literal shell string. Deny-by-default: unset or empty means
    /// no key resolves.
    pub fn command_actions(&self) -> HashMap<String, CommandDef> {
        self.snapshot().command_actions.unwrap_or_default()
    }

    /// Register (or replace) a single `command_actions` entry.
    pub fn register_command(&self, key: &str, def: CommandDef) -> Result<()> {
        self.mutate(|obj| {
            let mut map: HashMap<String, CommandDef> = obj
                .get("command_actions")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            map.insert(key.to_string(), def);
            obj.insert("command_actions".into(), serde_json::to_value(map)?);
            Ok(())
        })
    }

    /// URL prefixes a `Webhook` action's `fn_name` must start with.
    /// Deny-by-default: unset or empty means no URL is permitted.
    pub fn allowed_webhook_base_urls(&self) -> Vec<String> {
        self.snapshot().allowed_webhook_base_urls.unwrap_or_default()
    }

    /// Replace the webhook base-URL allowlist wholesale.
    pub fn set_allowed_webhook_base_urls(&self, list: Vec<String>) -> Result<()> {
        self.set("allowed_webhook_base_urls", serde_json::to_value(list)?)
    }

    /// Remove a single `command_actions` entry. No-op if `key` isn't present.
    /// Used by `solx-packages::uninstall_package` to revoke exactly what a
    /// package's manifest granted (see `docs/next-steps.md` §1).
    pub fn deregister_command(&self, key: &str) -> Result<()> {
        self.mutate(|obj| {
            let mut map: HashMap<String, CommandDef> = obj
                .get("command_actions")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            map.remove(key);
            obj.insert("command_actions".into(), serde_json::to_value(map)?);
            Ok(())
        })
    }

    /// Append `prefix` to the webhook allowlist if not already present.
    pub fn add_allowed_webhook_base_url(&self, prefix: &str) -> Result<()> {
        self.mutate(|obj| {
            let mut list: Vec<String> = obj
                .get("allowed_webhook_base_urls")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            if !list.iter().any(|p| p == prefix) {
                list.push(prefix.to_string());
            }
            obj.insert("allowed_webhook_base_urls".into(), serde_json::to_value(list)?);
            Ok(())
        })
    }

    /// Remove a single webhook base-URL prefix. No-op if not present.
    pub fn remove_allowed_webhook_base_url(&self, prefix: &str) -> Result<()> {
        self.mutate(|obj| {
            let mut list: Vec<String> = obj
                .get("allowed_webhook_base_urls")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            list.retain(|p| p != prefix);
            obj.insert("allowed_webhook_base_urls".into(), serde_json::to_value(list)?);
            Ok(())
        })
    }

    // ── solx-server bearer token ─────────────────────────────────────────────

    /// Return the persisted `server_token`, generating and persisting a new
    /// random one (32 bytes, base64) if none exists yet. A single `mutate()`
    /// call makes generate-then-persist atomic under the same cross-process
    /// lock every other config write uses, so two `solx-server`s started
    /// concurrently against the same appdata dir can't race into two
    /// different tokens.
    pub fn ensure_server_token(&self) -> Result<String> {
        if let Some(token) = self.snapshot().server_token {
            if !token.trim().is_empty() {
                return Ok(token);
            }
        }
        let mut generated = String::new();
        self.mutate(|obj| {
            if let Some(existing) = obj.get("server_token").and_then(Value::as_str) {
                if !existing.trim().is_empty() {
                    generated = existing.to_string();
                    return Ok(());
                }
            }
            use rand::RngCore;
            let mut bytes = [0u8; 32];
            rand::thread_rng().fill_bytes(&mut bytes);
            generated = base64::engine::general_purpose::STANDARD.encode(bytes);
            obj.insert("server_token".into(), Value::String(generated.clone()));
            Ok(())
        })?;
        Ok(generated)
    }
}

fn read_file(path: &Path) -> Result<(Value, Option<SystemTime>)> {
    match fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => {
            let value = serde_json::from_str(&s)?;
            let mtime = fs::metadata(path).ok().and_then(|m| m.modified().ok());
            Ok((value, mtime))
        }
        Ok(_) => Ok((Value::Object(Map::new()), None)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok((Value::Object(Map::new()), None))
        }
        Err(e) => Err(e.into()),
    }
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_and_unknown_field_preserved() {
        let dir = tempfile::tempdir().unwrap();
        // Seed a file with an unknown field.
        fs::write(
            dir.path().join(CONFIG_FILE),
            r#"{"future_flag": 42, "data_directory": "/x"}"#,
        )
        .unwrap();

        let cfg = ConfigService::open_in(dir.path()).unwrap();
        assert_eq!(cfg.snapshot().data_directory.as_deref(), Some("/x"));

        cfg.set("files_directory", Value::String("/f".into())).unwrap();

        // Unknown field must survive the write.
        let raw = cfg.raw_snapshot();
        assert_eq!(raw.get("future_flag"), Some(&Value::from(42)));
        assert_eq!(cfg.snapshot().files_directory.as_deref(), Some("/f"));
    }

    #[test]
    fn cross_instance_reload_via_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let a = ConfigService::open_in(dir.path()).unwrap();
        let b = ConfigService::open_in(dir.path()).unwrap();

        a.set("data_directory", Value::String("/from-a".into())).unwrap();
        // b must observe a's write on next read (simulates a second process).
        // Ensure mtime differs even on coarse-resolution clocks.
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert_eq!(b.snapshot().data_directory.as_deref(), Some("/from-a"));
    }

    #[test]
    fn package_registry_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ConfigService::open_in(dir.path()).unwrap();
        cfg.register_package(InstalledPackage {
            name: "pkg".into(),
            version: "1.0".into(),
            path: "/p".into(),
            installed_at: "now".into(),
            granted_commands: vec![],
            granted_webhook_prefixes: vec![],
        })
        .unwrap();
        assert_eq!(cfg.list_packages().len(), 1);
        cfg.unregister_package("pkg").unwrap();
        assert!(cfg.list_packages().is_empty());
    }

    #[test]
    fn command_and_webhook_allowlists_default_empty_and_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ConfigService::open_in(dir.path()).unwrap();

        // Deny-by-default: nothing registered yet.
        assert!(cfg.command_actions().is_empty());
        assert!(cfg.allowed_webhook_base_urls().is_empty());

        cfg.register_command(
            "compress-pdf",
            CommandDef {
                command: "gs -o out.pdf in.pdf".into(),
                description: Some("Compress a PDF".into()),
                cwd: None,
            },
        )
        .unwrap();
        let map = cfg.command_actions();
        assert_eq!(map["compress-pdf"].command, "gs -o out.pdf in.pdf");

        cfg.set_allowed_webhook_base_urls(vec!["https://hooks.example.com".into()])
            .unwrap();
        assert_eq!(cfg.allowed_webhook_base_urls(), vec!["https://hooks.example.com".to_string()]);
    }

    #[test]
    fn ensure_server_token_generates_once_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ConfigService::open_in(dir.path()).unwrap();

        assert!(cfg.snapshot().server_token.is_none());
        let token = cfg.ensure_server_token().unwrap();
        assert!(!token.trim().is_empty());
        assert_eq!(cfg.snapshot().server_token.as_deref(), Some(token.as_str()));

        // Calling again (same instance, or a fresh one over the same file —
        // simulating a second solx-server process) must return the SAME
        // token, not regenerate.
        assert_eq!(cfg.ensure_server_token().unwrap(), token);
        let cfg2 = ConfigService::open_in(dir.path()).unwrap();
        assert_eq!(cfg2.ensure_server_token().unwrap(), token);
    }
}
