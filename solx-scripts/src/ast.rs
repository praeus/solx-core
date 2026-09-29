//! Structured statement tree built from the flat `;`-separated statement
//! list by [`crate::block`].

use crate::expr::Expr;

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    /// `$name = <pipeline>` or a bare `<pipeline>` — today's only statement
    /// kind, unchanged from before control flow existed.
    Pipeline { var: Option<String>, src: String },
    /// `$name = <expr>` or a bare `<expr>` — a value computed directly (see
    /// [`crate::expr`]) instead of by a command stage, e.g. `$port = 8765`,
    /// `$t = $params.timeout_secs || 300`, or `$result`.
    Value { var: Option<String>, expr: Expr },
    /// `if <expr> [then] ; ... [else if <expr> [then] ; ...]* [else ; ...]? endif`
    /// (a newline may stand in for each `;` after a keyword)
    If {
        branches: Vec<(Expr, Vec<Statement>)>,
        else_branch: Option<Vec<Statement>>,
    },
    /// `for $item in <pipeline-or-range> ; ... endfor`
    For {
        item_var: String,
        source: ForSource,
        body: Vec<Statement>,
    },
    /// `wait <seconds>` — amount is a raw (unsubstituted) token, resolved
    /// against `$var`s at execution time.
    Wait { amount_src: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ForSource {
    /// Evaluated once; iterates array elements (or a single-element/empty
    /// iteration for non-array results — see [`crate::interp`]).
    Pipeline(String),
    /// `start..end`, exclusive end, evaluated once at parse time (no `$var`
    /// substitution in range bounds).
    Range { start: i64, end: i64 },
}
