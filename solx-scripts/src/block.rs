//! Recursive-descent pass that turns the flat, quote-aware `;`-split
//! statement list into a nested [`Statement`] tree. Unlike a marker-scan
//! (find the matching end after the fact), mismatches are caught immediately
//! as parse errors: a `for` closed by `endif`, a missing `endfor`, an
//! `elseif` after `else`, etc.

use solx_surface::error::{Result, SolxError};

use crate::ast::{ForSource, Statement};
use crate::expr::{parse_expr, Expr};
use crate::{parse_assignment, split_respecting_quotes};

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

/// Parse a full script source into a statement tree.
pub fn parse_program(source: &str) -> Result<Vec<Statement>> {
    let stmts: Vec<String> = split_respecting_quotes(source, ';')
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

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
                out.push(Statement::Pipeline { var, src });
            }
        }
    }
    Ok(out)
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

    #[test]
    fn wait_requires_amount() {
        assert!(parse_program("wait").is_err());
        assert!(matches!(&parse("wait 5")[0], Statement::Wait { amount_src } if amount_src == "5"));
    }
}
