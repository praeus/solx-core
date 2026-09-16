//! Action consoles: an append-only, cursor-addressable log per action ref.
//!
//! Console identity **is** the action ref — `/packages/solx-ollama/ollama-chat`
//! has exactly one console, and every layer of that action's execution
//! (a WASM guest's `logger.log`, Command stderr via the loopback, webhook
//! request/response, and script stages) writes to it. See
//! `docs/console-implementation-plan.md` in the workspace root for the full
//! design rationale.
//!
//! Two things every entry carries to keep that simplicity from causing
//! trouble:
//!
//! * `invocation_id` — minted once per `exec_as` dispatch to a `Wasm`/
//!   `Script` action (see `solx_actions::caller::Caller::invocation_id`) and
//!   stamped on every entry that invocation writes. Without this, two
//!   concurrent runs of the same action would interleave into one
//!   indistinguishable stream — fine for status lines, actively wrong for
//!   streamed chunks. Reads do not filter by it by default (the default read
//!   is "everything, in order"), but it is there for a client that wants to
//!   separate runs itself, and for a future `invocation_id` filter to be
//!   added without a schema change.
//! * `run_id` — reserved, always `None` today. Would let a whole nested call
//!   tree (an orchestrator and everything it invokes) be queried together.
//!   Adding the column now and wiring it later is free; adding it later
//!   would be a migration.
//!
//! `seq` is the read cursor: monotonic per console, never reused even
//! across eviction, which is what makes `tail` cheap, replayable, and safe
//! for more than one concurrent reader.
//!
//! Storage lives in this crate's own database file, separate from
//! `solx-actions`' `actions` table — there are no DB-level foreign keys
//! linking them, only the `action_ref` string shared at the application
//! layer.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use libsql::Connection;
use serde_json::Value;
use solx_config::ConfigService;
use solx_surface::error::{Result, SolxError};

use crate::db::{map_db, opt, Db};

pub const DDL: &str = "\
CREATE TABLE IF NOT EXISTS consoles (\
    action_ref TEXT PRIMARY KEY,\
    created_at TEXT NOT NULL,\
    last_write TEXT NOT NULL,\
    next_seq INTEGER NOT NULL DEFAULT 1,\
    first_seq INTEGER NOT NULL DEFAULT 1,\
    dropped INTEGER NOT NULL DEFAULT 0\
);
CREATE TABLE IF NOT EXISTS console_entries (\
    action_ref TEXT NOT NULL,\
    seq INTEGER NOT NULL,\
    ts TEXT NOT NULL,\
    level TEXT NOT NULL,\
    invocation_id TEXT NOT NULL,\
    run_id TEXT NOT NULL DEFAULT '',\
    source TEXT NOT NULL,\
    message TEXT NOT NULL DEFAULT '',\
    data TEXT NOT NULL DEFAULT '',\
    PRIMARY KEY (action_ref, seq)\
);
CREATE INDEX IF NOT EXISTS idx_console_entries_invocation \
    ON console_entries(action_ref, invocation_id, seq);
";

/// Long-poll granularity for [`ConsoleStore::tail`]. `pub(crate)` so
/// `solx-actions`' `poll_invocation` can reuse the same cadence via
/// [`TAIL_POLL_INTERVAL`]/[`MAX_TAIL_WAIT_SECS`] rather than defining a
/// second set of long-poll constants.
pub const TAIL_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Ceiling on `tail`'s `wait_secs`, mirroring `oauth-await`'s timeout clamp —
/// an internal action call should never block indefinitely.
pub const MAX_TAIL_WAIT_SECS: u64 = 60;
/// Evict at least this fraction of the cap at once, so a console pinned at
/// its limit isn't paying for a `DELETE` on every single insert.
const EVICT_BATCH_FRACTION: f64 = 0.1;

#[derive(Debug, Clone)]
pub struct Entry {
    pub seq: i64,
    pub ts: String,
    pub level: String,
    pub invocation_id: String,
    pub run_id: Option<String>,
    pub source: String,
    pub message: Option<String>,
    pub data: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct ReadResult {
    pub entries: Vec<Entry>,
    /// Pass this back as the next call's cursor to continue from here.
    pub next_cursor: i64,
    /// Oldest `seq` still retained. Entries before this were evicted.
    pub first_seq: i64,
    /// Count of entries evicted over this console's lifetime so far.
    pub dropped: i64,
}

#[derive(Debug, Clone)]
pub struct CopyResult {
    /// How many entries were actually copied — may be fewer than `limit`
    /// asked for if fewer exist.
    pub copied: usize,
    /// Pass this back as the next call's `cursor` to continue copying this
    /// invocation's *source* console from where this call left off.
    pub next_cursor: i64,
}

#[derive(Debug, Clone)]
pub struct ConsoleSummary {
    pub action_ref: String,
    pub created_at: String,
    pub last_write: String,
    pub entry_count: i64,
    pub dropped: i64,
    /// The seq that will be assigned to the *next* entry written. Lets a
    /// caller (e.g. `solx exec`'s live renderer) start a `tail` from "now"
    /// without an O(history) read just to find the current tip.
    pub next_seq: i64,
}

#[derive(Clone)]
pub struct ConsoleStore {
    db: Db,
    /// Held rather than snapshotted once, so a live `SolxConfig` edit (cap,
    /// TTL) takes effect without a restart.
    config: Arc<ConfigService>,
}

impl ConsoleStore {
    pub fn new(db: Db, config: Arc<ConfigService>) -> Self {
        ConsoleStore { db, config }
    }

    pub async fn ensure_schema(&self) -> Result<()> {
        let conn = self.db.connect().await?;
        conn.execute_batch(DDL).await.map_err(map_db)?;
        Ok(())
    }

    /// Append one entry to `action_ref`'s console, creating it on first
    /// write. Returns the entry's `seq`.
    pub async fn print(
        &self,
        action_ref: &str,
        invocation_id: &str,
        run_id: Option<&str>,
        level: &str,
        source: &str,
        message: Option<String>,
        data: Option<Value>,
    ) -> Result<i64> {
        let conn = self.db.connect().await?;
        let now = Utc::now().to_rfc3339();

        // Single atomic upsert-and-claim: for a brand new console the INSERT
        // branch runs with next_seq hard-coded to 2, so `next_seq - 1` (the
        // claimed seq) is 1; for an existing console the ON CONFLICT branch
        // increments next_seq and RETURNING reflects the *post*-update row,
        // so `next_seq - 1` is exactly the pre-update value — the seq this
        // call is claiming. One round trip, no read-modify-write race
        // between concurrent printers.
        let mut rows = conn
            .query(
                "INSERT INTO consoles (action_ref, created_at, last_write, next_seq, first_seq, dropped) \
                 VALUES (?1, ?2, ?2, 2, 1, 0) \
                 ON CONFLICT(action_ref) DO UPDATE SET \
                     next_seq = next_seq + 1, last_write = excluded.last_write \
                 RETURNING next_seq - 1",
                libsql::params![action_ref, now.clone()],
            )
            .await
            .map_err(map_db)?;
        let seq: i64 = rows
            .next()
            .await
            .map_err(map_db)?
            .ok_or_else(|| SolxError::Db("console seq allocation returned no row".into()))?
            .get(0)
            .map_err(map_db)?;

        conn.execute(
            "INSERT INTO console_entries \
             (action_ref, seq, ts, level, invocation_id, run_id, source, message, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            libsql::params![
                action_ref,
                seq,
                now,
                level,
                invocation_id,
                run_id.unwrap_or(""),
                source,
                message.unwrap_or_default(),
                data.map(|d| d.to_string()).unwrap_or_default(),
            ],
        )
        .await
        .map_err(map_db)?;

        self.evict_if_over_cap(&conn, action_ref).await?;
        Ok(seq)
    }

    /// Copy one invocation's entries from `from_action_ref` into
    /// `to_action_ref`, renumbered into the destination's own `seq` sequence,
    /// oldest first. `to_label`, when given, is prefixed onto each copied
    /// entry's `message` as `[label] message`.
    ///
    /// Built for a caller that would otherwise read a child invocation's
    /// console and re-`print` each entry into its own one at a time — a
    /// detached llm call that streams a response chunk-by-chunk can produce
    /// hundreds of entries per run, and re-printing each individually costs
    /// one round trip and one insert apiece on the *caller's* side as well as
    /// this one. This does the whole batch as one call and one transaction.
    ///
    /// Filtered by `invocation_id` rather than a plain `seq` range because
    /// `action_ref` alone identifies a console (see the module doc), so a
    /// console shared by concurrent callers of the same action interleaves
    /// everyone's entries — copying "everything from cursor" would pull in
    /// entries that are not this caller's to copy.
    ///
    /// Deliberately copy-only, not move: deleting only one invocation's rows
    /// out of a shared console would not be a contiguous prefix the way
    /// [`Self::clear`]'s eviction is, and this store's `first_seq`/`dropped`
    /// bookkeeping assumes it always is. Nothing needs deleting from the
    /// source for the use case this exists for — draining a child's console
    /// into the caller's own doesn't require the child's copy to disappear.
    pub async fn copy(
        &self,
        from_action_ref: &str,
        to_action_ref: &str,
        invocation_id: &str,
        cursor: Option<i64>,
        limit: i64,
        to_label: Option<&str>,
    ) -> Result<CopyResult> {
        let from = cursor.unwrap_or(0).max(0);
        let limit = limit.clamp(1, 1000);
        let conn = self.db.connect().await?;

        // Read-only, and done before anything on the destination is touched:
        // this is what lets the exact number of seq slots to claim there be
        // known up front, in one block-claim, rather than one row at a time.
        let mut rows = conn
            .query(
                "SELECT seq, ts, level, run_id, source, message, data \
                 FROM console_entries WHERE action_ref = ?1 AND invocation_id = ?2 AND seq >= ?3 \
                 ORDER BY seq ASC LIMIT ?4",
                libsql::params![from_action_ref, invocation_id, from, limit],
            )
            .await
            .map_err(map_db)?;
        struct Source {
            seq: i64,
            ts: String,
            level: String,
            run_id: String,
            source: String,
            message: String,
            data: String,
        }
        let mut sources = Vec::new();
        while let Some(row) = rows.next().await.map_err(map_db)? {
            sources.push(Source {
                seq: row.get(0).map_err(map_db)?,
                ts: row.get(1).map_err(map_db)?,
                level: row.get(2).map_err(map_db)?,
                run_id: row.get(3).map_err(map_db)?,
                source: row.get(4).map_err(map_db)?,
                message: row.get(5).map_err(map_db)?,
                data: row.get(6).map_err(map_db)?,
            });
        }
        if sources.is_empty() {
            return Ok(CopyResult { copied: 0, next_cursor: from });
        }
        let next_cursor = sources.last().map(|s| s.seq + 1).unwrap_or(from);
        let count = sources.len() as i64;

        // Claim `count` seq slots on the destination in one round trip - the
        // same atomic upsert-and-claim `print` uses for one slot at a time,
        // generalized: `?3` (count) appears three times, all referring to the
        // one bound value (an established pattern in this file - see
        // `print`'s own `?2` reused for `created_at`/`last_write`). For a
        // brand new console the INSERT branch runs with `next_seq` set to
        // `1 + count`, so `next_seq - count` is 1, the block's start; for an
        // existing console the ON CONFLICT branch increments `next_seq` by
        // `count` and RETURNING reflects the *post*-update row, so
        // `next_seq - count` is exactly the pre-update value - the first
        // seq this call is claiming.
        //
        // Wrapped in an explicit transaction - the first in this crate -
        // because unlike `print`'s single insert, a crash between claiming
        // the block and finishing the inserts would otherwise leave a real
        // gap in the destination's `seq` sequence (a leaked range, not a
        // correctness bug on its own, but avoidable here for the same reason
        // `print`'s claim-then-insert already keeps both statements right
        // next to each other).
        let now = Utc::now().to_rfc3339();
        let tx = conn
            .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
            .await
            .map_err(map_db)?;
        let dest_start: i64 = {
            let mut claim = tx
                .query(
                    "INSERT INTO consoles (action_ref, created_at, last_write, next_seq, first_seq, dropped) \
                     VALUES (?1, ?2, ?2, 1 + ?3, 1, 0) \
                     ON CONFLICT(action_ref) DO UPDATE SET \
                         next_seq = next_seq + ?3, last_write = excluded.last_write \
                     RETURNING next_seq - ?3",
                    libsql::params![to_action_ref, now.clone(), count],
                )
                .await
                .map_err(map_db)?;
            claim
                .next()
                .await
                .map_err(map_db)?
                .ok_or_else(|| SolxError::Db("console seq allocation returned no row".into()))?
                .get(0)
                .map_err(map_db)?
        };

        for (i, entry) in sources.iter().enumerate() {
            let dest_seq = dest_start + i as i64;
            let message = match to_label {
                Some(label) => format!("[{label}] {}", entry.message),
                None => entry.message.clone(),
            };
            tx.execute(
                "INSERT INTO console_entries \
                 (action_ref, seq, ts, level, invocation_id, run_id, source, message, data) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                libsql::params![
                    to_action_ref,
                    dest_seq,
                    entry.ts.clone(),
                    entry.level.clone(),
                    invocation_id,
                    entry.run_id.clone(),
                    entry.source.clone(),
                    message,
                    entry.data.clone(),
                ],
            )
            .await
            .map_err(map_db)?;
        }
        tx.commit().await.map_err(map_db)?;

        // Outside the transaction and best-effort in the same sense `print`'s
        // own call is: an eviction failure here must not undo entries that
        // are already durably copied.
        self.evict_if_over_cap(&conn, to_action_ref).await?;
        Ok(CopyResult { copied: count as usize, next_cursor })
    }

    /// Read entries from `from_seq` (inclusive; default 0, i.e. the start of
    /// whatever remains retained) forward, oldest first.
    pub async fn read(&self, action_ref: &str, from_seq: Option<i64>, limit: i64) -> Result<ReadResult> {
        let conn = self.db.connect().await?;
        let (first_seq, _next_seq, dropped) = self.meta(&conn, action_ref).await?;
        let from = from_seq.unwrap_or(0).max(0);
        let limit = limit.clamp(1, 1000);

        let mut rows = conn
            .query(
                "SELECT seq, ts, level, invocation_id, run_id, source, message, data \
                 FROM console_entries WHERE action_ref = ?1 AND seq >= ?2 \
                 ORDER BY seq ASC LIMIT ?3",
                libsql::params![action_ref, from, limit],
            )
            .await
            .map_err(map_db)?;

        let mut entries = Vec::new();
        while let Some(row) = rows.next().await.map_err(map_db)? {
            let data_raw: String = row.get(7).map_err(map_db)?;
            entries.push(Entry {
                seq: row.get(0).map_err(map_db)?,
                ts: row.get(1).map_err(map_db)?,
                level: row.get(2).map_err(map_db)?,
                invocation_id: row.get(3).map_err(map_db)?,
                run_id: opt(row.get(4).map_err(map_db)?),
                source: row.get(5).map_err(map_db)?,
                message: opt(row.get(6).map_err(map_db)?),
                data: opt(data_raw).and_then(|s| serde_json::from_str(&s).ok()),
            });
        }

        let next_cursor = entries.last().map(|e| e.seq + 1).unwrap_or(from);
        Ok(ReadResult { entries, next_cursor, first_seq, dropped })
    }

    /// Like [`Self::read`], but if nothing new is available yet, polls up
    /// to `wait_secs` (clamped to [`MAX_TAIL_WAIT_SECS`]) before returning
    /// an empty result. Returns immediately once at least one entry exists.
    pub async fn tail(
        &self,
        action_ref: &str,
        cursor: Option<i64>,
        limit: i64,
        wait_secs: Option<u64>,
    ) -> Result<ReadResult> {
        let wait = wait_secs.map(|s| Duration::from_secs(s.min(MAX_TAIL_WAIT_SECS)));
        let deadline = wait.map(|w| tokio::time::Instant::now() + w);

        loop {
            let result = self.read(action_ref, cursor, limit).await?;
            if !result.entries.is_empty() {
                return Ok(result);
            }
            match deadline {
                Some(d) => {
                    let now = tokio::time::Instant::now();
                    if now >= d {
                        return Ok(result);
                    }
                    tokio::time::sleep(TAIL_POLL_INTERVAL.min(d - now)).await;
                }
                None => return Ok(result),
            }
        }
    }

    /// Drop entries older than `before_seq` (default: everything currently
    /// present). Returns the number removed.
    pub async fn clear(&self, action_ref: &str, before_seq: Option<i64>) -> Result<i64> {
        let conn = self.db.connect().await?;
        let (first_seq, next_seq, _dropped) = self.meta(&conn, action_ref).await?;
        // Clamp so first_seq never runs ahead of next_seq — an empty
        // console (or `before_seq` past the end) just becomes "nothing
        // retained", not an inconsistent cursor.
        let cutoff = before_seq.unwrap_or(next_seq).min(next_seq);
        if cutoff <= first_seq {
            return Ok(0);
        }

        let mut count_rows = conn
            .query(
                "SELECT COUNT(*) FROM console_entries WHERE action_ref = ?1 AND seq < ?2",
                libsql::params![action_ref, cutoff],
            )
            .await
            .map_err(map_db)?;
        let removed: i64 = count_rows
            .next()
            .await
            .map_err(map_db)?
            .map(|r| r.get(0))
            .transpose()
            .map_err(map_db)?
            .unwrap_or(0);

        conn.execute(
            "DELETE FROM console_entries WHERE action_ref = ?1 AND seq < ?2",
            libsql::params![action_ref, cutoff],
        )
        .await
        .map_err(map_db)?;
        conn.execute(
            "UPDATE consoles SET first_seq = ?2 WHERE action_ref = ?1",
            libsql::params![action_ref, cutoff],
        )
        .await
        .map_err(map_db)?;

        Ok(removed)
    }

    /// List known consoles, most recently written first.
    pub async fn list(&self, prefix: Option<&str>, limit: i64) -> Result<Vec<ConsoleSummary>> {
        let conn = self.db.connect().await?;
        let limit = limit.clamp(1, 1000);
        let mut rows = match prefix {
            Some(p) => {
                conn.query(
                    "SELECT action_ref, created_at, last_write, next_seq, first_seq, dropped \
                     FROM consoles WHERE action_ref LIKE ?1 ORDER BY last_write DESC LIMIT ?2",
                    libsql::params![format!("{p}%"), limit],
                )
                .await
                .map_err(map_db)?
            }
            None => {
                conn.query(
                    "SELECT action_ref, created_at, last_write, next_seq, first_seq, dropped \
                     FROM consoles ORDER BY last_write DESC LIMIT ?1",
                    libsql::params![limit],
                )
                .await
                .map_err(map_db)?
            }
        };

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(map_db)? {
            let next_seq: i64 = row.get(3).map_err(map_db)?;
            let first_seq: i64 = row.get(4).map_err(map_db)?;
            out.push(ConsoleSummary {
                action_ref: row.get(0).map_err(map_db)?,
                created_at: row.get(1).map_err(map_db)?,
                last_write: row.get(2).map_err(map_db)?,
                entry_count: next_seq - first_seq,
                dropped: row.get(5).map_err(map_db)?,
                next_seq,
            });
        }
        Ok(out)
    }

    /// Drop every console (and its entries) whose most recent write is
    /// older than the configured TTL. Intended to run once at startup;
    /// there is no background scheduler.
    pub async fn sweep_expired(&self) -> Result<i64> {
        let conn = self.db.connect().await?;
        let cutoff = (Utc::now() - chrono::Duration::days(self.config.console_ttl_days())).to_rfc3339();

        let mut rows = conn
            .query(
                "SELECT action_ref FROM consoles WHERE last_write < ?1",
                libsql::params![cutoff],
            )
            .await
            .map_err(map_db)?;
        let mut expired = Vec::new();
        while let Some(row) = rows.next().await.map_err(map_db)? {
            expired.push(row.get::<String>(0).map_err(map_db)?);
        }

        for action_ref in &expired {
            conn.execute(
                "DELETE FROM console_entries WHERE action_ref = ?1",
                libsql::params![action_ref.clone()],
            )
            .await
            .map_err(map_db)?;
            conn.execute(
                "DELETE FROM consoles WHERE action_ref = ?1",
                libsql::params![action_ref.clone()],
            )
            .await
            .map_err(map_db)?;
        }
        Ok(expired.len() as i64)
    }

    /// Current `next_seq` for exactly `action_ref` — the seq that will be
    /// assigned to the *next* entry written, or `1` if nothing has been
    /// written yet (no row exists in `consoles` before the first `print`).
    ///
    /// Used by `solx-actions`' `start_invocation` to capture a detached
    /// run's starting cursor. Deliberately an exact lookup through
    /// [`Self::meta`] rather than [`Self::list`]'s prefix `LIKE` match,
    /// which could return a *different* console's summary if another
    /// action's ref happens to share `action_ref` as a literal string
    /// prefix (e.g. `/pkg/foo` vs. `/pkg/foobar`).
    pub async fn current_next_seq(&self, action_ref: &str) -> Result<i64> {
        let conn = self.db.connect().await?;
        let (_first_seq, next_seq, _dropped) = self.meta(&conn, action_ref).await?;
        Ok(if next_seq == 0 { 1 } else { next_seq })
    }

    /// `(first_seq, next_seq, dropped)` for `action_ref`, or all-zero if the
    /// console has never been written to.
    async fn meta(&self, conn: &Connection, action_ref: &str) -> Result<(i64, i64, i64)> {
        let mut rows = conn
            .query(
                "SELECT first_seq, next_seq, dropped FROM consoles WHERE action_ref = ?1",
                libsql::params![action_ref],
            )
            .await
            .map_err(map_db)?;
        match rows.next().await.map_err(map_db)? {
            Some(row) => Ok((
                row.get(0).map_err(map_db)?,
                row.get(1).map_err(map_db)?,
                row.get(2).map_err(map_db)?,
            )),
            None => Ok((0, 0, 0)),
        }
    }

    async fn evict_if_over_cap(&self, conn: &Connection, action_ref: &str) -> Result<()> {
        let cap = self.config.console_max_entries();
        let (first_seq, next_seq, _dropped) = self.meta(conn, action_ref).await?;
        let retained = next_seq - first_seq;
        if retained <= cap {
            return Ok(());
        }
        let overflow = retained - cap;
        let batch = overflow.max(((cap as f64) * EVICT_BATCH_FRACTION).ceil() as i64).max(1);
        let new_first = (first_seq + batch).min(next_seq);

        conn.execute(
            "DELETE FROM console_entries WHERE action_ref = ?1 AND seq < ?2",
            libsql::params![action_ref, new_first],
        )
        .await
        .map_err(map_db)?;
        conn.execute(
            "UPDATE consoles SET first_seq = ?2, dropped = dropped + ?3 WHERE action_ref = ?1",
            libsql::params![action_ref, new_first, new_first - first_seq],
        )
        .await
        .map_err(map_db)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn test_store() -> (tempfile::TempDir, ConsoleStore) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("console_test.db")).await.unwrap();
        let config = Arc::new(ConfigService::open_in(dir.path()).unwrap());
        let store = ConsoleStore::new(db, config);
        store.ensure_schema().await.unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn print_assigns_increasing_seq_starting_at_one() {
        let (_d, store) = test_store().await;
        let s1 = store.print("/a/b", "inv-1", None, "info", "guest", Some("one".into()), None).await.unwrap();
        let s2 = store.print("/a/b", "inv-1", None, "info", "guest", Some("two".into()), None).await.unwrap();
        assert_eq!(s1, 1);
        assert_eq!(s2, 2);
    }

    #[tokio::test]
    async fn separate_consoles_have_independent_sequences() {
        let (_d, store) = test_store().await;
        let a1 = store.print("/a", "i", None, "info", "guest", Some("x".into()), None).await.unwrap();
        let b1 = store.print("/b", "i", None, "info", "guest", Some("y".into()), None).await.unwrap();
        assert_eq!(a1, 1);
        assert_eq!(b1, 1);
    }

    #[tokio::test]
    async fn read_returns_entries_in_order_with_a_correct_next_cursor() {
        let (_d, store) = test_store().await;
        for i in 0..3 {
            store.print("/a", "i", None, "info", "guest", Some(format!("m{i}")), None).await.unwrap();
        }
        let r = store.read("/a", None, 100).await.unwrap();
        assert_eq!(r.entries.len(), 3);
        assert_eq!(r.entries.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(r.next_cursor, 4);
        assert_eq!(r.first_seq, 1);
        assert_eq!(r.dropped, 0);
    }

    #[tokio::test]
    async fn read_from_a_cursor_only_returns_newer_entries() {
        let (_d, store) = test_store().await;
        for i in 0..5 {
            store.print("/a", "i", None, "info", "guest", Some(format!("m{i}")), None).await.unwrap();
        }
        let r = store.read("/a", Some(4), 100).await.unwrap();
        assert_eq!(r.entries.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![4, 5]);
    }

    #[tokio::test]
    async fn read_of_an_unknown_console_is_empty_not_an_error() {
        let (_d, store) = test_store().await;
        let r = store.read("/never/printed", None, 10).await.unwrap();
        assert!(r.entries.is_empty());
        assert_eq!(r.next_cursor, 0);
        assert_eq!(r.first_seq, 0);
    }

    // ── copy ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn copy_renumbers_into_the_destinations_own_sequence() {
        let (_d, store) = test_store().await;
        // The destination already has entries of its own, so the copied ones
        // must continue that sequence, not restart at 1.
        store.print("/dest", "other-inv", None, "info", "guest", Some("already here".into()), None).await.unwrap();
        for i in 0..3 {
            store.print("/src", "inv-1", None, "info", "guest", Some(format!("m{i}")), None).await.unwrap();
        }

        let result = store.copy("/src", "/dest", "inv-1", None, 100, None).await.unwrap();
        assert_eq!(result.copied, 3);
        assert_eq!(result.next_cursor, 4);

        let dest = store.read("/dest", None, 100).await.unwrap();
        assert_eq!(dest.entries.len(), 4);
        // Continues the destination's own sequence rather than colliding
        // with or restarting from the source's.
        assert_eq!(dest.entries.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
        let copied: Vec<&str> = dest.entries[1..].iter().map(|e| e.message.as_deref().unwrap()).collect();
        assert_eq!(copied, vec!["m0", "m1", "m2"]);
        // The source is untouched - this is copy, not move.
        let src = store.read("/src", None, 100).await.unwrap();
        assert_eq!(src.entries.len(), 3);
    }

    #[tokio::test]
    async fn copy_only_touches_the_named_invocation() {
        // A console is identified by action_ref alone, so a shared one
        // interleaves every concurrent caller's entries. Copying "everything
        // from cursor" would pull in entries that are not this caller's.
        let (_d, store) = test_store().await;
        store.print("/src", "inv-a", None, "info", "guest", Some("a1".into()), None).await.unwrap();
        store.print("/src", "inv-b", None, "info", "guest", Some("b1".into()), None).await.unwrap();
        store.print("/src", "inv-a", None, "info", "guest", Some("a2".into()), None).await.unwrap();

        let result = store.copy("/src", "/dest", "inv-a", None, 100, None).await.unwrap();
        assert_eq!(result.copied, 2);
        let dest = store.read("/dest", None, 100).await.unwrap();
        let messages: Vec<&str> = dest.entries.iter().map(|e| e.message.as_deref().unwrap()).collect();
        assert_eq!(messages, vec!["a1", "a2"]);
        assert!(dest.entries.iter().all(|e| e.invocation_id == "inv-a"));
    }

    #[tokio::test]
    async fn copy_prefixes_the_message_with_a_label_when_given() {
        let (_d, store) = test_store().await;
        store.print("/src", "inv-1", None, "info", "guest", Some("chunk".into()), None).await.unwrap();

        store.copy("/src", "/dest", "inv-1", None, 100, Some("summary")).await.unwrap();
        let dest = store.read("/dest", None, 100).await.unwrap();
        assert_eq!(dest.entries[0].message.as_deref(), Some("[summary] chunk"));
    }

    #[tokio::test]
    async fn copy_resumes_from_the_returned_cursor() {
        let (_d, store) = test_store().await;
        for i in 0..5 {
            store.print("/src", "inv-1", None, "info", "guest", Some(format!("m{i}")), None).await.unwrap();
        }

        let first = store.copy("/src", "/dest", "inv-1", None, 3, None).await.unwrap();
        assert_eq!(first.copied, 3);
        let second = store.copy("/src", "/dest", "inv-1", Some(first.next_cursor), 100, None).await.unwrap();
        assert_eq!(second.copied, 2);

        let dest = store.read("/dest", None, 100).await.unwrap();
        let messages: Vec<&str> = dest.entries.iter().map(|e| e.message.as_deref().unwrap()).collect();
        assert_eq!(messages, vec!["m0", "m1", "m2", "m3", "m4"]);
    }

    #[tokio::test]
    async fn copy_of_nothing_new_copies_nothing_and_holds_the_cursor() {
        let (_d, store) = test_store().await;
        store.print("/src", "inv-1", None, "info", "guest", Some("m0".into()), None).await.unwrap();

        let result = store.copy("/src", "/dest", "inv-1", Some(5), 100, None).await.unwrap();
        assert_eq!(result.copied, 0);
        assert_eq!(result.next_cursor, 5);
        let dest = store.read("/dest", None, 100).await.unwrap();
        assert!(dest.entries.is_empty());
    }

    #[tokio::test]
    async fn copy_preserves_level_and_data() {
        let (_d, store) = test_store().await;
        store
            .print("/src", "inv-1", None, "warn", "guest", Some("m".into()), Some(json!({ "n": 1 })))
            .await
            .unwrap();

        store.copy("/src", "/dest", "inv-1", None, 100, None).await.unwrap();
        let dest = store.read("/dest", None, 100).await.unwrap();
        assert_eq!(dest.entries[0].level, "warn");
        assert_eq!(dest.entries[0].data, Some(json!({ "n": 1 })));
    }

    #[tokio::test]
    async fn message_and_data_round_trip() {
        let (_d, store) = test_store().await;
        store
            .print("/a", "i", Some("run-1"), "warn", "command", Some("careful".into()), Some(json!({"n": 7})))
            .await
            .unwrap();
        let r = store.read("/a", None, 10).await.unwrap();
        let e = &r.entries[0];
        assert_eq!(e.level, "warn");
        assert_eq!(e.source, "command");
        assert_eq!(e.run_id.as_deref(), Some("run-1"));
        assert_eq!(e.message.as_deref(), Some("careful"));
        assert_eq!(e.data, Some(json!({"n": 7})));
    }

    #[tokio::test]
    async fn absent_message_and_data_round_trip_as_none() {
        let (_d, store) = test_store().await;
        store.print("/a", "i", None, "info", "guest", None, None).await.unwrap();
        let r = store.read("/a", None, 10).await.unwrap();
        assert_eq!(r.entries[0].message, None);
        assert_eq!(r.entries[0].data, None);
        assert_eq!(r.entries[0].run_id, None);
    }

    #[tokio::test]
    async fn tail_returns_immediately_when_entries_already_exist() {
        let (_d, store) = test_store().await;
        store.print("/a", "i", None, "info", "guest", Some("hi".into()), None).await.unwrap();
        let started = tokio::time::Instant::now();
        let r = store.tail("/a", None, 10, Some(30)).await.unwrap();
        assert_eq!(r.entries.len(), 1);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn tail_with_no_wait_returns_empty_immediately() {
        let (_d, store) = test_store().await;
        let started = tokio::time::Instant::now();
        let r = store.tail("/never/printed", None, 10, None).await.unwrap();
        assert!(r.entries.is_empty());
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[tokio::test]
    async fn tail_picks_up_an_entry_written_after_the_poll_started() {
        let (_d, store) = test_store().await;
        let store2 = store.clone();
        let writer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            store2.print("/a", "i", None, "info", "guest", Some("late".into()), None).await.unwrap();
        });
        let r = store.tail("/a", None, 10, Some(5)).await.unwrap();
        assert_eq!(r.entries.len(), 1);
        assert_eq!(r.entries[0].message.as_deref(), Some("late"));
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn clear_drops_from_the_front_and_reports_the_removed_count() {
        let (_d, store) = test_store().await;
        for i in 0..5 {
            store.print("/a", "i", None, "info", "guest", Some(format!("m{i}")), None).await.unwrap();
        }
        let removed = store.clear("/a", Some(3)).await.unwrap();
        assert_eq!(removed, 2);
        let r = store.read("/a", None, 100).await.unwrap();
        assert_eq!(r.entries.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![3, 4, 5]);
        assert_eq!(r.first_seq, 3);
    }

    #[tokio::test]
    async fn clear_with_no_before_seq_drops_everything_retained() {
        let (_d, store) = test_store().await;
        for i in 0..3 {
            store.print("/a", "i", None, "info", "guest", Some(format!("m{i}")), None).await.unwrap();
        }
        let removed = store.clear("/a", None).await.unwrap();
        assert_eq!(removed, 3);
        let r = store.read("/a", None, 100).await.unwrap();
        assert!(r.entries.is_empty());

        // A subsequent print must not collide with the cleared range.
        let seq = store.print("/a", "i", None, "info", "guest", Some("new".into()), None).await.unwrap();
        assert_eq!(seq, 4);
    }

    #[tokio::test]
    async fn clear_is_a_no_op_when_before_seq_is_not_past_first_seq() {
        let (_d, store) = test_store().await;
        store.print("/a", "i", None, "info", "guest", Some("m".into()), None).await.unwrap();
        let removed = store.clear("/a", Some(1)).await.unwrap();
        assert_eq!(removed, 0);
    }

    #[tokio::test]
    async fn current_next_seq_is_one_for_a_console_never_written_to() {
        let (_d, store) = test_store().await;
        assert_eq!(store.current_next_seq("/never/printed").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn current_next_seq_advances_with_writes_and_is_not_confused_by_a_shared_prefix() {
        let (_d, store) = test_store().await;
        store.print("/pkg/foo", "i", None, "info", "guest", Some("a".into()), None).await.unwrap();
        store.print("/pkg/foo", "i", None, "info", "guest", Some("b".into()), None).await.unwrap();
        // A sibling console whose ref happens to share "/pkg/foo" as a
        // literal string prefix, written to more recently — a prefix-based
        // `list()` lookup could mistake this for "/pkg/foo"'s own summary.
        store.print("/pkg/foobar", "i", None, "info", "guest", Some("z".into()), None).await.unwrap();

        assert_eq!(store.current_next_seq("/pkg/foo").await.unwrap(), 3);
    }

    #[tokio::test]
    async fn list_reflects_recent_writes_and_respects_prefix() {
        let (_d, store) = test_store().await;
        store.print("/pkg/a", "i", None, "info", "guest", Some("x".into()), None).await.unwrap();
        store.print("/pkg/b", "i", None, "info", "guest", Some("y".into()), None).await.unwrap();
        store.print("/other", "i", None, "info", "guest", Some("z".into()), None).await.unwrap();

        let all = store.list(None, 100).await.unwrap();
        assert_eq!(all.len(), 3);

        let pkg_only = store.list(Some("/pkg"), 100).await.unwrap();
        assert_eq!(pkg_only.len(), 2);
        assert!(pkg_only.iter().all(|c| c.action_ref.starts_with("/pkg")));
    }

    #[tokio::test]
    async fn eviction_caps_retained_entries_and_tracks_dropped_count() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).await.unwrap();
        let config = Arc::new(ConfigService::open_in(dir.path()).unwrap());
        config.patch(json!({ "console_max_entries": 10 })).unwrap();
        let store = ConsoleStore::new(db, config);
        store.ensure_schema().await.unwrap();

        for i in 0..25 {
            store.print("/a", "i", None, "info", "guest", Some(format!("m{i}")), None).await.unwrap();
        }

        let r = store.read("/a", None, 1000).await.unwrap();
        assert!(r.entries.len() <= 10, "retained {} entries, cap is 10", r.entries.len());
        assert!(r.dropped > 0, "expected some entries to have been evicted");
        assert_eq!(r.first_seq, r.entries.first().unwrap().seq);

        // The oldest retained entries are contiguous with first_seq — no gap.
        let seqs: Vec<i64> = r.entries.iter().map(|e| e.seq).collect();
        assert_eq!(seqs[0], r.first_seq);
    }

    #[tokio::test]
    async fn sweep_expired_removes_only_stale_consoles() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).await.unwrap();
        let config = Arc::new(ConfigService::open_in(dir.path()).unwrap());
        config.patch(json!({ "console_ttl_days": 1 })).unwrap();
        let store = ConsoleStore::new(db.clone(), config.clone());
        store.ensure_schema().await.unwrap();

        store.print("/fresh", "i", None, "info", "guest", Some("m".into()), None).await.unwrap();
        store.print("/stale", "i", None, "info", "guest", Some("m".into()), None).await.unwrap();

        // Backdate /stale's last_write past the TTL directly.
        let conn = db.connect().await.unwrap();
        let old = (Utc::now() - chrono::Duration::days(3)).to_rfc3339();
        conn.execute(
            "UPDATE consoles SET last_write = ?1 WHERE action_ref = '/stale'",
            libsql::params![old],
        )
        .await
        .unwrap();

        let swept = store.sweep_expired().await.unwrap();
        assert_eq!(swept, 1);

        let stale_after = store.read("/stale", None, 10).await.unwrap();
        assert!(stale_after.entries.is_empty());
        let fresh_after = store.read("/fresh", None, 10).await.unwrap();
        assert_eq!(fresh_after.entries.len(), 1);
    }
}
