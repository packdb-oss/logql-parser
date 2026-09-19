# packdb-logql-parser

A parser for [LogQL](https://grafana.com/docs/loki/latest/query/), the log query
language used by Grafana Loki. It turns a query string into a typed syntax tree
and returns structured errors for malformed or unsupported input. It does not
evaluate queries, and its only runtime dependency is `thiserror`.

The parser was written for PackDB's Loki-compatible query API and is maintained
as a standalone library.

## Usage

The crate is not yet published to crates.io. Until then, depend on a tagged
release of this repository:

```toml
[dependencies]
packdb-logql-parser = { git = "https://github.com/packdb-oss/logql-parser", tag = "v0.1.0" }
```

```rust
use packdb_logql_parser::{ast::syntax::Expr, parse, ParseError};

match parse(r#"sum by (status) (rate({app="web"} |= "error" [5m]))"#) {
    Ok(Expr::Metric(query)) => println!("{:?}", query.range_aggregation.function),
    Ok(other) => println!("{other:?}"),
    Err(ParseError::Unsupported(what)) => eprintln!("not supported: {what}"),
    Err(e) => eprintln!("{e}"),
}
```

`parse` returns:

- `ParseError::Syntax` for malformed input, including queries nested deeper
  than `MAX_NESTING_DEPTH`;
- `ParseError::Unsupported` for recognised LogQL functions that this parser does
  not implement;
- `ParseError::Validation` for input that is well formed but invalid.

## Supported LogQL

[COMPATIBILITY.md](COMPATIBILITY.md) lists supported and unsupported constructs.
The target is Loki v3.4 syntax. In summary: stream selectors; line filters;
`json`, `logfmt`, `regexp` and `pattern` parser stages; label filters; `keep`,
`drop`, `unwrap`, `line_format` and `label_format`; 14 range aggregations;
`sum`, `avg`, `min`, `max`, `count`, `topk` and `bottomk` with `by`/`without`;
arithmetic, comparison and set operators with vector matching; `offset`; scalar
literals; and `vector()`.

## Requirements

Rust 1.94.1 or later (edition 2024). The crate uses no `unsafe` code and has no
platform-specific code.

## Development

```sh
scripts/oss-ci.sh checks         # format, lint, tests, docs, policy and package checks
scripts/oss-ci.sh compatibility  # accept/reject corpus and adversarial inputs
scripts/oss-ci.sh coverage       # per-file line coverage (needs cargo-llvm-cov)
```

Set `OSS_CHECK_SCOPE=full` when running locally. See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Apache License 2.0; see [LICENSE](LICENSE). Contributions are accepted under the
same license.
