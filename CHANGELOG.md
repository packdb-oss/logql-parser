# Changelog

## 0.2.0

- Renamed the package from `packdb-logql-parser` to `logql-parser`. The library
  name is now `logql_parser`. Dependants that used Cargo's `package =` rename can
  drop it. The name `logql-parser` is registered to an unrelated project on
  crates.io, so publishing there needs a distribution name decided at launch.

## 0.1.0

First release as a standalone crate.

- LogQL parser covering the constructs in COMPATIBILITY.md.
- Nesting depth is bounded by `MAX_NESTING_DEPTH`; deeper input returns
  `ParseError::Syntax` instead of overflowing the stack.
- Accept/reject corpus and adversarial input tests.
