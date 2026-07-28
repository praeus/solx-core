//! Manager traits — the client/server seam.
//!
//! Every entity manager is an object-safe `#[async_trait]` trait using only
//! `solx-surface` DTOs and `serde_json::Value`. Phase 1 ships local impls
//! (libsql/tantivy/wasmtime); a future `solx-client` can implement the same
//! traits as HTTP proxies and `solx-server` can host a local impl, with no
//! change to callers (the CLI) that already depend only on `Arc<dyn _>`.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::entities::*;
use crate::error::Result;
use crate::query::*;

/// Type registry: JSON-schema types organized by path + type groups.
#[async_trait]
pub trait TypeManager: Send + Sync {
    async fn post(&self, path: &str, name: &str, input: TypeInput) -> Result<TypeEntity>;
    async fn get(&self, path: &str, name: &str) -> Result<TypeEntity>;
    async fn delete(&self, path: &str, name: &str) -> Result<()>;
    async fn list(&self, opts: ListOptions) -> Result<Page<TypeEntity>>;

    /// Resolve a type by full reference (`/path/Name`).
    async fn resolve(&self, type_ref: &str) -> Result<TypeEntity>;
    /// Validate a JSON value against the schema of `type_ref`.
    async fn validate(&self, value: &Value, type_ref: &str) -> Result<()>;
}

/// Byte store for files attached to docs/actions (no database of its own).
/// Paths are relative to the configured files root.
#[async_trait]
pub trait FileStore: Send + Sync {
    /// Write bytes to `rel_path`, returning the normalized stored path.
    async fn put(&self, rel_path: &str, bytes: Vec<u8>) -> Result<String>;
    async fn get(&self, rel_path: &str) -> Result<Vec<u8>>;
    async fn delete(&self, rel_path: &str) -> Result<()>;
    /// List stored relative paths under `prefix`.
    async fn list(&self, prefix: &str) -> Result<Vec<String>>;
}

/// Document store: CRUD + full-text/faceted search.
#[async_trait]
pub trait DocManager: Send + Sync {
    async fn post(&self, path: &str, name: &str, input: DocumentInput) -> Result<Document>;
    async fn get(&self, path: &str, name: &str) -> Result<Document>;
    async fn delete(&self, path: &str, name: &str) -> Result<()>;
    async fn list(&self, opts: ListOptions) -> Result<Page<Document>>;
    async fn search(&self, query: SearchQuery) -> Result<SearchResults>;
}

/// Action store: CRUD + execution (wasm/web/command, with oauth for web).
#[async_trait]
pub trait ActionManager: Send + Sync {
    async fn post(&self, path: &str, name: &str, input: ActionInput) -> Result<Action>;
    async fn get(&self, path: &str, name: &str) -> Result<Action>;
    async fn delete(&self, path: &str, name: &str) -> Result<()>;
    async fn list(&self, opts: ListOptions) -> Result<Page<Action>>;
    async fn exec(&self, path: &str, name: &str, params: Value) -> Result<ActionExecResult>;
}

/// Optional aggregate facade composing the four managers. A future
/// `solx-server` can expose one of these; a `solx-client` can implement it by
/// returning proxy managers. Phase 1's CLI can either use this or hold the four
/// `Arc`s directly.
pub trait Solx: Send + Sync {
    fn types(&self) -> Arc<dyn TypeManager>;
    fn files(&self) -> Arc<dyn FileStore>;
    fn docs(&self) -> Arc<dyn DocManager>;
    fn actions(&self) -> Arc<dyn ActionManager>;
}
