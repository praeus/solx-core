//! End-to-end coverage for the async WASM host.
//!
//! The property under test is that a nested `action-exec` chain costs no
//! OS threads. Before the host moved to wasmtime fibers, every nesting level
//! sat inside its own `spawn_blocking` task and `block_on`'d the level
//! beneath it, so depth N pinned N blocking-pool threads. `nesting_survives_a_single_blocking_thread`
//! is the direct regression test: it runs a deep chain on a runtime with
//! `max_blocking_threads(1)`, which under the old design deadlocked at
//! depth 2.
//!
//! These tests need a real component, so they build `tests/fixtures/nesting-guest`
//! for `wasm32-wasip2` on first use. If that target isn't installed the
//! tests skip with an explanatory message rather than failing.

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde_json::json;
use solx_actions::LocalActionManager;
use solx_surface::entities::{ActionInput, ActionType};
use solx_surface::managers::{ActionManager, DocManager, FileStore, TypeManager};

// ── Fixture build ────────────────────────────────────────────────────────────

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nesting-guest")
}

/// Build the guest once per test binary. `Ok(None)` means "skip" — the
/// toolchain isn't available — as distinct from a real build failure.
fn guest_wasm() -> Option<&'static Vec<u8>> {
    static GUEST: OnceLock<Option<Vec<u8>>> = OnceLock::new();
    GUEST
        .get_or_init(|| {
            let dir = fixture_dir();
            let out = Command::new(env!("CARGO"))
                .args(["build", "--target", "wasm32-wasip2", "--release"])
                .current_dir(&dir)
                // Don't inherit the outer `cargo test` invocation's job
                // server / target dir, which would deadlock the nested build.
                .env_remove("CARGO_MAKEFLAGS")
                .env_remove("RUSTFLAGS")
                .output();

            let out = match out {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("SKIP: could not run cargo for the wasm fixture: {e}");
                    return None;
                }
            };
            if !out.status.success() {
                eprintln!(
                    "SKIP: wasm fixture build failed (is the wasm32-wasip2 target installed? \
                     `rustup target add wasm32-wasip2`)\n{}",
                    String::from_utf8_lossy(&out.stderr)
                );
                return None;
            }
            let wasm = dir.join("target/wasm32-wasip2/release/solx_nesting_guest.wasm");
            match std::fs::read(&wasm) {
                Ok(b) => Some(b),
                Err(e) => {
                    eprintln!("SKIP: built fixture missing at {}: {e}", wasm.display());
                    None
                }
            }
        })
        .as_ref()
}

/// Skip the calling test (returning early) when the fixture is unavailable.
macro_rules! guest_or_skip {
    () => {
        match guest_wasm() {
            Some(w) => w,
            None => return,
        }
    };
}

// ── Harness ──────────────────────────────────────────────────────────────────

struct Harness {
    _dir: tempfile::TempDir,
    actions: Arc<LocalActionManager>,
}

impl Harness {
    async fn new(guest: &[u8]) -> Harness {
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

        // Publish the component where `load_wasm_bytes` looks for it.
        files
            .put(&solx_files::shared_action_file_path("guest.wasm"), guest.to_vec())
            .await
            .unwrap();

        let actions = Arc::new(
            LocalActionManager::open(&dir.path().join("actions.db"), cfg, types, docs, files)
                .await
                .unwrap(),
        );
        actions.set_self_ref(Arc::downgrade(&actions));

        actions
            .post(
                "/t",
                "guest",
                ActionInput {
                    action_type: Some(ActionType::Wasm),
                    bin_name: Some("guest.wasm".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        Harness { _dir: dir, actions }
    }

    /// Register a second action pointing at the same artifact, with its own
    /// `action_config` (used for the timeout tests).
    async fn post_variant(&self, name: &str, config: serde_json::Value) {
        self.actions
            .post(
                "/t",
                name,
                ActionInput {
                    action_type: Some(ActionType::Wasm),
                    bin_name: Some("guest.wasm".into()),
                    action_config: Some(config),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }

    async fn nest(&self, depth: u64) -> solx_surface::entities::ActionExecResult {
        self.actions
            .exec(
                "/t",
                "guest",
                json!({ "mode": "nest", "depth": depth, "self_ref": "/t/guest" }),
            )
            .await
            .unwrap()
    }
}

// ── The regression test ──────────────────────────────────────────────────────

/// **The point of the whole change.** A 64-deep chain on a runtime with a
/// single blocking thread. Each level used to hold one blocking-pool thread
/// while awaiting the next, so this deadlocked at depth 2; with fiber-based
/// async a suspended level holds no thread at all.
#[test]
fn nesting_survives_a_single_blocking_thread() {
    let guest = guest_or_skip!().clone();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async move {
        let h = Harness::new(&guest).await;
        let res = tokio::time::timeout(Duration::from_secs(120), h.nest(64))
            .await
            .expect("deep nesting deadlocked — a level is still holding a thread");
        assert!(res.success, "nesting failed: {:?}", res.message);
        assert_eq!(
            res.result["reached"], 64,
            "every level of the chain should have run"
        );
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deep_nesting_returns_the_right_depth() {
    let guest = guest_or_skip!();
    let h = Harness::new(guest).await;

    for depth in [0u64, 1, 8, 32] {
        let res = h.nest(depth).await;
        assert!(res.success, "depth {depth} failed: {:?}", res.message);
        assert_eq!(res.result["reached"], depth, "depth {depth}");
    }
}

/// Depth × concurrency is what actually exhausted the old thread budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_chains_all_complete() {
    let guest = guest_or_skip!();
    let h = Arc::new(Harness::new(guest).await);

    let mut tasks = Vec::new();
    for _ in 0..32 {
        let h = h.clone();
        tasks.push(tokio::spawn(async move { h.nest(8).await }));
    }
    for t in tasks {
        let res = t.await.unwrap();
        assert!(res.success, "concurrent chain failed: {:?}", res.message);
        assert_eq!(res.result["reached"], 8);
    }
}

// ── Preemption ───────────────────────────────────────────────────────────────

/// A compute-bound guest never awaits, so fibers alone don't save the
/// worker — epoch interruption makes it yield, and the wall-clock deadline
/// eventually ends it. Both halves are asserted: the spinner is terminated,
/// *and* unrelated work keeps making progress while it spins.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_spinning_guest_is_preempted_and_then_timed_out() {
    let guest = guest_or_skip!();
    let h = Arc::new(Harness::new(guest).await);
    h.post_variant("spinner", json!({ "timeout_secs": 2 })).await;

    let spinner = {
        let h = h.clone();
        tokio::spawn(async move { h.actions.exec("/t", "spinner", json!({ "mode": "spin" })).await })
    };

    // While the guest spins, the executor must still be responsive. Without
    // epoch yielding this starves on a 2-worker runtime.
    let mut ticks = 0;
    let deadline = Instant::now() + Duration::from_millis(900);
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
        ticks += 1;
    }
    assert!(ticks >= 5, "runtime was starved by the spinning guest (ticks={ticks})");

    let res = tokio::time::timeout(Duration::from_secs(30), spinner)
        .await
        .expect("spinning guest was never terminated")
        .unwrap()
        .unwrap();
    assert!(!res.success, "a spinning guest should not report success");
    assert!(
        res.message.as_deref().unwrap_or_default().contains("timed out"),
        "expected a timeout message, got {:?}",
        res.message
    );
}

// ── Component cache ──────────────────────────────────────────────────────────

/// The component used to be recompiled from scratch on every execution.
/// The second run of the same artifact should be dramatically cheaper.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_component_is_compiled_once_and_reused() {
    let guest = guest_or_skip!();
    let h = Harness::new(guest).await;

    let t0 = Instant::now();
    assert!(h.nest(0).await.success);
    let cold = t0.elapsed();

    let t1 = Instant::now();
    assert!(h.nest(0).await.success);
    let warm = t1.elapsed();

    assert!(
        warm * 3 < cold,
        "second execution should reuse the cached component \
         (cold={cold:?}, warm={warm:?})"
    );
}
