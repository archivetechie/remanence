# Software release checklist

These are the steps to cut a software release of Remanence, as the manifests,
the two GitHub workflows and the tools in `tools/` define them.
[RELEASING.md](../../RELEASING.md) gives the policy behind them. Format
documents are released separately, by the deposit procedure in RELEASING.md,
and a software release never publishes or resolves a reserved format DOI.

A step marked *Not confirmed* is not defined or enforced by anything in this
repository. It records practice or an expectation that a maintainer should
check.

## 1. Choose the version

- [ ] The version is `version` under `[workspace.package]` in the root
  `Cargo.toml`. Every crate under `crates/` inherits it through
  `version.workspace = true`. The release workflow refuses a tag unless every
  workspace package has the same version and the tag is exactly `v` followed
  by it.
- [ ] *Not confirmed:* under Cargo's rules for 0.x versions, a change that
  breaks a public API, such as a new variant in a public enum that is not
  `#[non_exhaustive]`, calls for the minor number to be raised. Nothing in CI
  checks this.

## 2. Bump the version in one commit on `main`

The v0.2.0 release commit (`release: v0.2.0`) changed these files.

- [ ] `Cargo.toml`: `[workspace.package] version`.
- [ ] `Cargo.lock`, `fuzz/Cargo.lock` and
  `fieldtest/tools/remfield-io/Cargo.lock`. The last two depend on workspace
  crates by path, and CI reads all three with `--locked`, so each must carry
  the new workspace versions. Check with `git diff` that only those version
  lines changed. *Not confirmed:* the repository does not name the command
  that refreshes them; a Cargo command run without `--locked` against each
  manifest does.
- [ ] `CITATION.cff`: `version`, `date-released`, and the abstract if what it
  says about the formats has changed.
- [ ] `README.md`: the `Alpha software (vX.Y.Z)` banner and the
  `Version X.Y.Z — alpha` line under Status.
- [ ] `docs/status.md`: the `This is alpha software, vX.Y.Z` line.
- [ ] `CHANGELOG.md`: rename `## Unreleased` to `## X.Y.Z — YYYY-MM-DD`.
  Released sections carry no `code-anchor` comment.
- [ ] *Not confirmed:* turn the draft in `docs/releases/` into the final note.
  The release workflow does not read it.

## 3. Run the gates

The gate that counts is a successful run of `.github/workflows/ci.yml` for the
exact release commit, started by a push to `main`; the release workflow looks
it up. Running the following locally first avoids a failed push.

- [ ] `cargo fmt --all --check`,
  `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo test --workspace`.
- [ ] Specification and Guide checks: `python3 tools/check_spec_versioning.py`
  and `make check-guide`.
- [ ] Publication vectors: `make verify-publication-test-vectors`, which runs
  `tools/verify_publication_test_vectors.py` against
  `specs/publication/remanence-test-vectors.tar`, re-deriving the REM-PARITY
  pins, and then `tools/check_spec_versioning.py`. CI also runs
  `python3 tools/test_check_spec_versioning.py` and
  `python3 tools/verify_publication_test_vectors.py --check-rem-object-mirror`,
  and `cargo test -p remanence-parity --test publication_damage_matrix` for
  the published/current REM-PARITY boundary.
- [ ] Wire-presence census: `cargo build -p remanence-api` (its build script
  writes the descriptor set the census reads), then
  `python3 tools/wire_presence_census.py --check-baseline` with the Python
  `protobuf` package installed. It fails on any Layer 5 field that has no
  presence verdict and is not in `tools/wire-presence-baseline.txt`. Record a
  verdict in `tools/wire-presence-ledger.txt`; never add a line to the
  baseline by hand.
- [ ] Dependency audit: `cargo audit` on `Cargo.lock`, `fuzz/Cargo.lock` and
  `fieldtest/tools/remfield-io/Cargo.lock`.

The CI run on a push has three jobs, all of which must pass:

- `fmt, clippy, test` (Rust 1.95.0): the checks above, plus the full-tape
  image digests, the draft vector archive, the terminal-index candidates, the
  second REM-PARITY implementation's decisions, the field-test client build,
  warning-free API documentation, the REM-OBJECT vectors, the core dependency
  graph guard and the release-evidence tool's tests.
- `Rust 1.88 minimum-version check`.
- `Static rem-recover artifact`, which builds `rem-recover` for musl and
  checks that it is statically linked.

The fuzz corpus replay and coverage jobs run only on the weekly schedule or a
manual dispatch. *Not confirmed:* whether to run them before a release.

## 4. Push and wait

- [ ] Push the release commit to `main`. A pull-request run does not count:
  the eligible run must be a `push` on `main` for the exact commit.
- [ ] Wait for that CI run to finish successfully.

## 5. Tag

- [ ] Create an annotated, signed tag on the release commit, for example
  `git tag -s vX.Y.Z -m "Remanence X.Y.Z — alpha"`. GitHub must show the
  signature as verified, so the signing key must be registered with the
  account. The message form follows the v0.1.0 tag.
- [ ] Push the tag: `git push origin vX.Y.Z`.

## 6. The release workflow

A `v*` tag starts `.github/workflows/release.yml`, which needs no input. It:

1. checks that the tag is `v` plus the workspace version;
2. runs `tools/release_evidence.py gate`, which requires an annotated tag on
   the checked-out commit, that commit's ancestry in `origin/main`, a
   GitHub-verified tag signature, and a successful `ci.yml` push run on `main`
   for the exact commit, and writes `eligibility.json`;
3. runs `cargo test --workspace --locked` and
   `cargo clippy --workspace --all-targets --locked -- -D warnings` with Rust
   1.95.0;
4. builds `rem`, `rem-debug` and `rem-daemon`, and `rem-recover` for
   `x86_64-unknown-linux-musl`;
5. packages `remanence-x86_64-linux.tar.gz` and
   `rem-recover-x86_64-linux-musl.tar.gz`, each with `LICENSE-APACHE-2.0` and
   `THIRD-PARTY-LICENSES.html` from `cargo about` 0.9.1;
6. writes `build-remanence-linux.json`, `release-manifest.json` and
   `SHA256SUMS` with `tools/release_evidence.py record` and `manifest`;
7. attests the files listed in `SHA256SUMS`; and
8. creates the GitHub release with the two archives, the three JSON files,
   `THIRD-PARTY-LICENSES.html` and `SHA256SUMS`, using GitHub's generated
   notes.

- [ ] Watch the run to completion. *Not confirmed:* what to do when it fails
  part-way. Re-running the workflow suits a transient failure; a failure in
  the gate means the tag or CI precondition was not met.

## 7. After the release

- [ ] Download the release files, run `sha256sum -c SHA256SUMS`, and verify
  the attestations with GitHub's attestation tooling, as
  [release verification](../guide-release-verification.md) describes.
  *Not confirmed:* the exact verification command.
- [ ] *Not confirmed:* replace the generated release notes with the final note.
- [ ] *Not confirmed:* the Zenodo software record. The README says released
  versions are archived under concept DOI 10.5281/zenodo.21551570. RELEASING.md
  says a GitHub release creates a Zenodo record by itself when the
  GitHub-to-Zenodo integration is on, and that the integration must be off
  while format records are deposited. Whether it is on for software releases
  is not recorded here.
- [ ] *Not confirmed:* open a new `## Unreleased` section in `CHANGELOG.md`.
