//! Detached (`action-start`) invocations: start, stop, and poll a run that
//! outlives the call that started it. See `docs/async-actions-plan.md`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{json, Value};
use solx_console::{console, invocations};
use solx_surface::error::{Result, SolxError};
use solx_surface::path::full_ref;
use uuid::Uuid;

use crate::LocalActionManager;

/// Whether this process is a long-lived host (`solx-server`, `solx-mcp`) —
/// the only kind that can safely run a detached `action-start` invocation.
/// A `tokio::spawn`'d task is not cancelled when an axum handler future
/// drops, which is what lets detachment survive client disconnect; but a
/// CLI process exits the moment `exec` returns, which would kill a
/// newly-started task before it could ever be polled. See
/// `docs/async-actions-plan.md` §6.
///
/// A process-wide flag rather than a field threaded through
/// `LocalActionManager::open` because the fact is about the *process*, not
/// about any one manager instance — every construction site (tests
/// included) would otherwise have to carry it.
static LONG_LIVED_HOST: AtomicBool = AtomicBool::new(false);

/// Call once at startup from a long-lived host's `main()` — `solx-server`
/// and `solx-mcp` do this; `solx-cli` deliberately never does.
pub fn set_long_lived_host(long_lived: bool) {
    LONG_LIVED_HOST.store(long_lived, Ordering::Relaxed);
}

fn is_long_lived_host() -> bool {
    LONG_LIVED_HOST.load(Ordering::Relaxed)
}

impl LocalActionManager {
    /// Start `path/name` detached: returns as soon as an `invocations` row
    /// exists, while the actual execution runs on its own `tokio::spawn`ed
    /// task. See `docs/async-actions-plan.md` §4.
    ///
    /// Refuses unless this process has called [`set_long_lived_host`] — a
    /// spawned task is not what keeps a process alive, so under a CLI
    /// process (which exits the instant `exec` returns) a "successfully
    /// started" invocation would be killed before it could ever be polled.
    pub async fn start_invocation(&self, path: &str, name: &str, params: Value) -> Result<Value> {
        if !is_long_lived_host() {
            return Err(SolxError::Exec(
                "action-start requires a long-lived host (solx-server or solx-mcp). \
                 This process exits when exec returns, which would kill the invocation \
                 before it could be polled. Point the CLI at a running server, or use exec."
                    .into(),
            ));
        }

        // Resolved (and validated) up front, outside the spawned task, so a
        // bad path/name/action_type fails the `start` call itself rather
        // than silently producing a row that immediately goes `failed`.
        let action = self.get_unmasked(path, name).await?;
        let action_ref = full_ref(&action.path, &action.name)?;
        if action.action_type.is_none() {
            return Err(SolxError::Exec(format!(
                "action {action_ref} has no action_type to execute"
            )));
        }

        let invocation_id = Uuid::new_v4().to_string();
        // Captured before the row exists, so a poller can jump straight to
        // this run's own console output — see `ConsoleStore::current_next_seq`.
        let console_seq_start = self.console.current_next_seq(&action_ref).await?;
        self.invocations.create(&invocation_id, &action_ref, console_seq_start).await?;

        let manager = self.self_arc()?;
        let path = path.to_string();
        let name = name.to_string();
        let task_id = invocation_id.clone();
        let handle = tokio::spawn(async move {
            let outcome = manager.exec_as_with(&path, &name, params, None, Some(&task_id)).await;
            let (status, result, error) = match outcome {
                Ok(r) if r.success => (invocations::status::OK, Some(r.result), None),
                Ok(r) => (invocations::status::FAILED, Some(r.result), r.message),
                Err(e) => (invocations::status::FAILED, None, Some(e.to_string())),
            };
            // Best-effort: if this write fails there is nothing left to do
            // with the error — the task is finished either way.
            let _ = manager.invocations.finish(&task_id, status, result, error).await;
            if let Ok(mut running) = manager.running.lock() {
                running.remove(&task_id);
            }
        });
        if let Ok(mut running) = self.running.lock() {
            running.insert(invocation_id.clone(), handle.abort_handle());
        }

        Ok(json!({
            "invocation_id": invocation_id,
            "action_ref": action_ref,
            "console_seq_start": console_seq_start,
        }))
    }

    /// Request that a detached invocation stop. Cooperative first: sets the
    /// flag a running Command child (via the loopback `/cancelled` route)
    /// or a running Wasm/Script/Internal caller (via `action-cancelled`)
    /// can observe and exit on its own. If it hasn't gone terminal within
    /// `grace_secs` (default `stop_grace_secs`), the task is force-aborted —
    /// dropping the future, which reaps a Command child via the existing
    /// `kill_on_drop(true)` and unwinds a Wasm guest's fiber.
    ///
    /// Returns immediately; does **not** block for the grace period unless
    /// `force` is set.
    ///
    /// Caveats:
    /// - Carried over unchanged from `exec::run_command`: force-abort kills
    ///   the *shell* a Command action spawned, not its descendants.
    /// - **Force-abort is process-local.** `cancel_requested` lives in the
    ///   shared `invocations` table, so the cooperative signal reaches the
    ///   running task regardless of which process calls `stop`. The
    ///   `AbortHandle` in [`Self::running`], though, only exists in the
    ///   memory of whichever process's [`Self::start_invocation`] actually
    ///   spawned the task. Calling `stop` on a *different* process's
    ///   `LocalActionManager` (e.g. a CLI run locally against the same
    ///   appdata dir a `solx-server` is also using, rather than proxied to
    ///   it via `server_url`) can mark the row `cancelled` in the database
    ///   without ever reaching the real task. The documented usage pattern
    ///   — a CLI pointed at `solx-server` via `server_url`, so `stop`/`poll`
    ///   execute inside the same process that ran `start` — doesn't hit
    ///   this; two independent `LocalActionManager`s sharing one database
    ///   file do.
    ///
    /// Cooperative exit is the reliable path; force is the backstop, not a
    /// guarantee.
    pub async fn stop_invocation(&self, invocation_id: &str, force: bool, grace_secs: Option<u64>) -> Result<Value> {
        let Some(inv) = self.invocations.request_cancel(invocation_id).await? else {
            return Err(SolxError::NotFound(format!("invocation {invocation_id}")));
        };
        if invocations::is_terminal(&inv.status) {
            return Ok(inv.to_json());
        }

        if force {
            self.force_abort(invocation_id).await?;
            let inv = self
                .invocations
                .get(invocation_id)
                .await?
                .ok_or_else(|| SolxError::NotFound(format!("invocation {invocation_id}")))?;
            return Ok(inv.to_json());
        }

        let grace = Duration::from_secs(grace_secs.unwrap_or_else(|| self.config.stop_grace_secs()));
        let manager = self.self_arc()?;
        let id = invocation_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            let _ = manager.force_abort(&id).await;
        });

        Ok(inv.to_json())
    }

    /// Abort the task behind `invocation_id` if it is still running, and
    /// mark the row `cancelled`. A no-op (not an error) if the task already
    /// finished on its own — [`invocations::InvocationStore::mark_aborted`]
    /// guards against exactly that race.
    async fn force_abort(&self, invocation_id: &str) -> Result<()> {
        if let Ok(mut running) = self.running.lock() {
            if let Some(handle) = running.remove(invocation_id) {
                handle.abort();
            }
        }
        self.invocations.mark_aborted(invocation_id).await
    }

    /// Current status of a detached invocation, optionally long-polling
    /// (reusing `console`'s 250ms/60s long-poll cadence) until it goes
    /// terminal.
    pub async fn poll_invocation(&self, invocation_id: &str, wait_secs: Option<u64>) -> Result<Value> {
        let wait = wait_secs.map(|s| Duration::from_secs(s.min(console::MAX_TAIL_WAIT_SECS)));
        let deadline = wait.map(|w| tokio::time::Instant::now() + w);

        loop {
            let inv = self
                .invocations
                .get(invocation_id)
                .await?
                .ok_or_else(|| SolxError::NotFound(format!("invocation {invocation_id}")))?;
            if invocations::is_terminal(&inv.status) {
                return Ok(inv.to_json());
            }
            match deadline {
                Some(d) => {
                    let now = tokio::time::Instant::now();
                    if now >= d {
                        return Ok(inv.to_json());
                    }
                    tokio::time::sleep(console::TAIL_POLL_INTERVAL.min(d - now)).await;
                }
                None => return Ok(inv.to_json()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use solx_config::ConfigService;
    use solx_surface::entities::{ActionInput, ActionType};
    use solx_surface::managers::ActionManager;

    // ── async actions: start/stop/poll ───────────────────────────────────────
    //
    // `#[serial]` on every test that touches `set_long_lived_host` —
    // `LONG_LIVED_HOST` is a process-wide static, and tests in this file run
    // in parallel threads within one process by default (`serial_test` is
    // already a dev-dependency for exactly this reason elsewhere).

    use serial_test::serial;

    /// Echoes a bare number immediately — valid JSON on both cmd.exe and sh
    /// without any quote-handling differences between the two, mirroring
    /// `exec_command_runs_fn_name_directly` above.
    fn quick_ok() -> &'static str {
        "echo 42"
    }

    /// A shell snippet with no natural end — internal to the shell (a `for`/
    /// `while` loop), not an external process, for the same reason
    /// `tests/command_nonblocking.rs`'s `a_hanging_command_hits_its_timeout`
    /// avoids one: `kill_on_drop` only reaps the shell we spawned, not an
    /// external descendant, which would otherwise leak and hold the test
    /// harness's pipes open.
    fn hangs_forever() -> &'static str {
        if cfg!(windows) {
            "for /L %i in (1,1,2000000000) do @rem"
        } else {
            "while :; do :; done"
        }
    }

    async fn save_command(m: &LocalActionManager, cfg: &ConfigService, name: &str, cmd: &str) {
        allow_command(cfg, name, cmd);
        m.save(
            "/t",
            name,
            ActionInput {
                action_type: Some(ActionType::Command),
                fn_name: Some(name.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn start_invocation_refuses_without_a_long_lived_host() {
        set_long_lived_host(false);
        let (_d, cfg, m) = setup_wired().await;
        save_command(&m, &cfg, "quick", quick_ok()).await;

        let err = m
            .start_invocation("/t", "quick", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("requires a long-lived host"), "{err}");
    }

    #[tokio::test]
    #[serial]
    async fn start_then_poll_reaches_ok_with_the_started_actions_result() {
        set_long_lived_host(true);
        let (_d, cfg, m) = setup_wired().await;
        save_command(&m, &cfg, "quick", quick_ok()).await;

        let started = m.start_invocation("/t", "quick", serde_json::json!({})).await.unwrap();
        let id = started["invocation_id"].as_str().unwrap().to_string();
        assert_eq!(started["action_ref"], "/t/quick");
        assert_eq!(started["console_seq_start"], 1);

        let polled = m.poll_invocation(&id, Some(10)).await.unwrap();
        assert_eq!(polled["status"], invocations::status::OK);
        assert_eq!(polled["result"], serde_json::json!(42));
        assert_eq!(polled["invocation_id"], id);
        assert!(polled["finished_at"].is_string());

        set_long_lived_host(false);
    }

    #[tokio::test]
    #[serial]
    async fn poll_invocation_of_an_unknown_id_is_not_found() {
        let (_d, _cfg, m) = setup_wired().await;
        let err = m.poll_invocation("no-such-id", None).await.unwrap_err();
        assert!(matches!(err, SolxError::NotFound(_)), "{err:?}");
    }

    #[tokio::test]
    #[serial]
    async fn stop_invocation_of_an_unknown_id_is_not_found() {
        let (_d, _cfg, m) = setup_wired().await;
        let err = m.stop_invocation("no-such-id", false, None).await.unwrap_err();
        assert!(matches!(err, SolxError::NotFound(_)), "{err:?}");
    }

    #[tokio::test]
    #[serial]
    async fn stop_force_aborts_a_hanging_command_promptly() {
        set_long_lived_host(true);
        let (_d, cfg, m) = setup_wired().await;
        save_command(&m, &cfg, "hangs", hangs_forever()).await;

        let started = m.start_invocation("/t", "hangs", serde_json::json!({})).await.unwrap();
        let id = started["invocation_id"].as_str().unwrap().to_string();

        // Give the child a moment to actually be spawned before stopping it.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let start = std::time::Instant::now();
        let stopped = m.stop_invocation(&id, true, None).await.unwrap();
        assert_eq!(stopped["status"], invocations::status::CANCELLED);
        // Force must not wait out the (effectively infinite) hang.
        assert!(start.elapsed() < Duration::from_secs(10), "{:?}", start.elapsed());

        let polled = m.poll_invocation(&id, None).await.unwrap();
        assert_eq!(polled["status"], invocations::status::CANCELLED);

        set_long_lived_host(false);
    }

    #[tokio::test]
    #[serial]
    async fn stop_cooperative_returns_cancelling_without_blocking_then_force_finishes_it() {
        set_long_lived_host(true);
        let (_d, cfg, m) = setup_wired().await;
        save_command(&m, &cfg, "hangs", hangs_forever()).await;

        let started = m.start_invocation("/t", "hangs", serde_json::json!({})).await.unwrap();
        let id = started["invocation_id"].as_str().unwrap().to_string();
        tokio::time::sleep(Duration::from_millis(200)).await;

        // A long grace period the test does not wait out — proves the
        // cooperative call itself does not block for it.
        let start = std::time::Instant::now();
        let stopped = m.stop_invocation(&id, false, Some(30)).await.unwrap();
        assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());
        assert_eq!(stopped["status"], invocations::status::CANCELLING);
        assert!(m.invocations().is_cancelled(&id).await.unwrap());

        // Clean up rather than let the 30s watcher fire on its own.
        let forced = m.stop_invocation(&id, true, None).await.unwrap();
        assert_eq!(forced["status"], invocations::status::CANCELLED);

        set_long_lived_host(false);
    }
}
