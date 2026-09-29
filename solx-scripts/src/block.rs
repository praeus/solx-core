//! Recursive-descent pass that turns the flat, quote-aware statement list
//! (split on `;`, plus newlines/`then` after keywords — see
//! [`push_statements`]) into a nested [`Statement`] tree. Unlike a marker-scan
//! (find the matching end after the fact), mismatches are caught immediately
//! as parse errors: a `for` closed by `endif`, a missing `endfor`, an
//! `elseif` after `else`, etc.

use solx_surface::error::{Result, SolxError};

use crate::ast::{ForSource, Statement};
use crate::expr::{parse_expr, Expr};
use crate::{parse_assignment, split_respecting_quotes, split_respecting_quotes_by};

enum Keyword {
    If(Expr),
    ElseIf(Expr),
    Else,
    EndIf,
    For { item_var: String, source: ForSource },
    EndFor,
    Wait(String),
    Plain(String),
}

/// Keywords whose statement also ends at the first unquoted newline, so
/// `if $x`, `else`, `endif` etc. don't need a trailing `;`. Plain statements
/// still end only at `;` and may span lines.
const LINE_KEYWORDS: &[&str] = &["if", "elseif", "else", "endif", "for", "endfor", "wait"];

/// Parse a full script source into a statement tree.
pub fn parse_program(source: &str) -> Result<Vec<Statement>> {
    let mut stmts = Vec::new();
    for piece in split_respecting_quotes(source, ';') {
        push_statements(&piece, &mut stmts);
    }

    let mut pos = 0;
    let body = parse_block(&stmts, &mut pos)?;
    if pos != stmts.len() {
        return Err(SolxError::Invalid(format!(
            "unexpected '{}' with no matching 'if'/'for'",
            stmts[pos]
        )));
    }
    Ok(body)
}

/// Split one `;`-delimited piece into statements. A piece starting with a
/// [`LINE_KEYWORDS`] word ends at its first unquoted newline, and an
/// `if`/`elseif` header additionally ends at a standalone `then`; whatever
/// follows is split again the same way.
fn push_statements(piece: &str, out: &mut Vec<String>) {
    let mut rest = piece.trim().to_string();
    while !rest.is_empty() {
        let (word, _) = split_first_word(&rest);
        if !LINE_KEYWORDS.contains(&word) {
            out.push(rest);
            return;
        }
        let is_conditional = word == "if" || word == "elseif" || is_else_if(&rest);
        let mut lines = split_respecting_quotes(&rest, '\n').into_iter();
        let header = lines.next().unwrap_or_default();
        let remainder = lines.collect::<Vec<_>>().join("\n");
        let (header, after_then) = if is_conditional {
            split_at_then(&header)
        } else {
            (header, String::new())
        };
        out.push(header.trim().to_string());
        rest = format!("{after_then}\n{remainder}").trim().to_string();
    }
}

/// Whether `stmt` is an `else if <cond>` header (same as `elseif <cond>`).
fn is_else_if(stmt: &str) -> bool {
    let (word, rest) = split_first_word(stmt);
    word == "else" && split_first_word(rest).0 == "if"
}

/// Split an `if`/`elseif` header line at its first unquoted, whitespace-
/// delimited `then`, returning `(header, statement after then)`. Without a
/// `then`, the whole line is the header.
fn split_at_then(line: &str) -> (String, String) {
    let words: Vec<String> = split_respecting_quotes_by(line, char::is_whitespace)
        .into_iter()
        .filter(|w| !w.is_empty())
        .collect();
    match words.iter().position(|w| w == "then") {
        Some(idx) => (words[..idx].join(" "), words[idx + 1..].join(" ")),
        None => (line.to_string(), String::new()),
    }
}

fn parse_block(stmts: &[String], pos: &mut usize) -> Result<Vec<Statement>> {
    let mut out = Vec::new();
    while *pos < stmts.len() {
        match classify(&stmts[*pos])? {
            Keyword::EndIf | Keyword::EndFor | Keyword::ElseIf(_) | Keyword::Else => break,
            Keyword::If(cond) => {
                *pos += 1;
                out.push(parse_if(cond, stmts, pos)?);
            }
            Keyword::For { item_var, source } => {
                *pos += 1;
                let body = parse_block(stmts, pos)?;
                expect_keyword(stmts, pos, "endfor")?;
                out.push(Statement::For {
                    item_var,
                    source,
                    body,
                });
            }
            Keyword::Wait(amount_src) => {
                *pos += 1;
                out.push(Statement::Wait { amount_src });
            }
            Keyword::Plain(raw) => {
                *pos += 1;
                let (var, src) = parse_assignment(&raw);
                if is_value_expr(&src) {
                    out.push(Statement::Value { var, expr: parse_expr(&src)? });
                } else {
                    out.push(Statement::Pipeline { var, src });
                }
            }
        }
    }
    Ok(out)
}

/// Whether a statement body is a value expression rather than a command
/// pipeline. A pipeline always starts with its command name (`exec`, `json`,
/// ...), so anything starting with a `$var`, a literal, `{`, `[`, `(` or `!`
/// can't be one.
fn is_value_expr(src: &str) -> bool {
    let first = split_first_word(src).0;
    matches!(first, "true" | "false" | "null")
        || src.chars().next().is_some_and(|c| {
            c.is_ascii_digit() || matches!(c, '$' | '-' | '"' | '\'' | '(' | '!' | '{' | '[')
        })
}

fn parse_if(first_cond: Expr, stmts: &[String], pos: &mut usize) -> Result<Statement> {
    let mut branches = vec![(first_cond, parse_block(stmts, pos)?)];
    let mut else_branch = None;
    loop {
        if *pos >= stmts.len() {
            return Err(SolxError::Invalid("unterminated 'if': missing 'endif'".into()));
        }
        match classify(&stmts[*pos])? {
            Keyword::ElseIf(cond) => {
                *pos += 1;
                branches.push((cond, parse_block(stmts, pos)?));
            }
            Keyword::Else => {
                *pos += 1;
                else_branch = Some(parse_block(stmts, pos)?);
                expect_keyword(stmts, pos, "endif")?;
                break;
            }
            Keyword::EndIf => {
                *pos += 1;
                break;
            }
            _ => {
                return Err(SolxError::Invalid(format!(
                    "expected 'elseif', 'else', or 'endif', found: {}",
                    stmts[*pos]
                )))
            }
        }
    }
    Ok(Statement::If {
        branches,
        else_branch,
    })
}

fn expect_keyword(stmts: &[String], pos: &mut usize, expected: &str) -> Result<()> {
    if *pos >= stmts.len() {
        return Err(SolxError::Invalid(format!("missing '{expected}'")));
    }
    let ok = match (classify(&stmts[*pos])?, expected) {
        (Keyword::EndIf, "endif") => true,
        (Keyword::EndFor, "endfor") => true,
        _ => false,
    };
    if !ok {
        return Err(SolxError::Invalid(format!(
            "expected '{expected}', found: {}",
            stmts[*pos]
        )));
    }
    *pos += 1;
    Ok(())
}

fn classify(stmt: &str) -> Result<Keyword> {
    let (word, rest) = split_first_word(stmt);
    match word {
        "if" => Ok(Keyword::If(parse_expr(rest)?)),
        "elseif" => Ok(Keyword::ElseIf(parse_expr(rest)?)),
        "else" => {
            if is_else_if(stmt) {
                let (_, cond) = split_first_word(rest);
                return Ok(Keyword::ElseIf(parse_expr(cond)?));
            }
            require_empty(rest, "else")?;
            Ok(Keyword::Else)
        }
        "endif" => {
            require_empty(rest, "endif")?;
            Ok(Keyword::EndIf)
        }
        "endfor" => {
            require_empty(rest, "endfor")?;
            Ok(Keyword::EndFor)
        }
        "for" => parse_for_header(rest),
        "then" => Err(SolxError::Invalid(
            "'then' must be on the same line as its 'if'/'elseif' condition".into(),
        )),
        "wait" => {
            let amount = rest.trim();
            if amount.is_empty() {
                return Err(SolxError::Invalid(
                    "'wait' requires an amount, e.g. 'wait 5' or 'wait $secs'".into(),
                ));
            }
            Ok(Keyword::Wait(amount.to_string()))
        }
        _ => Ok(Keyword::Plain(stmt.to_string())),
    }
}

fn require_empty(rest: &str, keyword: &str) -> Result<()> {
    if rest.trim().is_empty() {
        Ok(())
    } else {
        Err(SolxError::Invalid(format!(
            "'{keyword}' takes no arguments, got: {keyword} {rest}"
        )))
    }
}

fn parse_for_header(rest: &str) -> Result<Keyword> {
    let rest = rest.trim();
    let after_dollar = rest.strip_prefix('$').ok_or_else(|| {
        SolxError::Invalid(format!(
            "'for' must start with a loop variable, e.g. 'for $item in ...': for {rest}"
        ))
    })?;
    let (item_var, after_var) = split_first_word(after_dollar);
    if item_var.is_empty() || !item_var.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return Err(SolxError::Invalid(format!(
            "invalid loop variable name in 'for' header: for {rest}"
        )));
    }
    let (in_word, source_src) = split_first_word(after_var.trim());
    if in_word != "in" {
        return Err(SolxError::Invalid(format!(
            "expected 'in' after loop variable in 'for' header: for {rest}"
        )));
    }
    let source_src = source_src.trim();
    if source_src.is_empty() {
        return Err(SolxError::Invalid(format!(
            "'for' requires a source after 'in': for {rest}"
        )));
    }
    let source = match try_parse_range(source_src) {
        Some((start, end)) => ForSource::Range { start, end },
        None => ForSource::Pipeline(source_src.to_string()),
    };
    Ok(Keyword::For {
        item_var: item_var.to_string(),
        source,
    })
}

fn try_parse_range(s: &str) -> Option<(i64, i64)> {
    let (start, end) = s.split_once("..")?;
    let start = start.trim().parse::<i64>().ok()?;
    let end = end.trim().parse::<i64>().ok()?;
    Some((start, end))
}

fn split_first_word(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.find(char::is_whitespace) {
        Some(idx) => (&s[..idx], &s[idx..]),
        None => (s, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Vec<Statement> {
        parse_program(src).unwrap()
    }

    #[test]
    fn flat_script_is_all_pipelines() {
        let stmts = parse("$doc = get doc /a/b; get action $doc.name");
        assert_eq!(stmts.len(), 2);
        assert!(matches!(&stmts[0], Statement::Pipeline { var: Some(v), .. } if v == "doc"));
        assert!(matches!(&stmts[1], Statement::Pipeline { var: None, .. }));
    }

    #[test]
    fn simple_if_endif() {
        let stmts = parse("if $x; json 1; endif");
        assert_eq!(stmts.len(), 1);
        match &stmts[0] {
            Statement::If {
                branches,
                else_branch,
            } => {
                assert_eq!(branches.len(), 1);
                assert_eq!(branches[0].1.len(), 1);
                assert!(else_branch.is_none());
            }
            other => panic!("expected If, got {other:?}"),
        }
    }

    #[test]
    fn if_elseif_else_chain() {
        let stmts = parse("if $x == 1; json 1; elseif $x == 2; json 2; else; json 3; endif");
        match &stmts[0] {
            Statement::If {
                branches,
                else_branch,
            } => {
                assert_eq!(branches.len(), 2);
                assert!(else_branch.is_some());
            }
            other => panic!("expected If, got {other:?}"),
        }
    }

    #[test]
    fn nested_if_and_for() {
        let stmts = parse("for $item in $arr; if $item > 0; json $item; endif; endfor");
        match &stmts[0] {
            Statement::For { body, .. } => {
                assert_eq!(body.len(), 1);
                assert!(matches!(body[0], Statement::If { .. }));
            }
            other => panic!("expected For, got {other:?}"),
        }
    }

    #[test]
    fn for_range_form() {
        let stmts = parse("for $i in 0..3; json $i; endfor");
        match &stmts[0] {
            Statement::For {
                item_var, source, ..
            } => {
                assert_eq!(item_var, "i");
                assert_eq!(*source, ForSource::Range { start: 0, end: 3 });
            }
            other => panic!("expected For, got {other:?}"),
        }
    }

    #[test]
    fn missing_endif_is_error() {
        assert!(parse_program("if $x; json 1").is_err());
    }

    #[test]
    fn missing_endfor_is_error() {
        assert!(parse_program("for $i in 0..3; json $i").is_err());
    }

    #[test]
    fn mismatched_end_marker_is_error() {
        assert!(parse_program("if $x; json 1; endfor").is_err());
        assert!(parse_program("for $i in 0..3; json $i; endif").is_err());
    }

    #[test]
    fn stray_endif_is_error() {
        assert!(parse_program("json 1; endif").is_err());
    }

    #[test]
    fn else_with_condition_is_error() {
        assert!(parse_program("if $x; json 1; else $y; json 2; endif").is_err());
    }

    #[test]
    fn elseif_after_else_is_error() {
        assert!(parse_program("if $x; json 1; else; json 2; elseif $y; json 3; endif").is_err());
    }

    fn flat(src: &str) -> Vec<String> {
        let mut out = Vec::new();
        for piece in split_respecting_quotes(src, ';') {
            push_statements(&piece, &mut out);
        }
        out
    }

    #[test]
    fn keywords_end_at_newline_without_semicolon() {
        let src = "if $x == null\n  $y = json 1;\nelseif $x > 2\n  json 2;\nelse\n  for $i in 0..2\n    json $i;\n  endfor\nendif\nwait 1\njson 3";
        assert_eq!(
            flat(src),
            vec![
                "if $x == null", "$y = json 1", "elseif $x > 2", "json 2", "else",
                "for $i in 0..2", "json $i", "endfor", "endif", "wait 1", "json 3",
            ]
        );
    }

    #[test]
    fn semicolon_and_newline_styles_mix() {
        assert_eq!(
            flat("if $x;\n  json 1;\nelse\n  json 2;\nendif;"),
            vec!["if $x", "json 1", "else", "json 2", "endif"]
        );
    }

    #[test]
    fn plain_statements_still_span_lines() {
        assert_eq!(
            flat("if $x\n  exec /a\n    --json '''{\n\"k\": 1\n}''';\nendif"),
            vec!["if $x", "exec /a\n    --json '''{\n\"k\": 1\n}'''", "endif"]
        );
    }

    #[test]
    fn then_ends_the_condition() {
        assert_eq!(
            flat("if $x == 1 then json 1; elseif $x == 2 then json 2; else; json 3; endif"),
            vec!["if $x == 1", "json 1", "elseif $x == 2", "json 2", "else", "json 3", "endif"]
        );
        assert_eq!(
            flat("if $x then\n  json 1;\nendif"),
            vec!["if $x", "json 1", "endif"]
        );
        assert_eq!(flat("if $x then; json 1; endif"), vec!["if $x", "json 1", "endif"]);
        assert_eq!(
            flat("if $a then if $b then json 1; endif; endif"),
            vec!["if $a", "if $b", "json 1", "endif", "endif"]
        );
    }

    #[test]
    fn quoted_or_embedded_then_is_not_a_keyword() {
        assert_eq!(
            flat("if $x == \"a then b\"; json 1; endif"),
            vec!["if $x == \"a then b\"", "json 1", "endif"]
        );
        assert_eq!(flat("if $x.then; json 1; endif"), vec!["if $x.then", "json 1", "endif"]);
        // `then` is only special in an if/elseif header.
        assert_eq!(flat("json then"), vec!["json then"]);
    }

    #[test]
    fn then_on_its_own_line_is_error() {
        assert!(parse_program("if $x\nthen\n  json 1;\nendif").is_err());
    }

    #[test]
    fn else_if_is_elseif() {
        let a = parse("if $x then json 1; else if $y then json 2; else; json 3; endif");
        let b = parse("if $x; json 1; elseif $y; json 2; else; json 3; endif");
        assert_eq!(a, b);
        assert_eq!(
            flat("if $x\n  json 1;\nelse if $y then\n  json 2;\nendif"),
            vec!["if $x", "json 1", "else if $y", "json 2", "endif"]
        );
    }

    #[test]
    fn if_on_line_after_else_is_nested() {
        let stmts = parse("if $x; json 1; else\n  if $y; json 2; endif\nendif");
        let Statement::If { branches, else_branch } = &stmts[0] else { panic!() };
        assert_eq!(branches.len(), 1);
        assert!(matches!(else_branch.as_deref(), Some([Statement::If { .. }])));
    }

    #[test]
    fn wait_requires_amount() {
        assert!(parse_program("wait").is_err());
        assert!(matches!(&parse("wait 5")[0], Statement::Wait { amount_src } if amount_src == "5"));
    }
}
