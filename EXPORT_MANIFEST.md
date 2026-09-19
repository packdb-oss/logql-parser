# Export manifest

This repository was created from the LogQL parser in the PackDB project. Its
history starts with a single reviewed commit; earlier development history is not
included.

## Origin and attribution

- `src/` is the PackDB LogQL parser, written by PackDB contributors and
  licensed under Apache-2.0. Copyright notices are kept in each source file.
- `tests/corpus/parser_cases.jsonl` is the PackDB LogQL accept/reject corpus.
  COMPATIBILITY.md describes its provenance. No file is copied from Grafana Loki.
- `LICENSE` is the Apache License 2.0.
- Policy documents come from the PackDB open source standards kit.

Changes made during extraction: standalone package metadata (published under
the name `packdb-logql-parser`), added or corrected documentation comments,
ten corpus cases renamed so their ids and tags describe this parser's behaviour,
standalone corpus and adversarial tests, a repository CI adapter, and the policy
documents. Parsing behaviour is unchanged.

## Inventory

41 files, excluding this manifest. SHA-256 of each file at the initial
commit:

| File | SHA-256 |
| --- | --- |
| `.github/CODEOWNERS` | `5a4dbd4836353e4a9f151a6be0b4dbb7dedf0e14d66ccf47f8398234e988515b` |
| `.github/ISSUE_TEMPLATE/bug_report.md` | `ce575ab185eab1d319b89ad41b414da89a1e836a93f01295cffbca83b7ec32c4` |
| `.github/ISSUE_TEMPLATE/feature_request.md` | `866952ad8d2940cecabcf978e6c972838cb1b0d6cbaa99241ec026ee0095ca9d` |
| `.github/PULL_REQUEST_TEMPLATE.md` | `8be2381baf9196a5451bbb3ef80f014fc42348c283d5ddeacbfe64263acc4e03` |
| `.github/dependabot.yml` | `6a082e0c22e36e7ae754cb611bc1b8cbb96924355b87480dae1cbb7e57a8bcb2` |
| `.github/workflows/oss-candidate.yml` | `d93178e9729f2b386375163b1a785c12a2ec468c5a017c97ebfa54a731cbaf61` |
| `.github/workflows/oss-ci.yml` | `ee0f75b2204c82746823b09507799cd31170530cc1ce888cb784c4b0447e8a45` |
| `.github/workflows/oss-publish.yml` | `0cd92aa2c328ed307a2cdba07e2c83f0a4c50abbae7cbd4cd172b10a61276477` |
| `.github/workflows/oss-security.yml` | `0881ec1200b77cac6e0571abc37309ac5272f584dd96199c89b4acba03b9267e` |
| `.gitignore` | `4c41fc926fddfaae21cfa16fc3b3a86e406ad7ef4256e3c1490514edb21bc53f` |
| `AI_POLICY.md` | `f512a5c57f95f00664e21a855d4f223b68560097df02d2da9c69aa5c61d7b62c` |
| `CHANGELOG.md` | `569e4578d6f0080bf960fe8bbf4c2b4678f4187511f15500eeb3b98169b28b4c` |
| `CODE_OF_CONDUCT.md` | `3bec071ae7672b006573d09a616bdfdc73dd8c1ca5ea713336fc8176c75a8bb0` |
| `CODING_STANDARDS.md` | `5380828dd9f6f9a8c19dd99d7a58a3073be2125ca6cba7d09562ff3ed8cffddd` |
| `COMPATIBILITY.md` | `9b3e85d8303f60414ada5f09c8a94a3ca3d43c66c6607698818270d3fa666dbf` |
| `CONTRIBUTING.md` | `a77fa5dadbf534914871234b837cf4b5ee147381cb0c6d0e9473e556ca06983e` |
| `Cargo.lock` | `d35858f47f546aba71370c431b6af6c7709e029f6d5b0b9368d3a54d1e82acb1` |
| `Cargo.toml` | `a2397a6a166a3d3c6f5d893c78f918853ea93a577801f8274eef995bf5b62137` |
| `GOVERNANCE.md` | `c6dfa89a3f968e98d35b1e5d9f45e5a81898e08e645a41063b698d32e4ba3a47` |
| `LICENSE` | `e3ec3e590a85899de47ba64e68fbe540a1b3de2c7c1616edddddc5ca8733b2cc` |
| `MAINTAINERS.md` | `5b5519384484354399796ba094d927d2aad42529b3f1bd49ca15b5c83e44cf27` |
| `README.md` | `2ebe634088ca25a7489ad30964add0bfb893a1de68e9b053a1cbb7c23da5e6e7` |
| `RELEASING.md` | `ee51eef191136fc5a7e0acb34553f7ff57a335121b236e654fdeab0ddeb0ffc3` |
| `SECURITY.md` | `d63898d56a6ade84cf748bb4e7448b4292a64e121a9d6e61d409e1a2cc205d6d` |
| `SUPPORT.md` | `32984d3c9934c35a7ca7cd739ab6eb969f8fb6e6521e0ab975f60e2aa4cdfe10` |
| `deny.toml` | `eaa01fd08de47b001fd3ac015ff7643b0ac5f4128000862c1adf82c98dcae64f` |
| `docs/WORKFLOWS.md` | `45cf8e52f0d8d5fe56653e1eae6777e18eeb31daaf625bc21c78df1d6c21f926` |
| `rust-toolchain.toml` | `7dbcd3c9663e0120a47fdc32fa56cdcc3dbc075e3b8a1a8377036a9af1dec578` |
| `scripts/coverage_declaration_only.txt` | `073bcfa0066dbb08cf896ed8c0b8354bb9ac6aad27b11e95d04242217bc12e58` |
| `scripts/coverage_gate.py` | `4f99bdb02eb7cb2612c04e8b5cbc75d080ee3717ab74554f966431bf6f8e7bbf` |
| `scripts/export_clean.py` | `85544f53d1fabb69bf9855f95f6dcc3ba1bde08266c6545cab0e6a018169986a` |
| `scripts/oss-ci.sh` | `4176e3f49bf319de62f171ec3c01273373104fb2c1b49f8bcdb00826c8e8fdef` |
| `src/ast/mod.rs` | `27f6c4e77e361a4a64124e0e2a0306906ed0101b54587a86781b42d11e9e610c` |
| `src/ast/syntax.rs` | `3ba0dccc308a5e5c1b70f005b04d3a0a001e4251fc7c649af834e82164b59a4f` |
| `src/error.rs` | `e3a0aaf3e9b9d04e92b1af8db846057d05793c6b369cd459ad8d2a4fe41fef45` |
| `src/lib.rs` | `5fd5e94dea7949d9568ba14ceb71ca892459ad05291a056c90b518f39d63cba2` |
| `src/parser.rs` | `db14b5ce126e061e1b3736c0554fa75c806e46a4614db81e0a28b30a575c9b96` |
| `tests/adversarial.rs` | `862367b44b968f47debe65d4bf85af92466660fc7ec0fc33747130932c5c4af2` |
| `tests/corpus.rs` | `5769f9059ab8a96bd441f38e3f497dc2b13900d0f6f704e0ce1aaa762331fb21` |
| `tests/corpus/MANIFEST.sha256` | `991faf80e50fb59380cdeac889cdd1cc869628fe060c0ec9c8d0065bea016da0` |
| `tests/corpus/parser_cases.jsonl` | `9aa29868bbb4434fdf26143920933c3e2a5ecd3e89c46cfec08108057c078e27` |
