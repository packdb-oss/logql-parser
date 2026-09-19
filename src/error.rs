// Copyright 2025 PackDb Contributors
//
// Licensed under the Apache License, Version 2.0

//! Error types for the LogQL parser.

/// Errors produced during LogQL parsing.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ParseError {
    /// Syntax error in the LogQL input.
    #[error("parse error: {0}")]
    Syntax(String),

    /// The query uses a LogQL feature that this parser does not yet support.
    #[error("unsupported LogQL feature: {0}")]
    Unsupported(String),

    /// The query is well formed but its operands are incompatible, such as an
    /// ordering operator applied to a string label-filter value.
    #[error("validation error: {0}")]
    Validation(String),
}
