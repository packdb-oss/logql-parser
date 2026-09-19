#!/usr/bin/env bash
# Copyright 2025 PackDb Contributors
#
# Licensed under the Apache License, Version 2.0
#
# Repository adapter for the required workflows (see docs/WORKFLOWS.md).
#
#   scripts/oss-ci.sh checks | coverage | compatibility | security | candidate | publish
#
# Inputs arrive as environment variables and are validated, never evaluated.
# Every subcommand fails closed: a missing tool, base commit or report is a
# failure, not a skip.

set -euo pipefail

readonly MSRV="1.94.1"
readonly PACKAGE="packdb-logql-parser"
# Pinned tool versions. Changing one is a reviewed change to this file.
readonly CARGO_LLVM_COV_VERSION="0.9.1"
readonly CARGO_AUDIT_VERSION="0.22.2"
readonly CARGO_DENY_VERSION="0.20.2"
readonly GITLEAKS_VERSION="v8.30.1"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

die() {
    echo "oss-ci: $*" >&2
    exit 1
}

log() {
    echo "==> $*"
}

is_sha() {
    [[ "$1" =~ ^[0-9a-f]{40}$ ]] && [[ "$1" != 0000000000000000000000000000000000000000 ]]
}

scope() {
    local s="${OSS_CHECK_SCOPE:-}"
    case "$s" in
        affected | full) echo "$s" ;;
        *) die "OSS_CHECK_SCOPE must be 'affected' or 'full', got '${s}'" ;;
    esac
}

# Resolve a commit, fetching it if the checkout lacks it. Never substitutes
# another commit.
resolve_commit() {
    local name="$1" sha="$2"
    is_sha "$sha" || die "$name is not a usable commit SHA: '${sha}'"
    if ! git cat-file -e "${sha}^{commit}" 2>/dev/null; then
        git fetch --no-tags --depth=1 origin "$sha" >/dev/null 2>&1 ||
            die "$name $sha is not available and could not be fetched"
    fi
    git cat-file -e "${sha}^{commit}" 2>/dev/null || die "$name $sha is not a commit"
}

# Writes the changed paths for the validated range to $1. Affected scope uses
# the merge base for PRs and merge groups and a direct tree diff for pushes.
changed_files() {
    local out="$1"
    if [[ "$(scope)" == full ]]; then
        git ls-files >"$out"
        return
    fi
    local base="${OSS_BASE_SHA:-}" head="${OSS_HEAD_SHA:-}"
    resolve_commit OSS_BASE_SHA "$base"
    resolve_commit OSS_HEAD_SHA "$head"
    local from="$base"
    if [[ "${GITHUB_EVENT_NAME:-}" != push ]]; then
        from="$(git merge-base "$base" "$head")" || die "no merge base for $base..$head"
    fi
    git diff --name-only --no-renames "$from" "$head" >"$out"
    echo "tested checkout: $(git rev-parse HEAD); range: ${from}..${head}"
}

check_gate_input() {
    local expected="$1"
    if [[ -n "${OSS_GATE:-}" && "$OSS_GATE" != "$expected" ]]; then
        die "OSS_GATE='${OSS_GATE}' disagrees with subcommand '${expected}'"
    fi
}

require_tool() {
    command -v "$1" >/dev/null 2>&1 || die "required tool '$1' is not installed"
}

install_cargo_tool() {
    local crate="$1" version="$2" binary="$3"
    if command -v "$binary" >/dev/null 2>&1 && cargo "${binary#cargo-}" --version 2>/dev/null | grep -qF " $version"; then
        return
    fi
    [[ -n "${CI:-}" ]] || die "$crate $version is required; install it with: cargo install --locked $crate@$version"
    log "installing $crate $version"
    cargo install --locked "$crate@$version"
}

# --- checks ------------------------------------------------------------------

check_policy() {
    log "policy files and contacts"
    local f
    for f in LICENSE README.md SECURITY.md CODE_OF_CONDUCT.md CONTRIBUTING.md GOVERNANCE.md \
        SUPPORT.md AI_POLICY.md CODING_STANDARDS.md RELEASING.md MAINTAINERS.md \
        COMPATIBILITY.md CHANGELOG.md EXPORT_MANIFEST.md .github/CODEOWNERS; do
        [[ -s "$f" ]] || die "missing or empty policy file: $f"
    done
    if grep -rnE '\{\{[A-Z_]+\}\}' --include='*.md' . ; then
        die "unfilled template placeholder"
    fi
    grep -q 'security@packdb.io' SECURITY.md || die "SECURITY.md has no private contact"
    grep -q 'conduct@packdb.io' CODE_OF_CONDUCT.md || die "CODE_OF_CONDUCT.md has no contact"
    grep -q 'Apache License' LICENSE || die "LICENSE is not the Apache License"
}

check_action_pins() {
    log "workflow action pins"
    local bad
    bad="$(grep -rhoE '^\s*(- )?uses:\s*[^ #]+' .github/workflows |
        sed -E 's/^\s*(- )?uses:\s*//' | grep -vE '^[^@]+@[0-9a-f]{40}$' || true)"
    [[ -z "$bad" ]] || die "actions not pinned to a full commit SHA: $bad"
    if grep -rnE 'pull_request_target|workflow_run' .github/workflows; then
        die "privileged workflow trigger is not allowed"
    fi
}

check_package() {
    log "package manifest and contents"
    grep -qE '^publish = false$' Cargo.toml ||
        die "publish must stay false until a reviewed release allowlist change"
    local listed
    listed="$(cargo package --list --locked --allow-dirty -p "$PACKAGE" 2>/dev/null)" ||
        die "cargo package --list failed"
    local path
    while IFS= read -r path; do
        case "$path" in
            .cargo_vcs_info.json | Cargo.toml | Cargo.toml.orig | Cargo.lock | README.md | LICENSE | \
                COMPATIBILITY.md | CHANGELOG.md | src/*.rs | tests/*.rs | tests/corpus/*) ;;
            *) die "unexpected file in package: $path" ;;
        esac
    done <<<"$listed"
    for path in LICENSE README.md src/lib.rs; do
        grep -qx "$path" <<<"$listed" || die "package is missing $path"
    done
}

check_corpus_manifest() {
    log "corpus checksums"
    (cd tests/corpus && shasum -a 256 -c MANIFEST.sha256) || die "corpus does not match MANIFEST.sha256"
}

cargo_suite() {
    local toolchain="$1"
    log "tests on $toolchain"
    cargo "+$toolchain" test --locked --all-targets --no-default-features
    cargo "+$toolchain" test --locked --all-targets --all-features
    cargo "+$toolchain" test --locked --doc
}

cmd_checks() {
    check_gate_input checks
    local s
    s="$(scope)"
    local changed
    changed="$(mktemp)"
    changed_files "$changed"
    check_policy
    check_action_pins
    python3 scripts/export_clean.py
    python3 scripts/coverage_gate.py --self-test
    check_corpus_manifest
    log "format"
    cargo fmt --all -- --check
    log "clippy"
    RUSTFLAGS="-D warnings" cargo clippy --locked --all-targets --all-features -- -D warnings
    log "docs"
    RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
    cargo_suite "$MSRV"
    if [[ "$s" == full ]] || grep -qE '^(rust-toolchain\.toml|Cargo\.(toml|lock))$' "$changed"; then
        rustup toolchain install stable --profile minimal >/dev/null
        cargo_suite stable
    fi
    check_package
}

# --- coverage ----------------------------------------------------------------

cmd_coverage() {
    check_gate_input coverage
    scope >/dev/null
    python3 scripts/coverage_gate.py --self-test
    local changed lcov
    changed="$(mktemp)"
    lcov="$(mktemp)"
    changed_files "$changed"
    rustup component add llvm-tools-preview >/dev/null 2>&1 || rustup component add llvm-tools >/dev/null
    install_cargo_tool cargo-llvm-cov "$CARGO_LLVM_COV_VERSION" cargo-llvm-cov
    cargo llvm-cov --locked --all-features --all-targets --lcov --output-path "$lcov"
    python3 scripts/coverage_gate.py --lcov "$lcov" --changed-files "$changed" --repo-root "$ROOT"
}

# --- compatibility -----------------------------------------------------------

cmd_compatibility() {
    check_gate_input compatibility
    scope >/dev/null
    check_corpus_manifest
    log "LogQL accept/reject corpus"
    cargo test --locked --test corpus -- --nocapture
    log "malformed and adversarial input"
    cargo test --locked --test adversarial
}

# --- security ----------------------------------------------------------------

cmd_security() {
    local s
    s="$(scope)"
    install_cargo_tool cargo-audit "$CARGO_AUDIT_VERSION" cargo-audit
    install_cargo_tool cargo-deny "$CARGO_DENY_VERSION" cargo-deny
    log "RustSec advisories (fresh database)"
    cargo audit --deny warnings
    log "dependency licenses, bans and sources"
    cargo deny --locked --all-features check licenses bans sources
    if ! command -v gitleaks >/dev/null 2>&1; then
        [[ -n "${CI:-}" ]] || die "gitleaks $GITLEAKS_VERSION is required"
        require_tool go
        log "installing gitleaks $GITLEAKS_VERSION"
        GOBIN="$HOME/.local/bin" go install "github.com/zricethezav/gitleaks/v8@${GITLEAKS_VERSION}"
        export PATH="$HOME/.local/bin:$PATH"
    fi
    # A `go install` build reports no version of its own; read the module
    # version recorded in the binary instead.
    local installed
    installed="$(go version -m "$(command -v gitleaks)" 2>/dev/null | awk '$1 == "mod" { print $3 }')"
    [[ "$installed" == "$GITLEAKS_VERSION" ]] ||
        die "gitleaks is '${installed:-unknown}', expected $GITLEAKS_VERSION"
    log "secret scan ($s)"
    if [[ "$s" == full ]]; then
        gitleaks git --no-banner --redact .
    else
        local base="${OSS_BASE_SHA:-}" head="${OSS_HEAD_SHA:-}"
        resolve_commit OSS_BASE_SHA "$base"
        resolve_commit OSS_HEAD_SHA "$head"
        gitleaks git --no-banner --redact --log-opts="${base}..${head}" .
    fi
    python3 scripts/export_clean.py
}

# --- candidate ---------------------------------------------------------------

cmd_candidate() {
    local source_sha="${OSS_SOURCE_SHA:-}"
    is_sha "$source_sha" || die "OSS_SOURCE_SHA must be the workflow commit"
    [[ "$(git rev-parse HEAD)" == "$source_sha" ]] || die "checkout is not OSS_SOURCE_SHA"
    git diff --quiet HEAD || die "working tree differs from $source_sha"
    [[ "$(scope)" == full ]] || die "candidate requires OSS_CHECK_SCOPE=full"
    local run_id="${GITHUB_RUN_ID:-local}" run_attempt="${GITHUB_RUN_ATTEMPT:-0}"

    OSS_GATE='' cmd_checks
    OSS_GATE='' cmd_coverage
    OSS_GATE='' cmd_compatibility
    cmd_security

    local version
    version="$(cargo metadata --locked --no-deps --format-version 1 |
        python3 -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["version"])')"
    rm -rf dist target/package
    mkdir -p dist
    log "package and verify $PACKAGE $version"
    cargo package --locked -p "$PACKAGE"
    local crate="target/package/${PACKAGE}-${version}.crate"
    [[ -s "$crate" ]] || die "cargo package produced no archive"
    cp "$crate" dist/
    cargo package --list --locked -p "$PACKAGE" >dist/source-inventory.txt
    python3 - "$ROOT/Cargo.lock" >dist/dependencies.json <<'PY'
import json, sys, tomllib
lock = tomllib.load(open(sys.argv[1], "rb"))
print(json.dumps([
    {k: p.get(k) for k in ("name", "version", "source", "checksum")}
    for p in lock.get("package", [])
], indent=2))
PY
    (cd dist && shasum -a 256 ./*.crate source-inventory.txt dependencies.json >SHA256SUMS)
    python3 - "$source_sha" "$version" "$run_id" "$run_attempt" >dist/release-record.json <<'PY'
import json, subprocess, sys
sha, version, run_id, attempt = sys.argv[1:5]
rustc = subprocess.run(["rustc", "--version"], capture_output=True, text=True, check=True).stdout.strip()
print(json.dumps({
    "package": "packdb-logql-parser",
    "version": version,
    "source_sha": sha,
    "toolchain": rustc,
    "features": ["default"],
    "targets": ["source"],
    "gates": ["checks", "coverage", "compatibility", "security"],
    "scope": "full",
    "candidate_run_id": run_id,
    "candidate_run_attempt": attempt,
    "artifact_name": f"candidate-{sha}-{run_id}-{attempt}",
    "checksums": open("dist/SHA256SUMS").read().splitlines(),
}, indent=2))
PY
    log "candidate written to dist/"
}

# --- publish -----------------------------------------------------------------

cmd_publish() {
    # Public publication is a launch action outside private staging. The
    # workflow job only runs when OSS_PUBLIC_RELEASE_ENABLED is 'true'; until
    # the launch change implements candidate download, verification and
    # registry upload here, refuse rather than publish anything.
    die "publication is not enabled for this repository; see RELEASING.md"
}

case "${1:-}" in
    checks) cmd_checks ;;
    coverage) cmd_coverage ;;
    compatibility) cmd_compatibility ;;
    security) cmd_security ;;
    candidate) cmd_candidate ;;
    publish) cmd_publish ;;
    *) die "usage: scripts/oss-ci.sh checks|coverage|compatibility|security|candidate|publish" ;;
esac
