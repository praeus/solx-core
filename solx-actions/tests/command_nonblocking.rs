//! `Command` actions must not park the runtime.
//!
//! `run_command` used to be a plain `fn` using `std::process::Command` and
//! `wait_with_output()`, called from `async fn exec_as` with no `.await` and
//! no `spawn_blocking` — so every running command held an async *worker*
//! thread, of which there are only `num_cpus`. A handful of concurrent
//! commands could wedge the whole process, HTTP routes included.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use solx_actions::LocalActionManager;
use solx_surface::entities::{ActionInput, ActionType};
use solx_surface::managers::{ActionManager, DocManager, FileStore, TypeManager};

/// A shell snippet that sleeps for roughly two seconds without needing a TTY.
fn sleep_2s() -> &'static str {
    if cfg!(windows) {
        "ping -n 3 127.0.0.1 >nul && echo done"
    } else {
        "sleep 2 && echo done"
    }
}

/// A command that exits immediately without ever reading stdin.
fn ignores_stdin() -> &'static str {
    "echo done"
}

async fn setup() -> (tempfile::TempDir, Arc<LocalActionManager>) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Arc::new(solx_config::ConfigService::open_in(dir.path()).unwrap());
    let types: Arc<dyn TypeManager> = Arc::new(
        solx_types::LocalTypeManager::open(&dir.path().join("types.db"))
            .await
            .unwrap(),
    );
    let docs: Arc<dyn DocManager> = Arc::new(
        solx_docs::LocalDocManager::open(
            &dir.path().join("docs.db"),
            &dir.path().join("idx"),
            types.clone(),
        )
        .await
        .unwrap(),
    );
    let files: Arc<dyn FileStore> =
        Arc::new(solx_files::LocalFileStore::new(dir.path().join("files")));
    let actions = Arc::new(
        LocalActionManager::open(&dir.path().join("actions.db"), cfg, types, docs, files)
            .await
            .unwrap(),
    );
    actions.set_self_ref(Arc::downgrade(&actions));
    (dir, actions)
}

async fn post_cmd(
    actions: &LocalActionManager,
    name: &str,
    cmd: &str,
    config: Option<serde_json::Value>,
) {
    actions
        .post(
            "/t",
            name,
            ActionInput {
                action_type: Some(ActionType::Command),
                fn_name: Some(cmd.into()),
                action_config: config,
                ..Default::default()
            },
        )
        .await
        .unwrap();
}

/// Four concurrent 2-second commands on a **single-worker** runtime. If the
/// child wait blocks the worker they serialize into ~8s; concurrently they
/// finish in a shade over 2s.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn concurrent_commands_do_not_serialize_on_the_worker() {
    let (_d, actions) = setup().await;
    post_cmd(&actions, "slow", sleep_2s(), None).await;

    let start = Instant::now();
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let a = actions.clone();
        tasks.push(tokio::spawn(async move { a.exec("/t", "slow", json!({})).await }));
    }
    for t in tasks {
        assert!(t.await.unwrap().unwrap().success);
    }
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(6),
        "commands serialized on the worker thread (took {elapsed:?} for 4x ~2s in parallel)"
    );
}

/// While a command runs, the runtime must keep making progress — the direct
/// symptom of the old inline `wait_with_output()`.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn the_runtime_stays_responsive_during_a_command() {
    let (_d, actions) = setup().await;
    post_cmd(&actions, "slow", sleep_2s(), None).await;

    let a = actions.clone();
    let running = tokio::spawn(async move { a.exec("/t", "slow", json!({})).await });

    let mut ticks = 0;
    while !running.is_finished() && ticks < 100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        ticks += 1;
    }
    assert!(ticks >= 5, "runtime was starved while a command ran (ticks={ticks})");
    assert!(running.await.unwrap().unwrap().success);
}

/// A params payload larger than the OS pipe buffer (~64KB) sent to a child
/// that never drains stdin. Writing it all up front before reading stdout
/// deadlocked; the write now runs concurrently with the wait, and a broken
/// pipe is tolerated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_stdin_payload_does_not_deadlock() {
    let (_d, actions) = setup().await;
    post_cmd(&actions, "ignores-stdin", ignores_stdin(), None).await;

    let big = json!({ "blob": "x".repeat(512 * 1024) });
    let res = tokio::time::timeout(Duration::from_secs(30), actions.exec("/t", "ignores-stdin", big))
        .await
        .expect("writing a large payload to a child that ignores stdin deadlocked")
        .unwrap();
    assert!(res.success, "{:?}", res.message);
}

/// A command that never exits is bounded by `action_config.timeout_secs`
/// rather than hanging forever.
///
/// The long-runner is deliberately *internal to the shell* (a `for` loop in
/// `cmd`, a `while` loop in `sh`) rather than an external sleep. `kill_on_drop`
/// terminates the shell we spawned but not its descendants, so
/// `cmd /C ping ...` would leave an orphaned `ping.exe` holding the inherited
/// stdout pipe open — which hangs the *test harness*, not the action. See the
/// caveat on `kill_on_drop` in `exec.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hanging_command_hits_its_timeout() {
    let (_d, actions) = setup().await;
    let forever = if cfg!(windows) {
        "for /L %i in (1,1,2000000000) do @rem"
    } else {
        "while :; do :; done"
    };
    post_cmd(&actions, "hangs", forever, Some(json!({ "timeout_secs": 1 }))).await;

    let start = Instant::now();
    let err = tokio::time::timeout(Duration::from_secs(30), actions.exec("/t", "hangs", json!({})))
        .await
        .expect("hanging command was never bounded")
        .unwrap_err();
    assert!(err.to_string().contains("timed out"), "{err}");
    assert!(start.elapsed() < Duration::from_secs(15));
}
