# LogQL compatibility

Target: the LogQL syntax of Grafana Loki v3.4. The parser accepts the subset
below and rejects everything else with an error; it never guesses at an
unrecognised construct. "Rejected" means `parse` returns an error.

## Supported

| Area | Constructs |
| --- | --- |
| Stream selector | `{label="v"}` with `=`, `!=`, `=~`, `!~`; several matchers |
| Line filters | `\|=`, `!=`, `\|~`, `!~` with double-quoted or backtick strings; chained filters |
| Parser stages | `\| json`, `\| logfmt` (optionally with a label list), `\| regexp "…"`, `\| pattern "…"` |
| Label stages | `\| keep a, b`, `\| drop a, b`, `\| line_format "…"`, `\| label_format dst=src, dst="…"` |
| Label filters | `\| status >= 500`, string, number, duration (`10s`) and bytes (`1MB`) values; `and`, `or`, `,` and whitespace as `and`; parentheses. `and`/`or` have equal precedence and group left to right, as in Loki |
| Unwrap | `\| unwrap x`, `\| unwrap duration(x)`, `\| unwrap bytes(x)` |
| Range aggregations | `rate`, `count_over_time`, `bytes_rate`, `bytes_over_time`, `sum_over_time`, `avg_over_time`, `min_over_time`, `max_over_time`, `stdvar_over_time`, `stddev_over_time`, `quantile_over_time`, `first_over_time`, `last_over_time`, `absent_over_time`; `offset` inside the call: `rate({…}[5m] offset 1h)` |
| Vector aggregations | `sum`, `avg`, `min`, `max`, `count`, `topk`, `bottomk`; `by (…)`/`without (…)` before or after the arguments |
| Binary operators | `+ - * / % ^`; `== != > >= < <=` with optional `bool`; `and`, `or`, `unless`; `on(…)`/`ignoring(…)` with `group_left(…)`/`group_right(…)` |
| Literals | numbers, `NaN`, `Inf`, unary minus, `vector(<constant expression>)` |

## Accepted here, rejected by Loki's validation

The parser checks syntax only. Loki also rejects the following well-formed
selectors when it validates a query; callers that need Loki's behaviour must
check for them. Corpus cases tagged `loki_validation` pin this behaviour.

- Empty selectors: `{}`, including inside `rate({}[5m])`.
- Selectors with no positive matcher: `{a!="x"}`, `{a!~"x.*"}`.
- Selectors whose matchers all match the empty string: `{a=""}`, `{a=~".*"}`,
  `{a=~"x|"}`.

## Not supported

| Construct | Result |
| --- | --- |
| `sort`, `sort_desc`, `label_replace`, `count_values`, `stddev`, `stdvar`, `approx_topk`, `rate_counter` and other functions not listed above | `ParseError::Unsupported` |
| `\| unpack`, `\| decolorize`, `\| distinct` | syntax error |
| `\| json a="path"` and `\| logfmt` flags such as `--strict` | syntax error |
| `ip("…")` in line or label filters | syntax error |
| Line filter alternatives: `\|= "a" or "b"` | syntax error |
| Subqueries: `[5m:1m]` | syntax error |
| `offset` after a range aggregation call | syntax error |

Queries nested more deeply than `MAX_NESTING_DEPTH` (128 units; each
parenthesised or aggregated level uses two) are rejected so that hostile input
cannot exhaust the stack.

## Test corpus

`tests/corpus/parser_cases.jsonl` holds accept/reject cases, one JSON object per
line: `id`, `query`, `expect` (`accept` or `reject`), `source`, `tags` and an
optional `error_contains` substring for rejections. Every case must hold; there
is no skip or expected-failure list. `tests/corpus/MANIFEST.sha256` records the
corpus checksum.

`source` records why a case exists: `upstream` cases exercise syntax documented
for Loki, `grafana` cases have the shape of queries built in Grafana's log
explorer, `regression` cases pin fixed defects, and `unit_test` cases cover
parser features directly. All cases were written for this project. None is
copied from Loki's source or test files: a check against Loki v3.4.2's
`pkg/logql/syntax` tests found no case whose query text appears there.

`tests/adversarial.rs` feeds truncated, mutated, non-ASCII and deeply nested
input and requires an `Ok` or `Err` result without panicking or overflowing.
