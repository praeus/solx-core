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
//! ## Gotchas for script authors
//!
//! - **Comments are `#` to end of line**, stripped before the `;`-split (see
//!   [`strip_comments`]). A `#` inside a single- or double-quoted substring
//!   (e.g. a URL fragment in a JSON body) is left alone — only an unquoted
//!   `#` starts a comment. There's no block-comment form.
//! - **Wrap JSON bodies in `'''triple single quotes'''`, not `'...'`.**
//!   Everything between `'''` and the next `'''` is taken completely raw —
//!   no escaping of any kind — so a JSON body full of apostrophes, quotes,
//!   or backslashes can be pasted in unmodified. This is the recommended
//!   form for `--json '''{...}'''` bodies and condition string literals
//!   alike; the only limitation is that the content can't itself contain the
//!   literal 3-character sequence `'''`, which essentially never comes up.
//!   The older single/double-quoted form (`'...'`/`"..."`) still works for
//!   backward compatibility: a literal `'` or `"` inside one of those needs
//!   `\'` / `\"`, and getting that wrong is silent and severe rather than a
//!   parse error — an *unescaped* `'` ends the argument right there, and
//!   everything after it on the line becomes bare, unrelated tokens that
//!   still form a syntactically valid (but wrong) statement. This is exactly
//!   what broke an early package's `install.solx`, and is why `'''...'''` is
//!   preferred for anything that might contain an apostrophe.
//! - **Every statement needs its own `;`, including control-flow keywords.**
//!   Newlines are cosmetic — only `;` separates statements. `if $x == null`
//!   followed by a newline and `$y = ...` on the next line is one merged
//!   statement (`parse_expr` then chokes on the stray `=` from the
//!   assignment). Always write `if $x == null;`, `else;`, `endif;`.
//! - **`Script`-typed *actions* only support `exec`/`json` stages** (see
//!   `solx-actions::script::ActionCommandRunner`) — not the
//!   fuller CLI grammar (`save`/`get`/`delete`/`list`/`search`), and no
//!   `return` statement exists at all (a block evaluates to its last
//!   statement's value, so end the script with the value you want returned).
//! - **String concatenation / interpolation** has no dedicated operator.
//!   Build a templated string in one `json` stage instead, with the whole
//!   template single-quoted so the tokenizer doesn't consume the inner
//!   double quotes as its own delimiter:
//!   `$url = json '"https://host/path?id=$id&name=$name"';`
//!   Every `$var` inside that one token gets substituted before the `json`
//!   stage parses it. This also means bare `$a | $b` "fallback" pipelines
//!   don't work (each side would be dispatched as its own stage, and a bare
//!   value like `"null"` or a JSON key isn't `exec`/`json`) — use
//!   `if $a == null; ...; else; ...; endif` instead.
//! - **Quote string substitutions, don't quote everything else.** A `$var`
//!   holding a `String` substitutes as the *raw, unquoted* text (so it can
//!   sit inside an `exec` argument or be user-composed into a larger string);
//!   wrap it in explicit `"..."` when it needs to land as a valid JSON string
//!   value, e.g. `json '{"name":"$var"}'`. A `$var` holding a number/bool/
//!   object/array already serializes to valid JSON, so leave it bare:
//!   `json '{"port":$port}'`. Mixing these up produces "expected value" (a
//!   string landed unquoted) or a string containing literal `{...}` text (an
//!   object got wrapped in quotes it didn't need).
//! - **A missing/null field's substitution is the 4-character text `null`**,
//!   not an absent token — so `Value::Null` and the JSON string `"null"`
//!   become indistinguishable once a value has gone through the
//!   quote-wrapped-string convention above. If you need to tell a real `null`
//!   apart from the string `"null"`, compare the *unwrapped* value directly
//!   in an `if` (`if $existing.value == null; ...`), which evaluates against
//!   the real typed value via dotted-path navigation rather than through
//!   text substitution.

mod ast;
mod block;
mod expr;
mod interp;

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
            '\\' if in_double && chars.peek() == Some(&'"') => {
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

/// Split `s` on `sep`, respecting single/double/triple quoted substrings.
/// `\'` inside single quotes and `\"` inside double quotes are literal quote
/// characters; a `'''...'''` substring is taken raw, with no escaping.
pub fn split_respecting_quotes(s: &str, sep: char) -> Vec<String> {
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
/// matching quote become literal quote characters; a `'''...'''` substring
/// is taken raw (delimiters stripped, no escaping applied inside).
pub fn tokenize_stage(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut in_triple = false;
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
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
