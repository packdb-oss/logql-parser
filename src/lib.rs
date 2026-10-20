// Copyright 2025 PackDB Contributors
//
// Licensed under the Apache License, Version 2.0

//! A dependency-light parser for [LogQL], the log query language used by
//! Grafana Loki.
//!
//! [`parse`] turns a query string into the syntax tree in [`ast::syntax`]:
//! stream selectors, line filters, pipeline stages, range and vector
//! aggregations, binary operations and scalar literals. Constructs the parser
//! recognises but does not implement are rejected with
//! [`ParseError::Unsupported`] rather than being silently misparsed. See
//! `COMPATIBILITY.md` in the repository for the supported grammar.
//!
//! The crate only parses. It does not evaluate queries or depend on a query
//! engine.
//!
//! ```
//! use packdb_logql_parser::{ast::syntax::Expr, parse};
//!
//! let expr = parse(r#"{service_name="web"} |= "error""#).unwrap();
//! assert!(matches!(expr, Expr::Log(_)));
//!
//! let err = parse(r#"{service_name="web"} |="#).unwrap_err();
//! println!("{err}");
//! ```
//!
//! [LogQL]: https://grafana.com/docs/loki/latest/query/

#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![cfg_attr(not(test), deny(clippy::indexing_slicing))]
#![cfg_attr(not(test), deny(clippy::panic))]
#![cfg_attr(not(test), deny(clippy::unreachable))]

pub mod ast;
pub mod error;
mod parser;

pub use error::ParseError;
pub use parser::{MAX_NESTING_DEPTH, parse};
