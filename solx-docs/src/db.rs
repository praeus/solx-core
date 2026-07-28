//! Minimal libsql connection helper (duplicated per entity crate by design).

use std::path::Path;
use std::sync::Arc;

use libsql::{Builder, Connection, Database};
use solx_surface::error::{Result, SolxError};

pub fn map_db<E: std::fmt::Display>(e: E) -> SolxError {
    SolxError::Db(e.to_string())
}

#[derive(Clone)]
pub struct Db {
    db: Arc<Database>,
}

impl Db {
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Builder::new_local(path).build().await.map_err(map_db)?;
        Ok(Db { db: Arc::new(db) })
    }

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
