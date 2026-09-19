# Code standards

Use Rust 2024 with an explicit MSRV (initially 1.94.1). Changes to MSRV, public
features, dependencies, wire contracts or storage formats require release notes.
Prefer small domain modules and minimal dependencies. Parsers own syntax/AST/errors;
engines own planning/execution; CLIs own presentation over clients. Libraries must
not depend on applications. Test complete feature combinations deliberately.

Use rustfmt, snake_case modules/functions and CamelCase types. Production paths
use Result/Option, checked conversions and bounds checks. Deny Clippy's unwrap_used,
expect_used, indexing_slicing, panic and unreachable lints in production; narrow
exceptions need a documented invariant and review. Test-only allowances are scoped
to tests. Do not hide dead code or compiler warnings with broad allowances.

Document public APIs, errors, examples and feature behavior. Avoid needless public
items. Preserve semantic version compatibility and add migration notes for breaks.
Parsers must return structured errors on malformed input; test deep nesting, large
inputs and boundary cases. No network or engine dependency in parser runtime code.
CLI stdout is data; diagnostics go to stderr. Preserve exit codes and machine output
schemas; redact credentials and tenant data. Destructive operations need explicit
user intent. Library telemetry must degrade gracefully rather than panic.

Required CI runs formatting, Clippy with warnings denied, unit/integration/doc tests,
MSRV/stable and declared feature checks, package verification and dependency review.
Modified executable Rust files require at least 80% line coverage **per file**.
`scripts/coverage_gate.py` enforces this: every changed executable file under
`src/` must reach the threshold, a changed file with no coverage record fails, and
`--self-test` proves each rule still fires. The only exception is
`scripts/coverage_declaration_only.txt`: files with no executable statements, each
with a reason, re-verified on every run. There is no aggregate substitution or
silent missing data.

Move tests with code and retain compatibility corpora and provenance. Storage/query
changes cover metrics, logs and traces where applicable. Preserve documented WAL
recovery compatibility; destructive format changes require explicit migration or
export/restore instructions. Never silently wipe user data to pass a test.

No secrets, customer datasets or internal operational evidence in commits/fixtures.
Use synthetic data and loopback examples. Review dependency licenses and advisories.
Preserve upstream attribution; do not port source under an incompatible license.
Use neutral branches, concise imperative commits, and focused PRs with evidence.
Never bypass hooks/checks to merge. Do not include assistant attribution in commits.

Comments explain why: invariants, constraints, tradeoffs and surprising behavior.
Remove comments that narrate obvious statements, repeat signatures, report migration
status or cite private tickets/design identifiers. Rewrite useful rationale in
self-contained language. Public API docs still explain contracts, inputs, errors and
examples; license headers, safety justifications and machine-readable directives must
remain. Apply this standard before committing extracted code, not after publication.
