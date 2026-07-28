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

use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::SystemTime;

use fs2::FileExt;
use serde_json::{Map, Value};
use solx_surface::error::{Result, SolxError};

pub use types::{CommandActionDef, InstalledPackage, SolxConfig};

const CONFIG_FILE: &str = "solx-config.json";

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

    pub fn logs_dir(&self) -> PathBuf {
        self.appdata.join("logs")
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
        })
        .unwrap();
        assert_eq!(cfg.list_packages().len(), 1);
        cfg.unregister_package("pkg").unwrap();
        assert!(cfg.list_packages().is_empty());
    }
}
