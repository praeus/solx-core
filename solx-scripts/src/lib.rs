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
//!
//! ## Writing scripts
//!
//! ```text
//! # Values: no `json` stage needed.
//! $port = $params.port || 8765;
//! $url = "https://host/auth?client_id=${client_id}&port=${port}";
//!
//! # JSON arguments: write the object itself, unquoted.
//! $cb = exec /builtin/oauth/oauth-await --json {
//!     "state_value": $loopback.result.state_value,
//!     "timeout_secs": $timeout,
//! };
//!
//! if $cb.result.succeeded == true then
//!     $result = {"succeeded": true, "code": $cb.result.code};
//! else
//!     $result = {"succeeded": false, "error": $cb.result.error};
//! endif
//! $result;
//! ```
//!
//! - **JSON is written as object/array literals, never inside quotes.** An
//!   assignment (`$x = {...}`) or a command argument that starts with an
//!   unquoted `{` or `[` runs to its matching bracket — across spaces,
//!   newlines, and `||` — and is evaluated as an expression. Values are any
//!   expression (`$var.path`, literals, `$a || "default"`, nested
//!   objects/arrays) and keep their real types: a string stays a string, a
//!   missing field is a real `null`, a nested object stays an object. Keys
//!   must be quoted; a trailing comma is allowed. As a command argument the
//!   result is passed on as one JSON string, so
//!   `exec /x --json {"a": $a}` and `exec /x --json $body` (a `$var` holding
//!   an object) both work.
//! - **Pasted JSON is taken literally.** Double-quoted strings use JSON
//!   escapes (`\"`, `\\`, `\n`, `\uXXXX`, ...), and a bare `$` inside one
//!   is just a character, so `{"$schema": "http://json-schema.org/..."}`
//!   pastes in unchanged.
//! - **`${name.path}` fills a value into a string.** Only inside double
//!   quotes, only with braces: `"Hi ${user.name}."` Strings go in as their
//!   text, other values as JSON, a missing value as `null`. Use this for
//!   URLs and messages; for JSON, use an object literal instead of building
//!   a string. To write a literal `${...}` in text, escape the dollar:
//!   `"type \${name} to insert a name"` yields `type ${name} to insert a name`.
//! - **Plain values don't need `json`.** A statement starting with a `$var`,
//!   a literal (`8765`, `"text"`, `true`/`false`/`null`), `{`, `[`, `(` or
//!   `!` is an expression: `$timeout = $params.timeout_secs;`, or a bare
//!   `$result;` as a script's last line. `&&`/`||` return the operand that
//!   decided them, so `$port = $params.port || 8765;` is a fallback (`0`,
//!   `null`, `""`, `[]`, `{}` are falsy). There's no `$a | $b` pipe form —
//!   a `|` pipeline's stages must be commands.
//! - **Ordinary statements end with `;`; control-flow keywords end at the
//!   line.** An `exec`, `json`, or assignment runs until `;`, so it can wrap
//!   across lines. `if`, `else if`, `else`, `endif`, `for`, `endfor`, and
//!   `wait` end at the first `;` *or* newline, and an `if`/`else if`
//!   condition may also end with `then` on the same line
//!   (`if $x then json 1; endif`). `else if <cond>` on one line is the next
//!   branch of the same `if` (`elseif` also works); to nest a new `if`
//!   inside an `else`, put it on the following line.
//! - **Comments are `#` to end of line**, except inside quotes (see
//!   [`strip_comments`]). There's no block-comment form.
//! - **`Script`-typed *actions* only support `exec`/`json` stages** (see
//!   `solx-actions::script::ActionCommandRunner`) — not the fuller CLI
//!   grammar (`save`/`get`/`delete`/`list`/`search`) — and there's no
//!   `return`: a block evaluates to its last statement's value.
//!
//! ### Older forms (still accepted, not for new scripts)
//!
//! Before object literals, JSON arguments were quoted strings with values
//! spliced in as text: `--json '{"name":"'$name'","port":'$port'}'`, or
//! `--json '''{...}'''` (raw, no escaping) to survive apostrophes. Inside
//! those, and in any other command argument, a bare `$var`/`$var.path` is
//! replaced with its text (strings unquoted, other values as JSON, `null`
//! as the text `null`). These forms keep working so existing scripts don't
//! break, but they're error-prone: which values need `"..."`, escaping
//! `\'`, and `null` vs `"null"` are exactly what object literals remove.

mod ast;
mod block;
mod expr;
mod interp;

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::Value;
use solx_surface::error::{Result, SolxError};

use crate::expr::{eval_expr, parse_expr};

/// Runs a single already-substituted command stage. `tokens` is the argv-style
/// token list (without a leading program name); `piped` is the previous stage's
/// output, if any.
#[async_trait]
pub trait CommandRunner: Send + Sync {
    async fn run(&self, tokens: Vec<String>, piped: Option<Value>) -> Result<Value>;
}

/// Execute a full script, returning the last statement's result. Supports
/// `if`/`elseif`/`else`/`endif`, `for`/`endfor`, and `wait` control flow on
/// top of the base `;`/`|`/`$var` pipeline language — see `block`/`interp`.
pub async fn execute_script(runner: &dyn CommandRunner, source: &str) -> Result<Value> {
    execute_script_with_vars(runner, source, HashMap::new()).await
}

/// Like [`execute_script`], but seeds the interpreter's variable context with
/// `initial` before running — e.g. `{"params": <caller's params>}` so a
/// script can reference `$params`/`$params.field`. The script may freely
/// reassign any seeded name; it behaves exactly like a `$name = ...`
/// statement had already run.
pub async fn execute_script_with_vars(
    runner: &dyn CommandRunner,
    source: &str,
    initial: HashMap<String, Value>,
) -> Result<Value> {
    let source = strip_comments(source);
    let program = block::parse_program(&source)?;
    let mut ctx = initial;
    interp::exec_block(runner, &program, &mut ctx).await
}

/// Called with the first `'` of a possible `'''` already consumed as `ch`;
/// if the next two characters are also `'`, consumes them and returns `true`
/// (a full `'''` matched). Otherwise consumes nothing and returns `false`.
fn consume_triple_quote_rest(chars: &mut std::iter::Peekable<std::str::Chars>) -> bool {
    let mut lookahead = chars.clone();
    if lookahead.next() == Some('\'') && lookahead.next() == Some('\'') {
        chars.next();
        chars.next();
        true
    } else {
        false
    }
}

/// Strip `#`-to-end-of-line comments, respecting single/double/triple quoted
/// substrings — an unquoted `#` starts a comment, a quoted one (e.g. a URL
/// fragment inside a JSON body) is left alone. Called once, before the
/// `;`-split, so a comment line can sit anywhere a statement could.
pub fn strip_comments(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut in_single = false;
    let mut in_double = false;
    let mut in_triple = false;
    let mut chars = source.chars().peekable();

    while let Some(ch) = chars.next() {
        if in_triple {
            if ch == '\'' && consume_triple_quote_rest(&mut chars) {
                in_triple = false;
                out.push_str("'''");
            } else {
                out.push(ch);
            }
            continue;
        }
        if ch == '\'' && !in_single && !in_double && consume_triple_quote_rest(&mut chars) {
            in_triple = true;
            out.push_str("'''");
            continue;
        }
        match ch {
            '\\' if in_single && chars.peek() == Some(&'\'') => {
                out.push(ch);
                out.push(chars.next().unwrap());
            }
            // `\"` and `\\` both pair up, so `"a\\"` closes at its last `"`.
            '\\' if in_double && matches!(chars.peek(), Some('"' | '\\')) => {
                out.push(ch);
                out.push(chars.next().unwrap());
            }
            '\'' if !in_double => {
                in_single = !in_single;
                out.push(ch);
            }
            '"' if !in_single => {
                in_double = !in_double;
                out.push(ch);
            }
            '#' if !in_single && !in_double => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push(c);
                        break;
                    }
                }
            }
            _ => out.push(ch),
        }
    }
    out
}

pub(crate) fn parse_assignment(stmt: &str) -> (Option<String>, String) {
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

pub(crate) async fn execute_pipeline(
    runner: &dyn CommandRunner,
    pipeline_src: &str,
    ctx: &HashMap<String, Value>,
) -> Result<Value> {
    let mut piped: Option<Value> = None;
    // A `|` inside an unquoted `{...}`/`[...]` argument (e.g. `||`) isn't a
    // stage separator.
    let mut depth = 0i32;
    let stages = split_respecting_quotes_by(pipeline_src, |c| {
        match c {
            '{' | '[' => depth += 1,
            '}' | ']' => depth -= 1,
            _ => {}
        }
        c == '|' && depth <= 0
    });
    for stage in stages {
        let stage = stage.trim().to_string();
        if stage.is_empty() {
            continue;
        }
        let mut tokens = Vec::new();
        for arg in tokenize_stage_args(&stage) {
            tokens.push(match arg {
                StageArg::Text(t) => substitute_vars_in_token(&t, ctx),
                StageArg::Literal(src) => {
                    let value = eval_expr(&parse_expr(&src)?, ctx)?;
                    serde_json::to_string(&value)
                        .map_err(|e| SolxError::Invalid(format!("serialize {src}: {e}")))?
                }
            });
        }
        if tokens.is_empty() {
            continue;
        }
        piped = Some(runner.run(tokens, piped).await?);
    }
    Ok(piped.unwrap_or(Value::Null))
}

/// One argument of a command stage, as split by [`tokenize_stage_args`].
enum StageArg {
    /// Ordinary argument, quotes already stripped; `$var`s substituted later.
    Text(String),
    /// Unquoted `{...}`/`[...]` object or array literal (raw source), which
    /// is evaluated as an expression and passed on as its JSON text.
    Literal(String),
}

/// Split `s` on `sep`, respecting single/double/triple quoted substrings.
/// `\'` inside single quotes and `\"` inside double quotes are literal quote
/// characters; a `'''...'''` substring is taken raw, with no escaping.
pub fn split_respecting_quotes(s: &str, sep: char) -> Vec<String> {
    split_respecting_quotes_by(s, |c| c == sep)
}

/// Like [`split_respecting_quotes`], but splits on any unquoted character
/// matching `is_sep` (e.g. `char::is_whitespace`). `is_sep` sees every
/// unquoted character in order (never a quoted one or a quote delimiter), so
/// it may keep state — e.g. bracket depth.
pub(crate) fn split_respecting_quotes_by(s: &str, mut is_sep: impl FnMut(char) -> bool) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut in_triple = false;
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        if in_triple {
            if ch == '\'' && consume_triple_quote_rest(&mut chars) {
                in_triple = false;
                current.push_str("'''");
            } else {
                current.push(ch);
            }
            continue;
        }
        if ch == '\'' && !in_single && !in_double && consume_triple_quote_rest(&mut chars) {
            in_triple = true;
            current.push_str("'''");
            continue;
        }
        match ch {
            '\\' if in_single && chars.peek() == Some(&'\'') => {
                current.push(ch);
                current.push(chars.next().unwrap());
            }
            // `\"` and `\\` both pair up, so `"a\\"` closes at its last `"`.
            '\\' if in_double && matches!(chars.peek(), Some('"' | '\\')) => {
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
            c if !in_single && !in_double && is_sep(c) => {
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
/// matching quote become literal quote characters; a `'''...'''` substring
/// is taken raw (delimiters stripped, no escaping applied inside). An
/// unquoted `{...}`/`[...]` argument is returned as its raw source text.
pub fn tokenize_stage(s: &str) -> Vec<String> {
    tokenize_stage_args(s)
        .into_iter()
        .map(|arg| match arg {
            StageArg::Text(t) | StageArg::Literal(t) => t,
        })
        .collect()
}

/// Split a stage into arguments on unquoted whitespace (spaces, tabs, and
/// newlines, so a command may wrap across lines). An argument that *starts*
/// with an unquoted `{` or `[` runs to its matching close bracket — spaces,
/// newlines, and quotes inside it included — and comes back as
/// [`StageArg::Literal`], so `exec /x --json {"a": $a, "b": [1, 2]}` passes
/// one evaluated JSON argument.
fn tokenize_stage_args(s: &str) -> Vec<StageArg> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut in_triple = false;
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        if current.is_empty()
            && !in_single
            && !in_double
            && !in_triple
            && (ch == '{' || ch == '[')
        {
            tokens.push(StageArg::Literal(scan_bracketed(ch, &mut chars)));
            continue;
        }
        if in_triple {
            if ch == '\'' && consume_triple_quote_rest(&mut chars) {
                in_triple = false;
            } else {
                current.push(ch);
            }
            continue;
        }
        if ch == '\'' && !in_single && !in_double && consume_triple_quote_rest(&mut chars) {
            in_triple = true;
            continue;
        }
        match ch {
            '\\' if in_single && chars.peek() == Some(&'\'') => {
                current.push(chars.next().unwrap());
            }
            '\\' if in_double && chars.peek() == Some(&'"') => {
                current.push(chars.next().unwrap());
            }
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            c if c.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    tokens.push(StageArg::Text(std::mem::take(&mut current)));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(StageArg::Text(current));
    }
    tokens
}

/// Having consumed `open` (`{` or `[`), collect through its matching close
/// bracket, skipping brackets inside `"..."`, `'...'`, and `'''...'''`
/// strings. Returns the raw text including both brackets; if the input ends
/// first, returns what there is and lets the expression parser report it.
fn scan_bracketed(open: char, chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
    let mut out = String::from(open);
    let mut depth = 1;
    let mut quote: Option<char> = None;
    let mut in_triple = false;
    while let Some(ch) = chars.next() {
        out.push(ch);
        if in_triple {
            if ch == '\'' && consume_triple_quote_rest(chars) {
                out.push_str("''");
                in_triple = false;
            }
            continue;
        }
        if let Some(q) = quote {
            if ch == '\\' {
                if let Some(escaped) = chars.next() {
                    out.push(escaped);
                }
            } else if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' if consume_triple_quote_rest(chars) => {
                out.push_str("''");
                in_triple = true;
            }
            '"' | '\'' => quote = Some(ch),
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
    }
    out
}

pub(crate) fn substitute_vars_in_token(token: &str, ctx: &HashMap<String, Value>) -> String {
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
            // A trailing '.' is punctuation, not an empty path segment
            // ("Hi $name." -> "Hi Ann.").
            while end > start && chars[end - 1] == '.' {
                end -= 1;
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

pub(crate) fn navigate_json_path(val: &Value, path: &str) -> Value {
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

pub(crate) fn value_to_arg_string(val: &Value) -> String {
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

    async fn seen_for(src: &str) -> Vec<Vec<String>> {
        let r = Recorder {
            seen: Mutex::new(Vec::new()),
        };
        execute_script(&r, src).await.unwrap();
        let seen = r.seen.lock().unwrap().clone();
        seen
    }

    #[tokio::test]
    async fn inline_object_arg_is_one_evaluated_json_token() {
        let seen = seen_for(
            "$s = json 7;\n\
             exec /a --json {\n    \"state\": $s.name, \"n\": 3, \"miss\": $nope,\n    \"t\": \"it's }] \\\"q\\\"\", \"l\": [1, {\"x\": $s.name}],\n}",
        )
        .await;
        let call = &seen[1];
        assert_eq!(call[..3], ["exec", "/a", "--json"]);
        assert_eq!(call.len(), 4);
        let arg: Value = serde_json::from_str(&call[3]).unwrap();
        assert_eq!(
            arg,
            serde_json::json!({"state": "7", "n": 3, "miss": null, "t": "it's }] \"q\"", "l": [1, {"x": "7"}]})
        );
    }

    #[tokio::test]
    async fn or_inside_inline_object_is_not_a_pipe() {
        let seen = seen_for("exec /a --json {\"p\": $params.port || 8765} | exec /b").await;
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0], vec!["exec", "/a", "--json", r#"{"p":8765}"#]);
        assert_eq!(seen[1], vec!["exec", "/b"]);
    }

    #[tokio::test]
    async fn inline_array_and_json_escapes() {
        let seen = seen_for(r#"json ["a\nb", "C:\\dir\\", "\u00e9", 1e3]; json 'x'"#).await;
        let arg: Value = serde_json::from_str(&seen[0][1]).unwrap();
        assert_eq!(arg, serde_json::json!(["a\nb", "C:\\dir\\", "é", 1000.0]));
        // The `;` after the trailing `\\"` still ended the statement.
        assert_eq!(seen[1], vec!["json", "x"]);
    }

    #[tokio::test]
    async fn quoted_and_mid_token_braces_stay_text() {
        let seen = seen_for(r#"exec /a --json '{"x": "$y"}' k={a}"#).await;
        assert_eq!(seen[0], vec!["exec", "/a", "--json", r#"{"x": "$y"}"#, "k={a}"]);
    }

    #[tokio::test]
    async fn command_args_may_wrap_lines() {
        let seen = seen_for("exec /a\n    --json\n    '{}'").await;
        assert_eq!(seen[0], vec!["exec", "/a", "--json", "{}"]);
    }

    #[test]
    fn quote_aware_tokenize() {
        let toks = tokenize_stage(r#"save doc /a --json '{"x": 1}'"#);
        assert_eq!(toks, vec!["save", "doc", "/a", "--json", r#"{"x": 1}"#]);
    }

    #[test]
    fn tokenize_unescapes_a_quote_inside_the_matching_quote_kind() {
        // `\'` inside single quotes and `\"` inside double quotes are literal
        // quote characters, not delimiters -- this is what lets a JSON body
        // wrapped in single quotes contain a real apostrophe.
        let toks = tokenize_stage(r#"save doc /a --json '{"text":"can\'t stop"}'"#);
        assert_eq!(
            toks,
            vec!["save", "doc", "/a", "--json", r#"{"text":"can't stop"}"#]
        );
    }

    #[tokio::test]
    async fn escaped_apostrophe_does_not_truncate_the_rest_of_the_argument() {
        // The historical failure: an *unescaped* `'` inside a single-quoted
        // argument ends the argument on the spot, and everything after it on
        // the line becomes bare, unrelated tokens -- silently, since the
        // result is still a syntactically valid statement. An early
        // package's `install.solx` broke exactly this way. This locks in
        // that escaping fixes it: a two-statement script where the first
        // statement's JSON embeds an escaped apostrophe still parses as two
        // clean statements, not one mangled one.
        let r = Recorder {
            seen: Mutex::new(Vec::new()),
        };
        let src = r#"save doc /a --json '{"text":"can\'t stop"}'; save doc /b --json '{"text":"ok"}';"#;
        execute_script(&r, src).await.unwrap();
        let seen = r.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(
            seen[0],
            vec!["save", "doc", "/a", "--json", r#"{"text":"can't stop"}"#]
        );
        assert_eq!(
            seen[1],
            vec!["save", "doc", "/b", "--json", r#"{"text":"ok"}"#]
        );
    }

    #[test]
    fn triple_quoted_json_needs_no_escaping() {
        // '''...''' is taken raw: an embedded apostrophe (or `;`/`#`/`\`)
        // needs no escaping at all, unlike the '...'/"..." form above.
        let toks = tokenize_stage(r#"save doc /a --json '''{"text":"can't stop; #1 \o/"}'''"#);
        assert_eq!(
            toks,
            vec!["save", "doc", "/a", "--json", r#"{"text":"can't stop; #1 \o/"}"#]
        );
    }

    #[test]
    fn triple_quotes_protect_separators_and_comments() {
        let stmts = split_respecting_quotes(r#"json '''a; b'''; json 'c'"#, ';');
        assert_eq!(stmts, vec![r#"json '''a; b'''"#, r#" json 'c'"#]);

        let stripped = strip_comments(r#"json '''a # not a comment'''  # a real comment"#);
        assert_eq!(stripped, r#"json '''a # not a comment'''  "#);
    }

    #[tokio::test]
    async fn triple_quoted_apostrophe_does_not_truncate_the_rest_of_the_argument() {
        let r = Recorder {
            seen: Mutex::new(Vec::new()),
        };
        let src = r#"save doc /a --json '''{"text":"can't stop"}'''; save doc /b --json '{"text":"ok"}';"#;
        execute_script(&r, src).await.unwrap();
        let seen = r.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(
            seen[0],
            vec!["save", "doc", "/a", "--json", r#"{"text":"can't stop"}"#]
        );
        assert_eq!(
            seen[1],
            vec!["save", "doc", "/b", "--json", r#"{"text":"ok"}"#]
        );
    }

    #[test]
    fn strip_comments_removes_line_comments() {
        let src = "# a leading comment\nget doc /a; # trailing comment\nget doc /b";
        assert_eq!(strip_comments(src), "\nget doc /a; \nget doc /b");
    }

    #[test]
    fn strip_comments_leaves_quoted_hash_alone() {
        // A `#` inside a quoted JSON body (e.g. a URL fragment) is not a comment.
        let src = r#"json '"https://host/path#section"' # now a real comment"#;
        assert_eq!(strip_comments(src), r#"json '"https://host/path#section"' "#);
    }

    #[tokio::test]
    async fn comments_are_ignored_when_executing_a_script() {
        let r = Recorder {
            seen: Mutex::new(Vec::new()),
        };
        let src = "# set up\n$doc = get doc /a/b; # fetch it\nget action $doc.name # done";
        let out = execute_script(&r, src).await.unwrap();
        let seen = r.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1], vec!["get", "action", "/a/b"]);
        assert_eq!(out["name"], "/a/b");
    }

    #[tokio::test]
    async fn seeded_vars_are_visible_and_overridable() {
        let r = Recorder {
            seen: Mutex::new(Vec::new()),
        };
        let mut initial = HashMap::new();
        initial.insert("params".to_string(), serde_json::json!({"mode": "fast"}));
        let out = execute_script_with_vars(&r, "get mode $params.mode", initial)
            .await
            .unwrap();
        assert_eq!(out["name"], "fast");
    }

    #[tokio::test]
    async fn seeded_var_can_be_reassigned() {
        let r = Recorder {
            seen: Mutex::new(Vec::new()),
        };
        let mut initial = HashMap::new();
        initial.insert("params".to_string(), serde_json::json!({"mode": "fast"}));
        let out = execute_script_with_vars(
            &r,
            "$params = get override x; get mode $params.name",
            initial,
        )
        .await
        .unwrap();
        // Reassignment replaces the seeded value entirely: `$params.name`
        // resolves against the Recorder's echoed `{"name": "x"}`, not the
        // original seeded object (which had no `name` field at all).
        assert_eq!(out["name"], "x");
    }

    #[tokio::test]
    async fn execute_script_still_starts_from_empty_ctx() {
        let r = Recorder {
            seen: Mutex::new(Vec::new()),
        };
        // No seeded vars: `$params` is unresolved and passed through literally.
        let out = execute_script(&r, "get mode $params.mode").await.unwrap();
        assert_eq!(out["name"], "$params.mode");
    }
}
