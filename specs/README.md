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

## How a revision is frozen

A revision of a specification is prepared in [in-progress/](in-progress/),
under the rules in [in-progress/README.md](in-progress/README.md#the-rules).
It is then published for review in [publication/](publication/), with a
version that carries a `-draft.N` suffix. On the freeze date named in its
Status section, the finality promise in that section takes effect, and from
then on the change policy in that section governs every revision. The
conformance vectors that anchor a revision are pinned, and are never changed
or replaced;
[versioning-explained.md](../docs/versioning-explained.md#the-test-vectors--evidence-that-never-moves)
explains why. Entries
[18](../docs/versioning-register.md#18-the-specification-documents) and
[19](../docs/versioning-register.md#19-the-conformance-vector-archives) of the
versioning register record the current documents and vector archives.
[guide-specification-preservation.md](../docs/guide-specification-preservation.md)
describes the check that each change to a specification's text must pass.

This section records rules the project follows in that process which the
specifications themselves leave out. The specifications leave them out for the
reason each gives in its Section 1. Ignoring one of these rules would not
change the bytes a tool writes, the conclusions a reader draws from those
bytes, or what a tool may claim about them. The rules govern how the project
generates its vectors and releases its reference implementation. They bind the
project, not other implementations.

Some of the rules were carried whole out of a specification, and they keep its
capitalised requirement keywords. Those keywords have the meaning that BCP 14
gives them ([RFC 2119](https://www.rfc-editor.org/info/rfc2119),
[RFC 8174](https://www.rfc-editor.org/info/rfc8174)).

### Vectors that need fixed secrets

An encrypted vector can be reproduced only if every secret that goes into it
is fixed. The vector REM-OBJECT-TV-E2 fixes its data-encryption key and its
HPKE randomness (REM-ENCRYPT §13.1), and the X-Wing wrapping known-answer file
fixes its encapsulation randomness (REM-ENCRYPT §13.3). An ordinary seal must
not work that way. REM-ENCRYPT §12.1 requires that "Every seal MUST use a
fresh uniformly random 32-byte DEK and fresh HPKE encapsulation randomness for
every recipient". The means of fixing the secrets is therefore kept apart from
sealing:

Deterministic vector-generation hooks inject fixed secrets solely for
reproducible conformance artifacts and MUST NOT be exposed as production
sealing modes.

Production sealing uses the fresh randomness that REM-ENCRYPT §12.1 requires.
In the reference implementation the hook is
`seal_deterministic_for_test_vectors`, in `crates/remanence-aead`, beside the
production entry point `seal`. Only tests and the fuzz-corpus generator call
it; no command-line path does.

### Release controls for the envelope's cryptography

A verified ML-KEM implementation does not by itself verify the rest of the
envelope's construction. REM-ENCRYPT §12.11 names what remains: the HPKE
transcript, the combiner, serialization, entropy handling and zeroization.
Each release of the reference implementation is therefore held to three
controls.

| Control | What it checks | Remarks |
| --- | --- | --- |
| Static known-answer files | The X-Wing draft-10 construction (seed to key, encapsulation, shared secret) and the exact HPKE wrapping transcript | `xwing-draft10-kat.txt` and `xwing-wrap-kat.txt`, in the vector archive under `rem-object/kats/` (REM-ENCRYPT §5.3.1 and §13.3) |
| Independent verifier | Opens every encrypted vector from the specification text, using an ML-KEM implementation and an X25519 library independent of the reference implementation's, and reproduces both known-answer files byte for byte | `tools/verify_rem_object_vectors_independent.py`; REM-ENCRYPT §13.1 records what it verifies for REM-OBJECT-TV-E2 |
| Constant-time integration review | That the code joining the cryptographic libraries to the envelope does not let its timing depend on secret values | The ML-KEM library itself is formally verified ([LIBCRUX-MLKEM] in REM-ENCRYPT §16.2); this review covers the code around it |
