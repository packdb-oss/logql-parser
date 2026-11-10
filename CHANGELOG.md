# Changelog

## Unreleased

- Decode double-quoted selector and pipeline values once, preserving raw-string
  backslashes and rejecting malformed escapes.
- Detect selector operators before the value so operator-like text inside a
  service or label value cannot change the matcher.
- Reject non-UTF-8 byte-escape results because AST values are UTF-8 strings.

## 0.1.1

- Replace the LICENSE file with the verbatim Apache License 2.0 text. Earlier
  releases shipped an altered text: the Trademarks clause (section 6) was
  missing, sections 7 to 9 were renumbered 6 to 8, the copyright grant wording
  differed, and section 9 was retitled. The crate's licensing (Apache-2.0, as
  declared in `Cargo.toml` and the source headers) is unchanged. Earlier
  release archives keep their original text; this release supersedes them.

## 0.1.0

First release as a standalone crate.

- LogQL parser covering the constructs in COMPATIBILITY.md.
- Nesting depth is bounded by `MAX_NESTING_DEPTH`; deeper input returns
  `ParseError::Syntax` instead of overflowing the stack.
- Accept/reject corpus and adversarial input tests.
