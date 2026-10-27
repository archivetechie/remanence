# Release verification

This page describes what a Remanence binary release contains and how to check
that the files you downloaded are the ones the project built from a specific,
signed source revision.

Binary releases require an annotated, GitHub-verified signed tag on an ancestor
of `main`, successful main-push CI for the exact tagged commit, and a matching
workspace version. The release build repeats the formatting check, clippy and
the Rust tests with locked dependencies before packaging statically linked
Linux binaries. A separate job unpacks the operator archive and runs its
binaries before anything is published.

Release files include the operator archive
`remanence-<version>-x86_64-unknown-linux-musl.tar.gz`, the recovery archive
`rem-recover-x86_64-linux-musl.tar.gz`, `release-manifest.json`, `build-remanence-linux.json`,
`eligibility.json`, third-party licenses, and `SHA256SUMS`. The manifest binds
source and workflow identities, the verified tag object, eligible CI run links,
lockfile hashes, actual compiler/Cargo versions, runner identity, and artifact
hashes. Assembly rejects missing or changed artifacts and mismatched build inputs.

Download the complete release set and run `sha256sum -c SHA256SUMS`. Inspect the
manifest's source and CI links. GitHub build attestations bind the payloads and
manifest to the publishing workflow; verify them against this repository using
GitHub's attestation tooling.

Actions and Rust toolchains use pinned references. Runner images and operating
system package repositories remain externally maintained inputs, so these records
do not promise bit-identical rebuilds. A changed workflow is unverified until its
remote jobs actually run. Local helper tests are available with:

```sh
python3 -m unittest discover -s tools -p test_release_evidence.py
```

Binary release evidence is separate from normative specification acceptance,
specification publication, and external deposits. Passing local tests or generating
a manifest does not publish any of those artifacts.
