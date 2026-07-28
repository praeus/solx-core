//! Minimal libsql connection helper. Each entity crate owns its own DB, so this
//! small helper is duplicated (by design) rather than shared.

use std::path::Path;
use std::sync::Arc;

use libsql::{Builder, Connection, Database};
use solx_surface::error::{Result, SolxError};

/// Map any backend error into [`SolxError::Db`].
pub fn map_db<E: std::fmt::Display>(e: E) -> SolxError {
    SolxError::Db(e.to_string())
}

/// A libsql database handle (an `Arc`-backed connection factory).
#[derive(Clone)]
pub struct Db {
    db: Arc<Database>,
}

impl Db {
    /// Open (or create) a local libsql database at `path`.
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Builder::new_local(path).build().await.map_err(map_db)?;
        Ok(Db { db: Arc::new(db) })
    }

    /// Create a fresh connection with a 10s busy timeout.
    pub async fn connect(&self) -> Result<Connection> {
        let conn = self.db.connect().map_err(map_db)?;
        let mut rows = conn
            .query("PRAGMA busy_timeout=10000", ())
            .await
            .map_err(map_db)?;
        while rows.next().await.map_err(map_db)?.is_some() {}
        Ok(conn)
    }
}
