// Copyright 2025 PackDb Contributors
//
// Licensed under the Apache License, Version 2.0

//! Accept/reject compatibility corpus.
//!
//! Each line of `tests/corpus/parser_cases.jsonl` names a query and whether a
//! LogQL implementation compatible with Grafana Loki accepts or rejects it.
//! Every case must hold: there is no skip or expected-failure list, so a
//! regression cannot be hidden by reclassifying it. `COMPATIBILITY.md` describes
//! the case format and provenance. Set `LOGQL_TEST_FILTER` to run the cases
//! whose id or tags contain that substring.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashSet;
use std::path::Path;

use packdb_logql_parser::parse;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    id: String,
    query: String,
    expect: Expect,
    source: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    error_contains: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum Expect {
    Accept,
    Reject,
}

fn load_cases() -> Vec<Case> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/parser_cases.jsonl");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read corpus {}: {e}", path.display()));
    content
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(i, line)| {
            serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("corpus line {}: {e}\n{line}", i + 1))
        })
        .collect()
}

fn check(case: &Case) -> Result<(), String> {
    match (&case.expect, parse(&case.query)) {
        (Expect::Accept, Ok(_)) => Ok(()),
        (Expect::Accept, Err(e)) => Err(format!("expected accept, got error: {e}")),
        (Expect::Reject, Ok(expr)) => Err(format!("expected reject, parsed as {expr:?}")),
        (Expect::Reject, Err(e)) => match &case.error_contains {
            Some(needle) if !e.to_string().contains(needle.as_str()) => Err(format!(
                "rejected, but error `{e}` does not contain `{needle}`"
            )),
            _ => Ok(()),
        },
    }
}

#[test]
fn corpus_is_well_formed() {
    let cases = load_cases();
    assert!(!cases.is_empty(), "corpus is empty");
    let mut ids = HashSet::new();
    for case in &cases {
        assert!(
            ids.insert(case.id.as_str()),
            "duplicate case id `{}`",
            case.id
        );
        assert!(
            matches!(
                case.source.as_str(),
                "upstream" | "grafana" | "regression" | "unit_test"
            ),
            "case `{}` has unknown source `{}`",
            case.id,
            case.source
        );
        if case.error_contains.is_some() {
            assert!(
                matches!(case.expect, Expect::Reject),
                "case `{}` sets error_contains on an accept case",
                case.id
            );
        }
    }
}

#[test]
fn corpus_cases_hold() {
    let filter = std::env::var("LOGQL_TEST_FILTER").ok();
    let cases = load_cases();
    let mut run = 0usize;
    let mut failures = Vec::new();
    for case in &cases {
        if let Some(f) = &filter
            && !case.id.contains(f.as_str())
            && !case.tags.iter().any(|t| t.contains(f.as_str()))
        {
            continue;
        }
        run += 1;
        if let Err(msg) = check(case) {
            failures.push(format!("{}: {msg}\n    query: {}", case.id, case.query));
        }
    }
    println!(
        "corpus: {run} run, {} failed, {} total",
        failures.len(),
        cases.len()
    );
    assert!(run > 0, "filter {filter:?} selected no cases");
    assert!(
        failures.is_empty(),
        "{} case(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
