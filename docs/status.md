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
review is whether they describe the implemented formats correctly, not whether
the design works — so the expected outcome is corrections to the text. But a
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

**The on-tape formats.** The `rem-object-v1` object body, the REM-ENCRYPT
encrypted representation, and Reed-Solomon sidecar parity with recovery,
resume, and a catalog-less terminal-index scan. All three are specified,
implemented, and pinned by the test vectors that ship with the source.

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
all. A separate `rem-recover` binary decrypts and extracts archive objects with
no daemon, catalog, or configuration file — the disaster-recovery path of last
resort.

**Testing.** Chaos fault-injection, fuzzing of the format parsers, and
Lean/Aeneas proofs over the parity and format cores in [`verif/`](../verif).

## What does not work yet

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
independent clean-room recovery drill, and formal proofs over their cores. The
software around them is younger than the formats it implements. Restore and
verify real material from your own hardware before relying on either.
