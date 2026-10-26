<!-- code-anchor: Cargo.toml crates proto/layer5.proto @ 4dc2a3c6 -->
# What works, and what does not

This page is for someone deciding whether Remanence is ready for a particular
job. It lists what the software does today, what it does not do, and how well
tested each part is, and it names limitations in preference to capabilities. The
root [README](../README.md) has a shorter version.

## The short version

**This is alpha software, v0.2.0.** The implementation has had limited
operational testing, so do not make it the only copy of anything. The format
specifications are versioned separately and are at 1.0.

The parts that put bytes on tape and get them back are the mature parts. A tape
written today can be recovered without the catalog, without the daemon, and
without a configuration file. The parts that are still moving are the control
surfaces above that: the gRPC API, the authorization model, and some of the
library-robotics operations.

The format specifications are review drafts until they freeze on 31 July 2027.
For REM-OBJECT and REM-ENCRYPT, the review checks whether the documents describe
the implemented formats correctly. For REM-PARITY, the published copy does not
yet describe the layout current software writes (see
[below](#the-rem-parity-publication-copy)). A review finding could still change
what goes on tape, and the promise that no tape a format validates will ever be
invalidated takes effect at the freeze. Treat material written during the
review window as re-writable, and keep the source it was made from until the
formats are final.

## What works

**Talking to hardware.** SCSI primitives, library and standalone-drive
discovery and identity, robotics, and hot-plug watching. Library daemons use a
per-library allowlist. Direct standalone commands bind the current `/dev/sgN`
path to an explicitly authorized drive serial and refuse writes unless
discovery proves that no changer owns the drive.

**Writing and reading tape.** Pipelined fixed-block tape I/O backed by a
staging ring, with position proofs. This is the only path for writing and reading object data. (Verification reads single blocks and does not use it.) Reads are served through a
host-RAM reservoir that supports ranged access, so a partial-file restore does
not have to stream a whole object.

**The on-tape formats.** The `rem-object-v1` object body and REM-ENCRYPT
encrypted representation match their review specifications and pinned vectors.
Reed-Solomon sidecar parity, resume, and catalog-less terminal-index recovery
are implemented as the REM-PARITY revision in `specs/in-progress/` describes
them.

**Encryption.** Each object gets a fresh key, wrapped to one or more recipients
using HPKE with the X-Wing hybrid KEM — ML-KEM-768 combined with X25519 — and
stored in the object's own header. A tape's encrypted objects can therefore be
opened by any recipient they were wrapped to, without a key server.

**State.** An audit log, per-tape journals, and a SQLite catalog that is a
rebuildable projection rather than an authority, plus media-readiness records
and tape-I/O fences.

**The daemon.** Catalog queries, pool-targeted write sessions, object, file and
byte-range read sessions with a cold-resume contract that re-checks tape
identity and proves device position after an application restart, library
inspection and robotics, drive stewardship, alarms and live status, over a Unix
socket and optionally over mTLS TCP.

**Operator tools.** `rem` and `rem-debug`, including a destructive-safety
gauntlet for tape initialization and quarantine tooling for suspect media.
`rem` can run initialization, checkpointed put/get, finalization,
verification, adoption, and catalogless restore directly against one manually
loaded standalone drive, with home-mode state and no daemon. Object build,
inspect and extract also work on local files with no tape hardware. A separate
`rem-recover` binary decrypts and extracts encrypted envelope objects with no
daemon, catalog, or configuration file. Plaintext REM-OBJECTs remain ordinary
pax archives readable with `bsdtar`.

**Testing.** Chaos fault-injection, fuzzing of the format parsers, and
Lean/Aeneas proofs over the parity and format cores in [`verif/`](../verif).

## What does not work yet

### The REM-PARITY publication copy

The bootstrap is the self-describing block at the beginning of a tape, and
`schema_major` is the field in it that names the tape layout. Current software
writes and accepts only `schema_major = 2`, in which the tape's closing
catalog (the terminal index) is three complete copies, called terminal
replicas, separated by typed extents. The
[glossary](reference-glossary.md) defines these terms. The REM-PARITY copy under `specs/publication/` (1.0.0-draft.2) and its
frozen vectors describe `schema_major = 1`. Current software rejects tapes
written by the released v0.1.0 binaries, which are `schema_major = 1`, and an
implementation written from the published text cannot read tapes written by
current software. The text that matches current software is in
`specs/in-progress/`. The publication copy and its vectors have not been
updated to it yet.

The specification concept DOIs are reserved and their first deposits are still
pending. Until the matching text is published, keep the source material for
every tape, and do not describe a tape written by current software as
conforming to the publication copy of REM-PARITY.

**Authorization is coarse.** It is a role matrix with no scope below the role:
a client is readonly, operator, orchestrator, admin or system, and there is no
way to narrow a role to particular pools, tapes, or objects.

**Some daemon operations are not implemented.** Library import and export
through the mailslot, library-event streaming, and write-session restart all
return `unimplemented`, as do drive-targeted write sessions (a pool-targeted
or pinned-tape write session both work). Caller-supplied `idempotency_key`
values are rejected; write-session replay detection exists but does not use
that field.

**Appending to a parity tape is session-only.** A committed parity tape can
accept further objects through a write session, but the single-object write
path still refuses it.

<!-- code-anchor: crates/remanence-library/src/lib.rs crates/remanence-cli/src/standalone.rs crates/remanence-library/tests @ 4dc2a3c6 -->
## Tested hardware

| Hardware | Evidence and limits |
|---|---|
| QuadStor VTL | Library integration and failure-recovery tests. A VTL configuration that exposes a drive without a changer is pending qualification. It does not establish real standalone-drive support. |
| HPE MSL3040 with LTO-9 drives | Physical library reads, writes and recovery testing. Physical test time is episodic. |
| HP Ultrium 7 | INQUIRY/VPD captures for discovery and identity; these captures do not establish full media read/write coverage. |
| Real hardware below LTO-7 | No testing. Accepted L5/L6 generation values are not a hardware qualification. |
| Autoloader | No testing. |
| Real standalone drive | No testing yet. Model tests and a VTL view do not substitute for a physical standalone drive. |

A hardware report that cannot be reproduced stays open once a
`rem diagnostics capture --out diagnostics.tar` tarball is attached. For a
specific standalone drive, add `--device /dev/sgN --allow-device SERIAL`.
Include the failing command and its output with the capture.

`DriveMappingUnavailable` means the changer does not report the identities
needed to map its drive bays. It is a discovery limitation, not permission to
treat those drives as standalone. Unresolved bays block standalone writes;
an ownership pin cannot bypass that refusal. Report the changer model and
attach the diagnostic capture so its mapping can be investigated.

## Maturity

Remanence has been developed against a Quadstor virtual tape library and
field-tested on an HPE MSL3040 with LTO-9 drives. The format and parity cores
are exercised against the virtual library on every test run; time on physical
tape hardware is episodic rather than continuous.

The standalone control path has model-backed integration tests for discovery,
authorization, initialization, checkpointed write and restore, metadata
changes, release, and failure fencing. That is stronger than a parser-only
claim, but it is not broad physical-drive validation. Standalone users should
expect hardware-specific findings and include a `rem diagnostics capture`
archive when reporting them.

The formats are the part that has to last the planning horizon of 10 to 20
years, and they are the part with specifications, pinned vectors, a clean-room
recovery drill run by an AI system (technically independent of the source, but
not an independent institution), and formal proofs over their cores. The
software around them is younger than the formats it implements. Restore and
verify real material on your own hardware before relying on either.
