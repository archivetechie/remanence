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
generates and handles its vectors, what a revision must show before it is
frozen, and how the project releases its reference implementation. They bind
the project, not other implementations.

Some of the rules were carried whole out of a specification, and they keep its
capitalised requirement keywords. Those keywords have the meaning that BCP 14
gives them ([RFC 2119](https://www.rfc-editor.org/info/rfc2119),
[RFC 8174](https://www.rfc-editor.org/info/rfc8174)).

### Candidate vectors

A revision in preparation can change the bytes its vectors must contain.
REM-PARITY 1.0.0-draft.5 does: its generation-2 terminal layout has no
counterpart in the vector archive that the published revisions pin. Its new
bytes are therefore generated as candidate vectors, which
[REM-PARITY](in-progress/rem-parity-1-specification.md) §17 describes. They are
evidence for the review of that revision. They become conformance vectors only
when the revision is frozen and publishes its own archive, and until then they
are kept apart from the published artifacts.

The terminal bytes of REM-PARITY 1.0.0-draft.5 are review-only candidate vectors
under `fixtures/rem-parity-terminal-index-draft/`. Candidate vectors MUST NOT be
copied into or substituted for publication artifacts before independent review
and freeze.

A candidate set of valid tapes shows that a reader accepts what it should, but
not that it rejects what it should. The candidate set therefore also holds
negative vectors, each of which breaks one rule a reader has to enforce.
REM-PARITY 1.0.0-draft.5 set their minimum coverage, and it is carried here
whole:

Before freeze, negative candidates MUST cover at least: bad role magic; CRC
failure; reserved/padding nonzero; ordinal/count mismatch; payload slot
truncation; map↔Object-row mismatch; header/footer disagreement; local
observation mismatch; nonzero gap interior; missing filemark; A/B/C survivor
conflict; all three replicas invalid with explicit BOT fallback; and arithmetic
overflow in every size/location formula.

Item TT-2 of REM-PARITY's Appendix D records how far the review-only set has
got.

### REM-PARITY's freeze criteria

From its freeze date, REM-PARITY promises that no tape it validates will ever
be invalidated and that no reader guarantee will be withdrawn. A promise of
that kind is safe to make only once the text has been shown to describe a
format that works in practice. The criteria below ask for that evidence. They
gate the freeze of the specification; they are not part of what a conforming
implementation must do. Candidate-vector, proof, hermetic, VTL and incremental
review results are evidence gathered before the freeze, not a declaration that
the specification is frozen. REM-PARITY's Section 18 says that its criteria are
kept here, and its
[Appendix D](in-progress/rem-parity-1-specification.md#appendix-d-open-items-informative)
lists the items that remain open.

The criteria below were carried word for word, under the same numbers, from
Section 18 of the REM-PARITY preparing copy, which now points here. Keeping the
numbers means that "REM-PARITY freeze criterion 3" names the same criterion
wherever it is cited, in code comments and tool help included. In the
criteria, "this document" is REM-PARITY, and section numbers are REM-PARITY's.

1. At least one complete implementation implements this document in every
   role — Writer, Scanner, Recoverer, Resumer, Verifier — with no known
   divergences from this document.
2. The Section 17 fixtures are present in the companion archive and pass, including the
   damage matrix and the byte-pinned minimal tape image. Every
   **[pinned-at-generation]** value is independently re-derived by a second
   implementation (different language or library) before freezing, so a
   reference-implementation bug cannot be frozen into the conformance
   anchor.
3. Coverage-guided fuzzing of the bootstrap, sidecar, terminal-replica, and separation
   parsers and of the scan walk reaches a corpus plateau with no panics,
   hangs, or unbounded allocations.
4. A live round-trip passes on real or virtualized tape hardware: write
   with injected damage (a fault-injecting transport), scan catalog-less, recover,
   and verify — at two distinct block sizes.
5. A long-term-recovery drill: an independent party reconstructs the
   minimal tape image's map and recovers one damaged block using only this
   document and a generic CBOR/SHA-256/HMAC toolkit — including re-deriving
   the Cauchy matrix from Section 6.
6. Accelerated arithmetic (table- or SIMD-based GF(2⁸) and CRC kernels) is
   proven byte-identical to the Section 5.1/6.1 definitions via the
   Section 17 vectors. Not a format change — but freeze SHOULD wait for it,
   so adopting an accelerator never silently changes emitted bytes.

Criterion 5 was exercised by an AI system under a clean-room, no-hint
protocol: it had the specification and generic language/library facilities,
but no implementation source. That is technical independence, not the social
independence of a second institution; a human or institutional reproduction
remains explicitly invited under RP-4.

RP-4 is the open item "An independent implementation from the prose alone" of
the published revision 1.0.0-draft.2, in its Appendix E. It asks anyone outside
the project to build a reader from the text and to report where it disagrees
with the vectors.

The reference implementation gathers the evidence for four of the criteria with
tools kept in its repository.

| Criterion | Evidence in the reference implementation | Remarks |
| --- | --- | --- |
| 2 | `tools/verify_terminal_index_vectors.py` re-derives the candidate terminal bytes without calling the Rust codec; the generator is `crates/remanence-parity/examples/generate_terminal_index_vectors.rs` | REM-PARITY §17 lists what the verifier re-derives |
| 3 | Five coverage-guided targets in `fuzz/fuzz_targets/`: `rem_parity_bootstrap_parse`, `rem_parity_bootstrap_structured`, `rem_parity_map_parse`, `rem_parity_scan_walk` and `rem_parity_sidecar_parse`, run by `tools/run_rem_parity_fuzz_campaign.sh` and `tools/run_rem_parity_fuzz_overnight.sh` | No target yet covers the terminal-replica or separation parsers, so the criterion is open (Appendix D, TT-6) |
| 4 | `rem-debug tape freeze-drill` ([reference-cli.md](../docs/reference-cli.md)) writes drill objects through a full finalization on a scratch tape, injects a chosen read-side medium-error damage plan, and verifies recovery | One run covers one block size, and the criterion asks for two. The supervised physical-tape runs are open (Appendix D, TT-3) |
| 5 | A clean-room exercise by an AI system, given the specification and generic libraries but no implementation source | Technical independence only, as the note above says; RP-4 remains open |

Criterion 1 is judged against the whole implementation, and criterion 6 names
its own evidence, the Section 17 vectors.

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

### In plain terms

Before a revision of a specification is fixed for good, the project has to
show that it works: that it can be built from its text, that its test files
are right, that it survives damaged and hostile input, and that someone else
can use it. This section records how the project gathers that evidence, and it
keeps test material that has not yet been reviewed away from the published
files. None of it changes what a tape or an object is, or what another
implementation must do.
