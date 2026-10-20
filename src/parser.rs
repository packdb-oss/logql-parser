// Copyright 2025 PackDB Contributors
//
// Licensed under the Apache License, Version 2.0

//! LogQL parser implementation.
//!
//! Recursive-descent parser for LogQL covering:
//! - Stream selectors with label matchers
//! - Line filter pipeline stages
//! - Range aggregations: `rate`, `count_over_time`, `bytes_rate`, `bytes_over_time`
//! - Vector aggregations: `sum`, `avg`, `min`, `max`, `count`, `topk`, `bottomk`
//! - Binary arithmetic with precedence: `+`, `-`, `*`, `/`, `%`, `^`
//! - Duration literals: `[5m]`, `[30s]`, `[1h]`

use std::cell::Cell;
use std::time::Duration;

use crate::ast::syntax::*;
use crate::error::ParseError;

/// Nesting budget for [`parse`]. Each recursive parser frame (a parenthesised
/// or aggregated sub-expression costs two, unary minus one) and each operator in
/// a binary-operator or label-filter chain uses one unit.
///
/// Parsing, dropping and every consumer that walks the tree recurse once per
/// level. Without a bound, a short query such as 5,000 nested parentheses
/// exhausts an 8 MiB thread stack, and a stack overflow aborts the whole
/// process rather than failing the query. Handwritten and generated queries
/// stay far below this limit.
pub const MAX_NESTING_DEPTH: usize = 128;

thread_local! {
    static NESTING_DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// Holds `levels` units of the per-thread nesting budget until dropped, so
/// every early return releases what its frame reserved.
struct NestingGuard {
    levels: usize,
}

impl NestingGuard {
    fn enter() -> Result<Self, ParseError> {
        let mut guard = Self { levels: 0 };
        guard.descend()?;
        Ok(guard)
    }

    /// Reserve one more level for this frame: one recursive step, or one more
    /// operator folded into a left-deep chain held by this frame.
    fn descend(&mut self) -> Result<(), ParseError> {
        let depth = NESTING_DEPTH.with(Cell::get);
        if depth >= MAX_NESTING_DEPTH {
            return Err(ParseError::Syntax(format!(
                "query nesting exceeds the maximum depth of {MAX_NESTING_DEPTH}"
            )));
        }
        NESTING_DEPTH.with(|d| d.set(depth + 1));
        self.levels += 1;
        Ok(())
    }
}

impl Drop for NestingGuard {
    fn drop(&mut self) {
        let levels = self.levels;
        NESTING_DEPTH.with(|d| d.set(d.get().saturating_sub(levels)));
    }
}

/// Parse a LogQL query string into a syntax-level AST.
///
/// # Errors
///
/// Returns [`ParseError::Syntax`] for malformed input, or
/// [`ParseError::Unsupported`] for valid LogQL that this parser
/// does not yet handle.
pub fn parse(input: &str) -> Result<Expr, ParseError> {
    let trimmed = input.trim();

    if trimmed.is_empty() {
        return Err(ParseError::Syntax("empty query".to_string()));
    }

    // Bare selectors with optional line filters.
    if trimmed.starts_with('{') {
        return parse_log_query(trimmed).map(Expr::Log);
    }

    // Metric expressions: aggregations, binary ops.
    let (expr, rest) = parse_expr(trimmed, 0)?;
    let rest = rest.trim();
    if !rest.is_empty() {
        return Err(ParseError::Syntax(format!(
            "unexpected trailing input: '{rest}'"
        )));
    }
    Ok(expr)
}

// ---------------------------------------------------------------------------
// Quote-aware byte scanner (shared by strip and stage-end detection)
// ---------------------------------------------------------------------------

/// Tracks quote/escape state while scanning ASCII LogQL input byte-by-byte.
struct QuoteScanner {
    quote: Option<char>,
    escape: bool,
}

impl QuoteScanner {
    pub(crate) fn new() -> Self {
        Self {
            quote: None,
            escape: false,
        }
    }

    /// Returns `true` if the character is inside a quoted string.
    /// Must be called for every byte in order.
    pub(crate) fn advance(&mut self, ch: char) -> bool {
        if let Some(q) = self.quote {
            if q == '`' {
                if ch == '`' {
                    self.quote = None;
                }
            } else if self.escape {
                self.escape = false;
            } else if ch == '\\' {
                self.escape = true;
            } else if ch == q {
                self.quote = None;
            }
            return true;
        }

        if matches!(ch, '"' | '\'' | '`') {
            self.quote = Some(ch);
            return true; // opening quote char is "inside" for caller purposes
        }

        false
    }
}

// ===========================================================================
// Expression parser (Pratt / precedence climbing)
// ===========================================================================

/// Binary operator precedence (higher = binds tighter).
///
/// Matches upstream Loki precedence:
/// `or`(1) < `and`/`unless`(2) < comparisons(3) < `+`/`-`(4) < `*`/`/`/`%`(5) < `^`(6)
fn binop_precedence(op: BinOp) -> u8 {
    match op {
        BinOp::Or => 1,
        BinOp::And | BinOp::Unless => 2,
        BinOp::Eq | BinOp::Neq | BinOp::Gt | BinOp::Gte | BinOp::Lt | BinOp::Lte => 3,
        BinOp::Add | BinOp::Sub => 4,
        BinOp::Mul | BinOp::Div | BinOp::Mod => 5,
        BinOp::Pow => 6,
    }
}

/// Try to read a binary operator at the start of `input`.
/// Returns `(BinOp, remaining)` or `None`.
fn try_parse_binop(input: &str) -> Option<(BinOp, &str)> {
    let input = input.trim();

    // Multi-char operators first (must check before single-char to avoid
    // matching `=` from `==` or `>` from `>=`).
    if let Some(r) = input.strip_prefix("==") {
        return Some((BinOp::Eq, r.trim()));
    }
    if let Some(r) = input.strip_prefix("!=") {
        return Some((BinOp::Neq, r.trim()));
    }
    if let Some(r) = input.strip_prefix(">=") {
        return Some((BinOp::Gte, r.trim()));
    }
    if let Some(r) = input.strip_prefix("<=") {
        return Some((BinOp::Lte, r.trim()));
    }

    // Single-char operators.
    let first = input.as_bytes().first()?;
    let (op, len) = match first {
        b'>' => (BinOp::Gt, 1),
        b'<' => (BinOp::Lt, 1),
        b'+' => (BinOp::Add, 1),
        b'-' => (BinOp::Sub, 1),
        b'*' => (BinOp::Mul, 1),
        b'/' => (BinOp::Div, 1),
        b'%' => (BinOp::Mod, 1),
        b'^' => (BinOp::Pow, 1),
        _ => {
            // Keyword operators: must be followed by whitespace or '(' to
            // avoid matching identifiers like "android", "order", "unless_flag".
            for (kw, op) in [
                ("unless", BinOp::Unless),
                ("and", BinOp::And),
                ("or", BinOp::Or),
            ] {
                if let Some(rest) = input.strip_prefix(kw)
                    && (rest.is_empty()
                        || rest.starts_with(char::is_whitespace)
                        || rest.starts_with('('))
                {
                    return Some((op, rest.trim()));
                }
            }
            return None;
        }
    };
    Some((op, input.get(len..)?.trim()))
}

/// Try to consume the `bool` modifier after a comparison operator.
/// Only valid for comparison operators; callers must validate.
/// Returns the remaining input after `bool`, or the original input unchanged.
fn try_consume_bool_modifier(input: &str) -> (bool, &str) {
    if let Some(rest) = input.strip_prefix("bool")
        && (rest.is_empty() || rest.starts_with(char::is_whitespace) || rest.starts_with('('))
    {
        return (true, rest.trim());
    }
    (false, input)
}

/// Try to consume an `on(...)` or `ignoring(...)` matching modifier,
/// optionally followed by `group_left(...)` or `group_right(...)`.
///
/// Grammar:
/// ```text
/// matching := ("on" | "ignoring") "(" label_list ")" group_modifier?
/// group_modifier := ("group_left" | "group_right") ("(" label_list ")")?
/// label_list := (identifier ("," identifier)*)?
/// ```
///
/// Returns `(Some(BinOpMatching), remaining)` if a modifier is found,
/// or `(None, original_input)` otherwise.
fn try_parse_matching_modifier(input: &str) -> Result<(Option<BinOpMatching>, &str), ParseError> {
    let trimmed = input.trim();

    // Check for on/ignoring keyword followed by '('
    let (is_on, after_kw) = if let Some(rest) = trimmed.strip_prefix("on") {
        if rest.trim_start().starts_with('(') {
            (true, rest.trim_start())
        } else {
            return Ok((None, input));
        }
    } else if let Some(rest) = trimmed.strip_prefix("ignoring") {
        if rest.trim_start().starts_with('(') {
            (false, rest.trim_start())
        } else {
            return Ok((None, input));
        }
    } else {
        return Ok((None, input));
    };

    // Parse label list: "(" label1, label2, ... ")"
    let after_open = after_kw
        .strip_prefix('(')
        .ok_or_else(|| ParseError::Syntax("expected '(' after on/ignoring".into()))?;
    let (labels, after_labels) = parse_label_list(after_open)?;
    let after_close = after_labels
        .trim()
        .strip_prefix(')')
        .ok_or_else(|| ParseError::Syntax("expected ')' after label list".into()))?
        .trim();

    let mode = if is_on {
        BinOpMatchMode::On(labels)
    } else {
        BinOpMatchMode::Ignoring(labels)
    };

    // Try to parse optional group_left/group_right
    let (group, rest) = try_parse_group_modifier(after_close)?;

    Ok((Some(BinOpMatching { mode, group }), rest))
}

/// Try to consume `group_left(...)` or `group_right(...)`.
fn try_parse_group_modifier(input: &str) -> Result<(Option<BinOpGroupModifier>, &str), ParseError> {
    let trimmed = input.trim();

    let (is_left, after_kw) = if let Some(rest) = trimmed.strip_prefix("group_left") {
        // Token boundary: next char must not be alphanumeric or underscore
        if rest.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
            return Ok((None, input));
        }
        (true, rest)
    } else if let Some(rest) = trimmed.strip_prefix("group_right") {
        if rest.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
            return Ok((None, input));
        }
        (false, rest)
    } else {
        return Ok((None, input));
    };

    let after_kw_trimmed = after_kw.trim_start();

    // group_left/group_right can appear with or without parens
    let (labels, rest) = if after_kw_trimmed.starts_with('(') {
        let after_open = after_kw_trimmed
            .strip_prefix('(')
            .ok_or_else(|| ParseError::Syntax("expected '('".into()))?;
        let (labels, after_labels) = parse_label_list(after_open)?;
        let after_close = after_labels
            .trim()
            .strip_prefix(')')
            .ok_or_else(|| ParseError::Syntax("expected ')' after group modifier labels".into()))?;
        (labels, after_close.trim())
    } else {
        // No parens: empty include list
        (Vec::new(), after_kw_trimmed)
    };

    let modifier = if is_left {
        BinOpGroupModifier::Left(labels)
    } else {
        BinOpGroupModifier::Right(labels)
    };

    Ok((Some(modifier), rest))
}

/// Parse a comma-separated label list (possibly empty).
/// Input starts just after '('. Returns labels and remaining input (before ')').
fn parse_label_list(input: &str) -> Result<(Vec<String>, &str), ParseError> {
    let mut labels = Vec::new();
    let mut rest = input.trim();

    // Empty list: ")"
    if rest.starts_with(')') {
        return Ok((labels, rest));
    }

    loop {
        // Parse identifier: must start with [a-zA-Z_], then [a-zA-Z0-9_]*
        let first = rest.chars().next();
        if !matches!(first, Some(c) if c.is_ascii_alphabetic() || c == '_') {
            return Err(ParseError::Syntax(format!(
                "expected label name, got '{}'",
                rest.chars().take(20).collect::<String>()
            )));
        }
        let end = rest
            .find(|c: char| !c.is_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        labels.push(rest.get(..end).unwrap_or_default().to_string());
        rest = rest.get(end..).unwrap_or_default().trim();

        if rest.starts_with(',') {
            rest = rest.get(1..).unwrap_or_default().trim();
        } else {
            break;
        }
    }

    Ok((labels, rest))
}

/// Parse an expression with precedence climbing.
/// Returns `(Expr, remaining_input)`.
fn parse_expr(input: &str, min_prec: u8) -> Result<(Expr, &str), ParseError> {
    let mut nesting = NestingGuard::enter()?;
    let (mut lhs, mut rest) = parse_primary(input)?;

    loop {
        let trimmed = rest.trim();
        let Some((op, after_op)) = try_parse_binop(trimmed) else {
            break;
        };
        let prec = binop_precedence(op);
        if prec < min_prec {
            break;
        }
        // Each operator folded into `lhs` makes the tree one level deeper.
        nesting.descend()?;

        // Parse optional `bool` modifier (only valid for comparisons).
        let (return_bool, after_bool) = if op.is_comparison() {
            try_consume_bool_modifier(after_op)
        } else {
            (false, after_op)
        };

        // Parse optional matching modifier: on(...)/ignoring(...) group_left/right(...)
        let (matching, after_matching) = try_parse_matching_modifier(after_bool)?;

        // Validate: group_left/group_right not valid on logical operators
        if op.is_logical()
            && let Some(ref m) = matching
            && m.group.is_some()
        {
            return Err(ParseError::Syntax(
                "group_left/group_right not allowed with logical/set operators".into(),
            ));
        }

        // Right-associative for ^, left-associative otherwise.
        let next_min = if op == BinOp::Pow { prec } else { prec + 1 };
        let (rhs, after_rhs) = parse_expr(after_matching, next_min)?;
        lhs = Expr::BinOp {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
            return_bool,
            matching,
        };
        rest = after_rhs;
    }

    Ok((lhs, rest))
}

/// Parse a primary expression: function call, parenthesized expr, or bare selector.
fn parse_primary(input: &str) -> Result<(Expr, &str), ParseError> {
    let _nesting = NestingGuard::enter()?;
    let input = input.trim();

    // Parenthesized sub-expression.
    if input.starts_with('(') {
        let after_open = input.get(1..).unwrap_or("").trim();
        let (expr, rest) = parse_expr(after_open, 0)?;
        let rest = rest.trim();
        if !rest.starts_with(')') {
            return Err(ParseError::Syntax(
                "missing closing ')' for parenthesized expression".to_string(),
            ));
        }
        return Ok((expr, rest.get(1..).unwrap_or("")));
    }

    // Unary minus: `-expr` → negate the following primary.
    if input.starts_with('-') {
        let after_minus = input.get(1..).unwrap_or("").trim();
        // Try as negative scalar first (avoids creating a BinOp for `-100`).
        if let Some((val, rest)) = try_parse_scalar(after_minus) {
            return Ok((Expr::Scalar(-val), rest));
        }
        // Otherwise parse as `0 - expr` via full expression.
        let (inner, rest) = parse_primary(after_minus)?;
        return Ok((
            Expr::BinOp {
                op: BinOp::Sub,
                lhs: Box::new(Expr::Scalar(0.0)),
                rhs: Box::new(inner),
                return_bool: false,
                matching: None,
            },
            rest,
        ));
    }

    // Try scalar literal before function names so that NaN/Inf are not
    // mistaken for unsupported function names.
    if let Some((val, rest)) = try_parse_scalar(input) {
        return Ok((Expr::Scalar(val), rest));
    }

    // Try function name.
    if let Some((fn_name, after_name)) = try_split_function_name(input) {
        if fn_name == "vector" {
            let (val, rest) = parse_vector_literal(after_name)?;
            return Ok((Expr::Vector(val), rest));
        }
        if let Some(range_fn) = lookup_range_agg_fn(fn_name) {
            let (metric, rest) = parse_range_aggregation(range_fn, after_name)?;
            return Ok((Expr::Metric(metric), rest));
        }
        if let Some(vec_fn) = lookup_vector_agg_fn(fn_name) {
            let (metric, rest) = parse_vector_aggregation(vec_fn, after_name)?;
            return Ok((Expr::Metric(metric), rest));
        }
        // Known unsupported function.
        return Err(ParseError::Unsupported(format!(
            "function '{fn_name}' is not supported"
        )));
    }

    Err(ParseError::Syntax(format!(
        "expected function name, scalar, or '(', got: '{}'",
        &input[..input.len().min(40)]
    )))
}

/// Parse the argument of `vector(<scalar>)`, with `input` positioned just after
/// the `vector` keyword.
///
/// The argument is any constant scalar expression — a literal (`vector(0)`,
/// `vector(-1)`, `vector(NaN)`) or arithmetic over literals (`vector(1+2)`,
/// `vector(60*5)`), which is constant-folded to its value. An argument that is
/// not a constant scalar (a metric/selector, or an operator that needs vector
/// context) is rejected as unsupported. Returns the folded value and the input
/// remaining after the closing `)`.
fn parse_vector_literal(input: &str) -> Result<(f64, &str), ParseError> {
    let input = input.trim();
    if !input.starts_with('(') {
        return Err(ParseError::Syntax("expected '(' after vector".to_string()));
    }
    let inner = input.get(1..).unwrap_or("").trim();
    let (arg, rest) = parse_expr(inner, 0)?;
    let rest = rest.trim();
    if !rest.starts_with(')') {
        return Err(ParseError::Syntax(
            "missing closing ')' for vector()".to_string(),
        ));
    }
    let after_close = rest.get(1..).unwrap_or("");
    match fold_scalar_expr(&arg) {
        Some(v) => Ok((v, after_close)),
        None => Err(ParseError::Unsupported(
            "vector() argument must be a constant scalar expression".to_string(),
        )),
    }
}

/// Constant-fold a scalar expression to its `f64` value, or `None` if it is not
/// a pure constant scalar — it references a metric/selector/`vector(...)`, uses
/// the `bool` modifier or a matching clause, or uses an operator that needs
/// vector context (comparisons, logical/set ops). Supports literals and
/// arithmetic over them (`+ - * / % ^`), which is all `vector(<scalar>)` needs.
fn fold_scalar_expr(expr: &Expr) -> Option<f64> {
    match expr {
        Expr::Scalar(v) => Some(*v),
        Expr::BinOp {
            op,
            lhs,
            rhs,
            return_bool,
            matching,
        } => {
            if *return_bool || matching.is_some() {
                return None;
            }
            let l = fold_scalar_expr(lhs)?;
            let r = fold_scalar_expr(rhs)?;
            match op {
                BinOp::Add => Some(l + r),
                BinOp::Sub => Some(l - r),
                BinOp::Mul => Some(l * r),
                BinOp::Div => Some(l / r),
                BinOp::Mod => Some(l % r),
                BinOp::Pow => Some(l.powf(r)),
                // Comparisons and logical/set ops are not constant scalars in
                // this context (they need a `bool` modifier or vector operands).
                BinOp::Eq
                | BinOp::Neq
                | BinOp::Gt
                | BinOp::Gte
                | BinOp::Lt
                | BinOp::Lte
                | BinOp::And
                | BinOp::Or
                | BinOp::Unless => None,
            }
        }
        // A metric, selector, or nested vector() is not a constant scalar.
        Expr::Log(_) | Expr::Metric(_) | Expr::Vector(_) => None,
    }
}

/// Try to parse a scalar numeric literal at the start of `input`.
///
/// Handles: integers (`123`), floats (`0.5`, `.5`), scientific (`1e10`, `1.5e-3`),
/// and special values (`NaN`, `Inf`, `+Inf`, `-Inf`).
///
/// Returns `(value, remaining)` or `None` if input doesn't start with a number.
fn try_parse_scalar(input: &str) -> Option<(f64, &str)> {
    let input = input.trim();

    // Special named values.
    for (name, val) in [
        ("NaN", f64::NAN),
        ("+Inf", f64::INFINITY),
        ("-Inf", f64::NEG_INFINITY),
        ("Inf", f64::INFINITY),
    ] {
        if let Some(rest) = input.strip_prefix(name) {
            // Must not be followed by an identifier character (avoid matching "NaNo").
            if rest.is_empty() || !rest.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
            {
                return Some((val, rest));
            }
        }
    }

    // Numeric: starts with digit or '.' followed by digit.
    let first = input.as_bytes().first()?;
    if !first.is_ascii_digit() && *first != b'.' {
        return None;
    }
    if *first == b'.' {
        // `.` must be followed by a digit to be a scalar (not just a stray dot).
        let second = input.as_bytes().get(1)?;
        if !second.is_ascii_digit() {
            return None;
        }
    }

    // Consume the numeric literal: digits, '.', 'e'/'E', '+'/'-' after e.
    let mut end = 0usize;
    let bytes = input.as_bytes();
    let mut saw_dot = false;
    let mut saw_e = false;

    while end < bytes.len() {
        let Some(&b) = bytes.get(end) else { break };
        match b {
            b'0'..=b'9' => end += 1,
            b'.' if !saw_dot && !saw_e => {
                saw_dot = true;
                end += 1;
            }
            b'e' | b'E' if !saw_e => {
                saw_e = true;
                end += 1;
                // Optional sign after exponent.
                if let Some(&sign) = bytes.get(end)
                    && (sign == b'+' || sign == b'-')
                {
                    end += 1;
                }
            }
            _ => break,
        }
    }

    if end == 0 {
        return None;
    }

    let num_str = input.get(..end)?;
    let val: f64 = num_str.parse().ok()?;
    Some((val, input.get(end..)?))
}

/// Split off an identifier-like function name at the start of input.
/// Returns `(name, remaining)` or `None` if input doesn't start with a letter.
fn try_split_function_name(input: &str) -> Option<(&str, &str)> {
    let end = input
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(input.len());
    if end == 0 {
        return None;
    }
    Some((&input[..end], &input[end..]))
}

fn lookup_range_agg_fn(name: &str) -> Option<RangeAggregationFn> {
    match name {
        "rate" => Some(RangeAggregationFn::Rate),
        "count_over_time" => Some(RangeAggregationFn::CountOverTime),
        "bytes_rate" => Some(RangeAggregationFn::BytesRate),
        "bytes_over_time" => Some(RangeAggregationFn::BytesOverTime),
        "sum_over_time" => Some(RangeAggregationFn::SumOverTime),
        "avg_over_time" => Some(RangeAggregationFn::AvgOverTime),
        "min_over_time" => Some(RangeAggregationFn::MinOverTime),
        "max_over_time" => Some(RangeAggregationFn::MaxOverTime),
        "stdvar_over_time" => Some(RangeAggregationFn::StdvarOverTime),
        "stddev_over_time" => Some(RangeAggregationFn::StddevOverTime),
        "quantile_over_time" => Some(RangeAggregationFn::QuantileOverTime),
        "first_over_time" => Some(RangeAggregationFn::FirstOverTime),
        "last_over_time" => Some(RangeAggregationFn::LastOverTime),
        "absent_over_time" => Some(RangeAggregationFn::AbsentOverTime),
        _ => None,
    }
}

fn lookup_vector_agg_fn(name: &str) -> Option<VectorAggregationFn> {
    match name {
        "sum" => Some(VectorAggregationFn::Sum),
        "avg" => Some(VectorAggregationFn::Avg),
        "min" => Some(VectorAggregationFn::Min),
        "max" => Some(VectorAggregationFn::Max),
        "count" => Some(VectorAggregationFn::Count),
        "topk" => Some(VectorAggregationFn::Topk),
        "bottomk" => Some(VectorAggregationFn::Bottomk),
        _ => None,
    }
}

// ===========================================================================
// Range aggregation: rate({...}[5m])
// ===========================================================================

/// Parse `(<log_query> [<duration>])` after the function name has been consumed.
fn parse_range_aggregation(
    function: RangeAggregationFn,
    input: &str,
) -> Result<(MetricQuery, &str), ParseError> {
    let input = input.trim();
    if !input.starts_with('(') {
        return Err(ParseError::Syntax(format!(
            "expected '(' after range aggregation function, got: '{}'",
            &input[..input.len().min(20)]
        )));
    }
    let inner = input.get(1..).unwrap_or("").trim();

    // Find the matching ')' for the outer function call.
    let close_paren = find_matching_paren(inner)?;
    let body = inner.get(..close_paren).unwrap_or("").trim();
    let after_close = inner.get(close_paren + 1..).unwrap_or("");

    // `quantile_over_time` takes a leading scalar parameter before the log
    // range: `quantile_over_time(0.99, {…} | unwrap latency [5m])`. Consume it
    // here so the remainder parses as an ordinary log-range body.
    let (parameter, body) = if function == RangeAggregationFn::QuantileOverTime {
        // Accept an optional leading sign so out-of-range quantiles (e.g.
        // `-0.1`) parse and reach the engine's Loki-compatible edge handling
        // (φ < 0 → -Inf, φ > 1 → +Inf); `try_parse_scalar` alone rejects a
        // bare `-` prefix.
        let (sign, magnitude_input) = match body.strip_prefix('-') {
            Some(rest) => (-1.0, rest.trim_start()),
            None => (
                1.0,
                body.strip_prefix('+').map(str::trim_start).unwrap_or(body),
            ),
        };
        let (magnitude, after) = try_parse_scalar(magnitude_input).ok_or_else(|| {
            ParseError::Syntax(
                "quantile_over_time requires a scalar quantile parameter, e.g. \
                 quantile_over_time(0.99, {…} | unwrap latency [5m])"
                    .to_string(),
            )
        })?;
        let after = after.trim().strip_prefix(',').ok_or_else(|| {
            ParseError::Syntax(
                "quantile_over_time parameter must be followed by ',' then the log range"
                    .to_string(),
            )
        })?;
        (Some(sign * magnitude), after.trim())
    } else {
        (None, body)
    };

    // Inside the parens: {selector} pipeline [duration] [offset <dur>]
    // Parse selector + pipeline, then duration, then optional offset.
    let (log_query, after_query) = parse_log_query_with_remainder(body)?;
    let after_query = after_query.trim();
    let (range, after_duration) = parse_duration_bracket(after_query)?;
    let after_duration = after_duration.trim();

    // Optional offset modifier: `offset 1h`
    let (offset, after_offset) = try_parse_offset(after_duration);
    let after_offset = after_offset.trim();
    if !after_offset.is_empty() {
        return Err(ParseError::Syntax(format!(
            "unexpected input after duration in range aggregation: '{after_offset}'"
        )));
    }

    // Optional grouping after closing paren (e.g. `rate(...) by (labels)`).
    let after_close = after_close.trim();
    let (range_grouping, rest) = try_parse_grouping(after_close);

    // If there's a grouping on a bare range aggregation, promote it to
    // a trivial vector aggregation wrapper (the engine expects grouping
    // to live on the VectorAggregation, not on the RangeAggregation).
    let vector_aggregation = range_grouping.map(|g| VectorAggregation {
        function: VectorAggregationFn::Sum,
        grouping: Some(g),
        parameter: None,
    });

    Ok((
        MetricQuery {
            vector_aggregation,
            range_aggregation: RangeAggregation {
                function,
                log_query,
                range,
                offset,
                parameter,
            },
        },
        rest,
    ))
}

// ===========================================================================
// Vector aggregation: sum by (label) (rate({...}[5m]))
// ===========================================================================

/// Parse vector aggregation after the function name has been consumed.
fn parse_vector_aggregation(
    function: VectorAggregationFn,
    input: &str,
) -> Result<(MetricQuery, &str), ParseError> {
    let input = input.trim();

    // Optional grouping BEFORE the opening paren: `sum by (labels) (...)`
    let (pre_grouping, after_pre) = try_parse_grouping(input);
    let after_pre = after_pre.trim();

    if !after_pre.starts_with('(') {
        return Err(ParseError::Syntax(format!(
            "expected '(' after vector aggregation, got: '{}'",
            &after_pre[..after_pre.len().min(20)]
        )));
    }
    let inner = after_pre.get(1..).unwrap_or("").trim();

    // For topk/bottomk, parse the parameter: `topk(5, ...)`
    let (parameter, expr_input) = if matches!(
        function,
        VectorAggregationFn::Topk | VectorAggregationFn::Bottomk
    ) {
        parse_topk_parameter(inner)?
    } else {
        (None, inner)
    };

    // Parse the inner expression (recursive — supports nested aggregations and binops).
    let (inner_expr, after_inner) = parse_expr(expr_input.trim(), 0)?;
    let after_inner = after_inner.trim();

    if !after_inner.starts_with(')') {
        return Err(ParseError::Syntax(
            "missing closing ')' for vector aggregation".to_string(),
        ));
    }
    let after_close = after_inner.get(1..).unwrap_or("");

    // Optional grouping AFTER the closing paren: `sum(...) by (labels)`
    let after_close = after_close.trim();
    let (post_grouping, rest) = try_parse_grouping(after_close);

    let grouping = pre_grouping.or(post_grouping);

    // The inner expression must resolve to a MetricQuery.
    let inner_metric = match inner_expr {
        Expr::Metric(m) => m,
        _ => {
            return Err(ParseError::Syntax(
                "vector aggregation inner expression must be a metric query".to_string(),
            ));
        }
    };

    Ok((
        MetricQuery {
            vector_aggregation: Some(VectorAggregation {
                function,
                grouping,
                parameter,
            }),
            range_aggregation: inner_metric.range_aggregation,
        },
        rest,
    ))
}

/// Parse `k ,` at the start of topk/bottomk arguments.
fn parse_topk_parameter(input: &str) -> Result<(Option<u64>, &str), ParseError> {
    let input = input.trim();
    // Find the comma separating parameter from inner expression.
    let comma_pos = input.find(',').ok_or_else(|| {
        ParseError::Syntax("topk/bottomk requires 'k, <expr>' arguments".to_string())
    })?;
    let param_str = input.get(..comma_pos).unwrap_or("").trim();
    let k: u64 = param_str.parse().map_err(|_| {
        ParseError::Syntax(format!("invalid topk/bottomk parameter: '{param_str}'"))
    })?;
    let after_comma = input.get(comma_pos + 1..).unwrap_or("");
    Ok((Some(k), after_comma))
}

// ===========================================================================
// Grouping: by (label1, label2) / without (label1)
// ===========================================================================

/// Try to parse `by (labels)` or `without (labels)` at the current position.
/// Returns `(Some(Grouping), remaining)` or `(None, input)`.
fn try_parse_grouping(input: &str) -> (Option<Grouping>, &str) {
    let input = input.trim();
    let (kind, after_keyword) = if let Some(rest) = input.strip_prefix("by") {
        // Must be followed by whitespace or '(' to avoid matching identifiers like "bytes".
        if rest.starts_with(|c: char| c.is_whitespace() || c == '(') {
            (GroupingKind::By, rest)
        } else {
            return (None, input);
        }
    } else if let Some(rest) = input.strip_prefix("without") {
        if rest.starts_with(|c: char| c.is_whitespace() || c == '(') {
            (GroupingKind::Without, rest)
        } else {
            return (None, input);
        }
    } else {
        return (None, input);
    };

    let after_keyword = after_keyword.trim();
    if !after_keyword.starts_with('(') {
        return (None, input);
    }

    // Find the matching ')'.
    let close = match after_keyword.find(')') {
        Some(pos) => pos,
        None => return (None, input),
    };
    let labels_str = after_keyword.get(1..close).unwrap_or("");
    let labels: Vec<String> = labels_str
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let rest = after_keyword.get(close + 1..).unwrap_or("");
    (Some(Grouping { kind, labels }), rest)
}

// ===========================================================================
// Duration: [5m], [30s], [1h]
// ===========================================================================

/// Parse `[<duration>]` and return `(Duration, remaining)`.
fn parse_duration_bracket(input: &str) -> Result<(Duration, &str), ParseError> {
    let input = input.trim();
    if !input.starts_with('[') {
        return Err(ParseError::Syntax(format!(
            "expected '[' for duration, got: '{}'",
            &input[..input.len().min(20)]
        )));
    }
    let after_bracket = input.get(1..).unwrap_or("");
    let close = after_bracket.find(']').ok_or_else(|| {
        ParseError::Syntax("unterminated duration bracket: missing ']'".to_string())
    })?;
    let duration_str = after_bracket.get(..close).unwrap_or("").trim();
    let duration = parse_duration_literal(duration_str)?;
    let rest = after_bracket.get(close + 1..).unwrap_or("");
    Ok((duration, rest))
}

/// Parse a duration literal like `5m`, `30s`, `1h`, `500ms`.
fn parse_duration_literal(s: &str) -> Result<Duration, ParseError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ParseError::Syntax("empty duration".to_string()));
    }

    // Find where digits end and unit begins.
    let num_end = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let num_str = s.get(..num_end).unwrap_or("");
    let unit_str = s.get(num_end..).unwrap_or("");

    let value: f64 = num_str
        .parse()
        .map_err(|_| ParseError::Syntax(format!("invalid duration number: '{num_str}'")))?;

    if value <= 0.0 {
        return Err(ParseError::Syntax("duration must be positive".to_string()));
    }

    let nanos = match unit_str {
        "ns" => value as u64,
        "us" | "µs" => (value * 1_000.0) as u64,
        "ms" => (value * 1_000_000.0) as u64,
        "s" => (value * 1_000_000_000.0) as u64,
        "m" => (value * 60_000_000_000.0) as u64,
        "h" => (value * 3_600_000_000_000.0) as u64,
        _ => {
            return Err(ParseError::Syntax(format!(
                "unknown duration unit: '{unit_str}'"
            )));
        }
    };

    Ok(Duration::from_nanos(nanos))
}

/// Try to parse an `offset <duration>` modifier.
///
/// Returns `(Some(duration), rest)` if present, `(None, input)` otherwise.
/// The duration is consumed from the start of the remaining input after
/// the `offset` keyword.
fn try_parse_offset(input: &str) -> (Option<Duration>, &str) {
    let input = input.trim();
    if !input.starts_with("offset") {
        return (None, input);
    }
    let after_keyword = input.get("offset".len()..).unwrap_or("");
    // Word boundary: next char must not be alphanumeric/underscore.
    if after_keyword
        .chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
    {
        return (None, input);
    }
    let after_keyword = after_keyword.trim();
    // Extract the duration token (digits + unit suffix).
    let dur_end = find_duration_end(after_keyword);
    if dur_end == 0 {
        return (None, input);
    }
    let dur_str = after_keyword.get(..dur_end).unwrap_or("");
    let rest = after_keyword.get(dur_end..).unwrap_or("");
    match parse_duration_literal(dur_str) {
        Ok(dur) => (Some(dur), rest),
        Err(_) => (None, input),
    }
}

/// Find the end of a duration token (e.g., `5m`, `1h`, `30s`, `500ms`).
///
/// Scans digits (including `.`), then a unit suffix.
fn find_duration_end(s: &str) -> usize {
    let num_end = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    if num_end == 0 {
        return 0;
    }
    let unit_start = s.get(num_end..).unwrap_or("");
    // Match known unit suffixes.
    for unit in &["ns", "us", "µs", "ms", "s", "m", "h"] {
        if unit_start.starts_with(unit) {
            return num_end + unit.len();
        }
    }
    0
}

// ===========================================================================
// Utility: find matching parenthesis
// ===========================================================================

/// Find the position of the matching ')' in `input`, accounting for nesting
/// and quoted strings. `input` starts AFTER the opening '('.
fn find_matching_paren(input: &str) -> Result<usize, ParseError> {
    let mut depth: u32 = 0;
    let mut scanner = QuoteScanner::new();
    for (i, ch) in input.char_indices() {
        if scanner.advance(ch) {
            continue;
        }
        match ch {
            '(' => depth += 1,
            ')' if depth == 0 => return Ok(i),
            ')' => depth -= 1,
            _ => {}
        }
    }
    Err(ParseError::Syntax(
        "unterminated parenthesized expression: missing ')'".to_string(),
    ))
}

// ---------------------------------------------------------------------------
// Log query (selector + pipeline) parser
// ---------------------------------------------------------------------------

fn parse_log_query(input: &str) -> Result<LogQuery, ParseError> {
    let (query, rest) = parse_log_query_with_remainder(input)?;
    let rest = rest.trim();
    if !rest.is_empty() {
        return Err(ParseError::Syntax(format!(
            "unexpected trailing input: '{rest}'"
        )));
    }
    Ok(query)
}

/// Parse a log query and return the remaining unparsed input.
/// Used by range aggregation parsing where `[duration]` follows the log query.
fn parse_log_query_with_remainder(input: &str) -> Result<(LogQuery, &str), ParseError> {
    let close = find_closing_brace(input).ok_or_else(|| {
        ParseError::Syntax("unterminated stream selector: missing '}'".to_string())
    })?;
    let selector_body = &input[1..close];
    let remainder = input.get(close + 1..).unwrap_or("").trim();

    let matchers = parse_selector_matchers(selector_body)?;

    // Parse pipeline stages, stopping when we hit `[` (duration bracket for range aggs).
    let (pipeline, rest) = parse_pipeline_stages_until_bracket(remainder)?;

    Ok((
        LogQuery {
            selector: StreamSelector { matchers },
            pipeline,
        },
        rest,
    ))
}

// ---------------------------------------------------------------------------
// Selector parsing
// ---------------------------------------------------------------------------

fn find_closing_brace(s: &str) -> Option<usize> {
    let mut in_quotes = false;
    let mut escape = false;
    for (i, ch) in s.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        match ch {
            '\\' => escape = true,
            '"' => in_quotes = !in_quotes,
            '}' if !in_quotes && i > 0 => return Some(i),
            _ => {}
        }
    }
    None
}

fn parse_selector_matchers(body: &str) -> Result<Vec<LabelMatcher>, ParseError> {
    let body = body.trim();
    if body.is_empty() {
        return Ok(vec![]);
    }

    let mut matchers = Vec::new();
    for part in split_on_unquoted_comma(body) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        matchers.push(parse_single_matcher(part)?);
    }
    Ok(matchers)
}

fn split_on_unquoted_comma(body: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let mut escape = false;
    for (i, ch) in body.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        match ch {
            '\\' => escape = true,
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                parts.push(&body[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&body[start..]);
    parts
}

fn parse_single_matcher(s: &str) -> Result<LabelMatcher, ParseError> {
    let (name, op, rest) = if let Some(pos) = s.find("=~") {
        (&s[..pos], MatchOp::Re, &s[pos + 2..])
    } else if let Some(pos) = s.find("!~") {
        (&s[..pos], MatchOp::Nre, &s[pos + 2..])
    } else if let Some(pos) = s.find("!=") {
        (&s[..pos], MatchOp::Neq, &s[pos + 2..])
    } else if let Some(pos) = s.find('=') {
        (&s[..pos], MatchOp::Eq, &s[pos + 1..])
    } else {
        return Err(ParseError::Syntax(format!(
            "no match operator found in '{s}'"
        )));
    };

    let name = name.trim();
    let rest = rest.trim();

    // Value must be quoted with double quotes.
    if !rest.starts_with('"') || !rest.ends_with('"') || rest.len() < 2 {
        return Err(ParseError::Syntax(format!(
            "matcher value must be quoted in '{s}'"
        )));
    }
    let value = &rest[1..rest.len() - 1];

    Ok(LabelMatcher {
        name: name.to_string(),
        op,
        value: value.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Pipeline stage parsing
// ---------------------------------------------------------------------------

/// Parse pipeline stages, stopping at end-of-input or `[` (duration bracket).
/// Returns `(stages, remaining_input)`.
fn parse_pipeline_stages_until_bracket(
    remainder: &str,
) -> Result<(Vec<PipelineStage>, &str), ParseError> {
    let mut remainder = remainder.trim();
    if remainder.is_empty() || remainder.starts_with('[') {
        return Ok((vec![], remainder));
    }

    let mut stages = Vec::new();

    while !remainder.is_empty() {
        remainder = remainder.trim();
        if remainder.is_empty() || remainder.starts_with('[') {
            break;
        }

        // Label filter — must precede keyword-stage parsing so a label named like
        // a pipeline keyword (`| json = "x"`, `| unwrap = "y"`) is parsed as a
        // filter on that label rather than consumed as the keyword stage.
        if let Some(after_pipe) = remainder.strip_prefix('|')
            && starts_label_filter(after_pipe)
        {
            let (expr, rest) = parse_label_filter_expr(after_pipe.trim_start())?;
            stages.push(PipelineStage::LabelFilter(expr));
            remainder = rest;
            continue;
        }

        // Line filter operators.
        let (op, after_op) = if let Some(r) = remainder.strip_prefix("|=") {
            (LineFilterOp::Contains, r)
        } else if let Some(r) = remainder.strip_prefix("!=") {
            (LineFilterOp::NotContains, r)
        } else if let Some(r) = remainder.strip_prefix("|~") {
            (LineFilterOp::Regex, r)
        } else if let Some(r) = remainder.strip_prefix("!~") {
            (LineFilterOp::NotRegex, r)
        } else if let Some((labels, rest)) = try_parse_parser_stage(remainder, "json") {
            stages.push(PipelineStage::Json(labels));
            remainder = rest;
            continue;
        } else if let Some((labels, rest)) = try_parse_parser_stage(remainder, "logfmt") {
            stages.push(PipelineStage::Logfmt(labels));
            remainder = rest;
            continue;
        } else if let Some((expr, rest)) = try_parse_quoted_stage(remainder, "regexp")? {
            stages.push(PipelineStage::Regexp(expr));
            remainder = rest;
            continue;
        } else if let Some((expr, rest)) = try_parse_quoted_stage(remainder, "pattern")? {
            stages.push(PipelineStage::Pattern(expr));
            remainder = rest;
            continue;
        } else if let Some((tmpl, rest)) = try_parse_quoted_stage(remainder, "line_format")? {
            stages.push(PipelineStage::LineFormat(tmpl));
            remainder = rest;
            continue;
        } else if let Some((items, rest)) = try_parse_label_format_stage(remainder)? {
            stages.push(PipelineStage::LabelFormat(items));
            remainder = rest;
            continue;
        } else if let Some((stage, rest)) = try_parse_label_stage(remainder, "drop") {
            stages.push(PipelineStage::Drop(stage));
            remainder = rest;
            continue;
        } else if let Some((stage, rest)) = try_parse_label_stage(remainder, "keep") {
            stages.push(PipelineStage::Keep(stage));
            remainder = rest;
            continue;
        } else if let Some((unwrap, rest)) = try_parse_unwrap_stage(remainder) {
            stages.push(PipelineStage::Unwrap(unwrap));
            remainder = rest;
            continue;
        } else {
            return Err(ParseError::Syntax(format!(
                "unexpected trailing input: '{remainder}'"
            )));
        };

        let after_op = after_op.trim();
        let (value, remaining) = parse_quoted_value(after_op)?;
        stages.push(PipelineStage::LineFilter(LineFilter { op, value }));
        remainder = remaining;
    }

    Ok((stages, remainder))
}

/// Try to parse a `| drop <labels>` or `| keep <labels>` pipeline stage.
/// Returns `None` if the remainder doesn't match.
fn try_parse_label_stage<'a>(remainder: &'a str, keyword: &str) -> Option<(Vec<String>, &'a str)> {
    // Match "| drop " or "|drop " (with or without space after pipe).
    let after_pipe = remainder.strip_prefix('|')?.trim();
    let after_keyword = after_pipe.strip_prefix(keyword)?;

    // Require whitespace after keyword to avoid matching e.g. "dropx".
    if !after_keyword.starts_with(char::is_whitespace) {
        return None;
    }

    let (labels, rest) = parse_label_list_until_bracket(after_keyword.trim());
    if labels.is_empty() {
        // `| drop` with no labels is invalid LogQL — don't match as a stage.
        return None;
    }
    Some((labels, rest))
}

/// Does the text after a leading `|` begin a label-filter predicate — `(` or a
/// `<label-name> <comparison-op>`? Used to route to label-filter parsing before
/// the keyword stages, so a label named like a pipeline keyword (`| json = "x"`)
/// is a filter rather than the `json` stage. `| json` / `| json foo` (no
/// comparison operator after the name) are NOT label filters.
fn starts_label_filter(after_pipe: &str) -> bool {
    let s = after_pipe.trim_start();
    if s.starts_with('(') {
        return true;
    }
    // A label name (starting `[A-Za-z_]`) immediately followed — after optional
    // whitespace — by a comparison operator (`= == != =~ !~ > >= < <=`).
    if !s
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
    {
        return false;
    }
    let name_end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(s.len());
    let after = s.get(name_end..).unwrap_or("").trim_start();
    after.starts_with(['=', '!', '>', '<'])
}

/// `expr := unary ( (and | or | ",") unary )*` — Loki evaluates label-filter
/// predicates **left-to-right with equal precedence** for `and`/`or` (comma and
/// bare whitespace are synonyms for `and`); only parentheses override grouping.
/// So `a or b and c` parses as `((a or b) and c)`, not `a or (b and c)`.
fn parse_label_filter_expr(input: &str) -> Result<(LabelFilterExpr, &str), ParseError> {
    let mut nesting = NestingGuard::enter()?;
    let (mut lhs, mut rest) = parse_label_filter_unary(input)?;
    loop {
        let trimmed = rest.trim_start();
        let (op, after) = if let Some(after) = strip_bareword(trimmed, "and") {
            (LabelFilterBoolOp::And, after)
        } else if let Some(after) = strip_bareword(trimmed, "or") {
            (LabelFilterBoolOp::Or, after)
        } else if let Some(after) = trimmed.strip_prefix(',') {
            (LabelFilterBoolOp::And, after)
        } else if starts_label_filter(trimmed) {
            // Adjacent predicates with no explicit combinator: Loki treats the
            // whitespace between them as `and` (e.g. `duration >= 20ms method!~"2.."`).
            (LabelFilterBoolOp::And, trimmed)
        } else {
            break;
        };
        // Each combinator folded into `lhs` makes the tree one level deeper.
        nesting.descend()?;
        let (rhs, next) = parse_label_filter_unary(after)?;
        lhs = LabelFilterExpr::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        };
        rest = next;
    }
    Ok((lhs, rest))
}

/// `unary := "(" expr ")" | comparison`
fn parse_label_filter_unary(input: &str) -> Result<(LabelFilterExpr, &str), ParseError> {
    let _nesting = NestingGuard::enter()?;
    let trimmed = input.trim_start();
    if let Some(after) = trimmed.strip_prefix('(') {
        let (inner, rest) = parse_label_filter_expr(after)?;
        let rest = rest
            .trim_start()
            .strip_prefix(')')
            .ok_or_else(|| ParseError::Syntax("unterminated '(' in label filter".to_string()))?;
        return Ok((inner, rest));
    }
    let (cmp, rest) = parse_label_filter_comparison(trimmed)?;
    Ok((LabelFilterExpr::Comparison(cmp), rest))
}

/// `comparison := IDENT op value`
fn parse_label_filter_comparison(input: &str) -> Result<(LabelFilterComparison, &str), ParseError> {
    let input = input.trim_start();
    // Label name: [A-Za-z_][A-Za-z0-9_]* — the first char must not be a digit or
    // a symbol (the stage-level check only guards the first term, so a nested
    // `… and 1bad=2` reaches here directly).
    if !input
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
    {
        return Err(ParseError::Syntax(format!(
            "expected a label name in label filter, got: '{input}'"
        )));
    }
    let name_end = input
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(input.len());
    let name = input.get(..name_end).unwrap_or("");
    let after_name = input.get(name_end..).unwrap_or("").trim_start();

    // Operator — longest match first so `>=`/`<=`/`==`/`=~`/`!~`/`!=` win over
    // their single-char prefixes.
    let (op, after_op) = if let Some(r) = after_name.strip_prefix("=~") {
        (LabelFilterOp::Re, r)
    } else if let Some(r) = after_name.strip_prefix("!~") {
        (LabelFilterOp::Nre, r)
    } else if let Some(r) = after_name.strip_prefix("!=") {
        (LabelFilterOp::Neq, r)
    } else if let Some(r) = after_name.strip_prefix(">=") {
        (LabelFilterOp::Gte, r)
    } else if let Some(r) = after_name.strip_prefix("<=") {
        (LabelFilterOp::Lte, r)
    } else if let Some(r) = after_name.strip_prefix("==") {
        (LabelFilterOp::Eq, r)
    } else if let Some(r) = after_name.strip_prefix('=') {
        (LabelFilterOp::Eq, r)
    } else if let Some(r) = after_name.strip_prefix('>') {
        (LabelFilterOp::Gt, r)
    } else if let Some(r) = after_name.strip_prefix('<') {
        (LabelFilterOp::Lt, r)
    } else {
        return Err(ParseError::Syntax(format!(
            "expected a comparison operator after label '{name}', got: '{after_name}'"
        )));
    };

    let (value, rest) = parse_label_filter_value(after_op.trim_start())?;

    // Operator/value type compatibility.
    match &value {
        LabelFilterValue::String(_) if op.is_ordering() => {
            return Err(ParseError::Validation(format!(
                "ordering operator on label '{name}' requires a numeric value, not a string"
            )));
        }
        LabelFilterValue::Number(_)
        | LabelFilterValue::Duration(_)
        | LabelFilterValue::Bytes(_)
            if op.is_regex() =>
        {
            return Err(ParseError::Validation(format!(
                "regex operator on label '{name}' requires a string value"
            )));
        }
        _ => {}
    }

    Ok((
        LabelFilterComparison {
            name: name.to_string(),
            op,
            value,
        },
        rest,
    ))
}

/// Parse a label-filter value: a quoted string, or a bare numeric / Go-duration /
/// bytes literal (classified in that order).
fn parse_label_filter_value(input: &str) -> Result<(LabelFilterValue, &str), ParseError> {
    let first = input
        .chars()
        .next()
        .ok_or_else(|| ParseError::Syntax("expected a value in label filter".to_string()))?;
    if first == '"' || first == '`' {
        let (s, rest) = parse_quoted_value(input)?;
        return Ok((LabelFilterValue::String(s), rest));
    }
    // Bare token: up to whitespace or a structural char.
    let tok_end = input
        .find(|c: char| c.is_whitespace() || matches!(c, ',' | ')' | '|' | '['))
        .unwrap_or(input.len());
    let token = input.get(..tok_end).unwrap_or("");
    let rest = input.get(tok_end..).unwrap_or("");
    if token.is_empty() {
        return Err(ParseError::Syntax(
            "expected a value in label filter".to_string(),
        ));
    }
    if let Ok(n) = token.parse::<f64>() {
        Ok((LabelFilterValue::Number(n), rest))
    } else if let Ok(d) = parse_duration_literal(token) {
        Ok((LabelFilterValue::Duration(d), rest))
    } else if let Some(b) = parse_bytes_literal(token) {
        Ok((LabelFilterValue::Bytes(b), rest))
    } else {
        Err(ParseError::Syntax(format!(
            "invalid label-filter value '{token}': expected a quoted string, number, duration, or bytes"
        )))
    }
}

/// Strip a bareword keyword (`and`/`or`) with a trailing word boundary. Returns
/// the remainder after the keyword, or `None` if `input` doesn't start with it
/// as a whole word (so a label named e.g. `android` isn't mistaken for `and`).
fn strip_bareword<'a>(input: &'a str, keyword: &str) -> Option<&'a str> {
    let after = input.strip_prefix(keyword)?;
    match after.chars().next() {
        Some(c) if c.is_ascii_alphanumeric() || c == '_' => None,
        _ => Some(after),
    }
}

/// Parse a bytes literal like `1MB`, `512B`, `4KiB`. Decimal units (`kB`/`MB`/…)
/// are 1000-based; binary units (`KiB`/`MiB`/…) are 1024-based. Returns `None`
/// if `s` is not a bytes literal (e.g. a bare number or a duration).
fn parse_bytes_literal(s: &str) -> Option<u64> {
    let num_end = s.find(|c: char| !c.is_ascii_digit() && c != '.')?;
    if num_end == 0 {
        return None;
    }
    let value: f64 = s.get(..num_end)?.parse().ok()?;
    if value < 0.0 {
        return None;
    }
    let factor: f64 = match s.get(num_end..)? {
        "B" => 1.0,
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        "PB" => 1e15,
        "KiB" => 1024.0,
        "MiB" => 1024f64.powi(2),
        "GiB" => 1024f64.powi(3),
        "TiB" => 1024f64.powi(4),
        "PiB" => 1024f64.powi(5),
        _ => return None,
    };
    Some((value * factor) as u64)
}

/// Try to parse a `| json` or `| logfmt` parser stage, optionally followed by a label list.
/// `| json` (no labels) extracts all keys; `| json label1, label2` extracts specific labels.
fn try_parse_parser_stage<'a>(remainder: &'a str, keyword: &str) -> Option<(Vec<String>, &'a str)> {
    let after_pipe = remainder.strip_prefix('|')?.trim();
    let after_keyword = after_pipe.strip_prefix(keyword)?;

    // End-of-input right after keyword: `| json` with no labels.
    if after_keyword.is_empty() {
        return Some((vec![], after_keyword));
    }

    // Must have whitespace or a pipeline/bracket delimiter after keyword.
    let first = after_keyword.chars().next()?;
    if !first.is_whitespace() && !matches!(first, '|' | '[' | ')') {
        return None;
    }

    let trimmed = after_keyword.trim();
    // No labels (next token is another pipeline stage, bracket, or end).
    if trimmed.is_empty() || trimmed.starts_with('|') || trimmed.starts_with('[') {
        return Some((vec![], trimmed));
    }

    // Optional label list with identifier validation.
    // Each label must start with [a-zA-Z_] and contain only [a-zA-Z0-9_].
    let mut labels = Vec::new();
    let mut rest = trimmed;
    loop {
        // Check if next char starts a valid identifier.
        let first = rest.chars().next()?;
        if !first.is_ascii_alphabetic() && first != '_' {
            // Not a label — if we haven't collected any labels, this is
            // not a label list (could be next pipeline stage). Return
            // empty labels and let the caller decide.
            if labels.is_empty() {
                return Some((vec![], trimmed));
            }
            // We already started collecting labels but hit a non-identifier:
            // stop here (malformed trailing content after a comma is caught
            // by the outer pipeline loop as unexpected trailing input).
            break;
        }
        let end = rest
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        labels.push(rest.get(..end).unwrap_or_default().to_string());
        rest = rest.get(end..).unwrap_or_default().trim();

        if rest.starts_with(',') {
            rest = rest.get(1..).unwrap_or_default().trim();
        } else {
            break;
        }
    }
    Some((labels, rest))
}

/// Try to parse a `| regexp "..."` or `| pattern "..."` pipeline stage.
/// The keyword must be followed by a quoted string argument.
/// Returns `None` if the keyword doesn't match; `Err` if it matches but the
/// quoted string is malformed.
fn try_parse_quoted_stage<'a>(
    remainder: &'a str,
    keyword: &str,
) -> Result<Option<(String, &'a str)>, ParseError> {
    let after_pipe = match remainder.strip_prefix('|') {
        Some(s) => s.trim(),
        None => return Ok(None),
    };
    let after_keyword = match after_pipe.strip_prefix(keyword) {
        Some(s) => s,
        None => return Ok(None),
    };

    // Must have whitespace after keyword to avoid matching e.g. "regexpx".
    if after_keyword.is_empty() {
        return Err(ParseError::Syntax(format!(
            "{keyword} requires a quoted string argument"
        )));
    }
    let first = after_keyword.chars().next().unwrap_or(' ');
    if !first.is_whitespace() {
        return Ok(None);
    }

    let trimmed = after_keyword.trim();
    if trimmed.is_empty() || (!trimmed.starts_with('"') && !trimmed.starts_with('`')) {
        return Err(ParseError::Syntax(format!(
            "{keyword} requires a quoted string argument, got: '{}'",
            trimmed.chars().take(30).collect::<String>()
        )));
    }

    let (value, rest) = parse_quoted_value(trimmed)?;
    Ok(Some((value, rest.trim())))
}

/// Try to parse a `| label_format dst=src, out="{{.a}}"` pipeline stage.
///
/// Each comma-separated item is `<dst> = <source>`, where the source is either a
/// bareword label name (rename) or a quoted template string. Returns `Ok(None)`
/// when the remainder does not begin the `label_format` keyword, and
/// `Err(..)` when the keyword is present but the operation list is malformed.
fn try_parse_label_format_stage(
    remainder: &str,
) -> Result<Option<(Vec<LabelFormatItem>, &str)>, ParseError> {
    let after_pipe = match remainder.strip_prefix('|') {
        Some(s) => s.trim(),
        None => return Ok(None),
    };
    let after_keyword = match after_pipe.strip_prefix("label_format") {
        Some(s) => s,
        None => return Ok(None),
    };
    // Require whitespace after the keyword to avoid matching e.g. "label_formatx".
    if !after_keyword.starts_with(char::is_whitespace) {
        return Ok(None);
    }

    let mut rest = after_keyword.trim_start();
    let mut items = Vec::new();
    loop {
        // Destination label name: `[A-Za-z_][A-Za-z0-9_]*`.
        let name_end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        let dst = rest.get(..name_end).unwrap_or("");
        if dst.is_empty() || dst.starts_with(|c: char| c.is_ascii_digit()) {
            return Err(ParseError::Syntax(format!(
                "label_format expects a label name before '=', got: '{}'",
                rest.chars().take(30).collect::<String>()
            )));
        }
        rest = rest.get(name_end..).unwrap_or("").trim_start();
        rest = match rest.strip_prefix('=') {
            Some(r) => r.trim_start(),
            None => {
                return Err(ParseError::Syntax(format!(
                    "label_format expects '=' after label name '{dst}'"
                )));
            }
        };

        // Source: quoted template (`"..."`/backtick) or bareword rename.
        if rest.starts_with('"') || rest.starts_with('`') {
            let (tmpl, after) = parse_quoted_value(rest)?;
            items.push(LabelFormatItem {
                dst: dst.to_string(),
                value: LabelFormatValue::Template(tmpl),
            });
            rest = after.trim_start();
        } else {
            let src_end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            let src = rest.get(..src_end).unwrap_or("");
            // A rename source is a label identifier (`[A-Za-z_][A-Za-z0-9_]*`).
            // Reject a digit-leading token (e.g. `a=123`, a number the user
            // forgot to quote) so it fails at parse time instead of silently
            // renaming from a nonexistent label to the empty string.
            if src.is_empty() || src.starts_with(|c: char| c.is_ascii_digit()) {
                return Err(ParseError::Syntax(format!(
                    "label_format expects a source label or quoted template after '{dst}='"
                )));
            }
            items.push(LabelFormatItem {
                dst: dst.to_string(),
                value: LabelFormatValue::Rename(src.to_string()),
            });
            rest = rest.get(src_end..).unwrap_or("").trim_start();
        }

        // Another operation follows a comma; otherwise the stage is complete.
        match rest.strip_prefix(',') {
            Some(r) => rest = r.trim_start(),
            None => break,
        }
    }

    Ok(Some((items, rest)))
}

/// Try to parse a `| unwrap <label>` or `| unwrap duration(<label>)` pipeline stage.
/// Returns `None` if the remainder doesn't match.
fn try_parse_unwrap_stage(remainder: &str) -> Option<(UnwrapExpr, &str)> {
    let after_pipe = remainder.strip_prefix('|')?.trim();
    let after_kw = after_pipe.strip_prefix("unwrap")?;

    // Require whitespace after keyword to avoid matching e.g. "unwrapx".
    if !after_kw.starts_with(char::is_whitespace) {
        return None;
    }
    let body = after_kw.trim();

    // Check for conversion functions: `duration(label)` or `bytes(label)`.
    for (prefix, conv) in [
        ("duration(", UnwrapConversion::Duration),
        ("bytes(", UnwrapConversion::Bytes),
    ] {
        if let Some(inner) = body.strip_prefix(prefix) {
            let close = inner.find(')')?;
            let label = inner.get(..close)?.trim();
            if label.is_empty() {
                return None;
            }
            let rest = inner.get(close + 1..)?.trim();
            return Some((
                UnwrapExpr {
                    label: label.to_string(),
                    conversion: Some(conv),
                },
                rest,
            ));
        }
    }

    // Plain unwrap: `| unwrap label_name`
    // Label ends at whitespace, `|`, `[`, `)`, or end of input.
    let end = body
        .find(|c: char| c.is_whitespace() || matches!(c, '|' | '[' | ')'))
        .unwrap_or(body.len());
    let label = body.get(..end)?.trim();
    if label.is_empty() {
        return None;
    }
    let rest = body.get(end..)?.trim();
    Some((
        UnwrapExpr {
            label: label.to_string(),
            conversion: None,
        },
        rest,
    ))
}

/// Parse a comma-separated list of label names until the next `|`, `[`, or end of input.
fn parse_label_list_until_bracket(input: &str) -> (Vec<String>, &str) {
    let end = input.find(['|', '[']).unwrap_or(input.len());
    let labels_str = input.get(..end).unwrap_or("");
    let labels = labels_str
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let rest = input.get(end..).unwrap_or("");
    (labels, rest)
}

/// Parse a quoted value: `"..."` or `` `...` ``.
fn parse_quoted_value(input: &str) -> Result<(String, &str), ParseError> {
    let quote = input.chars().next().ok_or_else(|| {
        ParseError::Syntax("line filter value must be quoted, got: ''".to_string())
    })?;

    if quote != '"' && quote != '`' {
        return Err(ParseError::Syntax(format!(
            "line filter value must be quoted, got: '{input}'"
        )));
    }

    let value_start = quote.len_utf8();
    let mut end = None;
    let mut escape = false;
    for (i, ch) in input[value_start..].char_indices() {
        if quote == '"' {
            if escape {
                escape = false;
                continue;
            }
            if ch == '\\' {
                escape = true;
                continue;
            }
        }

        if ch == quote {
            end = Some(value_start + i);
            break;
        }
    }

    let end_pos = end.ok_or_else(|| {
        ParseError::Syntax(format!("unterminated string in line filter: '{input}'"))
    })?;
    let value = input[value_start..end_pos].to_string();
    let remaining = input.get(end_pos + quote.len_utf8()..).unwrap_or("");
    Ok((value, remaining))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Selector parsing
    // -----------------------------------------------------------------------

    #[test]
    fn parse_simple_selector() {
        let expr = parse(r#"{service_name="web"}"#).unwrap();
        let Expr::Log(q) = expr else {
            panic!("expected Log expression")
        };
        assert_eq!(q.selector.matchers.len(), 1);
        assert_eq!(q.selector.matchers[0].name, "service_name");
        assert_eq!(q.selector.matchers[0].op, MatchOp::Eq);
        assert_eq!(q.selector.matchers[0].value, "web");
        assert!(q.pipeline.is_empty());
    }

    #[test]
    fn parse_multiple_matchers() {
        let expr = parse(r#"{app="api", env!="prod"}"#).unwrap();
        let Expr::Log(q) = expr else {
            panic!("expected Log expression")
        };
        assert_eq!(q.selector.matchers.len(), 2);
        assert_eq!(q.selector.matchers[1].op, MatchOp::Neq);
    }

    #[test]
    fn parse_regex_matchers() {
        let expr = parse(r#"{app=~"web.*", env!~"test.*"}"#).unwrap();
        let Expr::Log(q) = expr else {
            panic!("expected Log expression")
        };
        assert_eq!(q.selector.matchers[0].op, MatchOp::Re);
        assert_eq!(q.selector.matchers[1].op, MatchOp::Nre);
    }

    #[test]
    fn parse_empty_selector() {
        let expr = parse(r#"{}"#).unwrap();
        let Expr::Log(q) = expr else {
            panic!("expected Log expression")
        };
        assert!(q.selector.matchers.is_empty());
    }

    // -----------------------------------------------------------------------
    // vector(scalar)
    // -----------------------------------------------------------------------

    #[test]
    fn parse_vector_constant() {
        assert_eq!(parse("vector(0)").unwrap(), Expr::Vector(0.0));
        assert_eq!(parse("vector(1)").unwrap(), Expr::Vector(1.0));
        assert_eq!(parse("vector(-1)").unwrap(), Expr::Vector(-1.0));
        assert_eq!(parse("vector( 42 )").unwrap(), Expr::Vector(42.0));
    }

    #[test]
    fn parse_vector_folds_scalar_expression() {
        // A constant scalar expression is folded to its value.
        assert_eq!(parse("vector(1+2)").unwrap(), Expr::Vector(3.0));
        assert_eq!(parse("vector(60*5)").unwrap(), Expr::Vector(300.0));
        assert_eq!(parse("vector(2^10)").unwrap(), Expr::Vector(1024.0));
        assert_eq!(parse("vector((1+2)*3)").unwrap(), Expr::Vector(9.0));
    }

    #[test]
    fn parse_vector_rejects_non_scalar_and_malformed() {
        // A metric argument parses but is not a scalar literal → Unsupported.
        assert!(matches!(
            parse(r#"vector(rate({app="x"}[5m]))"#),
            Err(ParseError::Unsupported(_))
        ));
        // A selector argument, missing parens, and an empty argument are all
        // rejected (as syntax errors).
        assert!(parse(r#"vector({app="x"})"#).is_err());
        assert!(parse("vector").is_err());
        assert!(parse("vector()").is_err());
    }

    #[test]
    fn parse_metric_or_vector_default() {
        // The alerting default-to-zero idiom parses as `<metric> or vector(0)`.
        let expr = parse(r#"sum(count_over_time({app="x"}[5m])) or vector(0)"#).unwrap();
        let Expr::BinOp { op, rhs, .. } = expr else {
            panic!("expected a binary expression");
        };
        assert_eq!(op, BinOp::Or);
        assert_eq!(*rhs, Expr::Vector(0.0));
    }

    // -----------------------------------------------------------------------
    // Line filter parsing
    // -----------------------------------------------------------------------

    fn assert_line_filter(query: &str, expected_op: LineFilterOp, expected_value: &str) {
        let expr = parse(query).unwrap();
        let Expr::Log(q) = expr else {
            panic!("expected Log expression")
        };
        assert!(!q.pipeline.is_empty(), "pipeline should not be empty");
        let PipelineStage::LineFilter(f) = &q.pipeline[0] else {
            panic!("expected LineFilter stage")
        };
        assert_eq!(f.op, expected_op);
        assert_eq!(f.value, expected_value);
    }

    #[test]
    fn parse_line_filter_contains() {
        assert_line_filter(r#"{app="web"} |= "error""#, LineFilterOp::Contains, "error");
    }

    #[test]
    fn parse_line_filter_not_contains() {
        assert_line_filter(
            r#"{app="web"} != "debug""#,
            LineFilterOp::NotContains,
            "debug",
        );
    }

    #[test]
    fn parse_line_filter_regex() {
        assert_line_filter(r#"{app="web"} |~ "err.*""#, LineFilterOp::Regex, "err.*");
    }

    #[test]
    fn parse_multiple_line_filters() {
        let expr = parse(r#"{app="web"} |= "error" != "timeout""#).unwrap();
        let Expr::Log(q) = expr else {
            panic!("expected Log expression")
        };
        assert_eq!(q.pipeline.len(), 2);
    }

    #[test]
    fn parse_backtick_quoted_value() {
        assert_line_filter("{app=\"web\"} |= ``", LineFilterOp::Contains, "");
    }

    #[test]
    fn parse_backtick_with_content() {
        assert_line_filter(
            "{app=\"web\"} |= `hello world`",
            LineFilterOp::Contains,
            "hello world",
        );
    }

    // -----------------------------------------------------------------------
    // Label-filter stage parsing
    // -----------------------------------------------------------------------

    fn log_pipeline(query: &str) -> Vec<PipelineStage> {
        match parse(query).unwrap_or_else(|e| panic!("parse {query}: {e:?}")) {
            Expr::Log(q) => q.pipeline,
            other => panic!("{query}: expected Log expression, got {other:?}"),
        }
    }

    fn sole_label_filter(query: &str) -> LabelFilterExpr {
        let pipeline = log_pipeline(query);
        assert_eq!(pipeline.len(), 1, "{query}: expected one stage");
        match pipeline.into_iter().next() {
            Some(PipelineStage::LabelFilter(e)) => e,
            other => panic!("{query}: expected LabelFilter, got {other:?}"),
        }
    }

    fn sole_comparison(query: &str) -> LabelFilterComparison {
        match sole_label_filter(query) {
            LabelFilterExpr::Comparison(c) => c,
            other => panic!("{query}: expected a single comparison, got {other:?}"),
        }
    }

    #[test]
    fn label_filter_string_ops() {
        let c = sole_comparison(r#"{app="web"} | level = "error""#);
        assert_eq!(c.name, "level");
        assert_eq!(c.op, LabelFilterOp::Eq);
        assert_eq!(c.value, LabelFilterValue::String("error".to_string()));

        for (q, op) in [
            (r#"{a="b"} | level != "x""#, LabelFilterOp::Neq),
            (r#"{a="b"} | path =~ "/api/.*""#, LabelFilterOp::Re),
            (r#"{a="b"} | path !~ "/health""#, LabelFilterOp::Nre),
        ] {
            assert_eq!(sole_comparison(q).op, op, "{q}");
        }
    }

    #[test]
    fn label_filter_numeric_ops() {
        for (q, op, val) in [
            (r#"{a="b"} | status = 500"#, LabelFilterOp::Eq, 500.0),
            (r#"{a="b"} | status == 500"#, LabelFilterOp::Eq, 500.0),
            (r#"{a="b"} | status != 500"#, LabelFilterOp::Neq, 500.0),
            (r#"{a="b"} | status > 500"#, LabelFilterOp::Gt, 500.0),
            (r#"{a="b"} | status >= 500"#, LabelFilterOp::Gte, 500.0),
            (r#"{a="b"} | status < 500"#, LabelFilterOp::Lt, 500.0),
            (r#"{a="b"} | status <= 500"#, LabelFilterOp::Lte, 500.0),
            (r#"{a="b"} | ratio < 0.5"#, LabelFilterOp::Lt, 0.5),
        ] {
            let c = sole_comparison(q);
            assert_eq!(c.op, op, "{q}");
            assert_eq!(c.value, LabelFilterValue::Number(val), "{q}");
        }
    }

    #[test]
    fn label_filter_duration_and_bytes_values() {
        let d = sole_comparison(r#"{a="b"} | duration > 10s"#);
        assert_eq!(d.op, LabelFilterOp::Gt);
        assert_eq!(d.value, LabelFilterValue::Duration(Duration::from_secs(10)));

        assert_eq!(
            sole_comparison(r#"{a="b"} | size >= 1MB"#).value,
            LabelFilterValue::Bytes(1_000_000)
        );
        assert_eq!(
            sole_comparison(r#"{a="b"} | size > 4KiB"#).value,
            LabelFilterValue::Bytes(4096)
        );
    }

    #[test]
    fn label_filter_left_to_right_equal_precedence() {
        // Loki evaluates predicates left-to-right with equal precedence, so
        // `a or b and c` parses as `((a or b) and c)` — And at the root with an
        // Or on the left.
        let e = sole_label_filter(r#"{a="b"} | level="error" or status>=500 and status<600"#);
        match e {
            LabelFilterExpr::Binary {
                op: LabelFilterBoolOp::And,
                lhs,
                rhs,
            } => {
                assert!(matches!(
                    *lhs,
                    LabelFilterExpr::Binary {
                        op: LabelFilterBoolOp::Or,
                        ..
                    }
                ));
                assert!(matches!(*rhs, LabelFilterExpr::Comparison(_)));
            }
            other => panic!("expected And at root, got {other:?}"),
        }
    }

    #[test]
    fn label_filter_whitespace_is_and() {
        // Adjacent predicates with no combinator are AND (Loki whitespace-as-and).
        let e = sole_label_filter(r#"{a="b"} | duration >= 20ms method !~ "2..""#);
        match e {
            LabelFilterExpr::Binary {
                op: LabelFilterBoolOp::And,
                lhs,
                rhs,
            } => {
                assert!(matches!(*lhs, LabelFilterExpr::Comparison(_)));
                assert!(matches!(*rhs, LabelFilterExpr::Comparison(_)));
            }
            other => panic!("expected And at root, got {other:?}"),
        }
    }

    #[test]
    fn label_filter_comma_is_and_and_parens_override() {
        assert!(matches!(
            sole_label_filter(r#"{a="b"} | status>=500, status<600"#),
            LabelFilterExpr::Binary {
                op: LabelFilterBoolOp::And,
                ..
            }
        ));
        // `(a or b) and c` → And at the root.
        assert!(matches!(
            sole_label_filter(r#"{a="b"} | (level="error" or level="warn") and status>=500"#),
            LabelFilterExpr::Binary {
                op: LabelFilterBoolOp::And,
                ..
            }
        ));
    }

    #[test]
    fn label_filter_error_label_and_stage_separation() {
        let c = sole_comparison(r#"{a="b"} | __error__ = """#);
        assert_eq!(c.name, "__error__");
        assert_eq!(c.value, LabelFilterValue::String(String::new()));

        // `| json | <filter>` is two stages; `| f1 | f2` is two filter stages.
        let after_json = log_pipeline(r#"{a="b"} | json | status >= 500"#);
        assert_eq!(after_json.len(), 2);
        assert!(matches!(after_json[0], PipelineStage::Json(_)));
        assert!(matches!(after_json[1], PipelineStage::LabelFilter(_)));

        let two = log_pipeline(r#"{a="b"} | status>=500 | level="error""#);
        assert_eq!(two.len(), 2);
        assert!(
            two.iter()
                .all(|s| matches!(s, PipelineStage::LabelFilter(_)))
        );
    }

    #[test]
    fn label_filter_type_mismatch_is_validation() {
        // Ordering operator on a string value.
        assert!(matches!(
            parse(r#"{a="b"} | level > "x""#),
            Err(ParseError::Validation(_))
        ));
        // Regex operator on a numeric value.
        assert!(matches!(
            parse(r#"{a="b"} | status =~ 500"#),
            Err(ParseError::Validation(_))
        ));
    }

    #[test]
    fn label_filter_name_must_not_start_with_digit() {
        // The stage-level check guards the first term; the comparison parser
        // guards nested and parenthesized terms.
        assert!(parse(r#"{a="b"} | 1bad=2"#).is_err());
        assert!(parse(r#"{a="b"} | status=500 and 1bad=2"#).is_err());
        assert!(parse(r#"{a="b"} | (1bad=2)"#).is_err());
    }

    #[test]
    fn label_filter_on_keyword_named_labels() {
        // A label named like a pipeline keyword, followed by a comparison op, is
        // a label filter — not the keyword stage it shadows.
        for kw in [
            "json", "logfmt", "regexp", "pattern", "unwrap", "drop", "keep",
        ] {
            let q = format!(r#"{{a="b"}} | {kw} = "x""#);
            let c = sole_comparison(&q);
            assert_eq!(c.name, kw, "{q}");
            assert_eq!(c.op, LabelFilterOp::Eq, "{q}");
            assert_eq!(c.value, LabelFilterValue::String("x".to_string()), "{q}");
        }
        // The keyword STAGES still parse when no comparison operator follows.
        assert!(matches!(
            log_pipeline(r#"{a="b"} | json"#)[0],
            PipelineStage::Json(_)
        ));
        assert!(matches!(
            log_pipeline(r#"{a="b"} | json foo"#)[0],
            PipelineStage::Json(_)
        ));
        assert!(matches!(
            log_pipeline(r#"{a="b"} | regexp "(?P<x>.)""#)[0],
            PipelineStage::Regexp(_)
        ));
        assert!(matches!(
            log_pipeline(r#"{a="b"} | unwrap latency"#)[0],
            PipelineStage::Unwrap(_)
        ));
        assert!(matches!(
            log_pipeline(r#"{a="b"} | drop level"#)[0],
            PipelineStage::Drop(_)
        ));
    }

    // -----------------------------------------------------------------------
    // Drop/keep stage parsing
    // -----------------------------------------------------------------------

    fn assert_label_stage(query: &str, expect_drop: bool, expected_labels: &[&str]) {
        let expr = parse(query).unwrap();
        let Expr::Log(q) = expr else {
            panic!("expected Log expression")
        };
        assert!(!q.pipeline.is_empty(), "pipeline should not be empty");
        match &q.pipeline[0] {
            PipelineStage::Drop(labels) if expect_drop => {
                let expected: Vec<String> = expected_labels.iter().map(|s| s.to_string()).collect();
                assert_eq!(*labels, expected);
            }
            PipelineStage::Keep(labels) if !expect_drop => {
                let expected: Vec<String> = expected_labels.iter().map(|s| s.to_string()).collect();
                assert_eq!(*labels, expected);
            }
            other => panic!(
                "expected {} stage, got {other:?}",
                if expect_drop { "Drop" } else { "Keep" }
            ),
        }
    }

    #[test]
    fn parse_drop_stage() {
        assert_label_stage(r#"{app="web"} | drop level"#, true, &["level"]);
    }

    #[test]
    fn parse_keep_stage() {
        assert_label_stage(r#"{app="web"} | keep app, env"#, false, &["app", "env"]);
    }

    // -----------------------------------------------------------------------
    // line_format / label_format stage parsing
    // -----------------------------------------------------------------------

    #[test]
    fn parse_line_format_stage() {
        let pipeline = log_pipeline(r#"{app="web"} | line_format "{{.status}} {{.method}}""#);
        assert_eq!(pipeline.len(), 1);
        match &pipeline[0] {
            PipelineStage::LineFormat(tmpl) => assert_eq!(tmpl, "{{.status}} {{.method}}"),
            other => panic!("expected LineFormat, got {other:?}"),
        }
    }

    #[test]
    fn parse_line_format_backtick_and_trailing_stage() {
        // Backtick template, followed by a line filter — both stages survive.
        let pipeline = log_pipeline(r#"{app="web"} | line_format `{{.msg}}` |= "boom""#);
        assert_eq!(pipeline.len(), 2);
        assert!(matches!(&pipeline[0], PipelineStage::LineFormat(t) if t == "{{.msg}}"));
        assert!(matches!(&pipeline[1], PipelineStage::LineFilter(_)));
    }

    #[test]
    fn parse_line_format_requires_quoted_arg() {
        assert!(parse(r#"{app="web"} | line_format {{.msg}}"#).is_err());
    }

    #[test]
    fn parse_label_format_rename_and_template() {
        let pipeline =
            log_pipeline(r#"{app="web"} | label_format level=detected_level, out="{{.a}}-{{.b}}""#);
        assert_eq!(pipeline.len(), 1);
        let items = match &pipeline[0] {
            PipelineStage::LabelFormat(items) => items,
            other => panic!("expected LabelFormat, got {other:?}"),
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].dst, "level");
        assert_eq!(
            items[0].value,
            LabelFormatValue::Rename("detected_level".to_string())
        );
        assert_eq!(items[1].dst, "out");
        assert_eq!(
            items[1].value,
            LabelFormatValue::Template("{{.a}}-{{.b}}".to_string())
        );
    }

    #[test]
    fn parse_label_format_template_with_comma_inside_quotes() {
        // A comma inside the quoted template must not split the operation list.
        let pipeline = log_pipeline(r#"{app="web"} | label_format msg="{{.a}}, {{.b}}""#);
        let items = match &pipeline[0] {
            PipelineStage::LabelFormat(items) => items,
            other => panic!("expected LabelFormat, got {other:?}"),
        };
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].value,
            LabelFormatValue::Template("{{.a}}, {{.b}}".to_string())
        );
    }

    #[test]
    fn parse_label_format_in_metric_pipeline() {
        // Range aggregation with a trailing label_format before the duration.
        let expr =
            parse(r#"sum by (out) (count_over_time({app="web"} | label_format out=svc [5m]))"#)
                .unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric expression")
        };
        let pipeline = &m.range_aggregation.log_query.pipeline;
        assert_eq!(pipeline.len(), 1);
        assert!(matches!(&pipeline[0], PipelineStage::LabelFormat(items)
            if items.len() == 1 && items[0].dst == "out"));
    }

    #[test]
    fn parse_label_format_malformed_is_error() {
        assert!(parse(r#"{app="web"} | label_format =foo"#).is_err());
        assert!(parse(r#"{app="web"} | label_format foo"#).is_err());
    }

    #[test]
    fn parse_label_format_digit_leading_source_is_error() {
        // A bare number as a rename source (a value the user forgot to quote) is
        // not a valid label identifier — reject it rather than silently rename
        // from a nonexistent label.
        assert!(parse(r#"{app="web"} | label_format a=123"#).is_err());
        // The quoted form is still accepted as a template.
        assert!(parse(r#"{app="web"} | label_format a="123""#).is_ok());
    }

    // -----------------------------------------------------------------------
    // Drop/keep stages with label lists
    // -----------------------------------------------------------------------

    #[test]
    fn parse_error_drop_stage() {
        let expr = parse(r#"{app="web"} | drop __error__"#).unwrap();
        let Expr::Log(q) = expr else {
            panic!("expected Log expression")
        };
        assert_eq!(q.pipeline.len(), 1);
        assert!(matches!(&q.pipeline[0], PipelineStage::Drop(labels) if labels == &["__error__"]));
    }

    #[test]
    fn parse_error_details_drop_stage() {
        let expr = parse(r#"{app="web"} | drop __error__, __error_details__"#).unwrap();
        let Expr::Log(q) = expr else {
            panic!("expected Log expression")
        };
        assert_eq!(q.pipeline.len(), 1);
        assert!(
            matches!(&q.pipeline[0], PipelineStage::Drop(labels) if labels == &["__error__", "__error_details__"])
        );
    }

    #[test]
    fn parse_non_error_drop_stage() {
        let expr = parse(r#"{app="web"} | drop level"#).unwrap();
        let Expr::Log(q) = expr else {
            panic!("expected Log expression")
        };
        assert_eq!(q.pipeline.len(), 1);
        assert!(matches!(&q.pipeline[0], PipelineStage::Drop(labels) if labels == &["level"]));
    }

    #[test]
    fn bare_drop_no_labels_is_error() {
        // `| drop` with no label names is invalid LogQL.
        let err = parse(r#"{app="web"} | drop"#).unwrap_err();
        assert!(
            matches!(err, ParseError::Syntax(_)),
            "expected Syntax error, got: {err}"
        );
    }

    #[test]
    fn bare_drop_followed_by_duration_is_error() {
        // `| drop [5m]` — empty drop before a duration bracket is invalid.
        let err = parse(r#"{app="web"} | drop [5m]"#).unwrap_err();
        assert!(
            matches!(err, ParseError::Syntax(_)),
            "expected Syntax error, got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Error cases
    // -----------------------------------------------------------------------

    #[test]
    fn empty_query_is_error() {
        assert!(parse("").is_err());
        assert!(parse("   ").is_err());
    }

    #[test]
    fn unterminated_selector_is_error() {
        let err = parse(r#"{app="web""#).unwrap_err();
        assert!(matches!(err, ParseError::Syntax(_)));
    }

    #[test]
    fn unquoted_matcher_value_is_error() {
        let err = parse(r#"{app=web}"#).unwrap_err();
        assert!(matches!(err, ParseError::Syntax(_)));
    }

    // -----------------------------------------------------------------------
    // Range aggregation parsing
    // -----------------------------------------------------------------------

    fn assert_range_agg(query: &str, expected_fn: RangeAggregationFn) {
        let expr = parse(query).unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric expression, got {expr:?}")
        };
        assert!(m.vector_aggregation.is_none());
        assert_eq!(m.range_aggregation.function, expected_fn);
    }

    #[test]
    fn parse_rate() {
        assert_range_agg(
            r#"rate({service_name="web"}[5m])"#,
            RangeAggregationFn::Rate,
        );
    }

    #[test]
    fn parse_count_over_time() {
        assert_range_agg(
            r#"count_over_time({severity="error"}[1h])"#,
            RangeAggregationFn::CountOverTime,
        );
    }

    #[test]
    fn parse_bytes_rate() {
        assert_range_agg(
            r#"bytes_rate({service_name="web"}[5m])"#,
            RangeAggregationFn::BytesRate,
        );
    }

    #[test]
    fn parse_bytes_over_time() {
        assert_range_agg(
            r#"bytes_over_time({service_name="web"}[5m])"#,
            RangeAggregationFn::BytesOverTime,
        );
    }

    #[test]
    fn parse_rate_duration_seconds() {
        let expr = parse(r#"rate({service_name="web"}[30s])"#).unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        assert_eq!(m.range_aggregation.range, Duration::from_secs(30));
    }

    #[test]
    fn parse_rate_duration_hours() {
        let expr = parse(r#"rate({service_name="web"}[1h])"#).unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        assert_eq!(m.range_aggregation.range, Duration::from_secs(3600));
    }

    #[test]
    fn parse_rate_with_line_filter() {
        let expr = parse(r#"rate({service_name="web"} |= "error" [5m])"#).unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        assert_eq!(m.range_aggregation.function, RangeAggregationFn::Rate);
        assert_eq!(m.range_aggregation.log_query.pipeline.len(), 1);
        let PipelineStage::LineFilter(f) = &m.range_aggregation.log_query.pipeline[0] else {
            panic!("expected LineFilter")
        };
        assert_eq!(f.value, "error");
    }

    #[test]
    fn parse_rate_with_backtick_filter() {
        let expr = parse(r#"rate({service_name="web"} |= `error` [5m])"#).unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        assert_eq!(m.range_aggregation.log_query.pipeline.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Vector aggregation parsing
    // -----------------------------------------------------------------------

    #[test]
    fn parse_sum_by_rate() {
        let expr = parse(r#"sum by (service_name) (rate({severity="error"}[5m]))"#).unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        let va = m.vector_aggregation.as_ref().unwrap();
        assert_eq!(va.function, VectorAggregationFn::Sum);
        let g = va.grouping.as_ref().unwrap();
        assert_eq!(g.kind, GroupingKind::By);
        assert_eq!(g.labels, vec!["service_name"]);
        assert_eq!(m.range_aggregation.function, RangeAggregationFn::Rate);
    }

    #[test]
    fn parse_sum_by_count_over_time() {
        let expr = parse(r#"sum by (level) (count_over_time({service_name="web"}[5m]))"#).unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        assert_eq!(
            m.vector_aggregation.as_ref().unwrap().function,
            VectorAggregationFn::Sum,
        );
        assert_eq!(
            m.range_aggregation.function,
            RangeAggregationFn::CountOverTime,
        );
    }

    #[test]
    fn parse_topk() {
        let expr = parse(r#"topk(5, rate({service_name="web"}[5m]))"#).unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        let va = m.vector_aggregation.as_ref().unwrap();
        assert_eq!(va.function, VectorAggregationFn::Topk);
        assert_eq!(va.parameter, Some(5));
    }

    #[test]
    fn parse_bottomk() {
        let expr = parse(r#"bottomk(3, count_over_time({severity="error"}[1h]))"#).unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        let va = m.vector_aggregation.as_ref().unwrap();
        assert_eq!(va.function, VectorAggregationFn::Bottomk);
        assert_eq!(va.parameter, Some(3));
    }

    #[test]
    fn parse_avg_by() {
        let expr = parse(r#"avg by (service_name) (rate({service_name=~".+"}[5m]))"#).unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        assert_eq!(
            m.vector_aggregation.as_ref().unwrap().function,
            VectorAggregationFn::Avg,
        );
    }

    #[test]
    fn parse_max_without() {
        let expr = parse(r#"max without (instance) (rate({service_name="web"}[5m]))"#).unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        let va = m.vector_aggregation.as_ref().unwrap();
        assert_eq!(va.function, VectorAggregationFn::Max);
        let g = va.grouping.as_ref().unwrap();
        assert_eq!(g.kind, GroupingKind::Without);
        assert_eq!(g.labels, vec!["instance"]);
    }

    #[test]
    fn parse_count_by_multiple_labels() {
        let expr =
            parse(r#"count by (service_name, severity) (rate({service_name=~".+"}[5m]))"#).unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        let g = m
            .vector_aggregation
            .as_ref()
            .unwrap()
            .grouping
            .as_ref()
            .unwrap();
        assert_eq!(g.labels, vec!["service_name", "severity"]);
    }

    // -----------------------------------------------------------------------
    // Binary operators
    // -----------------------------------------------------------------------

    fn assert_binop(query: &str, expected_op: BinOp) {
        let expr = parse(query).unwrap();
        let Expr::BinOp { op, .. } = expr else {
            panic!("expected BinOp, got {expr:?}")
        };
        assert_eq!(op, expected_op);
    }

    #[test]
    fn parse_binary_add() {
        assert_binop(
            r#"sum(rate({service_name="a"}[5m])) + sum(rate({service_name="b"}[5m]))"#,
            BinOp::Add,
        );
    }

    #[test]
    fn parse_binary_sub() {
        assert_binop(
            r#"sum(rate({service_name="a"}[5m])) - sum(rate({service_name="b"}[5m]))"#,
            BinOp::Sub,
        );
    }

    #[test]
    fn parse_binary_mul() {
        assert_binop(
            r#"sum(rate({service_name="a"}[5m])) * sum(rate({service_name="b"}[5m]))"#,
            BinOp::Mul,
        );
    }

    #[test]
    fn parse_binary_div() {
        assert_binop(
            r#"sum(rate({service_name="a"}[5m])) / sum(rate({service_name="b"}[5m]))"#,
            BinOp::Div,
        );
    }

    #[test]
    fn parse_binary_mod() {
        assert_binop(
            r#"sum(rate({service_name="a"}[5m])) % sum(rate({service_name="b"}[5m]))"#,
            BinOp::Mod,
        );
    }

    #[test]
    fn parse_binary_pow() {
        assert_binop(
            r#"sum(rate({service_name="a"}[5m])) ^ sum(rate({service_name="b"}[5m]))"#,
            BinOp::Pow,
        );
    }

    #[test]
    fn parse_binary_precedence_mul_over_add() {
        // a + b * c should parse as a + (b * c)
        let query = r#"rate({a="1"}[5m]) + rate({b="2"}[5m]) * rate({c="3"}[5m])"#;
        let expr = parse(query).unwrap();
        let Expr::BinOp { op, rhs, .. } = expr else {
            panic!("expected BinOp")
        };
        assert_eq!(op, BinOp::Add);
        let Expr::BinOp { op: inner_op, .. } = rhs.as_ref() else {
            panic!("expected inner BinOp")
        };
        assert_eq!(*inner_op, BinOp::Mul);
    }

    // -----------------------------------------------------------------------
    // Comparison operators
    // -----------------------------------------------------------------------

    #[test]
    fn parse_comparison_gt_scalar() {
        let query = r#"rate({app="web"}[5m]) > 100"#;
        let expr = parse(query).unwrap();
        let Expr::BinOp {
            op,
            return_bool,
            rhs,
            ..
        } = expr
        else {
            panic!("expected BinOp, got {expr:?}")
        };
        assert_eq!(op, BinOp::Gt);
        assert!(!return_bool);
        assert!(matches!(rhs.as_ref(), Expr::Scalar(v) if (*v - 100.0).abs() < f64::EPSILON));
    }

    #[test]
    fn parse_comparison_eq_vector() {
        assert_binop(
            r#"sum(rate({app="a"}[5m])) == sum(rate({app="b"}[5m]))"#,
            BinOp::Eq,
        );
    }

    #[test]
    fn parse_comparison_neq() {
        assert_binop(r#"rate({app="web"}[5m]) != 0"#, BinOp::Neq);
    }

    #[test]
    fn parse_comparison_gte() {
        assert_binop(r#"rate({app="web"}[5m]) >= 100"#, BinOp::Gte);
    }

    #[test]
    fn parse_comparison_lte() {
        assert_binop(r#"rate({app="web"}[5m]) <= 0.5"#, BinOp::Lte);
    }

    #[test]
    fn parse_comparison_lt() {
        assert_binop(r#"rate({app="web"}[5m]) < 1000"#, BinOp::Lt);
    }

    // -----------------------------------------------------------------------
    // Bool modifier
    // -----------------------------------------------------------------------

    #[test]
    fn parse_comparison_bool_modifier() {
        let query = r#"rate({app="web"}[5m]) > bool 0"#;
        let expr = parse(query).unwrap();
        let Expr::BinOp {
            op, return_bool, ..
        } = expr
        else {
            panic!("expected BinOp, got {expr:?}")
        };
        assert_eq!(op, BinOp::Gt);
        assert!(return_bool);
    }

    #[test]
    fn parse_comparison_eq_bool() {
        let query = r#"rate({app="web"}[5m]) == bool 42"#;
        let expr = parse(query).unwrap();
        let Expr::BinOp {
            op, return_bool, ..
        } = expr
        else {
            panic!("expected BinOp")
        };
        assert_eq!(op, BinOp::Eq);
        assert!(return_bool);
    }

    // -----------------------------------------------------------------------
    // Logical / set operators
    // -----------------------------------------------------------------------

    #[test]
    fn parse_logical_and() {
        let query = r#"rate({app="web"}[5m]) > 100 and rate({app="web"}[5m]) < 1000"#;
        let expr = parse(query).unwrap();
        let Expr::BinOp { op, .. } = expr else {
            panic!("expected BinOp")
        };
        assert_eq!(op, BinOp::And);
    }

    #[test]
    fn parse_logical_or() {
        let query = r#"rate({app="web"}[5m]) > 100 or rate({app="web"}[5m]) == 0"#;
        let expr = parse(query).unwrap();
        let Expr::BinOp { op, .. } = expr else {
            panic!("expected BinOp")
        };
        assert_eq!(op, BinOp::Or);
    }

    #[test]
    fn parse_logical_unless() {
        let query = r#"rate({app="web"}[5m]) > 100 unless rate({app="web"}[5m]) > 500"#;
        let expr = parse(query).unwrap();
        let Expr::BinOp { op, .. } = expr else {
            panic!("expected BinOp")
        };
        assert_eq!(op, BinOp::Unless);
    }

    // -----------------------------------------------------------------------
    // Scalar literals
    // -----------------------------------------------------------------------

    #[test]
    fn parse_scalar_integer() {
        let expr = parse("100").unwrap();
        assert!(matches!(expr, Expr::Scalar(v) if (v - 100.0).abs() < f64::EPSILON));
    }

    #[test]
    fn parse_scalar_float() {
        let expr = parse("0.5").unwrap();
        assert!(matches!(expr, Expr::Scalar(v) if (v - 0.5).abs() < f64::EPSILON));
    }

    #[test]
    fn parse_scalar_nan() {
        let expr = parse("NaN").unwrap();
        assert!(matches!(expr, Expr::Scalar(v) if v.is_nan()));
    }

    #[test]
    fn parse_scalar_inf() {
        let expr = parse("Inf").unwrap();
        assert!(matches!(expr, Expr::Scalar(v) if v == f64::INFINITY));
    }

    #[test]
    fn parse_scalar_negative() {
        let expr = parse("-42").unwrap();
        assert!(matches!(expr, Expr::Scalar(v) if (v - (-42.0)).abs() < f64::EPSILON));
    }

    // -----------------------------------------------------------------------
    // Precedence: comparisons vs arithmetic vs logical
    // -----------------------------------------------------------------------

    #[test]
    fn precedence_arithmetic_binds_tighter_than_comparison() {
        // a + b > c  should parse as (a + b) > c
        let query = r#"rate({a="1"}[5m]) + rate({b="2"}[5m]) > 100"#;
        let expr = parse(query).unwrap();
        let Expr::BinOp { op, lhs, .. } = expr else {
            panic!("expected BinOp")
        };
        assert_eq!(op, BinOp::Gt);
        assert!(matches!(lhs.as_ref(), Expr::BinOp { op: BinOp::Add, .. }));
    }

    #[test]
    fn precedence_comparison_binds_tighter_than_and() {
        // a > 1 and b > 2  should parse as (a > 1) and (b > 2)
        let query = r#"rate({a="1"}[5m]) > 1 and rate({b="2"}[5m]) > 2"#;
        let expr = parse(query).unwrap();
        let Expr::BinOp { op, lhs, rhs, .. } = expr else {
            panic!("expected BinOp")
        };
        assert_eq!(op, BinOp::And);
        assert!(matches!(lhs.as_ref(), Expr::BinOp { op: BinOp::Gt, .. }));
        assert!(matches!(rhs.as_ref(), Expr::BinOp { op: BinOp::Gt, .. }));
    }

    #[test]
    fn precedence_and_binds_tighter_than_or() {
        // a > 1 and b > 2 or c > 3  should parse as ((a > 1) and (b > 2)) or (c > 3)
        let query = r#"rate({a="1"}[5m]) > 1 and rate({b="2"}[5m]) > 2 or rate({c="3"}[5m]) > 3"#;
        let expr = parse(query).unwrap();
        let Expr::BinOp { op, lhs, rhs, .. } = expr else {
            panic!("expected BinOp")
        };
        assert_eq!(op, BinOp::Or);
        assert!(matches!(lhs.as_ref(), Expr::BinOp { op: BinOp::And, .. }));
        assert!(matches!(rhs.as_ref(), Expr::BinOp { op: BinOp::Gt, .. }));
    }

    #[test]
    fn precedence_unless_same_as_and() {
        // a > 1 unless b > 2 and c > 3  should parse as ((a > 1) unless (b > 2)) and (c > 3)
        // because unless and and are same precedence, left-associative
        let query =
            r#"rate({a="1"}[5m]) > 1 unless rate({b="2"}[5m]) > 2 and rate({c="3"}[5m]) > 3"#;
        let expr = parse(query).unwrap();
        let Expr::BinOp { op, lhs, .. } = expr else {
            panic!("expected BinOp")
        };
        assert_eq!(op, BinOp::And);
        assert!(matches!(
            lhs.as_ref(),
            Expr::BinOp {
                op: BinOp::Unless,
                ..
            }
        ));
    }

    // -----------------------------------------------------------------------
    // Matching modifiers: on/ignoring/group_left/group_right
    // -----------------------------------------------------------------------

    #[test]
    fn parse_on_modifier() {
        let query = r#"rate({a="1"}[5m]) / on(service) rate({b="2"}[5m])"#;
        let expr = parse(query).unwrap();
        let Expr::BinOp {
            op,
            matching,
            return_bool,
            ..
        } = expr
        else {
            panic!("expected BinOp")
        };
        assert_eq!(op, BinOp::Div);
        assert!(!return_bool);
        let m = matching.unwrap();
        assert!(matches!(&m.mode, BinOpMatchMode::On(labels) if labels == &["service"]));
        assert!(m.group.is_none());
    }

    #[test]
    fn parse_ignoring_modifier() {
        let query = r#"rate({a="1"}[5m]) + ignoring(instance) rate({b="2"}[5m])"#;
        let expr = parse(query).unwrap();
        let Expr::BinOp { op, matching, .. } = expr else {
            panic!("expected BinOp")
        };
        assert_eq!(op, BinOp::Add);
        let m = matching.unwrap();
        assert!(matches!(&m.mode, BinOpMatchMode::Ignoring(labels) if labels == &["instance"]));
        assert!(m.group.is_none());
    }

    #[test]
    fn parse_on_group_left() {
        let query = r#"rate({a="1"}[5m]) * on(service) group_left(region) rate({b="2"}[5m])"#;
        let expr = parse(query).unwrap();
        let Expr::BinOp { op, matching, .. } = expr else {
            panic!("expected BinOp")
        };
        assert_eq!(op, BinOp::Mul);
        let m = matching.unwrap();
        assert!(matches!(&m.mode, BinOpMatchMode::On(labels) if labels == &["service"]));
        assert!(
            matches!(&m.group, Some(BinOpGroupModifier::Left(labels)) if labels == &["region"])
        );
    }

    #[test]
    fn parse_ignoring_group_right() {
        let query = r#"rate({a="1"}[5m]) / ignoring(instance) group_right() rate({b="2"}[5m])"#;
        let expr = parse(query).unwrap();
        let Expr::BinOp { op, matching, .. } = expr else {
            panic!("expected BinOp")
        };
        assert_eq!(op, BinOp::Div);
        let m = matching.unwrap();
        assert!(matches!(&m.mode, BinOpMatchMode::Ignoring(labels) if labels == &["instance"]));
        assert!(matches!(&m.group, Some(BinOpGroupModifier::Right(labels)) if labels.is_empty()));
    }

    #[test]
    fn parse_on_empty_labels() {
        let query = r#"rate({a="1"}[5m]) + on() rate({b="2"}[5m])"#;
        let expr = parse(query).unwrap();
        let Expr::BinOp { matching, .. } = expr else {
            panic!("expected BinOp")
        };
        let m = matching.unwrap();
        assert!(matches!(&m.mode, BinOpMatchMode::On(labels) if labels.is_empty()));
    }

    #[test]
    fn parse_on_multiple_labels() {
        let query = r#"rate({a="1"}[5m]) + on(service, instance, region) rate({b="2"}[5m])"#;
        let expr = parse(query).unwrap();
        let Expr::BinOp { matching, .. } = expr else {
            panic!("expected BinOp")
        };
        let m = matching.unwrap();
        assert!(
            matches!(&m.mode, BinOpMatchMode::On(labels) if labels == &["service", "instance", "region"])
        );
    }

    #[test]
    fn parse_comparison_with_on() {
        let query = r#"rate({a="1"}[5m]) > on(service) rate({b="2"}[5m])"#;
        let expr = parse(query).unwrap();
        let Expr::BinOp {
            op,
            matching,
            return_bool,
            ..
        } = expr
        else {
            panic!("expected BinOp")
        };
        assert_eq!(op, BinOp::Gt);
        assert!(!return_bool);
        let m = matching.unwrap();
        assert!(matches!(&m.mode, BinOpMatchMode::On(labels) if labels == &["service"]));
    }

    #[test]
    fn parse_comparison_bool_with_on() {
        let query = r#"rate({a="1"}[5m]) > bool on(service) rate({b="2"}[5m])"#;
        let expr = parse(query).unwrap();
        let Expr::BinOp {
            op,
            return_bool,
            matching,
            ..
        } = expr
        else {
            panic!("expected BinOp")
        };
        assert_eq!(op, BinOp::Gt);
        assert!(return_bool);
        let m = matching.unwrap();
        assert!(matches!(&m.mode, BinOpMatchMode::On(labels) if labels == &["service"]));
    }

    #[test]
    fn parse_no_matching_modifier() {
        let query = r#"rate({a="1"}[5m]) + rate({b="2"}[5m])"#;
        let expr = parse(query).unwrap();
        let Expr::BinOp { matching, .. } = expr else {
            panic!("expected BinOp")
        };
        assert!(matching.is_none());
    }

    #[test]
    fn parse_group_left_no_parens() {
        let query = r#"rate({a="1"}[5m]) * on(service) group_left rate({b="2"}[5m])"#;
        let expr = parse(query).unwrap();
        let Expr::BinOp { matching, .. } = expr else {
            panic!("expected BinOp")
        };
        let m = matching.unwrap();
        assert!(matches!(&m.group, Some(BinOpGroupModifier::Left(labels)) if labels.is_empty()));
    }

    // -----------------------------------------------------------------------
    // Unsupported / error cases for metric expressions
    // -----------------------------------------------------------------------

    #[test]
    fn parse_on_rejects_numeric_label() {
        // Label names must start with [a-zA-Z_], not digits
        let err = parse(r#"rate({a="1"}[5m]) + on(123) rate({b="2"}[5m])"#).unwrap_err();
        assert!(matches!(err, ParseError::Syntax(_)));
    }

    #[test]
    fn label_replace_is_unsupported() {
        let err =
            parse(r#"label_replace(rate({service_name="web"}[5m]), "dst", "$1", "src", "(.*)")"#)
                .unwrap_err();
        assert!(matches!(err, ParseError::Unsupported(_)));
    }

    // -----------------------------------------------------------------------
    // Grafana volume query (drop __error__ parsed natively)
    // -----------------------------------------------------------------------

    #[test]
    fn parse_grafana_volume_query() {
        let query = r#"sum by (level, detected_level) (count_over_time({service_name="demo-logs"} | drop __error__, __error_details__ [5m]))"#;
        let expr = parse(query).unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        let va = m.vector_aggregation.as_ref().unwrap();
        assert_eq!(va.function, VectorAggregationFn::Sum);
        let g = va.grouping.as_ref().unwrap();
        assert_eq!(g.labels, vec!["level", "detected_level"]);
        // Drop stages are parsed natively; filtering happens in the conversion layer.
        assert_eq!(m.range_aggregation.log_query.pipeline.len(), 1);
        assert!(matches!(
            &m.range_aggregation.log_query.pipeline[0],
            PipelineStage::Drop(labels) if labels == &["__error__", "__error_details__"]
        ));
    }

    // -----------------------------------------------------------------------
    // Offset modifier
    // -----------------------------------------------------------------------

    #[test]
    fn parse_offset_on_rate() {
        let expr = parse(r#"rate({service_name="web"}[5m] offset 1h)"#).unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        assert_eq!(m.range_aggregation.function, RangeAggregationFn::Rate);
        assert_eq!(m.range_aggregation.range, Duration::from_secs(300));
        assert_eq!(m.range_aggregation.offset, Some(Duration::from_secs(3600)));
    }

    #[test]
    fn parse_offset_on_count_over_time() {
        let expr = parse(r#"count_over_time({service_name="web"}[10m] offset 30m)"#).unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        assert_eq!(
            m.range_aggregation.function,
            RangeAggregationFn::CountOverTime
        );
        assert_eq!(m.range_aggregation.offset, Some(Duration::from_secs(1800)));
    }

    #[test]
    fn parse_no_offset_is_none() {
        let expr = parse(r#"rate({service_name="web"}[5m])"#).unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        assert_eq!(m.range_aggregation.offset, None);
    }

    #[test]
    fn parse_offset_with_vector_aggregation() {
        let expr =
            parse(r#"sum by (service_name) (rate({service_name="web"}[5m] offset 2h))"#).unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        assert!(m.vector_aggregation.is_some());
        assert_eq!(m.range_aggregation.offset, Some(Duration::from_secs(7200)));
    }

    #[test]
    fn parse_offset_with_seconds() {
        let expr = parse(r#"rate({service_name="web"}[1m] offset 30s)"#).unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        assert_eq!(m.range_aggregation.offset, Some(Duration::from_secs(30)));
    }

    // ---------------------------------------------------------------
    // Unwrap pipeline stage
    // ---------------------------------------------------------------

    #[test]
    fn parse_unwrap_plain() {
        let expr = parse(r#"sum_over_time({service_name="web"} | unwrap duration [5m])"#).unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        let stages = &m.range_aggregation.log_query.pipeline;
        assert_eq!(stages.len(), 1);
        match &stages[0] {
            PipelineStage::Unwrap(u) => {
                assert_eq!(u.label, "duration");
                assert_eq!(u.conversion, None);
            }
            other => panic!("expected Unwrap, got: {other:?}"),
        }
    }

    #[test]
    fn parse_quantile_over_time_captures_parameter() {
        let expr = parse(r#"quantile_over_time(0.99, {service_name="web"} | unwrap latency [5m])"#)
            .unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        assert_eq!(
            m.range_aggregation.function,
            RangeAggregationFn::QuantileOverTime
        );
        assert_eq!(m.range_aggregation.parameter, Some(0.99));
        assert_eq!(m.range_aggregation.range, Duration::from_secs(300));
        assert!(matches!(
            m.range_aggregation.log_query.pipeline.last(),
            Some(PipelineStage::Unwrap(_))
        ));
    }

    #[test]
    fn parse_quantile_over_time_with_grouping() {
        let expr = parse(
            r#"quantile_over_time(0.5, {service_name="web"} | unwrap latency [5m]) by (path)"#,
        )
        .unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        assert_eq!(m.range_aggregation.parameter, Some(0.5));
        assert!(m.vector_aggregation.is_some());
    }

    #[test]
    fn parse_quantile_over_time_missing_parameter_is_error() {
        // No leading scalar before the selector.
        assert!(
            parse(r#"quantile_over_time({service_name="web"} | unwrap latency [5m])"#).is_err()
        );
    }

    #[test]
    fn parse_quantile_over_time_accepts_signed_parameter() {
        // A signed φ parses (the engine applies Loki's out-of-range edge
        // behavior); the sign must not be rejected by scalar parsing.
        let neg = parse(r#"quantile_over_time(-0.1, {service_name="web"} | unwrap latency [5m])"#)
            .unwrap();
        let Expr::Metric(m) = neg else {
            panic!("expected Metric")
        };
        assert_eq!(m.range_aggregation.parameter, Some(-0.1));

        let pos = parse(r#"quantile_over_time(+0.5, {service_name="web"} | unwrap latency [5m])"#)
            .unwrap();
        let Expr::Metric(m) = pos else {
            panic!("expected Metric")
        };
        assert_eq!(m.range_aggregation.parameter, Some(0.5));
    }

    #[test]
    fn parse_other_range_aggs_have_no_parameter() {
        let expr = parse(r#"sum_over_time({service_name="web"} | unwrap latency [5m])"#).unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        assert_eq!(m.range_aggregation.parameter, None);
    }

    #[test]
    fn parse_unwrap_duration_conversion() {
        let expr =
            parse(r#"sum_over_time({service_name="web"} | unwrap duration(response_time) [5m])"#)
                .unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        let stages = &m.range_aggregation.log_query.pipeline;
        assert_eq!(stages.len(), 1);
        match &stages[0] {
            PipelineStage::Unwrap(u) => {
                assert_eq!(u.label, "response_time");
                assert_eq!(u.conversion, Some(UnwrapConversion::Duration));
            }
            other => panic!("expected Unwrap, got: {other:?}"),
        }
    }

    #[test]
    fn parse_unwrap_bytes_conversion() {
        let expr =
            parse(r#"sum_over_time({service_name="web"} | unwrap bytes(payload_size) [5m])"#)
                .unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        let stages = &m.range_aggregation.log_query.pipeline;
        assert_eq!(stages.len(), 1);
        match &stages[0] {
            PipelineStage::Unwrap(u) => {
                assert_eq!(u.label, "payload_size");
                assert_eq!(u.conversion, Some(UnwrapConversion::Bytes));
            }
            other => panic!("expected Unwrap, got: {other:?}"),
        }
    }

    #[test]
    fn parse_unwrap_with_line_filter() {
        let expr = parse(r#"avg_over_time({service_name="web"} |= "error" | unwrap latency [5m])"#)
            .unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        let stages = &m.range_aggregation.log_query.pipeline;
        assert_eq!(stages.len(), 2);
        assert!(matches!(&stages[0], PipelineStage::LineFilter(_)));
        match &stages[1] {
            PipelineStage::Unwrap(u) => {
                assert_eq!(u.label, "latency");
                assert_eq!(u.conversion, None);
            }
            other => panic!("expected Unwrap, got: {other:?}"),
        }
    }

    #[test]
    fn parse_unwrap_in_vector_aggregation() {
        let expr = parse(
            r#"sum by (service_name) (sum_over_time({service_name="web"} | unwrap duration [5m]))"#,
        )
        .unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        assert!(m.vector_aggregation.is_some());
        let stages = &m.range_aggregation.log_query.pipeline;
        assert_eq!(stages.len(), 1);
        assert!(matches!(&stages[0], PipelineStage::Unwrap(_)));
    }

    // ── json / logfmt pipeline stages ──────────────────────────────────

    #[test]
    fn parse_json_bare() {
        let expr = parse(r#"{service_name="web"} | json"#).unwrap();
        let Expr::Log(log) = expr else {
            panic!("expected Log")
        };
        assert_eq!(log.pipeline.len(), 1);
        assert!(matches!(&log.pipeline[0], PipelineStage::Json(labels) if labels.is_empty()));
    }

    #[test]
    fn parse_json_with_labels() {
        let expr = parse(r#"{service_name="web"} | json level, msg"#).unwrap();
        let Expr::Log(log) = expr else {
            panic!("expected Log")
        };
        assert_eq!(log.pipeline.len(), 1);
        match &log.pipeline[0] {
            PipelineStage::Json(labels) => {
                assert_eq!(labels, &["level", "msg"]);
            }
            other => panic!("expected Json, got: {other:?}"),
        }
    }

    #[test]
    fn parse_json_then_line_filter() {
        let expr = parse(r#"{service_name="web"} | json |= "error""#).unwrap();
        let Expr::Log(log) = expr else {
            panic!("expected Log")
        };
        assert_eq!(log.pipeline.len(), 2);
        assert!(matches!(&log.pipeline[0], PipelineStage::Json(labels) if labels.is_empty()));
        assert!(matches!(&log.pipeline[1], PipelineStage::LineFilter(_)));
    }

    #[test]
    fn parse_json_in_metric() {
        let expr = parse(r#"rate({service_name="web"} | json [5m])"#).unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        let stages = &m.range_aggregation.log_query.pipeline;
        assert_eq!(stages.len(), 1);
        assert!(matches!(&stages[0], PipelineStage::Json(labels) if labels.is_empty()));
    }

    #[test]
    fn parse_logfmt_bare() {
        let expr = parse(r#"{service_name="web"} | logfmt"#).unwrap();
        let Expr::Log(log) = expr else {
            panic!("expected Log")
        };
        assert_eq!(log.pipeline.len(), 1);
        assert!(matches!(&log.pipeline[0], PipelineStage::Logfmt(labels) if labels.is_empty()));
    }

    #[test]
    fn parse_logfmt_with_labels() {
        let expr = parse(r#"{service_name="web"} | logfmt level, msg"#).unwrap();
        let Expr::Log(log) = expr else {
            panic!("expected Log")
        };
        assert_eq!(log.pipeline.len(), 1);
        match &log.pipeline[0] {
            PipelineStage::Logfmt(labels) => {
                assert_eq!(labels, &["level", "msg"]);
            }
            other => panic!("expected Logfmt, got: {other:?}"),
        }
    }

    #[test]
    fn parse_logfmt_then_line_filter() {
        let expr = parse(r#"{service_name="web"} | logfmt |= "error""#).unwrap();
        let Expr::Log(log) = expr else {
            panic!("expected Log")
        };
        assert_eq!(log.pipeline.len(), 2);
        assert!(matches!(&log.pipeline[0], PipelineStage::Logfmt(labels) if labels.is_empty()));
        assert!(matches!(&log.pipeline[1], PipelineStage::LineFilter(_)));
    }

    #[test]
    fn parse_logfmt_in_metric() {
        let expr = parse(r#"rate({service_name="web"} | logfmt [5m])"#).unwrap();
        let Expr::Metric(m) = expr else {
            panic!("expected Metric")
        };
        let stages = &m.range_aggregation.log_query.pipeline;
        assert_eq!(stages.len(), 1);
        assert!(matches!(&stages[0], PipelineStage::Logfmt(labels) if labels.is_empty()));
    }

    #[test]
    fn parse_json_does_not_match_jsonx() {
        // "jsonx" should NOT be matched as "json" + trailing "x"
        let result = parse(r#"{service_name="web"} | jsonx"#);
        assert!(result.is_err());
    }

    #[test]
    fn parse_logfmt_does_not_match_logfmtx() {
        let result = parse(r#"{service_name="web"} | logfmtx"#);
        assert!(result.is_err());
    }

    #[test]
    fn parse_json_rejects_leading_comma() {
        let result = parse(r#"{service_name="web"} | json , msg"#);
        assert!(result.is_err(), "leading comma should be rejected");
    }

    #[test]
    fn parse_json_rejects_numeric_label() {
        let result = parse(r#"{service_name="web"} | json 123"#);
        assert!(result.is_err(), "numeric label should be rejected");
    }

    #[test]
    fn parse_logfmt_rejects_equals_sign() {
        let result = parse(r#"{service_name="web"} | logfmt ="#);
        assert!(result.is_err(), "bare equals should be rejected");
    }

    #[test]
    fn parse_json_rejects_close_paren_as_label() {
        let result = parse(r#"{service_name="web"} | json )"#);
        assert!(result.is_err(), "close paren should be rejected");
    }

    // ---------------------------------------------------------------
    // regexp / pattern pipeline stages
    // ---------------------------------------------------------------

    #[test]
    fn parse_regexp_basic() {
        let expr = parse(r#"{app="web"} | regexp "(?P<method>\\w+) (?P<path>.+)""#).unwrap();
        match expr {
            Expr::Log(log) => {
                assert_eq!(log.pipeline.len(), 1);
                match &log.pipeline[0] {
                    PipelineStage::Regexp(re) => {
                        assert!(re.contains("method"));
                    }
                    other => panic!("expected Regexp, got: {other:?}"),
                }
            }
            other => panic!("expected Log, got: {other:?}"),
        }
    }

    #[test]
    fn parse_pattern_basic() {
        let expr = parse(r#"{app="web"} | pattern "<_> <method> <path>""#).unwrap();
        match expr {
            Expr::Log(log) => {
                assert_eq!(log.pipeline.len(), 1);
                match &log.pipeline[0] {
                    PipelineStage::Pattern(p) => {
                        assert!(p.contains("<method>"));
                    }
                    other => panic!("expected Pattern, got: {other:?}"),
                }
            }
            other => panic!("expected Log, got: {other:?}"),
        }
    }

    #[test]
    fn parse_regexp_requires_quoted_arg() {
        let result = parse(r#"{app="web"} | regexp"#);
        assert!(result.is_err(), "regexp with no arg should fail");
    }

    #[test]
    fn parse_pattern_requires_quoted_arg() {
        let result = parse(r#"{app="web"} | pattern"#);
        assert!(result.is_err(), "pattern with no arg should fail");
    }

    #[test]
    fn parse_regexp_rejects_unquoted_arg() {
        let result = parse(r#"{app="web"} | regexp method"#);
        assert!(result.is_err(), "regexp with unquoted arg should fail");
    }

    #[test]
    fn parse_regexpx_does_not_match() {
        let result = parse(r#"{app="web"} | regexpx "test""#);
        assert!(result.is_err(), "regexpx should not match regexp");
    }

    #[test]
    fn parse_regexp_with_pipeline_continuation() {
        let expr = parse(r#"{app="web"} | regexp "(?P<level>\\w+)" |= "error""#).unwrap();
        match expr {
            Expr::Log(log) => {
                assert_eq!(log.pipeline.len(), 2);
                assert!(matches!(&log.pipeline[0], PipelineStage::Regexp(_)));
                assert!(matches!(&log.pipeline[1], PipelineStage::LineFilter(_)));
            }
            other => panic!("expected Log, got: {other:?}"),
        }
    }

    #[test]
    fn parse_pattern_with_backtick() {
        let expr = parse(r#"{app="web"} | pattern `<ip> - <_>`"#).unwrap();
        match expr {
            Expr::Log(log) => {
                assert_eq!(log.pipeline.len(), 1);
                match &log.pipeline[0] {
                    PipelineStage::Pattern(p) => {
                        assert!(p.contains("<ip>"));
                    }
                    other => panic!("expected Pattern, got: {other:?}"),
                }
            }
            other => panic!("expected Log, got: {other:?}"),
        }
    }

    // -- Nesting depth bound -------------------------------------------------

    fn nested_inputs(depth: usize) -> Vec<(&'static str, String)> {
        vec![
            (
                "parentheses",
                format!("{}1{}", "(".repeat(depth), ")".repeat(depth)),
            ),
            ("unary minus", format!("{}(1)", "-".repeat(depth))),
            (
                "vector aggregation",
                format!(
                    "{}rate({{a=\"b\"}}[1m]){}",
                    "sum(".repeat(depth),
                    ")".repeat(depth)
                ),
            ),
            ("operator chain", format!("1{}", " + 1".repeat(depth))),
            (
                "label filter parentheses",
                format!(
                    "{{a=\"b\"}} | {}x=\"1\"{}",
                    "(".repeat(depth),
                    ")".repeat(depth)
                ),
            ),
            (
                "label filter chain",
                format!("{{a=\"b\"}} | x=\"1\"{}", " and x=\"1\"".repeat(depth)),
            ),
        ]
    }

    #[test]
    fn nesting_beyond_the_limit_is_a_syntax_error() {
        for (name, query) in nested_inputs(MAX_NESTING_DEPTH + 1) {
            match parse(&query) {
                Err(ParseError::Syntax(msg)) => assert!(
                    msg.contains("maximum depth"),
                    "{name}: unexpected message {msg}"
                ),
                other => panic!("{name}: expected depth error, got {other:?}"),
            }
        }
    }

    #[test]
    fn nesting_well_within_the_limit_still_parses() {
        for (name, query) in nested_inputs(MAX_NESTING_DEPTH / 4) {
            assert!(parse(&query).is_ok(), "{name} rejected: {query}");
        }
    }

    #[test]
    fn rejected_query_releases_its_nesting_budget() {
        for (_, query) in nested_inputs(MAX_NESTING_DEPTH * 4) {
            assert!(parse(&query).is_err());
            assert_eq!(NESTING_DEPTH.with(Cell::get), 0);
        }
        assert!(parse("sum(rate({a=\"b\"}[1m]))").is_ok());
        assert_eq!(NESTING_DEPTH.with(Cell::get), 0);
    }

    /// Hostile depths must fail cleanly rather than overflow. At the limit an
    /// unoptimized build needs about 1 MiB of stack and an optimized one about
    /// 128 KiB, so a 2 MiB thread has headroom here while query threads (8 MiB)
    /// have far more. Accepted trees are also formatted and dropped, since both
    /// recurse over the tree.
    #[test]
    fn hostile_nesting_does_not_overflow_a_small_stack() {
        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(|| {
                for depth in [MAX_NESTING_DEPTH, 10_000, 100_000] {
                    for (_, query) in nested_inputs(depth) {
                        if let Ok(expr) = parse(&query) {
                            let _ = format!("{expr:?}");
                        }
                    }
                }
            })
            .unwrap()
            .join()
            .expect("parser overflowed its stack on nested input");
    }
}
