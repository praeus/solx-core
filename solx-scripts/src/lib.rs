//! `solx-scripts` — the solx shell pipeline scripting language, decoupled from
//! the CLI.
//!
//! A script is a sequence of statements separated by `;`. Each statement is a
//! pipeline of stages separated by `|`, where each stage's JSON output feeds
//! the next stage as piped input. A statement may capture its result into a
//! variable with `$name = <pipeline>`, and later stages can reference `$name`
//! or `$name.field.sub` (substituted before the stage runs).
//!
//! The executor is transport-agnostic: it tokenizes/substitutes and hands each
//! stage's tokens to a [`CommandRunner`], which the CLI implements by parsing
//! the tokens with clap and dispatching. (This mirrors the old CLI's
//! `execute_pipeline`, lifted into a library.)

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::Value;
use solx_surface::error::Result;

/// Runs a single already-substituted command stage. `tokens` is the argv-style
/// token list (without a leading program name); `piped` is the previous stage's
/// output, if any.
#[async_trait]
pub trait CommandRunner: Send + Sync {
    async fn run(&self, tokens: Vec<String>, piped: Option<Value>) -> Result<Value>;
}

/// Execute a full script, returning the last statement's result.
pub async fn execute_script(runner: &dyn CommandRunner, source: &str) -> Result<Value> {
    let mut ctx: HashMap<String, Value> = HashMap::new();
    let mut last = Value::Null;

    for stmt in split_respecting_quotes(source, ';') {
        let stmt = stmt.trim().to_string();
        if stmt.is_empty() {
            continue;
        }
        let (var_name, pipeline_src) = parse_assignment(&stmt);
        let result = execute_pipeline(runner, &pipeline_src, &ctx).await?;
        if let Some(var) = var_name {
            ctx.insert(var, result.clone());
        }
        last = result;
    }
    Ok(last)
}

fn parse_assignment(stmt: &str) -> (Option<String>, String) {
    if stmt.starts_with('$') {
        if let Some(eq) = stmt.find('=') {
            let name = stmt[1..eq].trim();
            if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                return (Some(name.to_string()), stmt[eq + 1..].trim().to_string());
            }
        }
    }
    (None, stmt.to_string())
}

async fn execute_pipeline(
    runner: &dyn CommandRunner,
    pipeline_src: &str,
    ctx: &HashMap<String, Value>,
) -> Result<Value> {
    let mut piped: Option<Value> = None;
    for stage in split_respecting_quotes(pipeline_src, '|') {
        let stage = stage.trim().to_string();
        if stage.is_empty() {
            continue;
        }
        let tokens = substitute_vars(tokenize_stage(&stage), ctx);
        if tokens.is_empty() {
            continue;
        }
        piped = Some(runner.run(tokens, piped).await?);
    }
    Ok(piped.unwrap_or(Value::Null))
}

/// Split `s` on `sep`, respecting single/double quoted substrings. `\'` inside
/// single quotes and `\"` inside double quotes are literal quote characters.
pub fn split_respecting_quotes(s: &str, sep: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\\' if in_single && chars.peek() == Some(&'\'') => {
                current.push(ch);
                current.push(chars.next().unwrap());
            }
            '\\' if in_double && chars.peek() == Some(&'"') => {
                current.push(ch);
                current.push(chars.next().unwrap());
            }
            '\'' if !in_double => {
                in_single = !in_single;
                current.push(ch);
            }
            '"' if !in_single => {
                in_double = !in_double;
                current.push(ch);
            }
            c if c == sep && !in_single && !in_double => {
                parts.push(current.clone());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    if !current.trim().is_empty() {
        parts.push(current);
    }
    parts
}

/// Tokenize a stage into argv tokens, stripping quotes. `\'`/`\"` inside the
/// matching quote become literal quote characters.
pub fn tokenize_stage(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\\' if in_single && chars.peek() == Some(&'\'') => {
                current.push(chars.next().unwrap());
            }
            '\\' if in_double && chars.peek() == Some(&'"') => {
                current.push(chars.next().unwrap());
            }
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ' ' | '\t' if !in_single && !in_double => {
                if !current.is_empty() {
                    tokens.push(current.clone());
                    current.clear();
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn substitute_vars(tokens: Vec<String>, ctx: &HashMap<String, Value>) -> Vec<String> {
    tokens
        .into_iter()
        .map(|t| substitute_vars_in_token(&t, ctx))
        .collect()
}

fn substitute_vars_in_token(token: &str, ctx: &HashMap<String, Value>) -> String {
    if !token.contains('$') {
        return token.to_string();
    }
    let mut result = String::new();
    let chars: Vec<char> = token.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' {
            let start = i + 1;
            let mut end = start;
            while end < chars.len()
                && (chars[end].is_alphanumeric() || chars[end] == '_' || chars[end] == '.')
            {
                end += 1;
            }
            let var_expr: String = chars[start..end].iter().collect();
            if var_expr.is_empty() {
                result.push('$');
                i += 1;
                continue;
            }
            let (var_name, field_path) = match var_expr.find('.') {
                Some(dot) => (&var_expr[..dot], Some(&var_expr[dot + 1..])),
                None => (var_expr.as_str(), None),
            };
            if let Some(val) = ctx.get(var_name) {
                let resolved = match field_path {
                    Some(path) => navigate_json_path(val, path),
                    None => val.clone(),
                };
                result.push_str(&value_to_arg_string(&resolved));
            } else {
                result.push('$');
                result.push_str(&var_expr);
            }
            i = end;
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }
    result
}

fn navigate_json_path(val: &Value, path: &str) -> Value {
    let mut current = val;
    for key in path.split('.') {
        current = match current {
            Value::Object(map) => map.get(key).unwrap_or(&Value::Null),
            Value::Array(arr) => arr
                .get(key.parse::<usize>().unwrap_or(usize::MAX))
                .unwrap_or(&Value::Null),
            _ => &Value::Null,
        };
    }
    current.clone()
}

fn value_to_arg_string(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_string(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "null".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records the token lists it receives and echoes a fixed JSON per command.
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

    #[tokio::test]
    async fn assigns_and_substitutes() {
        let r = Recorder {
            seen: Mutex::new(Vec::new()),
        };
        let out = execute_script(&r, "$doc = get doc /a/b; get action $doc.name")
            .await
            .unwrap();
        let seen = r.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        // Second stage's $doc.name resolved to the first stage's echoed name.
        assert_eq!(seen[1], vec!["get", "action", "/a/b"]);
        assert_eq!(out["name"], "/a/b");
    }

    #[test]
    fn quote_aware_tokenize() {
        let toks = tokenize_stage(r#"post doc /a --json '{"x": 1}'"#);
        assert_eq!(toks, vec!["post", "doc", "/a", "--json", r#"{"x": 1}"#]);
    }
}
