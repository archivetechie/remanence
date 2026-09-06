# Releasing Remanence

Releases are cut only from a clean `main` after the CI, format-vector, audit,
MSRV, and documentation gates pass. The release tag must be annotated and
cryptographically signed by a key GitHub recognizes as verified. The release
workflow checks the tag object, its `main` ancestry, and a successful CI run for
the exact tagged commit before publishing. It also requires the tag to be
exactly `v<workspace.package.version>`, so the release name and the versions
reported by its binaries cannot diverge. Existing historical unsigned tags are
not rewritten.

A `v*` tag runs the release workflow, rebuilds the test and lint gates, and
creates a GitHub release containing checksummed Linux operator binaries and a
separate static-musl `rem-recover` archive. The recovery archive is separate so
an operator can preserve it with key escrow without depending on a full
Remanence installation. Each archive includes Remanence's Apache-2.0 license
and a generated inventory containing the full selected license texts for its
third-party Rust dependency graph.

Format documents are released independently. A software release must not
publish or resolve a reserved format DOI; a format deposit is made only when its
document, vector archive, checksum, and compatibility statement agree.
