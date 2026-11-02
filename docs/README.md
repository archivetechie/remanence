# Remanence documentation

Remanence writes archives to LTO tape and reads them back years later. This
directory holds the documentation for people who install and run it, write
software against it, or review how it works. The root [README](../README.md)
is the project's front page.

If you are new, read [Concepts](concepts.md) for the vocabulary, then follow
the [Quickstart](guide-quickstart.md). If your hardware is one manually loaded
LTO drive, follow [Using a standalone LTO drive](guide-standalone-drive.md)
instead. [What works, and what does not](status.md) says how mature each part
is and what has been released.

The pages are grouped by what the reader wants: tutorials to learn by doing,
how-to guides to reach a goal, references to look things up, and explanations
to understand why.

## Tutorials

- [Quickstart](guide-quickstart.md): from a clean checkout to a local archive,
  an encrypted copy, and a first tape in a library.
- [Using a standalone LTO drive](guide-standalone-drive.md): initialize, write,
  restore, finalize and recover tapes with one drive and no daemon.

## How-to guides

- [Installing Remanence](guide-install.md): build from source, install the
  binaries, and prepare a host for tape hardware.
- [Troubleshooting](guide-troubleshooting.md): failure modes, the messages
  they produce, and their remedies.
- [Full verification](guide-full-verification.md): what
  `rem tape verify-index` checks, how to read its findings, and how long it
  takes.
- [Damaged tapes](guide-damaged-tapes.md): what an operator sees when a tape's
  index or bootstrap is damaged, and what each refusal means.
- [Importing and recovering Remanence tapes](importing-and-recovering-remanence-tapes.md):
  rebuild the identity of a Remanence tape this host does not know, before
  inventory and catalog recovery.
- [Sizing objects and write sessions](guide-object-sizing.md): capacity terms
  for the terminal index, and the trade-offs in object and bundle size.
- [Release verification](guide-release-verification.md): what a binary release
  contains and how to check that it was built from the signed source.

## Reference

- [CLI reference](reference-cli.md): the `rem`, `rem-debug`, `rem-daemon` and
  `rem-recover` commands and their exit codes.
- [Configuration reference](reference-configuration.md): the configuration
  file, defaults, environment variables and on-disk state.
- [On-tape layout reference](reference-tape-layout.md): what a written
  cartridge contains.
- [Glossary](reference-glossary.md): project and tape vocabulary.
- [API reference](reference-api.md): notes on the write-session contract; the
  gRPC contract itself is [proto/layer5.proto](../proto/layer5.proto).
- [Extended attributes and file metadata](reference-extended-attributes.md):
  what file metadata is preserved, how it is stored, and how it behaves on
  restore.
- [`rem archive extract-stream` protocol](reference-extract-stream-protocol.md):
  the contract for streaming ranged ciphertext out of an encrypted object.
- [Foreign-format adapters](reference-foreign-format-adapters.md): the
  read-only interface for archive formats written by other software.
- [What works, and what does not](status.md): maturity, releases, known
  limitations and tested hardware.
- [The versioning register](versioning-register.md): every versioned component
  of the system, with its current value and what an unknown value means.
- [Formal verification status](formal-verification-status.md): what is proved
  in Lean, and what is deliberately not.

## Explanation

- [Concepts](concepts.md): how a tape behaves, and the terms the rest of the
  documentation uses.
- [Architecture overview](architecture-overview.md): the crate stack, the write
  and read paths, and the design invariants.
- [Why Remanence?](why-remanence.md): what Remanence is for, compared with the
  other ways of putting data on tape.
- [Why Remanence speaks SCSI directly](why-sg-passthrough.md): why the stack
  bypasses the kernel `st` driver and `mt`/`mtx`.
- [Why Remanence keeps only user extended attributes](why-user-xattrs-only.md):
  why security labels, ACLs, file capabilities and file flags are not
  preserved, and how to keep them when a collection needs them.
- [Encryption explained](encryption-explained.md): why REM-ENCRYPT seals objects
  in software instead of using drive encryption, and what an operator still
  manages.
- [How these formats change](versioning-explained.md): the change policy for
  format revisions, in plain language.
- [Tape identity and the retire operation](tape-identity-lifecycle-explainer.md):
  how a cartridge acquires and keeps its identity.
- [Partial-file restore on LTO](pfr-reference.md): how one byte range of a file
  is read from tape, with the drive mechanics and the arithmetic.

## The specifications and their companions

The format specifications and their companion documents are in
[`specs/publication/`](../specs/publication):

- [The formats, explained](../specs/publication/formats-explained.md): a
  plain-language companion to the specifications.
- [REM Implementation and Operations Guide](../specs/publication/rem-implementation-guide.md):
  informative, tool-neutral practice for tools that read or write the REM
  formats. [rem-implementation-guide.md](rem-implementation-guide.md) in this
  directory points to it.

## For contributors

These pages are for people who change Remanence itself: its code, its
specifications or its test vectors.

- [API module organization](api-module-organization.md): where each behaviour
  of the `remanence-api` crate lives in the source.
- [Development tools](development-tools.md): building `rem-dev`, and its
  review-vector generator, two scratch-tape drills and dump writer.
- [Checking specification preservation](guide-specification-preservation.md):
  the check that every piece of a specification's text survives an edit or is
  accounted for, with a worked example.
- [Running the proof inventory](formal-verification-status.md#running-the-inventory-gate):
  what `make proof-inventory` checks and what it needs installed. The rest of
  [Formal verification status](formal-verification-status.md) says what each
  proof covers.
- [POSIX pax and ustar](reference/posix/README.md): the tar format that the
  plaintext body of an object follows, and the editions of the standard to
  read.

## Records

- [Public decision log](decision-log.md): format and compatibility decisions
  that affect independent implementations.
- [Software release checklist](releases/CHECKLIST.md): the maintainers' release
  procedure.
- [Remanence 0.2.1 draft release note](releases/v0.2.1-draft.md): a draft, not
  a published release.

This page lists every document in this directory.
