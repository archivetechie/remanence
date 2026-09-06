<!-- code-anchor: Cargo.toml crates proto/layer5.proto @ 244bc6de -->
# What works, and what does not

This page is the detailed counterpart to the Status section of the root
[README](../README.md). It is written to be read by someone deciding whether
Remanence is ready for a particular job, so it errs towards naming limitations
rather than capabilities.

## The short version

**This is alpha software, v0.1.0.** It was published as v1.0.0 in July 2026 and
renumbered downward on 7 August 2026 because that number overstated how much
operational testing the implementation had had. Treat it accordingly: do not
make it the only copy of anything. The format specifications are versioned
separately and stay at 1.0.

The parts that put bytes on tape and get them back are the mature parts. A tape
written today can be recovered without the catalog, without the daemon, and
without a configuration file. The parts that are still moving are the control
surfaces above that: the gRPC API, the authorization model, and some of the
library-robotics operations.

One qualification matters more than any of the capability detail below. The
format specifications are review drafts until 31 July 2027. What is under
review for REM-OBJECT and REM-ENCRYPT is whether they describe the implemented
formats correctly. REM-PARITY has a known generation split described below. A
review finding could still change what goes on tape, and the promise that no
tape a format validates will ever be invalidated takes effect at the freeze,
not today. Material written during the review window should be treated as
re-writable: keep the source it was made from until the formats are final.

## What works

**Talking to hardware.** SCSI primitives, library discovery and identity,
robotics, and hot-plug watching, with per-library allowlisting so a daemon only
touches the libraries it was told about.

**Writing and reading tape.** Pipelined fixed-block tape I/O backed by a
staging ring, with position proofs — this is the only write and read path, and
the earlier non-pipelined mode has been removed. Reads are served through a
host-RAM reservoir that supports ranged access, so a partial-file restore does
not have to stream a whole object.

**The on-tape formats.** The `rem-object-v1` object body and REM-ENCRYPT
encrypted representation match their review specifications and pinned vectors.
Reed-Solomon sidecar parity, resume, and catalog-less terminal-index recovery
are implemented for REM-PARITY generation 2; the published-directory parity
text and publication vectors still describe generation 1.

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
Object build, inspect and extract work on local files with no tape hardware at
all. A separate `rem-recover` binary decrypts and extracts encrypted envelope
objects with no daemon, catalog, or configuration file. Plaintext REM-OBJECTs
remain ordinary pax archives readable with `bsdtar`.

**Testing.** Chaos fault-injection, fuzzing of the format parsers, and
Lean/Aeneas proofs over the parity and format cores in [`verif/`](../verif).

## What does not work yet

### The REM-PARITY generation boundary

Current software writes and accepts only bootstrap `schema_major = 2`, whose
terminal authority is three complete index replicas separated by typed extents.
The repository copy under `specs/publication/` and its frozen vectors define
`schema_major = 1`. Consequently current software cannot read tapes written by
the v0.1.0/v1.0.0 release, and an implementation of the generation-1 review
text cannot read tapes written by current `main`. Generation 2 is documented in
`specs/in-progress/`; it has not yet replaced the generation-1 publication
candidate or vectors.

The specification concept DOIs are reserved but their first deposits are still
pending. Until publication catches up, keep source material for every tape and
do not describe a current generation-2 tape as conforming to the generation-1
REM-PARITY review document.

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

## Maturity

Remanence has been developed against a Quadstor virtual tape library and
field-tested on an HPE MSL3040 with LTO-9 drives. The format and parity cores
are exercised against the virtual library on every test run; time on physical
tape hardware is episodic rather than continuous.

That is the honest shape of the risk. The formats are the part that has to last
decades, and they are the part with specifications, pinned vectors, an
an AI-system clean-room recovery drill (technically independent of the source,
but not an independent institution), and formal proofs over their cores. The
software around them is younger than the formats it implements. Restore and
verify real material from your own hardware before relying on either.
