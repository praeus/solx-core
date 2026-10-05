//! The `actions` table and its trigger-maintained FTS5 index, created (and
//! migrated) when [`crate::LocalActionManager::open`] runs.

use libsql::Connection;
use solx_surface::error::Result;

use crate::db::map_db;

pub(crate) const DDL: &str = "\
CREATE TABLE IF NOT EXISTS actions (\
    id TEXT PRIMARY KEY,\
    path TEXT NOT NULL,\
    name TEXT NOT NULL,\
    caption TEXT NOT NULL DEFAULT '',\
    description TEXT NOT NULL DEFAULT '',\
    capabilities TEXT NOT NULL DEFAULT '[]',\
    phrases TEXT NOT NULL DEFAULT '[]',\
    category TEXT NOT NULL DEFAULT '',\
    param_type_ref TEXT NOT NULL DEFAULT '',\
    result_type_ref TEXT NOT NULL DEFAULT '',\
    action_type TEXT NOT NULL DEFAULT '',\
    fn_name TEXT NOT NULL DEFAULT '',\
    bin_name TEXT NOT NULL DEFAULT '',\
    action_config TEXT NOT NULL DEFAULT 'null',\
    files TEXT NOT NULL DEFAULT '[]',\
    trusted INTEGER NOT NULL DEFAULT 0,\
    created_at TEXT NOT NULL,\
    updated_at TEXT NOT NULL,\
    UNIQUE(path,name)\
);";

/// External-content FTS5 index over the action catalogue, kept in sync with
/// `actions` purely by trigger — `save`/`delete`/`seed_builtins` never touch
/// this table directly. `id` is `TEXT PRIMARY KEY` (not `INTEGER PRIMARY
/// KEY`), so `actions` keeps SQLite's implicit `rowid`, which is what
/// `content_rowid='rowid'` links against.
const FTS_DDL: &str = "\
CREATE VIRTUAL TABLE IF NOT EXISTS actions_fts USING fts5(\
    path, name, caption, description, category, phrases, \
    content='actions', content_rowid='rowid', tokenize='porter unicode61'\
);\
CREATE TRIGGER IF NOT EXISTS actions_ai AFTER INSERT ON actions BEGIN \
  INSERT INTO actions_fts(rowid,path,name,caption,description,category,phrases)\
  VALUES (new.rowid,new.path,new.name,new.caption,new.description,new.category,new.phrases);\
END;\
CREATE TRIGGER IF NOT EXISTS actions_ad AFTER DELETE ON actions BEGIN \
  INSERT INTO actions_fts(actions_fts,rowid,path,name,caption,description,category,phrases)\
  VALUES('delete',old.rowid,old.path,old.name,old.caption,old.description,old.category,old.phrases);\
END;\
CREATE TRIGGER IF NOT EXISTS actions_au AFTER UPDATE ON actions BEGIN \
  INSERT INTO actions_fts(actions_fts,rowid,path,name,caption,description,category,phrases)\
  VALUES('delete',old.rowid,old.path,old.name,old.caption,old.description,old.category,old.phrases);\
  INSERT INTO actions_fts(rowid,path,name,caption,description,category,phrases)\
  VALUES (new.rowid,new.path,new.name,new.caption,new.description,new.category,new.phrases);\
END;";

/// Create `actions` and `actions_fts` if they don't exist yet, rebuilding
/// the index once when `actions_fts` is first created over existing rows.
pub(crate) async fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(DDL).await.map_err(map_db)?;
    // `actions_fts` is `content='actions'` (external content), so a bare
    // `SELECT ... FROM actions_fts` passes straight through to `actions`
    // rather than reading the index — a `WHERE rowid NOT IN (SELECT
    // rowid FROM actions_fts)` backfill would therefore always compare
    // `actions.rowid` against itself and index nothing for rows that
    // predate this table. The documented fix is `INSERT INTO
    // actions_fts(actions_fts) VALUES ('rebuild')`, run once, only the
    // first time the table is created (mirroring `solx-docs`'s
    // `created_fresh` reindex), so a normal restart doesn't pay for a
    // full rebuild every time. New rows index fine via the `FTS_DDL` triggers
    // regardless.
    let fts_existed_before: bool = {
        let mut rows = conn
            .query(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='actions_fts'",
                (),
            )
            .await
            .map_err(map_db)?;
        rows.next().await.map_err(map_db)?.is_some()
    };
    conn.execute_batch(FTS_DDL).await.map_err(map_db)?;
    if !fts_existed_before {
        conn.execute("INSERT INTO actions_fts(actions_fts) VALUES('rebuild')", ())
            .await
            .map_err(map_db)?;
    }
    Ok(())
}
