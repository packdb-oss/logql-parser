// Copyright 2025 PackDB Contributors
//
// Licensed under the Apache License, Version 2.0

//! Decoded values must retain literal meaning across selectors and pipeline stages.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use packdb_logql_parser::{
    ast::syntax::{Expr, MatchOp, PipelineStage},
    parse,
};

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[test]
fn selector_operators_are_not_read_from_values() {
    for (operator, expected) in [
        ("=", MatchOp::Eq),
        ("!=", MatchOp::Neq),
        ("=~", MatchOp::Re),
        ("!~", MatchOp::Nre),
    ] {
        for value in [
            "plain",
            "quote\"x",
            "backslash\\x",
            "both\"\\x",
            "a!=b",
            "a=~b",
            "a!~b",
            "a=b",
            "a,b",
            "日本語",
        ] {
            let query = format!("{{app{operator}{}}}", quote(value));
            let Expr::Log(log) = parse(&query).unwrap_or_else(|e| panic!("{query}: {e}")) else {
                panic!("not a log query")
            };
            let matcher = log.selector.matchers.first().unwrap();
            assert_eq!(matcher.name, "app", "{query}");
            assert_eq!(matcher.op, expected, "{query}");
            assert_eq!(matcher.value, value, "{query}");
        }
    }
}

#[test]
fn line_filters_decode_quoted_values_once() {
    for op in ["|=", "!=", "|~", "!~"] {
        for value in ["quote\"x", "backslash\\x", "literal\\n", "Error", "a!=b"] {
            let query = format!("{{app=\"web\"}} {op} {}", quote(value));
            let Expr::Log(log) = parse(&query).unwrap() else {
                panic!("not a log query")
            };
            let PipelineStage::LineFilter(filter) = log.pipeline.first().unwrap() else {
                panic!("not a line filter")
            };
            assert_eq!(filter.value, value, "{query}");
        }
    }
}

#[test]
fn quoted_strings_decode_go_escapes_and_utf8_bytes() {
    for (literal, expected) in [
        (r#""\a\b\f\n\r\t\v\\\"""#, "\u{7}\u{8}\u{c}\n\r\t\u{b}\\\""),
        (r#""\x41\101\u65e5\U0001F642""#, "AA日🙂"),
        (r#""\xe6\x97\xa5""#, "日"),
    ] {
        let query = format!("{{app={literal}}} |= {literal}");
        let Expr::Log(log) = parse(&query).unwrap_or_else(|e| panic!("{query}: {e}")) else {
            panic!("not a log query")
        };
        assert_eq!(log.selector.matchers.first().unwrap().value, expected);
        let PipelineStage::LineFilter(filter) = log.pipeline.first().unwrap() else {
            panic!("not a line filter")
        };
        assert_eq!(filter.value, expected);
    }
}

#[test]
fn raw_pipeline_strings_keep_backslashes_and_remove_carriage_returns() {
    let Expr::Log(log) = parse("{app=\"web\"} |= `literal\\n\r\nnext`").unwrap() else {
        panic!("not a log query")
    };
    let PipelineStage::LineFilter(filter) = log.pipeline.first().unwrap() else {
        panic!("not a line filter")
    };
    assert_eq!(filter.value, "literal\\n\nnext");
}

#[test]
fn malformed_interpreted_strings_are_rejected() {
    for literal in [
        r#""\q""#,
        r#""\x0""#,
        r#""\xGG""#,
        r#""\uD800""#,
        r#""\U00110000""#,
        r#""\400""#,
        r#""\12""#,
        r#""\xff""#,
        r#""\xc3""#,
        r#""\'""#,
        "\"a\nb\"",
        "\"a\"junk\"b\"",
    ] {
        for query in [
            format!("{{app={literal}}}"),
            format!("{{app=\"web\"}} |= {literal}"),
        ] {
            assert!(parse(&query).is_err(), "accepted {query}");
        }
    }
}

#[test]
fn other_quoted_stages_share_decoding() {
    for stage in [
        r#"regexp "(?P<x>\\w+)""#,
        r#"pattern "quote\" <x>""#,
        r#"line_format "quote\" {{.x}}""#,
    ] {
        let Expr::Log(log) = parse(&format!("{{app=\"web\"}} | {stage}")).unwrap() else {
            panic!("not a log query")
        };
        match log.pipeline.first().unwrap() {
            PipelineStage::Regexp(value) => assert_eq!(value, r"(?P<x>\w+)"),
            PipelineStage::Pattern(value) => assert_eq!(value, "quote\" <x>"),
            PipelineStage::LineFormat(value) => assert_eq!(value, "quote\" {{.x}}"),
            other => panic!("unexpected {other:?}"),
        }
    }
}
