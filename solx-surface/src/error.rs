//! Shared error and result types for the solx workspace.
//!
//! `solx-surface` is dependency-light on purpose (so it can back both a local
//! impl and a future HTTP client/server), so backend errors from libsql,
//! tantivy, wasmtime, etc. are mapped into these variants (usually via
//! `.to_string()`) at the edges of the impl crates.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, SolxError>;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SolxError {
    /// Entity or file does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// Malformed input (bad path/name, missing required field, bad JSON).
    #[error("invalid input: {0}")]
    Invalid(String),

    /// A value failed JSON-schema validation against its type.
    #[error("validation failed: {0}")]
    Validation(String),

    /// Uniqueness / concurrent-write conflict.
    #[error("conflict: {0}")]
    Conflict(String),

    /// Filesystem error.
    #[error("io error: {0}")]
    Io(String),

    /// Database backend error.
    #[error("database error: {0}")]
    Db(String),

    /// Action execution failure.
    #[error("execution error: {0}")]
    Exec(String),

    /// Config service error.
    #[error("config error: {0}")]
    Config(String),

    /// Anything else.
    #[error("{0}")]
    Other(String),
}

impl From<std::io::Error> for SolxError {
    fn from(e: std::io::Error) -> Self {
        SolxError::Io(e.to_string())
    }
}

impl From<serde_json::Error> for SolxError {
    fn from(e: serde_json::Error) -> Self {
        SolxError::Invalid(format!("json: {e}"))
    }
}

impl SolxError {
    pub fn other(msg: impl Into<String>) -> Self {
        SolxError::Other(msg.into())
    }
}
