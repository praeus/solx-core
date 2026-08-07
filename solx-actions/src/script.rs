//! `Script`-typed action execution: runs a `solx-scripts` script whose
//! stages may invoke other actions.
//!
//! Deliberately narrow: [`ActionCommandRunner`] understands exactly two
//! stage verbs, `exec` and `json` — not the full `solx-cli` grammar
//! (`post`/`get`/`delete`/`list`/`search`). A script that needs entity CRUD
//! reaches it the same way a WASM guest or an MCP tool call already does —
//! `exec /builtin/doc_post ...` etc. — so it stays subject to the same
//! executable-action guard and param-type validation the `Internal`
//! handlers apply (see `crate::internal::entity`). Bypassing that by giving
//! scripts direct access to `docs`/`types`/`files` would be a real
//! capability escalation, not just a convenience.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use solx_scripts::CommandRunner;
use solx_surface::error::{Result, SolxError};
use solx_surface::path::split_ref;

use crate::caller::Caller;
use crate::LocalActionManager;

pub struct ActionCommandRunner {
    pub actions: Arc<LocalActionManager>,
    pub caller: Caller,
}

#[async_trait]
impl CommandRunner for ActionCommandRunner {
    async fn run(&self, tokens: Vec<String>, piped: Option<Value>) -> Result<Value> {
        match tokens.first().map(String::as_str) {
            Some("exec") => self.run_exec(&tokens, piped).await,
            Some("json") => run_json(&tokens),
            Some(other) => Err(SolxError::Invalid(format!(
                "unsupported script stage '{other}' (Script actions only support \
                 'exec <path/name> [--json '<params>']' and 'json <value>')"
            ))),
            None => Ok(Value::Null),
        }
    }
}

impl ActionCommandRunner {
    async fn run_exec(&self, tokens: &[String], piped: Option<Value>) -> Result<Value> {
        let (reference, json) = parse_exec_stage(tokens)?;
        let params = match json {
            Some(j) => serde_json::from_str(&j)
                .map_err(|e| SolxError::Invalid(format!("parse --json params: {e}")))?,
            None => piped.unwrap_or_else(|| Value::Object(Default::default())),
        };
        let (path, name) = split_ref(&reference)?;
        let result = self
            .actions
            .exec_as(&path, &name, params, Some(&self.caller))
            .await?;
        serde_json::to_value(&result)
            .map_err(|e| SolxError::Invalid(format!("serialize exec result: {e}")))
    }
}

fn parse_exec_stage(tokens: &[String]) -> Result<(String, Option<String>)> {
    let mut reference: Option<String> = None;
    let mut json: Option<String> = None;
    let mut i = 1;
    while i < tokens.len() {
        match tokens[i].as_str() {
            "--json" | "-j" => {
                i += 1;
                let value = tokens.get(i).ok_or_else(|| {
                    SolxError::Invalid("exec: '--json' requires a value".into())
                })?;
                json = Some(value.clone());
            }
            other => {
                if reference.is_some() {
                    return Err(SolxError::Invalid(format!(
                        "exec: unexpected argument '{other}'"
                    )));
                }
                reference = Some(other.to_string());
            }
        }
        i += 1;
    }
    let reference = reference.ok_or_else(|| {
        SolxError::Invalid("exec requires an action reference, e.g. 'exec /pkg/name'".into())
    })?;
    Ok((reference, json))
}

fn run_json(tokens: &[String]) -> Result<Value> {
    if tokens.len() != 2 {
        return Err(SolxError::Invalid(
            "json takes exactly one argument, e.g. 'json \"hello\"' or 'json 5'".into(),
        ));
    }
    serde_json::from_str(&tokens[1])
        .map_err(|e| SolxError::Invalid(format!("parse json value: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_exec_stage_ref_then_flag() {
        let tokens = vec![
            "exec".to_string(),
            "/pkg/name".to_string(),
            "--json".to_string(),
            r#"{"x":1}"#.to_string(),
        ];
        let (reference, json) = parse_exec_stage(&tokens).unwrap();
        assert_eq!(reference, "/pkg/name");
        assert_eq!(json.as_deref(), Some(r#"{"x":1}"#));
    }

    #[test]
    fn parse_exec_stage_flag_then_ref() {
        let tokens = vec![
            "exec".to_string(),
            "-j".to_string(),
            "{}".to_string(),
            "/pkg/name".to_string(),
        ];
        let (reference, json) = parse_exec_stage(&tokens).unwrap();
        assert_eq!(reference, "/pkg/name");
        assert_eq!(json.as_deref(), Some("{}"));
    }

    #[test]
    fn parse_exec_stage_ref_only() {
        let tokens = vec!["exec".to_string(), "/pkg/name".to_string()];
        let (reference, json) = parse_exec_stage(&tokens).unwrap();
        assert_eq!(reference, "/pkg/name");
        assert_eq!(json, None);
    }

    #[test]
    fn parse_exec_stage_missing_ref_errors() {
        assert!(parse_exec_stage(&["exec".to_string()]).is_err());
    }

    #[test]
    fn parse_exec_stage_missing_json_value_errors() {
        let tokens = vec!["exec".to_string(), "/a/b".to_string(), "--json".to_string()];
        assert!(parse_exec_stage(&tokens).is_err());
    }

    #[test]
    fn parse_exec_stage_duplicate_ref_errors() {
        let tokens = vec!["exec".to_string(), "/a/b".to_string(), "/c/d".to_string()];
        assert!(parse_exec_stage(&tokens).is_err());
    }

    #[test]
    fn run_json_parses_literal() {
        let tokens = vec!["json".to_string(), "5".to_string()];
        assert_eq!(run_json(&tokens).unwrap(), Value::from(5));
    }

    #[test]
    fn run_json_requires_exactly_one_arg() {
        assert!(run_json(&["json".to_string()]).is_err());
        assert!(run_json(&[
            "json".to_string(),
            "1".to_string(),
            "2".to_string()
        ])
        .is_err());
    }

    #[test]
    fn run_json_invalid_literal_errors() {
        let tokens = vec!["json".to_string(), "not-json".to_string()];
        assert!(run_json(&tokens).is_err());
    }
}
