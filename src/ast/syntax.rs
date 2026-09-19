// Copyright 2025 PackDb Contributors
//
// Licensed under the Apache License, Version 2.0

//! Syntax-level AST for LogQL queries.
//!
//! These types represent the direct output of the parser before any
//! semantic analysis or normalization. They map 1:1 to LogQL grammar
//! productions and carry no evaluation semantics.

use std::time::Duration;

/// Top-level LogQL expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A log stream query: `{service_name="web"} |= "error"`
    Log(LogQuery),
    /// A metric query: `rate({service_name="web"}[5m])`
    Metric(MetricQuery),
    /// A scalar literal: `100`, `0.5`, `NaN`, `Inf`
    Scalar(f64),
    /// A constant instant vector: `vector(0)`, `vector(1)`.
    ///
    /// Unlike [`Expr::Scalar`], this carries the *vector* type — it evaluates to
    /// a single sample with no labels at every step, so `<metric> or vector(0)`
    /// (the standard alerting default-to-zero idiom) type-checks where a scalar
    /// would not. The argument is a constant scalar expression (a literal or
    /// arithmetic over literals, e.g. `vector(1+2)`), constant-folded to a value.
    Vector(f64),
    /// A binary expression: arithmetic, comparison, or logical/set
    ///
    /// The `return_bool` flag is only meaningful for comparison operators
    /// (`== != > >= < <=`). When `true`, the comparison returns `1`/`0`
    /// instead of filtering non-matching series.
    BinOp {
        /// The operator.
        op: BinOp,
        /// Left operand.
        lhs: Box<Expr>,
        /// Right operand.
        rhs: Box<Expr>,
        /// Whether a comparison carries the `bool` modifier.
        return_bool: bool,
        /// Optional `on`/`ignoring` vector matching.
        matching: Option<BinOpMatching>,
    },
}

/// A log stream query with optional pipeline stages.
#[derive(Debug, Clone, PartialEq)]
pub struct LogQuery {
    /// Stream selector: `{label="value", ...}`
    pub selector: StreamSelector,
    /// Pipeline stages (line filters, parsers, etc.)
    pub pipeline: Vec<PipelineStage>,
}

/// Stream selector: a set of label matchers enclosed in `{}`.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamSelector {
    /// Label matchers, in source order.
    pub matchers: Vec<LabelMatcher>,
}

/// A single label matcher within a stream selector.
#[derive(Debug, Clone, PartialEq)]
pub struct LabelMatcher {
    /// Label name.
    pub name: String,
    /// Match operator.
    pub op: MatchOp,
    /// Unquoted value, or the regular expression for `=~`/`!~`.
    pub value: String,
}

/// Label match operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchOp {
    /// Exact equality: `=`
    Eq,
    /// Not equal: `!=`
    Neq,
    /// Regex match: `=~`
    Re,
    /// Negative regex match: `!~`
    Nre,
}

/// Pipeline stages applied after stream selection.
#[derive(Debug, Clone, PartialEq)]
pub enum PipelineStage {
    /// Line filter: `|= "text"`, `!= "text"`, `|~ "regex"`, `!~ "regex"`
    LineFilter(LineFilter),
    /// JSON parser: `| json`, `| json label1, label2`
    Json(Vec<String>),
    /// Logfmt parser: `| logfmt`, `| logfmt label1, label2`
    Logfmt(Vec<String>),
    /// Regexp parser: `| regexp "(?P<name>pattern)"`
    Regexp(String),
    /// Pattern parser: `| pattern "<_> <method> <path>"`
    Pattern(String),
    /// Drop labels: `| drop label1, label2`
    Drop(Vec<String>),
    /// Keep labels: `| keep label1, label2`
    Keep(Vec<String>),
    /// Unwrap: `| unwrap label`, `| unwrap duration(label)`, `| unwrap bytes(label)`
    Unwrap(UnwrapExpr),
    /// Label filter predicate over (parser-extracted or stream) labels:
    /// `| status >= 500`, `| level = "error"`, `| duration > 10s and status < 600`.
    LabelFilter(LabelFilterExpr),
    /// Line format: `| line_format "{{.status}} {{.method}}"` — rewrites the log
    /// line from a Go-template-style string referencing the current labels.
    LineFormat(String),
    /// Label format: `| label_format dst=src, out="{{.a}}-{{.b}}"` — renames a
    /// label (`dst=src`) or sets one from a template (`dst="..."`). Multiple
    /// comma-separated operations apply against the same pre-stage label
    /// snapshot, so `a=b, b=a` swaps.
    LabelFormat(Vec<LabelFormatItem>),
}

/// A single `dst = <source>` operation within a `| label_format` stage.
#[derive(Debug, Clone, PartialEq)]
pub struct LabelFormatItem {
    /// Destination label name to set.
    pub dst: String,
    /// How the destination value is produced.
    pub value: LabelFormatValue,
}

/// The right-hand side of a `label_format` operation.
#[derive(Debug, Clone, PartialEq)]
pub enum LabelFormatValue {
    /// Bareword form `dst=src`: rename label `src` into `dst`.
    Rename(String),
    /// Quoted form `dst="{{.a}} {{.b}}"`: set `dst` to the rendered template.
    Template(String),
}

/// A label-filter predicate expression: a single comparison or a boolean
/// combination. Predicates evaluate **left-to-right with equal precedence** for
/// `and`/`or` (comma and bare whitespace are synonyms for `and`); only
/// parentheses override grouping — so `a or b and c` is `((a or b) and c)`,
/// matching Loki. See [`LabelFilterComparison`].
#[derive(Debug, Clone, PartialEq)]
pub enum LabelFilterExpr {
    /// A leaf comparison: `status >= 500`.
    Comparison(LabelFilterComparison),
    /// A boolean combination: `<lhs> and <rhs>` / `<lhs> or <rhs>`.
    Binary {
        /// The combinator.
        op: LabelFilterBoolOp,
        /// Left predicate.
        lhs: Box<LabelFilterExpr>,
        /// Right predicate.
        rhs: Box<LabelFilterExpr>,
    },
}

/// Boolean combinators within a label-filter expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelFilterBoolOp {
    /// `and` (also spelled `,`)
    And,
    /// `or`
    Or,
}

/// A single label-filter comparison: `<label> <op> <value>`.
#[derive(Debug, Clone, PartialEq)]
pub struct LabelFilterComparison {
    /// Label name.
    pub name: String,
    /// Comparison operator.
    pub op: LabelFilterOp,
    /// Typed comparison value.
    pub value: LabelFilterValue,
}

/// Label-filter comparison operators. The string forms (`=`, `!=`, `=~`, `!~`)
/// require a [`LabelFilterValue::String`]; the ordering forms (`>`, `>=`, `<`,
/// `<=`) require a numeric value; `=`/`==` and `!=` accept either (string
/// equality or numeric equality depending on the value type).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelFilterOp {
    /// `=` / `==`
    Eq,
    /// `!=`
    Neq,
    /// `=~` (regex match)
    Re,
    /// `!~` (regex non-match)
    Nre,
    /// `>`
    Gt,
    /// `>=`
    Gte,
    /// `<`
    Lt,
    /// `<=`
    Lte,
}

impl LabelFilterOp {
    /// Ordering operators (`>`, `>=`, `<`, `<=`) — numeric value only.
    pub fn is_ordering(self) -> bool {
        matches!(self, Self::Gt | Self::Gte | Self::Lt | Self::Lte)
    }

    /// Regex operators (`=~`, `!~`) — string value only.
    pub fn is_regex(self) -> bool {
        matches!(self, Self::Re | Self::Nre)
    }
}

/// The typed right-hand side of a label-filter comparison.
#[derive(Debug, Clone, PartialEq)]
pub enum LabelFilterValue {
    /// Quoted string literal: `level = "error"`.
    String(String),
    /// Bare numeric literal: `status >= 500`, `ratio < 0.5`.
    Number(f64),
    /// Duration literal: `duration > 10s`.
    Duration(Duration),
    /// Bytes literal: `size > 1MB`.
    Bytes(u64),
}

/// An unwrap expression that extracts a numeric value from a label.
#[derive(Debug, Clone, PartialEq)]
pub struct UnwrapExpr {
    /// The label name to extract the numeric value from.
    pub label: String,
    /// Optional conversion function applied to the label value.
    pub conversion: Option<UnwrapConversion>,
}

/// Conversion functions for unwrap expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnwrapConversion {
    /// Parse as a Go duration string: `duration(label)`
    Duration,
    /// Parse as a bytes string: `bytes(label)`
    Bytes,
}

/// A line filter applied to log line content.
#[derive(Debug, Clone, PartialEq)]
pub struct LineFilter {
    /// Filter operator.
    pub op: LineFilterOp,
    /// Unquoted filter text or regular expression.
    pub value: String,
}

/// Line filter operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineFilterOp {
    /// Contains: `|=`
    Contains,
    /// Not contains: `!=`
    NotContains,
    /// Regex match: `|~`
    Regex,
    /// Negative regex: `!~`
    NotRegex,
}

/// A metric aggregation query over a log range.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricQuery {
    /// Optional outer vector aggregation: `sum by (label) (...)`
    pub vector_aggregation: Option<VectorAggregation>,
    /// The range aggregation expression
    pub range_aggregation: RangeAggregation,
}

/// Range aggregation applied to a log stream over a time window.
#[derive(Debug, Clone, PartialEq)]
pub struct RangeAggregation {
    /// The aggregation function
    pub function: RangeAggregationFn,
    /// The inner log query
    pub log_query: LogQuery,
    /// The time range window (e.g., 5m)
    pub range: Duration,
    /// Optional offset modifier: `offset 1h` shifts the evaluation window back
    pub offset: Option<Duration>,
    /// The leading scalar parameter, e.g. the quantile `φ` in
    /// `quantile_over_time(0.99, {…} | unwrap latency [5m])`. `None` for every
    /// other range aggregation function.
    pub parameter: Option<f64>,
}

/// Supported range aggregation functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeAggregationFn {
    /// `rate`
    Rate,
    /// `count_over_time`
    CountOverTime,
    /// `bytes_rate`
    BytesRate,
    /// `bytes_over_time`
    BytesOverTime,
    /// `sum_over_time`
    SumOverTime,
    /// `avg_over_time`
    AvgOverTime,
    /// `min_over_time`
    MinOverTime,
    /// `max_over_time`
    MaxOverTime,
    /// `stdvar_over_time`
    StdvarOverTime,
    /// `stddev_over_time`
    StddevOverTime,
    /// `quantile_over_time`
    QuantileOverTime,
    /// `first_over_time`
    FirstOverTime,
    /// `last_over_time`
    LastOverTime,
    /// `absent_over_time`
    AbsentOverTime,
}

/// Optional vector aggregation wrapping a range aggregation.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorAggregation {
    /// The aggregation function.
    pub function: VectorAggregationFn,
    /// Optional `by`/`without` clause.
    pub grouping: Option<Grouping>,
    /// For topk/bottomk, the k parameter
    pub parameter: Option<u64>,
}

/// Supported vector aggregation functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorAggregationFn {
    /// `sum`
    Sum,
    /// `avg`
    Avg,
    /// `min`
    Min,
    /// `max`
    Max,
    /// `count`
    Count,
    /// `topk`
    Topk,
    /// `bottomk`
    Bottomk,
}

/// Grouping clause: `by (label1, label2)` or `without (label1)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Grouping {
    /// Whether `labels` are kept (`by`) or removed (`without`).
    pub kind: GroupingKind,
    /// Label names in the clause.
    pub labels: Vec<String>,
}

/// Whether grouping is by inclusion or exclusion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupingKind {
    /// `by (...)`: keep only the listed labels.
    By,
    /// `without (...)`: drop the listed labels.
    Without,
}

/// Binary operators for LogQL expressions.
///
/// Covers arithmetic, comparison, and logical/set operators.
/// Precedence (lowest to highest):
/// `or` < `and`/`unless` < comparisons < `+`/`-` < `*`/`/`/`%` < `^`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    // Arithmetic.
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Mod,
    /// `^`
    Pow,
    // Comparisons.
    /// `==`
    Eq,
    /// `!=`
    Neq,
    /// `>`
    Gt,
    /// `>=`
    Gte,
    /// `<`
    Lt,
    /// `<=`
    Lte,
    // Logical and set operators.
    /// `and`
    And,
    /// `or`
    Or,
    /// `unless`
    Unless,
}

impl BinOp {
    /// Returns `true` for comparison operators that accept the `bool` modifier.
    pub fn is_comparison(self) -> bool {
        matches!(
            self,
            Self::Eq | Self::Neq | Self::Gt | Self::Gte | Self::Lt | Self::Lte
        )
    }

    /// Returns `true` for logical/set operators (`and`, `or`, `unless`).
    pub fn is_logical(self) -> bool {
        matches!(self, Self::And | Self::Or | Self::Unless)
    }
}

/// Label matching mode for binary operator matching modifiers.
#[derive(Debug, Clone, PartialEq)]
pub enum BinOpMatchMode {
    /// `on(label1, label2, ...)` — match only on the specified labels.
    On(Vec<String>),
    /// `ignoring(label1, label2, ...)` — match on all labels except these.
    Ignoring(Vec<String>),
}

/// Group modifier for many-to-one or one-to-many matching.
#[derive(Debug, Clone, PartialEq)]
pub enum BinOpGroupModifier {
    /// `group_left(label1, ...)` — many-to-one, include listed labels from RHS.
    Left(Vec<String>),
    /// `group_right(label1, ...)` — one-to-many, include listed labels from LHS.
    Right(Vec<String>),
}

/// Complete binary operator matching configuration (parser-level).
#[derive(Debug, Clone, PartialEq)]
pub struct BinOpMatching {
    /// Label matching mode: `on(...)` or `ignoring(...)`.
    pub mode: BinOpMatchMode,
    /// Optional cardinality modifier: `group_left(...)` or `group_right(...)`.
    pub group: Option<BinOpGroupModifier>,
}
