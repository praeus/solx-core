//! Boolean expression grammar used by `if`/`elseif` conditions.
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

use crate::navigate_json_path;

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Value),
    Var(String),
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
    Num(f64),
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
            '\'' | '"' => {
                let quote = c;
                chars.next();
                let mut s = String::new();
                loop {
                    match chars.next() {
                        Some('\\') if chars.peek() == Some(&quote) => {
                            s.push(chars.next().unwrap());
                        }
                        Some(ch) if ch == quote => break,
                        Some(ch) => s.push(ch),
                        None => {
                            return Err(SolxError::Invalid(format!(
                                "unterminated string literal in condition: {src}"
                            )))
                        }
                    }
                }
                tokens.push(Token::Str(s));
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
                        "empty variable reference in condition: {src}"
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
                    } else {
                        break;
                    }
                }
                let val = num.parse::<f64>().map_err(|_| {
                    SolxError::Invalid(format!("invalid number literal in condition: {num}"))
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
                            "unexpected word '{other}' in condition: {src}"
                        )))
                    }
                }
            }
            other => {
                return Err(SolxError::Invalid(format!(
                    "unexpected character '{other}' in condition: {src}"
                )))
            }
        }
    }

    Ok(tokens)
}

fn expect_char(chars: &mut Peekable<Chars>, expected: char, op: &str) -> Result<()> {
    if chars.next_if_eq(&expected).is_some() {
        Ok(())
    } else {
        Err(SolxError::Invalid(format!(
            "expected '{op}' in condition (single '{expected}' is not a valid operator)"
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
                    _ => Err(SolxError::Invalid("missing closing ')' in condition".into())),
                }
            }
            Some(Token::Var(name)) => Ok(Expr::Var(name)),
            Some(Token::Str(s)) => Ok(Expr::Literal(Value::String(s))),
            Some(Token::Num(n)) => Ok(Expr::Literal(
                serde_json::Number::from_f64(n)
                    .map(Value::Number)
                    .unwrap_or(Value::Null),
            )),
            Some(Token::Bool(b)) => Ok(Expr::Literal(Value::Bool(b))),
            Some(Token::Null) => Ok(Expr::Literal(Value::Null)),
            other => Err(SolxError::Invalid(format!(
                "expected a value in condition, found {other:?}"
            ))),
        }
    }
}

/// Parse a condition expression, e.g. `$status == "ready" && $count > 0`.
pub fn parse_expr(src: &str) -> Result<Expr> {
    let tokens = lex(src)?;
    if tokens.is_empty() {
        return Err(SolxError::Invalid("empty condition".into()));
    }
    let mut parser = Parser { tokens, pos: 0 };
    let expr = parser.parse_or()?;
    if parser.pos != parser.tokens.len() {
        return Err(SolxError::Invalid(format!(
            "trailing tokens after condition: {src}"
        )));
    }
    Ok(expr)
}

// ── Evaluator ────────────────────────────────────────────────────────────────

pub fn eval_expr(expr: &Expr, ctx: &HashMap<String, Value>) -> Result<Value> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Var(name) => Ok(resolve_var(name, ctx)),
        Expr::Not(inner) => Ok(Value::Bool(!is_truthy(&eval_expr(inner, ctx)?))),
        Expr::And(a, b) => {
            let lhs = eval_expr(a, ctx)?;
            if !is_truthy(&lhs) {
                return Ok(Value::Bool(false));
            }
            Ok(Value::Bool(is_truthy(&eval_expr(b, ctx)?)))
        }
        Expr::Or(a, b) => {
            let lhs = eval_expr(a, ctx)?;
            if is_truthy(&lhs) {
                return Ok(Value::Bool(true));
            }
            Ok(Value::Bool(is_truthy(&eval_expr(b, ctx)?)))
        }
        Expr::Compare(op, a, b) => {
            let lhs = eval_expr(a, ctx)?;
            let rhs = eval_expr(b, ctx)?;
            eval_compare(*op, &lhs, &rhs)
        }
    }
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
