# Releasing

Only named release maintainers may publish. All packages stay `publish = false`
during private staging. Public publishing requires a reviewed allowlist change,
verified registry ownership and a release record for the exact commit. Never run
an unqualified workspace-wide publish. Repository visibility does not make
crates.io private.

## Prepare and rehearse

1. Select versions, update changelogs, compatibility/support matrices and lockfile.
   Run required CI on the exact commit, with package-specific feature selection.
   Tag single-package repos `vVERSION`; multi-package repos `PACKAGE-vVERSION`.
   Protect tags against replacement. A prerelease version has a prerelease suffix.
2. Audit `cargo package --list -p PACKAGE` and extracted archives: source, generated
   inputs, README, LICENSE, applicable notices and no private data. Add registry
   versions to local path dependencies. Resolve all normal/build/optional dependencies
   on the public registry before Cargo publication. A Git/path-only dependency is
   not a public package solution. [Cargo dependency reference](https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html).
3. Rehearse private candidates without registry uploads. For `publish = false`
   packages, use `cargo package` where supported; cross-repository private dependency
   closures may require an isolated assembly workspace. This is build evidence,
   not proof that a registry upload can resolve. In a disposable public-shaped
   checkout, enable only reviewed packages and run `cargo publish --dry-run -p PACKAGE
   --locked` after dependencies are available. Retain verification; no `--no-verify`.
4. Build binaries using the committed lockfile and explicit shipping features.
   Prebuilt `packdb`, `packdb-query` and `packdb-admin` executables are required
   release assets, not a later packaging enhancement. Initial CLI targets: Linux x86_64 GNU, macOS arm64 and Windows x86_64 MSVC.
   Add Linux arm64/macOS x86_64 only when native or trustworthy target smoke tests
   pass. The single-node `packdb` release starts with Linux x86_64 GNU, tested
   by its storage/recovery suite. Add other server targets after equivalent checks. Do not advertise untested
   builds. Document minimum OS/libc and CPU requirements.
5. Archive as `BINARY-VERSION-TARGET.tar.gz` (Unix) or `.zip` (Windows), with LICENSE,
   applicable notices and installation notes. Attach SHA-256 sums, dependency/SBOM
   inventory and build provenance identifying commit, toolchain and features.
   Smoke-test each extracted artifact, not just the build-tree executable.
   Private candidate assets stay in private repos with restricted artifact access.
   Follow docs/WORKFLOWS.md storage limits: use draft GitHub Release assets for large
   binary bundles, with a small hash/run manifest in Actions. Verify downloaded
   archives and checksum files from the release page before announcing it.

## Publish

The owner and independent reviewer check CI, source/asset provenance, contacts,
license obligations and advertised limits. Review old private artifacts and history
before the separate repository-visibility change. Apache-2.0 requires carrying the
license and applicable notices; preserve origin and modified-file notices where
required. [Apache license terms](https://apache.org/licenses/LICENSE-2.0.html).

Publish dependencies in topological order, wait for registry availability, then
publish consumers. For each version, verify the registry checksum and a clean
consumer build/install before advancing. On partial failure, record which versions
already exist and resume remaining packages; never overwrite a version or blindly
retry publication. Cargo versions cannot be replaced, and yanking does not erase
source. [Cargo publication behavior](https://doc.rust-lang.org/cargo/reference/publishing.html).

Use OIDC trusted publishing scoped to the exact org/repo/workflow/environment when
available; grant id-token permission only to the publish job. Bootstrap a new crate
with a short-lived narrowly scoped token if the registry still requires first
publication before configuring trust, then remove it. Recheck that requirement at
launch. GitHub Actions `pull_request_target` and `workflow_run` must not publish;
crates.io also blocks these trusted-publishing triggers.
[Rust registry update](https://blog.rust-lang.org/2026/01/21/crates-io-development-update/).

Attach the reviewed artifacts to a draft GitHub Release for the same protected tag,
then publish the draft after verification. Use ordinary versioned public releases;
release-candidate tags are for rehearsal. Version labels are not quality guarantees. A private-repo
release is accessible only to authorized readers; public distribution starts with
the visibility/release transition. [GitHub releases](https://docs.github.com/en/repositories/releasing-projects-on-github/about-releases).

Check `cargo install --locked packdb-query-cli --version VERSION` produces
`packdb-query`; similarly verify `packdb-admin`. Verify docs.rs for libraries and
the README install instructions from a clean environment. The community server
uses GitHub Releases first; enable Cargo installation only after its dependency
closure is publishable. Put actual versions in user-facing commands.

## Recovery and access

If a defect appears, publish a patch, mark affected GitHub assets/releases and yank
only when appropriate. Do not delete or replace assets silently. For exposed secrets,
revoke/rotate immediately: yanking cannot remove them from consumers. Publish security
advisories through the coordinated process. Storage downgrades require an explicitly
tested compatibility path; binary replacement alone is not rollback.

Maintain at least two trusted human registry owners and a verified organization
team where supported. Document owner recovery and revoke departing access. Separate
build, candidate upload and public publication permissions; never give fork PRs
release credentials. Follow docs/WORKFLOWS.md for required jobs and activation checks.

## Public descriptions

Describe capabilities and observed limitations without maturity or production-quality
claims. “Used in production” is permitted only with recorded evidence for that
component/version lineage; it provides no quality, HA, fitness, support or commercial
guarantee. Do not imply deployment usage merely from a passing workflow. Internal
commercial builds and guarantees, if any, are managed separately by the `packdb` org.
