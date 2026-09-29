//! Expression grammar used by `if`/`else if` conditions, value
//! assignments (`$x = $y.field || 300`), and object/array literals.
//!
//! `&&`/`||` return the operand that decided the result, JavaScript-style
//! (`$a || 300` is `$a` when truthy, else `300`), so conditions behave the
//! same as with plain booleans while assignments get a fallback idiom.
//!
//! Strings: `"double quotes"` take JSON escapes and fill in `${name.path}`
//! references; a bare `$` inside them is literal, so pasted JSON is taken
//! as-is. `'single'`/`'''triple'''` quotes are fully literal (older forms).
//!
//! Precedence, low to high: `||` → `&&` → unary `!` → comparison → atom.
//! Atoms are `$name[.field.sub]` variable references (resolved the same way
//! as pipeline substitution, via [`crate::navigate_json_path`]) or literals
//! (quoted strings, numbers, `true`/`false`/`null`, or a parenthesized
//! sub-expression).

use std::collections::HashMap;
use std::iter::Peekable;
use std::str::Chars;

use serde_json::Value;
use solx_surface::error::{Result, SolxError};

use crate::{consume_triple_quote_rest, navigate_json_path, value_to_arg_string};

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Value),
    Var(String),
    /// Double-quoted string containing `${name.path}` references, filled in
    /// at evaluation time (see [`interpolate`]).
    Template(String),
    /// `{"key": expr, ...}` — keys are string literals (a double-quoted key
    /// may use `${...}`, like any `"..."`).
    Object(Vec<(Expr, Expr)>),
    /// `[expr, ...]`
    Array(Vec<Expr>),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Compare(CompareOp, Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Contains,
}

// ── Lexer ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Token {
    LParen,
    RParen,
    And,
    Or,
    Not,
    Op(CompareOp),
    Var(String),
    Str(String),
    /// Double-quoted string: `${name}` references inside are filled in.
    Template(String),
    Num(serde_json::Number),
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Colon,
    Bool(bool),
    Null,
}

fn lex(src: &str) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut chars: Peekable<Chars> = src.chars().peekable();

    while let Some(&c) = chars.peek() {
        match c {
            c if c.is_whitespace() => {
                chars.next();
            }
            '(' => {
                chars.next();
                tokens.push(Token::LParen);
            }
            ')' => {
                chars.next();
                tokens.push(Token::RParen);
            }
            '{' | '}' | '[' | ']' | ',' | ':' => {
                chars.next();
                tokens.push(match c {
                    '{' => Token::LBrace,
                    '}' => Token::RBrace,
                    '[' => Token::LBracket,
                    ']' => Token::RBracket,
                    ',' => Token::Comma,
                    _ => Token::Colon,
                });
            }
            '&' => {
                chars.next();
                expect_char(&mut chars, '&', "&&")?;
                tokens.push(Token::And);
            }
            '|' => {
                chars.next();
                expect_char(&mut chars, '|', "||")?;
                tokens.push(Token::Or);
            }
            '!' => {
                chars.next();
                if chars.peek() == Some(&'=') {
                    chars.next();
                    tokens.push(Token::Op(CompareOp::Ne));
                } else {
                    tokens.push(Token::Not);
                }
            }
            '=' => {
                chars.next();
                expect_char(&mut chars, '=', "==")?;
                tokens.push(Token::Op(CompareOp::Eq));
            }
            '<' => {
                chars.next();
                if chars.peek() == Some(&'=') {
                    chars.next();
                    tokens.push(Token::Op(CompareOp::Le));
                } else {
                    tokens.push(Token::Op(CompareOp::Lt));
                }
            }
            '>' => {
                chars.next();
                if chars.peek() == Some(&'=') {
                    chars.next();
                    tokens.push(Token::Op(CompareOp::Ge));
                } else {
                    tokens.push(Token::Op(CompareOp::Gt));
                }
            }
            '\'' => {
                chars.next();
                if consume_triple_quote_rest(&mut chars) {
                    let mut s = String::new();
                    loop {
                        match chars.next() {
                            Some('\'') if consume_triple_quote_rest(&mut chars) => break,
                            Some(ch) => s.push(ch),
                            None => {
                                return Err(SolxError::Invalid(format!(
                                    "unterminated triple-quoted string literal in expression: {src}"
                                )))
                            }
                        }
                    }
                    tokens.push(Token::Str(s));
                } else {
                    let mut s = String::new();
                    loop {
                        match chars.next() {
                            Some('\\') if chars.peek() == Some(&'\'') => {
                                s.push(chars.next().unwrap());
                            }
                            Some('\'') => break,
                            Some(ch) => s.push(ch),
                            None => {
                                return Err(SolxError::Invalid(format!(
                                    "unterminated string literal in expression: {src}"
                                )))
                            }
                        }
                    }
                    tokens.push(Token::Str(s));
                }
            }
            '"' => {
                chars.next();
                let mut s = String::new();
                loop {
                    match chars.next() {
                        Some('\\') => lex_json_escape(&mut chars, &mut s, src)?,
                        Some('"') => break,
                        Some(ch) => s.push(ch),
                        None => {
                            return Err(SolxError::Invalid(format!(
                                "unterminated string literal in expression: {src}"
                            )))
                        }
                    }
                }
                tokens.push(Token::Template(s));
            }
            '$' => {
                chars.next();
                let mut name = String::new();
                while let Some(&ch) = chars.peek() {
                    if ch.is_alphanumeric() || ch == '_' || ch == '.' {
                        name.push(ch);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if name.is_empty() {
                    return Err(SolxError::Invalid(format!(
                        "empty variable reference in expression: {src}"
                    )));
                }
                tokens.push(Token::Var(name));
            }
            c if c.is_ascii_digit() || (c == '-' && starts_number(&chars)) => {
                let mut num = String::new();
                if c == '-' {
                    num.push(c);
                    chars.next();
                }
                while let Some(&ch) = chars.peek() {
                    if ch.is_ascii_digit() || ch == '.' {
                        num.push(ch);
                        chars.next();
                    } else if (ch == 'e' || ch == 'E') && !num.contains(['e', 'E']) {
                        // JSON exponent: 1e3, 2.5E-4
                        num.push(ch);
                        chars.next();
                        if let Some(&sign) = chars.peek().filter(|c| **c == '+' || **c == '-') {
                            num.push(sign);
                            chars.next();
                        }
                    } else {
                        break;
                    }
                }
                // Whole numbers stay integers, so `$port = 8765` substitutes
                // as `8765` rather than `8765.0`.
                let val = num
                    .parse::<i64>()
                    .ok()
                    .map(serde_json::Number::from)
                    .or_else(|| num.parse::<f64>().ok().and_then(serde_json::Number::from_f64))
                    .ok_or_else(|| {
                        SolxError::Invalid(format!("invalid number literal in expression: {num}"))
                    })?;
                tokens.push(Token::Num(val));
            }
            c if c.is_alphabetic() || c == '_' => {
                let mut word = String::new();
                while let Some(&ch) = chars.peek() {
                    if ch.is_alphanumeric() || ch == '_' {
                        word.push(ch);
                        chars.next();
                    } else {
                        break;
                    }
                }
                match word.as_str() {
                    "true" => tokens.push(Token::Bool(true)),
                    "false" => tokens.push(Token::Bool(false)),
                    "null" => tokens.push(Token::Null),
                    "contains" => tokens.push(Token::Op(CompareOp::Contains)),
                    other => {
                        return Err(SolxError::Invalid(format!(
                            "unexpected word '{other}' in expression: {src}"
                        )))
                    }
                }
            }
            other => {
                return Err(SolxError::Invalid(format!(
                    "unexpected character '{other}' in expression: {src}"
                )))
            }
        }
    }

    Ok(tokens)
}

/// Having consumed a `\` inside a double-quoted string, decode a JSON escape
/// (`\" \\ \/ \b \f \n \r \t \uXXXX`) into `out`. Any other escaped
/// character is kept as-is along with its backslash.
fn lex_json_escape(chars: &mut Peekable<Chars>, out: &mut String, src: &str) -> Result<()> {
    let bad = || SolxError::Invalid(format!("invalid \\u escape in string literal: {src}"));
    let hex4 =|chars: &mut Peekable<Chars>| -> Result<u32> {
        let digits: String = (0..4).filter_map(|_| chars.next()).collect();
        u32::from_str_radix(&digits, 16).map_err(|_| bad())
    };
    match chars.next() {
        Some('"') => out.push('"'),
        Some('\\') => out.push('\\'),
        Some('/') => out.push('/'),
        Some('b') => out.push('\u{8}'),
        Some('f') => out.push('\u{c}'),
        Some('n') => out.push('\n'),
        Some('r') => out.push('\r'),
        Some('t') => out.push('\t'),
        Some('u') => {
            let mut code = hex4(chars)?;
            // A UTF-16 surrogate pair spans two \u escapes.
            if (0xD800..0xDC00).contains(&code) {
                if chars.next() != Some('\\') || chars.next() != Some('u') {
                    return Err(bad());
                }
                let low = hex4(chars)?;
                code = 0x10000 + ((code - 0xD800) << 10) + (low.wrapping_sub(0xDC00) & 0x3FF);
            }
            out.push(char::from_u32(code).ok_or_else(bad)?);
        }
        Some(other) => {
            out.push('\\');
            out.push(other);
        }
        None => out.push('\\'),
    }
    Ok(())
}

fn expect_char(chars: &mut Peekable<Chars>, expected: char, op: &str) -> Result<()> {
    if chars.next_if_eq(&expected).is_some() {
        Ok(())
    } else {
        Err(SolxError::Invalid(format!(
            "expected '{op}' in expression (single '{expected}' is not a valid operator)"
        )))
    }
}

/// True when a leading `-` should be lexed as part of a numeric literal
/// (i.e. it is immediately followed by a digit), rather than treated as an
/// unexpected standalone character.
fn starts_number(chars: &Peekable<Chars>) -> bool {
    let mut clone = chars.clone();
    clone.next();
    matches!(clone.peek(), Some(c) if c.is_ascii_digit())
}

// ── Parser (recursive descent) ──────────────────────────────────────────────

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    /// Consume `tok` if it's next.
    fn eat(&mut self, tok: &Token) -> bool {
        if self.peek() == Some(tok) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// After an array/object item: a `,` (a trailing one before `close` is
    /// allowed) or the closing token itself, left for the caller's loop.
    fn end_of_item(&mut self, close: &Token, what: &str) -> Result<()> {
        if self.eat(&Token::Comma) || self.peek() == Some(close) {
            Ok(())
        } else {
            Err(SolxError::Invalid(format!(
                "expected ',' or closing bracket in {what}, found {:?}",
                self.peek()
            )))
        }
    }

    fn parse_or(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_and()?;
        while matches!(self.peek(), Some(Token::Or)) {
            self.next();
            let rhs = self.parse_and()?;
            lhs = Expr::Or(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_unary()?;
        while matches!(self.peek(), Some(Token::And)) {
            self.next();
            let rhs = self.parse_unary()?;
            lhs = Expr::And(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        if matches!(self.peek(), Some(Token::Not)) {
            self.next();
            let inner = self.parse_unary()?;
            return Ok(Expr::Not(Box::new(inner)));
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<Expr> {
        let lhs = self.parse_atom()?;
        if let Some(Token::Op(op)) = self.peek() {
            let op = *op;
            self.next();
            let rhs = self.parse_atom()?;
            return Ok(Expr::Compare(op, Box::new(lhs), Box::new(rhs)));
        }
        Ok(lhs)
    }

    fn parse_atom(&mut self) -> Result<Expr> {
        match self.next() {
            Some(Token::LParen) => {
                let inner = self.parse_or()?;
                match self.next() {
                    Some(Token::RParen) => Ok(inner),
                    _ => Err(SolxError::Invalid("missing closing ')' in expression".into())),
                }
            }
            Some(Token::Var(name)) => Ok(Expr::Var(name)),
            Some(Token::Str(s)) => Ok(Expr::Literal(Value::String(s))),
            Some(Token::Template(s)) if s.contains("${") => Ok(Expr::Template(s)),
            Some(Token::Template(s)) => Ok(Expr::Literal(Value::String(s))),
            Some(Token::Num(n)) => Ok(Expr::Literal(Value::Number(n))),
            Some(Token::Bool(b)) => Ok(Expr::Literal(Value::Bool(b))),
            Some(Token::Null) => Ok(Expr::Literal(Value::Null)),
            Some(Token::LBracket) => {
                let mut items = Vec::new();
                while !self.eat(&Token::RBracket) {
                    items.push(self.parse_or()?);
                    self.end_of_item(&Token::RBracket, "array")?;
                }
                Ok(Expr::Array(items))
            }
            Some(Token::LBrace) => {
                let mut fields = Vec::new();
                while !self.eat(&Token::RBrace) {
                    let key = match self.next() {
                        Some(Token::Str(k)) => Expr::Literal(Value::String(k)),
                        Some(Token::Template(k)) if k.contains("${") => Expr::Template(k),
                        Some(Token::Template(k)) => Expr::Literal(Value::String(k)),
                        other => {
                            return Err(SolxError::Invalid(format!(
                                "object keys must be quoted strings, found {other:?}"
                            )))
                        }
                    };
                    if !self.eat(&Token::Colon) {
                        return Err(SolxError::Invalid("expected ':' after object key".into()));
                    }
                    fields.push((key, self.parse_or()?));
                    self.end_of_item(&Token::RBrace, "object")?;
                }
                Ok(Expr::Object(fields))
            }
            other => Err(SolxError::Invalid(format!(
                "expected a value in expression, found {other:?}"
            ))),
        }
    }
}

/// Parse a condition expression, e.g. `$status == "ready" && $count > 0`.
pub fn parse_expr(src: &str) -> Result<Expr> {
    let tokens = lex(src)?;
    if tokens.is_empty() {
        return Err(SolxError::Invalid("empty expression".into()));
    }
    let mut parser = Parser { tokens, pos: 0 };
    let expr = parser.parse_or()?;
    if parser.pos != parser.tokens.len() {
        return Err(SolxError::Invalid(format!(
            "trailing tokens after expression: {src}"
        )));
    }
    Ok(expr)
}

// ── Evaluator ────────────────────────────────────────────────────────────────

pub fn eval_expr(expr: &Expr, ctx: &HashMap<String, Value>) -> Result<Value> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Var(name) => Ok(resolve_var(name, ctx)),
        Expr::Template(s) => Ok(Value::String(interpolate(s, ctx))),
        Expr::Array(items) => Ok(Value::Array(
            items.iter().map(|e| eval_expr(e, ctx)).collect::<Result<_>>()?,
        )),
        Expr::Object(fields) => {
            let mut map = serde_json::Map::new();
            for (k, v) in fields {
                let key = match eval_expr(k, ctx)? {
                    Value::String(s) => s,
                    other => other.to_string(),
                };
                map.insert(key, eval_expr(v, ctx)?);
            }
            Ok(Value::Object(map))
        }
        Expr::Not(inner) => Ok(Value::Bool(!is_truthy(&eval_expr(inner, ctx)?))),
        Expr::And(a, b) => {
            let lhs = eval_expr(a, ctx)?;
            if !is_truthy(&lhs) {
                return Ok(lhs);
            }
            eval_expr(b, ctx)
        }
        Expr::Or(a, b) => {
            let lhs = eval_expr(a, ctx)?;
            if is_truthy(&lhs) {
                return Ok(lhs);
            }
            eval_expr(b, ctx)
        }
        Expr::Compare(op, a, b) => {
            let lhs = eval_expr(a, ctx)?;
            let rhs = eval_expr(b, ctx)?;
            eval_compare(*op, &lhs, &rhs)
        }
    }
}

/// Fill in each `${name.path}` in a double-quoted string with the variable's
/// text (strings raw, other values as JSON, missing as `null` — the same
/// rendering pipeline arguments use). A bare `$name` is literal, so pasted
/// JSON like `"$schema"` is left alone; a `${` that isn't a valid reference
/// (`${}`, `${a b}`, no closing `}`) is kept as-is.
fn interpolate(s: &str, ctx: &HashMap<String, Value>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let name = after.find('}').map(|end| &after[..end]).filter(|n| {
            !n.is_empty()
                && !n.starts_with('.')
                && !n.ends_with('.')
                && n.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.')
        });
        match name {
            Some(name) => {
                out.push_str(&value_to_arg_string(&resolve_var(name, ctx)));
                rest = &after[name.len() + 1..];
            }
            None => {
                out.push_str("${");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn resolve_var(name: &str, ctx: &HashMap<String, Value>) -> Value {
    let (var_name, field_path) = match name.find('.') {
        Some(dot) => (&name[..dot], Some(&name[dot + 1..])),
        None => (name, None),
    };
    match ctx.get(var_name) {
        Some(val) => match field_path {
            Some(path) => navigate_json_path(val, path),
            None => val.clone(),
        },
        None => Value::Null,
    }
}

fn eval_compare(op: CompareOp, lhs: &Value, rhs: &Value) -> Result<Value> {
    let result = match op {
        CompareOp::Eq => values_eq(lhs, rhs),
        CompareOp::Ne => !values_eq(lhs, rhs),
        CompareOp::Lt | CompareOp::Le | CompareOp::Gt | CompareOp::Ge => {
            let (l, r) = (as_number(lhs)?, as_number(rhs)?);
            match op {
                CompareOp::Lt => l < r,
                CompareOp::Le => l <= r,
                CompareOp::Gt => l > r,
                CompareOp::Ge => l >= r,
                _ => unreachable!(),
            }
        }
        CompareOp::Contains => contains(lhs, rhs),
    };
    Ok(Value::Bool(result))
}

fn values_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

fn as_number(v: &Value) -> Result<f64> {
    v.as_f64().ok_or_else(|| {
        SolxError::Invalid(format!(
            "ordering comparison requires a number, got: {v}"
        ))
    })
}

fn contains(haystack: &Value, needle: &Value) -> bool {
    match haystack {
        Value::String(s) => match needle {
            Value::String(n) => s.contains(n.as_str()),
            _ => false,
        },
        Value::Array(arr) => arr.iter().any(|el| values_eq(el, needle)),
        Value::Object(map) => match needle {
            Value::String(key) => map.contains_key(key),
            _ => false,
        },
        _ => false,
    }
}

/// Truthiness used both for bare-atom conditions and for `&&`/`||`/`!`
/// operands: `null`/`false` are false, numeric `0` is false, an empty
/// string/array/object is false, everything else is true.
pub fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    fn eval(src: &str, ctx: &HashMap<String, Value>) -> Value {
        eval_expr(&parse_expr(src).unwrap(), ctx).unwrap()
    }

    #[test]
    fn compares_numbers() {
        let ctx = HashMap::new();
        assert_eq!(eval("1 < 2", &ctx), Value::Bool(true));
        assert_eq!(eval("2 <= 2", &ctx), Value::Bool(true));
        assert_eq!(eval("3 > 2", &ctx), Value::Bool(true));
        assert_eq!(eval("2 >= 3", &ctx), Value::Bool(false));
        assert_eq!(eval("2 == 2.0", &ctx), Value::Bool(true));
        assert_eq!(eval("2 != 3", &ctx), Value::Bool(true));
    }

    #[test]
    fn compares_strings_and_vars() {
        let ctx = ctx_with(&[("status", Value::String("ready".into()))]);
        assert_eq!(eval(r#"$status == "ready""#, &ctx), Value::Bool(true));
        assert_eq!(eval(r#"$status != "pending""#, &ctx), Value::Bool(true));
    }

    #[test]
    fn compares_triple_quoted_string_with_apostrophe() {
        let ctx = ctx_with(&[("name", Value::String("can't stop".into()))]);
        assert_eq!(eval(r#"$name == '''can't stop'''"#, &ctx), Value::Bool(true));
    }

    #[test]
    fn dotted_var_path() {
        let ctx = ctx_with(&[(
            "res",
            serde_json::json!({"a": {"b": 5}}),
        )]);
        assert_eq!(eval("$res.a.b == 5", &ctx), Value::Bool(true));
    }

    #[test]
    fn and_or_not_precedence() {
        let ctx = HashMap::new();
        // && binds tighter than ||
        assert_eq!(eval("true || false && false", &ctx), Value::Bool(true));
        assert_eq!(eval("!false && true", &ctx), Value::Bool(true));
        assert_eq!(eval("!(true && false)", &ctx), Value::Bool(true));
    }

    #[test]
    fn short_circuits() {
        // Malformed rhs would fail to parse if evaluated, but a parenthesized
        // rhs is still parsed eagerly; short-circuit only affects evaluation,
        // not parsing. Verify evaluation-level short circuit via a comparison
        // that would error if it were evaluated.
        let ctx = ctx_with(&[("n", Value::String("x".into()))]);
        // false && (non-number > 1) -- must not error, since rhs is skipped.
        assert_eq!(eval("false && $n > 1", &ctx), Value::Bool(false));
        assert_eq!(eval("true || $n > 1", &ctx), Value::Bool(true));
    }

    #[test]
    fn contains_variants() {
        let ctx = ctx_with(&[
            ("s", Value::String("hello world".into())),
            ("arr", serde_json::json!([1, 2, 3])),
            ("obj", serde_json::json!({"k": 1})),
        ]);
        assert_eq!(eval(r#"$s contains "world""#, &ctx), Value::Bool(true));
        assert_eq!(eval("$arr contains 2", &ctx), Value::Bool(true));
        assert_eq!(eval("$arr contains 9", &ctx), Value::Bool(false));
        assert_eq!(eval(r#"$obj contains "k""#, &ctx), Value::Bool(true));
    }

    #[test]
    fn bare_var_is_truthy_check() {
        let ctx = ctx_with(&[("x", Value::Bool(true)), ("y", Value::Null)]);
        assert!(is_truthy(&eval("$x", &ctx)));
        assert!(!is_truthy(&eval("$y", &ctx)));
    }

    #[test]
    fn ordering_non_number_errors() {
        let ctx = ctx_with(&[("s", Value::String("abc".into()))]);
        let err = eval_expr(&parse_expr("$s > 1").unwrap(), &ctx).unwrap_err();
        assert!(matches!(err, SolxError::Invalid(_)));
    }

    #[test]
    fn and_or_return_deciding_operand() {
        let ctx = ctx_with(&[("t", serde_json::json!(30)), ("z", serde_json::json!(0))]);
        assert_eq!(eval("$t || 300", &ctx), serde_json::json!(30));
        assert_eq!(eval("$z || 300", &ctx), serde_json::json!(300));
        assert_eq!(eval("$missing || \"d\"", &ctx), serde_json::json!("d"));
        assert_eq!(eval("$z && 5", &ctx), serde_json::json!(0));
        assert_eq!(eval("$t && 5", &ctx), serde_json::json!(5));
    }

    #[test]
    fn whole_numbers_stay_integers() {
        let ctx = HashMap::new();
        assert_eq!(eval("8765", &ctx).to_string(), "8765");
        assert_eq!(eval("-3", &ctx).to_string(), "-3");
        assert_eq!(eval("1.5", &ctx).to_string(), "1.5");
    }

    #[test]
    fn braced_refs_interpolate_bare_dollars_are_literal() {
        let ctx = ctx_with(&[
            ("id", serde_json::json!(42)),
            ("r", serde_json::json!({"uri": "http://x/cb"})),
            ("name", serde_json::json!("Ann")),
            ("schema", serde_json::json!("OOPS")),
        ]);
        assert_eq!(
            eval(r#""a?id=${id}&u=${r.uri}""#, &ctx),
            serde_json::json!("a?id=42&u=http://x/cb")
        );
        assert_eq!(eval(r#""Hi ${name}.""#, &ctx), serde_json::json!("Hi Ann."));
        assert_eq!(eval(r#""x${missing}y""#, &ctx), serde_json::json!("xnully"));
        // Bare `$name` is literal even when a variable of that name exists,
        // so pasted JSON (`"$schema"`, `"$ref"`) is never rewritten.
        assert_eq!(eval(r#""$schema $name""#, &ctx), serde_json::json!("$schema $name"));
        assert_eq!(eval(r#""${} ${a b} ${.x} ${open""#, &ctx), serde_json::json!("${} ${a b} ${.x} ${open"));
        assert_eq!(eval("'${id}'", &ctx), serde_json::json!("${id}"));
        assert_eq!(eval(r#""${name}" == "Ann""#, &ctx), Value::Bool(true));
    }

    #[test]
    fn object_and_array_literals() {
        let ctx = ctx_with(&[
            ("port", serde_json::json!(8765)),
            ("page", serde_json::json!({"id": "1", "name": "P"})),
            ("who", serde_json::json!("Ann")),
        ]);
        assert_eq!(
            eval(
                r#"{"port": $port, "page": $page, "missing": $nope, "msg": "hi ${who}", 'lit': '${who}', "$schema": "$who", "list": [1, "two", $port, [], {},], "fb": $nope || 3}"#,
                &ctx
            ),
            serde_json::json!({
                "port": 8765, "page": {"id": "1", "name": "P"}, "missing": null,
                "msg": "hi Ann", "lit": "${who}", "$schema": "$who",
                "list": [1, "two", 8765, [], {}], "fb": 3
            })
        );
        assert_eq!(eval(r#"{"${who}": 1}"#, &ctx), serde_json::json!({"Ann": 1}));
        assert_eq!(eval("[]", &ctx), serde_json::json!([]));
        assert!(is_truthy(&eval("$page contains \"id\" && [1] != []", &ctx)));
    }

    #[test]
    fn malformed_literals_error() {
        for src in [r#"{a: 1}"#, r#"{"a" 1}"#, r#"{"a": 1"#, "[1 2]", "[1,", r#"{"a": 1,, }"#] {
            assert!(parse_expr(src).is_err(), "{src}");
        }
    }

    #[test]
    fn missing_var_resolves_null() {
        let ctx = HashMap::new();
        assert_eq!(eval("$missing", &ctx), Value::Null);
    }

    #[test]
    fn rejects_single_ampersand() {
        assert!(parse_expr("$x & $y").is_err());
    }

    #[test]
    fn rejects_trailing_garbage() {
        assert!(parse_expr("true true").is_err());
    }

    #[test]
    fn rejects_unterminated_paren() {
        assert!(parse_expr("(true").is_err());
    }
}
