# Format specifications

The repository review copies intended for publication live in
[publication/](publication/). Their concept DOIs are reserved, but no
specification revision has been deposited yet, so they are not DOI-citable
normative revisions today:

- [rem-object-core-1-specification.md](publication/rem-object-core-1-specification.md) — the
  **REM-OBJECT Core Format 1.0** specification: the archival object
  container, its manifest, and closed-form byte-range addressing.
- [rem-encrypt-1-specification.md](publication/rem-encrypt-1-specification.md) — the
  **REM-ENCRYPT 1.0** specification: the encrypted envelope around a
  canonical REM-OBJECT.
- [rem-parity-1-specification.md](publication/rem-parity-1-specification.md)
  — the **Rem Tape Parity (REM-PARITY) Format 1.0** specification: on-tape
  layout, sidecar parity, bootstrap blocks, and catalog-less recovery.
- [formats-explained.md](publication/formats-explained.md) — the
  plain-language companion: motivation and design, informative only.
- [remanence-test-vectors.tar](publication/remanence-test-vectors.tar) —
  the pinned test-vector archive; its SHA-256 is printed in the
  specifications.

REM-OBJECT and REM-ENCRYPT are the review fixed points implemented by the
reference tree. The published-directory REM-PARITY copy describes generation
1; current software writes and reads only the generation-2 replacement being
prepared under [in-progress/](in-progress/). This incompatibility is deliberate
but unresolved at the publication layer; see
[the status page](../docs/status.md#the-rem-parity-generation-boundary).
Earlier internal revisions and review records are preserved in git
history, not in the working tree.

## Revisions being prepared

[in-progress/](in-progress/) holds the next revision of a document while it is
being assembled. Nothing there is normative, and a revision is not published
every time an item closes — resolutions accumulate until there are enough to
warrant a revision and a deposit. Keeping them out of `publication/` is what
lets a reader open that directory and trust that everything in it is current.
See [in-progress/README.md](in-progress/README.md) for what is currently being
prepared.

## Which copy governs

The documents in this directory are the working copies from which each revision
is prepared. No revision has yet been deposited. Once one is, the **normative**
text of that published revision is the copy deposited under the document's
concept DOI, named in its Status section.

The two can differ legitimately — that is how the next revision is written — but
never under the same name: a version string is never reused for different bytes.
`DEPOSITED.sha256` records the digest published for each deposited revision, and
`tools/check_spec_versioning.py` fails the build if a document's current version
string appears there and its bytes have since changed. So if a copy anywhere
claims version X, it is byte-identical to the deposit of version X or it is
defective.

A copy found inside a Remanence source release is a convenience copy under the
same rule. It is deliberately shipped rather than replaced with a pointer,
because a reader unpacking an archive offline needs the text itself.
