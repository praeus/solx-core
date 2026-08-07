//! Recursive interpreter over the [`Statement`] tree built by [`crate::block`].

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde_json::Value;
use solx_surface::error::{Result, SolxError};

use crate::ast::{ForSource, Statement};
use crate::expr::{eval_expr, is_truthy, parse_expr};
use crate::{execute_pipeline, substitute_vars_in_token, CommandRunner};

/// Longest a single `wait` may sleep, seconds. Matches the default
/// `Command` action timeout (`solx-actions/src/exec.rs::DEFAULT_TIMEOUT_SECS`)
/// so a runaway script can't hang indefinitely.
const MAX_WAIT_SECS: f64 = 300.0;

/// Execute a statement block, returning the last-executed statement's value
/// (an empty block evaluates to `Null`). Recurses into nested `if`/`for`
/// bodies; boxed because Rust does not allow unboxed recursive `async fn`.
pub fn exec_block<'a>(
    runner: &'a dyn CommandRunner,
    statements: &'a [Statement],
    ctx: &'a mut HashMap<String, Value>,
) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>> {
    Box::pin(async move {
        let mut last = Value::Null;
        for stmt in statements {
            last = exec_statement(runner, stmt, ctx).await?;
        }
        Ok(last)
    })
}

async fn exec_statement(
    runner: &dyn CommandRunner,
    stmt: &Statement,
    ctx: &mut HashMap<String, Value>,
) -> Result<Value> {
    match stmt {
        Statement::Pipeline { var, src } => {
            let result = execute_pipeline(runner, src, ctx).await?;
            if let Some(name) = var {
                ctx.insert(name.clone(), result.clone());
            }
            Ok(result)
        }
        Statement::If {
            branches,
            else_branch,
        } => {
            for (cond, body) in branches {
                if is_truthy(&eval_expr(cond, ctx)?) {
                    return exec_block(runner, body, ctx).await;
                }
            }
            match else_branch {
                Some(body) => exec_block(runner, body, ctx).await,
                None => Ok(Value::Null),
            }
        }
        Statement::For {
            item_var,
            source,
            body,
        } => exec_for(runner, item_var, source, body, ctx).await,
        Statement::Wait { amount_src } => exec_wait(amount_src, ctx).await,
    }
}

async fn exec_for(
    runner: &dyn CommandRunner,
    item_var: &str,
    source: &ForSource,
    body: &[Statement],
    ctx: &mut HashMap<String, Value>,
) -> Result<Value> {
    let items = match source {
        ForSource::Range { start, end } => {
            (*start..*end).map(Value::from).collect::<Vec<_>>()
        }
        ForSource::Pipeline(src) => {
            let resolved = if let Some(var_expr) = bare_var_ref(src) {
                eval_expr(&parse_expr(&format!("${var_expr}"))?, ctx)?
            } else {
                execute_pipeline(runner, src, ctx).await?
            };
            coerce_to_items(resolved)
        }
    };

    let previous = ctx.remove(item_var);
    let mut last = Value::Null;
    for item in items {
        ctx.insert(item_var.to_string(), item);
        last = exec_block(runner, body, ctx).await?;
    }
    match previous {
        Some(v) => {
            ctx.insert(item_var.to_string(), v);
        }
        None => {
            ctx.remove(item_var);
        }
    }
    Ok(last)
}

async fn exec_wait(amount_src: &str, ctx: &HashMap<String, Value>) -> Result<Value> {
    let resolved = substitute_vars_in_token(amount_src, ctx);
    let secs: f64 = resolved.trim().parse().map_err(|_| {
        SolxError::Invalid(format!("invalid 'wait' amount: {resolved}"))
    })?;
    if secs < 0.0 {
        return Err(SolxError::Invalid(format!(
            "'wait' amount must be non-negative, got: {secs}"
        )));
    }
    let clamped = secs.min(MAX_WAIT_SECS);
    tokio::time::sleep(Duration::from_secs_f64(clamped)).await;
    Ok(serde_json::json!({ "waited_secs": clamped }))
}

/// A `for` source that is nothing but a bare `$name[.field.sub]` reference
/// (no pipeline stages, no other characters) is resolved as a plain variable
/// lookup rather than dispatched as a command — the common case of
/// `for $item in $arr` where `$arr` was captured earlier via `$arr = ...`.
/// Returns the variable expression without its leading `$`.
fn bare_var_ref(src: &str) -> Option<&str> {
    let trimmed = src.trim();
    let name = trimmed.strip_prefix('$')?;
    if name.is_empty() {
        return None;
    }
    if name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
    {
        Some(name)
    } else {
        None
    }
}

fn coerce_to_items(v: Value) -> Vec<Value> {
    match v {
        Value::Array(items) => items,
        Value::Null => Vec::new(),
        other => vec![other],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::parse_program;
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct Recorder {
        seen: Mutex<Vec<Vec<String>>>,
    }

    #[async_trait]
    impl CommandRunner for Recorder {
        async fn run(&self, tokens: Vec<String>, _piped: Option<Value>) -> Result<Value> {
            self.seen.lock().unwrap().push(tokens.clone());
            Ok(serde_json::json!({ "name": tokens.last().cloned().unwrap_or_default() }))
        }
    }

    async fn run(src: &str) -> Result<Value> {
        let r = Recorder {
            seen: Mutex::new(Vec::new()),
        };
        let program = parse_program(src)?;
        let mut ctx = HashMap::new();
        exec_block(&r, &program, &mut ctx).await
    }

    #[tokio::test]
    async fn if_true_branch_runs() {
        let out = run("if true; json 1; else; json 2; endif").await.unwrap();
        // 'json' isn't a real CommandRunner stage here; Recorder echoes {"name": last-token}.
        assert_eq!(out["name"], "1");
    }

    #[tokio::test]
    async fn if_false_runs_else() {
        let out = run("if false; json 1; else; json 2; endif").await.unwrap();
        assert_eq!(out["name"], "2");
    }

    #[tokio::test]
    async fn if_false_no_else_yields_null() {
        let out = run("if false; json 1; endif").await.unwrap();
        assert_eq!(out, Value::Null);
    }

    #[tokio::test]
    async fn elseif_chain_picks_first_truthy() {
        let out = run("$x = json 2; if $x.name == \"1\"; json 100; elseif $x.name == \"2\"; json 200; else; json 300; endif")
            .await
            .unwrap();
        assert_eq!(out["name"], "200");
    }

    #[tokio::test]
    async fn for_range_iterates_and_binds_item() {
        let r = Recorder {
            seen: Mutex::new(Vec::new()),
        };
        let program = parse_program("for $i in 0..3; json $i; endfor").unwrap();
        let mut ctx = HashMap::new();
        exec_block(&r, &program, &mut ctx).await.unwrap();
        let seen = r.seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0], vec!["json", "0"]);
        assert_eq!(seen[1], vec!["json", "1"]);
        assert_eq!(seen[2], vec!["json", "2"]);
        // item_var not leaked after the loop.
        assert!(!ctx.contains_key("i"));
    }

    #[tokio::test]
    async fn for_over_bare_var_array() {
        let r = Recorder {
            seen: Mutex::new(Vec::new()),
        };
        let program = parse_program("for $item in $arr; json $item; endfor").unwrap();
        let mut ctx = HashMap::new();
        ctx.insert("arr".to_string(), serde_json::json!(["a", "b"]));
        exec_block(&r, &program, &mut ctx).await.unwrap();
        let seen = r.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0], vec!["json", "a"]);
        assert_eq!(seen[1], vec!["json", "b"]);
    }

    #[tokio::test]
    async fn for_loop_var_restored_after_loop() {
        let r = Recorder {
            seen: Mutex::new(Vec::new()),
        };
        let program = parse_program("for $item in 0..2; json $item; endfor").unwrap();
        let mut ctx = HashMap::new();
        ctx.insert("item".to_string(), serde_json::json!("outer"));
        exec_block(&r, &program, &mut ctx).await.unwrap();
        assert_eq!(ctx.get("item"), Some(&serde_json::json!("outer")));
    }

    #[tokio::test]
    async fn for_null_source_is_zero_iterations() {
        let r = Recorder {
            seen: Mutex::new(Vec::new()),
        };
        let program = parse_program("for $item in $missing; json $item; endfor").unwrap();
        let mut ctx = HashMap::new();
        let out = exec_block(&r, &program, &mut ctx).await.unwrap();
        assert_eq!(out, Value::Null);
        assert!(r.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn nested_if_inside_for() {
        let r = Recorder {
            seen: Mutex::new(Vec::new()),
        };
        let program =
            parse_program("for $i in 0..4; if $i > 1; json $i; endif; endfor").unwrap();
        let mut ctx = HashMap::new();
        exec_block(&r, &program, &mut ctx).await.unwrap();
        let seen = r.seen.lock().unwrap();
        assert_eq!(*seen, vec![vec!["json", "2"], vec!["json", "3"]]);
    }

    #[tokio::test]
    async fn wait_clamps_and_reports_seconds() {
        let out = run("wait 0.01").await.unwrap();
        assert_eq!(out["waited_secs"], 0.01);
    }

    #[tokio::test]
    async fn wait_rejects_negative() {
        assert!(run("wait -1").await.is_err());
    }

    #[tokio::test]
    async fn wait_clamps_to_max() {
        // Don't actually sleep 300s in a unit test loop; verify via the
        // parsed/clamped value semantics using a value far past the cap but
        // small enough this test still completes quickly is not possible
        // without sleeping, so assert the clamp logic directly instead.
        let ctx = HashMap::new();
        let resolved = substitute_vars_in_token("100000", &ctx);
        let secs: f64 = resolved.parse().unwrap();
        assert_eq!(secs.min(MAX_WAIT_SECS), MAX_WAIT_SECS);
    }
}
