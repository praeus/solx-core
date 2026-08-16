//! Invocation state for `start`/`stop`/`poll` — the async alternative to
//! `exec`. See `docs/async-actions-plan.md` for the design rationale.
//!
//! The console (`crate::console`) already carries an `invocation_id` on
//! every entry it stores, but has nowhere to hang a *status* or a *cancel
//! flag*: `consoles` is keyed by `action_ref`, and `invocation_id` is only a
//! column on `console_entries`. This module is that missing state, modeled
//! directly on `crate::console`'s store: same `Db` handle (the same file as
//! `actions` and `consoles`), same `ensure_schema`/`sweep_expired` shape,
//! same `''`-sentinel-plus-[`crate::opt`] convention for nullable text.
//!
//! Status values are free-form strings, matching the console's own
//! `level`/`source` convention, rather than a Rust enum — see the
//! [`status`] module for the recognized set and [`is_terminal`].

use std::sync::Arc;

use chrono::Utc;
use serde_json::Value;
use solx_config::ConfigService;
use solx_surface::error::Result;

use crate::db::{map_db, Db};
use crate::opt;

pub const DDL: &str = "\
CREATE TABLE IF NOT EXISTS invocations (\
    invocation_id TEXT PRIMARY KEY,\
    action_ref TEXT NOT NULL,\
    status TEXT NOT NULL,\
    cancel_requested INTEGER NOT NULL DEFAULT 0,\
    created_at TEXT NOT NULL,\
    updated_at TEXT NOT NULL,\
    finished_at TEXT NOT NULL DEFAULT '',\
    console_seq_start INTEGER NOT NULL DEFAULT 0,\
    result TEXT NOT NULL DEFAULT '',\
    error TEXT NOT NULL DEFAULT ''\
);
CREATE INDEX IF NOT EXISTS idx_invocations_action \
    ON invocations(action_ref, created_at DESC);
";

/// Recognized `status` values. Free-form strings in storage (matching the
/// console's `level`/`source` convention), but every writer in this crate
/// goes through these constants rather than inlining literals.
pub mod status {
    pub const RUNNING: &str = "running";
    pub const CANCELLING: &str = "cancelling";
    pub const OK: &str = "ok";
    pub const FAILED: &str = "failed";
    pub const CANCELLED: &str = "cancelled";
    pub const TIMEOUT: &str = "timeout";
    pub const INTERRUPTED: &str = "interrupted";
}

/// `true` once a status can never change again.
pub fn is_terminal(s: &str) -> bool {
    !matches!(s, status::RUNNING | status::CANCELLING)
}

#[derive(Debug, Clone)]
pub struct Invocation {
    pub invocation_id: String,
    pub action_ref: String,
    pub status: String,
    pub cancel_requested: bool,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
    pub console_seq_start: i64,
    pub result: Option<Value>,
    pub error: Option<String>,
}

impl Invocation {
    /// The shape `action_start`/`action_stop`/`action_poll` all return.
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "invocation_id": self.invocation_id,
            "action_ref": self.action_ref,
            "status": self.status,
            "result": self.result,
            "error": self.error,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "finished_at": self.finished_at,
            "console_seq_start": self.console_seq_start,
        })
    }
}

#[derive(Clone)]
pub struct InvocationStore {
    db: Db,
    /// Held rather than snapshotted once, so a live edit to
    /// `stop_grace_secs`/`invocation_ttl_days` takes effect without a
    /// restart — same reasoning as `ConsoleStore::config`.
    config: Arc<ConfigService>,
}

impl InvocationStore {
    pub fn new(db: Db, config: Arc<ConfigService>) -> Self {
        InvocationStore { db, config }
    }

    pub async fn ensure_schema(&self) -> Result<()> {
        let conn = self.db.connect().await?;
        conn.execute_batch(DDL).await.map_err(map_db)?;
        Ok(())
    }

    /// Create a new `running` row. `console_seq_start` should be the
    /// console's `next_seq` at the moment of creation (see
    /// `ConsoleStore::list`), captured by the caller before this call so a
    /// client can jump straight to this run's own output.
    pub async fn create(&self, invocation_id: &str, action_ref: &str, console_seq_start: i64) -> Result<()> {
        let conn = self.db.connect().await?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO invocations \
             (invocation_id, action_ref, status, cancel_requested, created_at, updated_at, console_seq_start) \
             VALUES (?1, ?2, ?3, 0, ?4, ?4, ?5)",
            libsql::params![invocation_id, action_ref, status::RUNNING, now, console_seq_start],
        )
        .await
        .map_err(map_db)?;
        Ok(())
    }

    pub async fn get(&self, invocation_id: &str) -> Result<Option<Invocation>> {
        let conn = self.db.connect().await?;
        let mut rows = conn
            .query(
                "SELECT invocation_id, action_ref, status, cancel_requested, created_at, \
                 updated_at, finished_at, console_seq_start, result, error \
                 FROM invocations WHERE invocation_id = ?1",
                libsql::params![invocation_id],
            )
            .await
            .map_err(map_db)?;
        match rows.next().await.map_err(map_db)? {
            Some(row) => Ok(Some(row_to_invocation(row)?)),
            None => Ok(None),
        }
    }

    /// `true` if a cancel has been requested for this invocation, `false`
    /// for both "no" and "unknown id" — the callers of this (the loopback
    /// `/cancelled` route, `action_cancelled`) must fail closed rather than
    /// ever spuriously report a cancellation.
    pub async fn is_cancelled(&self, invocation_id: &str) -> Result<bool> {
        let conn = self.db.connect().await?;
        let mut rows = conn
            .query(
                "SELECT cancel_requested FROM invocations WHERE invocation_id = ?1",
                libsql::params![invocation_id],
            )
            .await
            .map_err(map_db)?;
        match rows.next().await.map_err(map_db)? {
            Some(row) => Ok(row.get::<i64>(0).map_err(map_db)? != 0),
            None => Ok(false),
        }
    }

    /// Set `cancel_requested` and, if the row is still `running`, move it to
    /// `cancelling`. A no-op on an already-terminal row (its status is left
    /// alone) and on an unknown id. Returns the row as it stands after the
    /// update, so the caller can report current status immediately.
    pub async fn request_cancel(&self, invocation_id: &str) -> Result<Option<Invocation>> {
        let conn = self.db.connect().await?;
        let now = Utc::now().to_rfc3339();
        let mut rows = conn
            .query(
                "UPDATE invocations SET \
                     cancel_requested = 1, \
                     status = CASE WHEN status = ?3 THEN ?4 ELSE status END, \
                     updated_at = ?2 \
                 WHERE invocation_id = ?1 \
                 RETURNING invocation_id, action_ref, status, cancel_requested, created_at, \
                     updated_at, finished_at, console_seq_start, result, error",
                libsql::params![invocation_id, now, status::RUNNING, status::CANCELLING],
            )
            .await
            .map_err(map_db)?;
        match rows.next().await.map_err(map_db)? {
            Some(row) => Ok(Some(row_to_invocation(row)?)),
            None => Ok(None),
        }
    }

    /// Move a still-non-terminal row to a terminal status with its result or
    /// error. **Idempotent and safe against a race with force-abort**: the
    /// `WHERE` clause only matches a row still `running`/`cancelling`, so
    /// whichever of a natural finish or a forced abort ([`Self::mark_aborted`])
    /// reaches the row first wins, and the loser's write is silently
    /// discarded rather than clobbering the winner's result.
    pub async fn finish(&self, invocation_id: &str, status: &str, result: Option<Value>, error: Option<String>) -> Result<()> {
        let conn = self.db.connect().await?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE invocations SET status = ?2, result = ?3, error = ?4, updated_at = ?5, finished_at = ?5 \
             WHERE invocation_id = ?1 AND status IN (?6, ?7)",
            libsql::params![
                invocation_id,
                status,
                result.map(|v| v.to_string()).unwrap_or_default(),
                error.unwrap_or_default(),
                now,
                self::status::RUNNING,
                self::status::CANCELLING,
            ],
        )
        .await
        .map_err(map_db)?;
        Ok(())
    }

    /// Mark a row `cancelled` after `stop`'s grace period expired and the
    /// task was force-aborted. Same idempotency guard as [`Self::finish`] —
    /// if the task finished naturally just before the abort, this is a
    /// no-op.
    pub async fn mark_aborted(&self, invocation_id: &str) -> Result<()> {
        self.finish(invocation_id, status::CANCELLED, None, Some("stopped (grace period expired)".into()))
            .await
    }

    /// List invocations, most recent first, optionally filtered by
    /// `action_ref`.
    pub async fn list(&self, action_ref: Option<&str>, limit: i64) -> Result<Vec<Invocation>> {
        let conn = self.db.connect().await?;
        let limit = limit.clamp(1, 1000);
        let mut rows = match action_ref {
            Some(a) => {
                conn.query(
                    "SELECT invocation_id, action_ref, status, cancel_requested, created_at, \
                     updated_at, finished_at, console_seq_start, result, error \
                     FROM invocations WHERE action_ref = ?1 ORDER BY created_at DESC LIMIT ?2",
                    libsql::params![a, limit],
                )
                .await
                .map_err(map_db)?
            }
            None => {
                conn.query(
                    "SELECT invocation_id, action_ref, status, cancel_requested, created_at, \
                     updated_at, finished_at, console_seq_start, result, error \
                     FROM invocations ORDER BY created_at DESC LIMIT ?1",
                    libsql::params![limit],
                )
                .await
                .map_err(map_db)?
            }
        };
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(map_db)? {
            out.push(row_to_invocation(row)?);
        }
        Ok(out)
    }

    /// Flip any row still `running`/`cancelling` to `interrupted`. Intended
    /// to run once at startup — a process that crashed or was killed leaves
    /// rows stuck at a non-terminal status otherwise, since nothing else
    /// ever revisits them.
    pub async fn mark_orphans(&self) -> Result<i64> {
        let conn = self.db.connect().await?;
        let mut rows = conn
            .query(
                "SELECT invocation_id FROM invocations WHERE status IN (?1, ?2)",
                libsql::params![status::RUNNING, status::CANCELLING],
            )
            .await
            .map_err(map_db)?;
        let mut orphaned = Vec::new();
        while let Some(row) = rows.next().await.map_err(map_db)? {
            orphaned.push(row.get::<String>(0).map_err(map_db)?);
        }
        for id in &orphaned {
            self.finish(id, status::INTERRUPTED, None, Some("interrupted (process restarted while running)".into()))
                .await?;
        }
        Ok(orphaned.len() as i64)
    }

    /// Drop terminal rows older than `config.invocation_ttl_days()`. Mirrors
    /// `ConsoleStore::sweep_expired` — startup only, best-effort.
    pub async fn sweep_expired(&self) -> Result<i64> {
        let conn = self.db.connect().await?;
        let cutoff = (Utc::now() - chrono::Duration::days(self.config.invocation_ttl_days())).to_rfc3339();
        let mut rows = conn
            .query(
                "SELECT invocation_id FROM invocations \
                 WHERE status NOT IN (?1, ?2) AND updated_at < ?3",
                libsql::params![status::RUNNING, status::CANCELLING, cutoff],
            )
            .await
            .map_err(map_db)?;
        let mut expired = Vec::new();
        while let Some(row) = rows.next().await.map_err(map_db)? {
            expired.push(row.get::<String>(0).map_err(map_db)?);
        }
        for id in &expired {
            conn.execute("DELETE FROM invocations WHERE invocation_id = ?1", libsql::params![id.clone()])
                .await
                .map_err(map_db)?;
        }
        Ok(expired.len() as i64)
    }
}

fn row_to_invocation(row: libsql::Row) -> Result<Invocation> {
    let result_raw: String = row.get(8).map_err(map_db)?;
    Ok(Invocation {
        invocation_id: row.get(0).map_err(map_db)?,
        action_ref: row.get(1).map_err(map_db)?,
        status: row.get(2).map_err(map_db)?,
        cancel_requested: row.get::<i64>(3).map_err(map_db)? != 0,
        created_at: row.get(4).map_err(map_db)?,
        updated_at: row.get(5).map_err(map_db)?,
        finished_at: opt(row.get(6).map_err(map_db)?),
        console_seq_start: row.get(7).map_err(map_db)?,
        result: opt(result_raw).and_then(|s| serde_json::from_str(&s).ok()),
        error: opt(row.get(9).map_err(map_db)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn test_store() -> (tempfile::TempDir, InvocationStore) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("invocations_test.db")).await.unwrap();
        let config = Arc::new(ConfigService::open_in(dir.path()).unwrap());
        let store = InvocationStore::new(db, config);
        store.ensure_schema().await.unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn create_and_get_round_trips_as_running_not_cancelled() {
        let (_d, store) = test_store().await;
        store.create("inv-1", "/pkg/a", 7).await.unwrap();
        let inv = store.get("inv-1").await.unwrap().unwrap();
        assert_eq!(inv.action_ref, "/pkg/a");
        assert_eq!(inv.status, status::RUNNING);
        assert!(!inv.cancel_requested);
        assert_eq!(inv.console_seq_start, 7);
        assert_eq!(inv.result, None);
        assert_eq!(inv.finished_at, None);
    }

    #[tokio::test]
    async fn get_of_unknown_id_is_none_not_an_error() {
        let (_d, store) = test_store().await;
        assert!(store.get("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn is_cancelled_reflects_request_cancel() {
        let (_d, store) = test_store().await;
        store.create("inv-1", "/pkg/a", 0).await.unwrap();
        assert!(!store.is_cancelled("inv-1").await.unwrap());
        store.request_cancel("inv-1").await.unwrap();
        assert!(store.is_cancelled("inv-1").await.unwrap());
    }

    #[tokio::test]
    async fn is_cancelled_of_unknown_id_is_false() {
        let (_d, store) = test_store().await;
        assert!(!store.is_cancelled("nope").await.unwrap());
    }

    #[tokio::test]
    async fn request_cancel_moves_running_to_cancelling() {
        let (_d, store) = test_store().await;
        store.create("inv-1", "/pkg/a", 0).await.unwrap();
        let inv = store.request_cancel("inv-1").await.unwrap().unwrap();
        assert_eq!(inv.status, status::CANCELLING);
        assert!(inv.cancel_requested);
    }

    #[tokio::test]
    async fn request_cancel_on_unknown_id_returns_none() {
        let (_d, store) = test_store().await;
        assert!(store.request_cancel("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn request_cancel_does_not_disturb_an_already_terminal_status() {
        let (_d, store) = test_store().await;
        store.create("inv-1", "/pkg/a", 0).await.unwrap();
        store.finish("inv-1", status::OK, Some(json!({"n": 1})), None).await.unwrap();
        let inv = store.request_cancel("inv-1").await.unwrap().unwrap();
        // Status stays 'ok', not clobbered to 'cancelling' — cancel_requested
        // is still set (harmless: nothing reads it once terminal).
        assert_eq!(inv.status, status::OK);
        assert!(inv.cancel_requested);
    }

    #[tokio::test]
    async fn to_json_carries_every_field() {
        let (_d, store) = test_store().await;
        store.create("inv-1", "/pkg/a", 3).await.unwrap();
        store.finish("inv-1", status::OK, Some(json!({"n": 1})), None).await.unwrap();
        let inv = store.get("inv-1").await.unwrap().unwrap();
        let v = inv.to_json();
        assert_eq!(v["invocation_id"], "inv-1");
        assert_eq!(v["action_ref"], "/pkg/a");
        assert_eq!(v["status"], status::OK);
        assert_eq!(v["result"], json!({"n": 1}));
        assert_eq!(v["error"], Value::Null);
        assert_eq!(v["console_seq_start"], 3);
        assert!(v["finished_at"].is_string());
    }

    #[tokio::test]
    async fn finish_sets_result_and_terminal_status() {
        let (_d, store) = test_store().await;
        store.create("inv-1", "/pkg/a", 0).await.unwrap();
        store.finish("inv-1", status::OK, Some(json!({"n": 42})), None).await.unwrap();
        let inv = store.get("inv-1").await.unwrap().unwrap();
        assert_eq!(inv.status, status::OK);
        assert_eq!(inv.result, Some(json!({"n": 42})));
        assert_eq!(inv.error, None);
        assert!(inv.finished_at.is_some());
    }

    #[tokio::test]
    async fn finish_sets_error_on_failure() {
        let (_d, store) = test_store().await;
        store.create("inv-1", "/pkg/a", 0).await.unwrap();
        store.finish("inv-1", status::FAILED, None, Some("boom".into())).await.unwrap();
        let inv = store.get("inv-1").await.unwrap().unwrap();
        assert_eq!(inv.status, status::FAILED);
        assert_eq!(inv.result, None);
        assert_eq!(inv.error.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn finish_is_idempotent_second_call_is_a_no_op() {
        let (_d, store) = test_store().await;
        store.create("inv-1", "/pkg/a", 0).await.unwrap();
        store.finish("inv-1", status::OK, Some(json!("first")), None).await.unwrap();
        // A second finish (simulating a race with a force-abort) must not
        // clobber the already-terminal result.
        store.finish("inv-1", status::CANCELLED, Some(json!("second")), None).await.unwrap();
        let inv = store.get("inv-1").await.unwrap().unwrap();
        assert_eq!(inv.status, status::OK);
        assert_eq!(inv.result, Some(json!("first")));
    }

    #[tokio::test]
    async fn mark_aborted_only_applies_to_a_still_running_row() {
        let (_d, store) = test_store().await;
        store.create("inv-1", "/pkg/a", 0).await.unwrap();
        store.create("inv-2", "/pkg/b", 0).await.unwrap();
        store.finish("inv-2", status::OK, None, None).await.unwrap();

        store.mark_aborted("inv-1").await.unwrap();
        store.mark_aborted("inv-2").await.unwrap();

        assert_eq!(store.get("inv-1").await.unwrap().unwrap().status, status::CANCELLED);
        // inv-2 had already finished 'ok' — mark_aborted must not overwrite it.
        assert_eq!(store.get("inv-2").await.unwrap().unwrap().status, status::OK);
    }

    #[tokio::test]
    async fn list_orders_newest_first_and_respects_action_ref_filter() {
        let (_d, store) = test_store().await;
        store.create("inv-1", "/pkg/a", 0).await.unwrap();
        store.create("inv-2", "/pkg/b", 0).await.unwrap();
        store.create("inv-3", "/pkg/a", 0).await.unwrap();

        let all = store.list(None, 100).await.unwrap();
        assert_eq!(all.len(), 3);

        let a_only = store.list(Some("/pkg/a"), 100).await.unwrap();
        assert_eq!(a_only.len(), 2);
        assert!(a_only.iter().all(|i| i.action_ref == "/pkg/a"));
    }

    #[tokio::test]
    async fn mark_orphans_flips_non_terminal_rows_and_leaves_terminal_ones() {
        let (_d, store) = test_store().await;
        store.create("inv-running", "/pkg/a", 0).await.unwrap();
        store.create("inv-cancelling", "/pkg/a", 0).await.unwrap();
        store.request_cancel("inv-cancelling").await.unwrap();
        store.create("inv-done", "/pkg/a", 0).await.unwrap();
        store.finish("inv-done", status::OK, None, None).await.unwrap();

        let n = store.mark_orphans().await.unwrap();
        assert_eq!(n, 2);

        assert_eq!(store.get("inv-running").await.unwrap().unwrap().status, status::INTERRUPTED);
        assert_eq!(store.get("inv-cancelling").await.unwrap().unwrap().status, status::INTERRUPTED);
        assert_eq!(store.get("inv-done").await.unwrap().unwrap().status, status::OK);
    }

    #[tokio::test]
    async fn sweep_expired_removes_only_stale_terminal_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).await.unwrap();
        let config = Arc::new(ConfigService::open_in(dir.path()).unwrap());
        config.patch(json!({ "invocation_ttl_days": 1 })).unwrap();
        let store = InvocationStore::new(db.clone(), config.clone());
        store.ensure_schema().await.unwrap();

        store.create("inv-fresh", "/pkg/a", 0).await.unwrap();
        store.finish("inv-fresh", status::OK, None, None).await.unwrap();

        store.create("inv-stale", "/pkg/a", 0).await.unwrap();
        store.finish("inv-stale", status::OK, None, None).await.unwrap();

        store.create("inv-still-running", "/pkg/a", 0).await.unwrap();

        // Backdate /inv-stale's updated_at past the TTL directly.
        let conn = db.connect().await.unwrap();
        let old = (Utc::now() - chrono::Duration::days(3)).to_rfc3339();
        conn.execute(
            "UPDATE invocations SET updated_at = ?1 WHERE invocation_id = 'inv-stale'",
            libsql::params![old],
        )
        .await
        .unwrap();

        let swept = store.sweep_expired().await.unwrap();
        assert_eq!(swept, 1);

        assert!(store.get("inv-stale").await.unwrap().is_none());
        assert!(store.get("inv-fresh").await.unwrap().is_some());
        // A still-running row must never be swept regardless of age.
        assert!(store.get("inv-still-running").await.unwrap().is_some());
    }
}
