<!-- code-anchor: .github/workflows/release.yml tools/release_evidence.py tools/build_publication_test_vectors.py specs/publication/remanence-test-vectors.tar @ 8238b7b3 -->
# Release verification

This guide shows you how to check that files you downloaded from a Remanence
release are the files the project published. It is for anyone who downloads a
release: a reviewer checking the test vectors, or, once binaries are
published, an operator installing them.

There is no binary release of Remanence yet. The only published release is
v0.1.0, and its only file is the conformance test-vector archive,
`remanence-test-vectors.tar`. To run Remanence you build it from source, as
[Installing Remanence](guide-install.md) describes, and
[What works, and what does not](status.md#releases-and-versions) gives the
release history. The first part of this guide checks the v0.1.0 archive. The
second part describes the checks a binary release will support.

## Check the v0.1.0 test-vector archive

The archive holds the objects, tape images and expected results that the
three format specifications are tested against. What ties it to the project
is its SHA-256 digest, which the specifications themselves quote:
REM-OBJECT §13, REM-ENCRYPT §13 and REM-PARITY §17 each name
`remanence-test-vectors.tar` with the digest
`77be73e780e9ff2c265c8357b6ba684b4c69800213820ae1331850f742b1d83d`. The
[versioning register](versioning-register.md#19-the-conformance-vector-archives)
records the same value. The v0.1.0 tag is annotated but not signed, and the
release has no build attestation, so the digest in the specifications is the
check.

You need `sha256sum`, `tar` and Python 3. The GitHub CLI (`gh`) is optional.

1. Download the archive, with the GitHub CLI or directly:

   ```sh
   gh release download v0.1.0 --repo archivetechie/remanence
   ```

   ```sh
   curl -LO https://github.com/archivetechie/remanence/releases/download/v0.1.0/remanence-test-vectors.tar
   ```

2. Compute its digest:

   ```sh
   sha256sum remanence-test-vectors.tar
   ```

   The digest must be the one the specifications quote:

   ```text
   77be73e780e9ff2c265c8357b6ba684b4c69800213820ae1331850f742b1d83d  remanence-test-vectors.tar
   ```

   With a source checkout, the archive must also be byte-identical to the
   copy in the repository:

   ```sh
   cmp remanence-test-vectors.tar specs/publication/remanence-test-vectors.tar
   ```

3. Unpack it into an empty directory and check every file in it against the
   archive's own checksum list. The command prints nothing when every file
   matches:

   ```sh
   mkdir vectors
   tar -xf remanence-test-vectors.tar -C vectors
   (cd vectors && sha256sum --strict -c --quiet CHECKSUMS.sha256)
   ```

4. Run the verifier the archive carries. It needs no source checkout and no
   packages beyond the Python standard library. It checks the checksums again,
   checks that every required vector is present, and re-derives the
   REM-PARITY values independently:

   ```sh
   python3 vectors/verify.py
   ```

   It prints one line per re-derived vector and ends with this line
   (shortened):

   ```text
   ...
   PASS: 79 REM-PARITY vectors and all archive checksums verified
   ```

The archive also carries `tools/verify_rem_object_vectors_independent.py`, a
second implementation, written without the Remanence Rust code, that rebuilds
the REM-OBJECT vectors and opens the REM-ENCRYPT ones. It needs the Python
packages listed in the archive's `tools/requirements-rem-object-independent.txt`.
The archive's `rem-object/manifests/README.md` describes what it checks.

The withdrawn release v1.0.0 carries an earlier archive with a different
digest. The specifications cite the v0.1.0 archive, so verify readers against
that one.

## Check a binary release

The release workflow, `.github/workflows/release.yml`, builds and publishes a
binary release when a version tag is pushed. Before it publishes, it requires
the tag to be annotated, signed and verified by GitHub, to name a commit in
the history of `main` whose CI run succeeded, and to match the workspace
version. It repeats the formatting, lint and test checks and builds the four
programs as static Linux binaries. It then unpacks the archive on a clean
runner and runs each binary in an otherwise empty root directory. [Installing
Remanence](guide-install.md#when-a-binary-release-is-published) lists the
files such a release carries.

To check one, replace `<VERSION>` below with the release's version, for
example `0.2.0`.

1. Download every file of the release into an empty directory:

   ```sh
   gh release download v<VERSION> --repo archivetechie/remanence
   ```

2. Check every file against `SHA256SUMS`, which lists the two archives, the
   license inventory, the build-evidence files and `release-manifest.json`:

   ```sh
   sha256sum --strict -c SHA256SUMS
   ```

3. Check the build attestation. The workflow attests every file listed in
   `SHA256SUMS` with GitHub's artifact attestations, which bind each file's
   digest to the workflow run that built it. With a GitHub CLI that has the
   `attestation` command, check each file, for example the operator archive:

   ```sh
   gh attestation verify remanence-<VERSION>-x86_64-unknown-linux-musl.tar.gz \
       --repo archivetechie/remanence
   ```

4. Check the tag signature as GitHub records it. The first command prints the
   tag object's id, and the second prints whether GitHub verified its
   signature:

   ```sh
   gh api repos/archivetechie/remanence/git/ref/tags/v<VERSION> --jq .object.sha
   gh api repos/archivetechie/remanence/git/tags/<OID> --jq '.verification | {verified, reason}'
   ```

   `<OID>` is the id the first command printed. A binary release prints
   `"verified":true`. For v0.1.0 the second command prints
   `{"reason":"unsigned","verified":false}`, which is why the test-vector
   check above relies on the digest.

5. Read `release-manifest.json` (schema `release-evidence.v1`). It records:

   | Field | Contents |
   | --- | --- |
   | `source_sha`, `ref`, `repository` | The commit, tag and repository the release was built from. |
   | `run_url`, `run_id`, `run_attempt`, `workflow` | The workflow run that built it. |
   | `eligibility` | The tag object id, GitHub's verification of its signature (`signature`), and the successful CI runs for the exact commit (`ci_runs`). |
   | `builds` | For each build: the exact `rustc -vV` and `cargo -V` output, the Python version, the runner's operating system and image, the lockfile digests, and each file's digest and size. |
   | `locks` | The SHA-256 of `Cargo.lock` as committed. |
   | `artifacts` | The SHA-256 and size of every published file. |

   Open the CI run links and check that `source_sha` is the commit the tag
   names.

## What the checks do not promise

The workflow pins its GitHub Actions and its Rust toolchain by exact
reference. The runner images and the operating system's package repositories
are maintained outside the project, so the records do not promise that a
rebuild produces identical bytes. They record what the build used.

A binary release is separate from the publication of the specifications and
from their deposits. Checking a release says nothing about which revision of
a specification is current; the [versioning
register](versioning-register.md) records that.

## What can go wrong

- `sha256sum` reports `FAILED` for a file, or the archive digest differs from
  the one the specifications quote. The file is not the one published.
  Download it again, from the release page itself.
- `sha256sum -c SHA256SUMS` reports a file as missing. Download the complete
  release; the check needs every file it lists.
- `gh attestation` is an unknown command. The installed GitHub CLI predates
  artifact attestations; install a newer one.
- `vectors/verify.py` ends with a line other than `PASS:`. The archive is
  damaged or not the published one.

## Related pages

- [Installing Remanence](guide-install.md): building from source, and the
  files a binary release will carry.
- [The versioning register](versioning-register.md): every versioned
  component, including the vector archives.
- [How the formats change](versioning-explained.md): why a vector archive is
  never changed once published.
