//! `script-exec` — parse and immediately run a raw solx script string,
//! reusing the same `solx-scripts` interpreter and `exec`/`json` stage
//! grammar as a stored `Script`-typed action (see `crate::script`).
//!
//! Unlike a stored `Script` action there is no `bin_name` to load from the
//! file store: the caller supplies the script source inline and gets its
//! result back synchronously. This exists so a script can be authored and
//! tried out (from the CLI, MCP, or another action) without first saving it
//! as an action row.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::caller::Caller;
use crate::script::ActionCommandRunner;
use crate::LocalActionManager;

use super::require_str;

/// Caller identity attributed to a script run through this action when it
/// has no caller of its own — i.e. invoked directly by the CLI, MCP, or the
/// HTTP route rather than nested inside another action's `exec` stage or a
/// WASM guest's recursive `action-exec`. Used only for console-log
/// attribution; a `None` config gives it no secret keys to resolve, exactly
/// like calling `get-secret` with no action caller at all.
const ANON_CALLER_REF: &str = "/builtin/script/exec";

/// Run `params.script` immediately and return its result. `params.params`
/// (default `{}`) seeds the interpreter's `$params` variable exactly like a
/// stored `Script` action's own exec params, so a script written and tested
/// through this action behaves identically once saved as one.
///
/// Nested `exec` stages inside the script run as `caller` when this call has
/// one (a script action or WASM guest invoking `script-exec` recursively),
/// so they reach exactly the same secrets that caller could reach directly —
/// otherwise they run as the caller-less identity above.
pub(super) async fn exec(
    params: &Value,
    local: &Arc<LocalActionManager>,
    caller: Option<&Caller>,
) -> Result<Value, String> {
    let source = require_str(params, "script")?;
    let script_params = params
        .get("params")
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));
    let timeout_secs = params.get("timeout_secs").and_then(Value::as_u64);

    let runner = ActionCommandRunner {
        actions: local.clone(),
        caller: caller
            .cloned()
            .unwrap_or_else(|| Caller::from_action(ANON_CALLER_REF, None)),
    };

    let initial = HashMap::from([("params".to_string(), script_params)]);
    let run = solx_scripts::execute_script_with_vars(&runner, source, initial);
    let budget = Duration::from_secs(timeout_secs.unwrap_or(crate::exec::DEFAULT_TIMEOUT_SECS));

    tokio::time::timeout(budget, run)
        .await
        .map_err(|_| "script-exec: script timed out".to_string())?
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn test_local() -> (tempfile::TempDir, Arc<LocalActionManager>) {
        let dir = tempfile::tempdir().unwrap();
        let types: Arc<dyn solx_surface::managers::TypeManager> = Arc::new(
            solx_types::LocalTypeManager::open(&dir.path().join("types.db"))
                .await
                .unwrap(),
        );
        let docs: Arc<dyn solx_surface::managers::DocManager> = Arc::new(
            solx_docs::LocalDocManager::open(&dir.path().join("docs.db"), types.clone())
                .await
                .unwrap(),
        );
        let files: Arc<dyn solx_surface::managers::FileStore> =
            Arc::new(solx_files::LocalFileStore::new(dir.path().join("files")));
        let cfg = Arc::new(solx_config::ConfigService::open_in(dir.path()).unwrap());
        let local = Arc::new(
            LocalActionManager::open(&dir.path().join("actions.db"), cfg, types, docs, files)
                .await
                .unwrap(),
        );
        local.set_self_ref(Arc::downgrade(&local));
        (dir, local)
    }

    #[tokio::test]
    async fn runs_a_json_only_script_and_returns_its_result() {
        let (_d, local) = test_local().await;
        let v = exec(&json!({"script": "json 5"}), &local, None).await.unwrap();
        assert_eq!(v, Value::from(5));
    }

    #[tokio::test]
    async fn params_are_seeded_as_the_dollar_params_variable() {
        let (_d, local) = test_local().await;
        let v = exec(
            &json!({"script": r#"json '"$params.mode"'"#, "params": {"mode": "fast"}}),
            &local,
            None,
        )
        .await
        .unwrap();
        assert_eq!(v, Value::from("fast"));
    }

    #[tokio::test]
    async fn missing_script_param_errors() {
        let (_d, local) = test_local().await;
        let err = exec(&json!({}), &local, None).await.unwrap_err();
        assert!(err.contains("missing required param: script"), "{err}");
    }

    #[tokio::test]
    async fn invalid_script_syntax_surfaces_as_an_error() {
        let (_d, local) = test_local().await;
        let err = exec(&json!({"script": "unsupported stuff here"}), &local, None)
            .await
            .unwrap_err();
        assert!(!err.is_empty(), "{err}");
    }
}
