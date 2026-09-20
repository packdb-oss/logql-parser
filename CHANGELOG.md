# Changelog

## 0.1.0

First release as a standalone crate.

- LogQL parser covering the constructs in COMPATIBILITY.md.
- Nesting depth is bounded by `MAX_NESTING_DEPTH`; deeper input returns
  `ParseError::Syntax` instead of overflowing the stack.
- Accept/reject corpus and adversarial input tests.
