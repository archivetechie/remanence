<!-- code-anchor: Cargo.toml crates proto/layer5.proto .github/workflows/release.yml @ 625f7044 -->
# What works, and what does not

This page is for someone deciding whether Remanence is ready for a particular
job. It lists what the software does today, what it does not do, and how well
each part has been tested, and it names limitations in preference to
capabilities. The root [README](../README.md) has a shorter version.

## The short version

Remanence is alpha software. It has had limited operational testing, so do not
make it the only copy of anything. There is no binary release yet; you build
it from source. [Releases and versions](#releases-and-versions) says what has
been published and how the version numbers relate.

The parts that put bytes on tape and get them back are the most mature. A tape
written today can be recovered without the catalog, without the daemon, and
without a configuration file. The parts still changing are the control
surfaces above them: the gRPC API, the authorization model, and some of the
library-robotics operations.

The format specifications are review drafts until they freeze on 31 July 2027.
For REM-OBJECT and REM-ENCRYPT, the review checks whether the documents
describe the implemented formats correctly. For REM-PARITY, the published copy
does not yet describe the layout current software writes (see
[below](#the-rem-parity-publication-copy)). A review finding could still change
what goes on tape, and the promise that no tape a format validates will ever be
invalidated takes effect only at the freeze. Treat material written during the
review window as re-writable, and keep the source it was made from until the
formats are final.

<!-- code-anchor: Cargo.toml crates/remanence-cli/Cargo.toml .github/workflows/release.yml @ 625f7044 -->
## Releases and versions

There is no binary release of Remanence yet. The only published release is
v0.1.0, and its only file is the conformance test-vector archive,
`remanence-test-vectors.tar`. An earlier tag, v1.0.0, was withdrawn and
renumbered to v0.1.0 because that number overstated the software's maturity;
[CHANGELOG.md](../CHANGELOG.md) records why. To run Remanence you build it
from source, as [Installing Remanence](guide-install.md) describes.

The source is at version 0.2.0. That is the workspace version in `Cargo.toml`,
and it is what `rem --version`, `rem-debug --version` and
`rem-daemon --version` print. Version 0.2.0 was prepared but never tagged or
published, so a binary that reports 0.2.0 was built from source. The
repository's release workflow builds statically linked Linux binaries when a
release is tagged, and [Release verification](guide-release-verification.md)
describes what such a release contains and how to check it.

The format specifications are versioned separately from the software and are
at 1.0: REM-OBJECT Core Format 1.0, REM-ENCRYPT 1.0 and REM-PARITY 1.0.
[How the formats change](versioning-explained.md) explains the two numbering
schemes.

## What works

### Hardware control

Remanence sends SCSI commands to drives and changers itself, through the Linux
SCSI generic driver (the `/dev/sgN` devices). It discovers libraries and
standalone drives, identifies each one by serial number, and moves cartridges.
In a build with the optional `linux-udev` feature, `rem watch` reports hot-plug
events; the daemon does not subscribe to them. The daemon acts only on the
libraries named in its configuration file. Direct standalone commands bind the
current `/dev/sgN` path to a drive serial that the operator has authorized, and
they refuse to write unless discovery proves that no changer owns the drive.

### Writing and reading tape

Tape input and output use fixed-size blocks and run as a pipeline through a
staging ring, a set of host buffers that keeps several SCSI commands in flight.
Write sessions and read-session opens return position proofs: real READ
POSITION results (the drive's report of where the tape is) rather than
computed positions. This pipeline is the only path that writes or reads object
data. Reads are served
through a host-RAM read reservoir that supports ranged access, so a
partial-file restore does not have to stream a whole object. Full verification
uses its own strict batch reader, which issues the same fixed-block commands
without the staging ring or the reservoir.

### The on-tape formats

The `rem-object-v1` object body and the REM-ENCRYPT encrypted representation
match their review specifications and the pinned test vectors. Reed-Solomon
sidecar parity, resuming a parity tape after an interruption, and recovery from
the tape's terminal index without a catalog are implemented as the REM-PARITY
revision in `specs/in-progress/` describes them. A sidecar is a tape file of
parity data covering the blocks written before it. The terminal index is the
closing inventory that finalization writes at the end of a tape.

### Encryption

Each object gets a fresh key. That key is wrapped to between two and eight
recipients with HPKE (hybrid public-key encryption, RFC 9180) using the X-Wing
key-encapsulation mechanism, which combines ML-KEM-768 with X25519. The wrapped
keys are stored in the object's own header, so any one recipient's private key
opens the object without a key server.

### Host state

The host keeps three kinds of record. The tape log is a file per tape that holds
a durable commit record for each tape file written; it is the host's authority
for that tape. The audit log is an append-only, hash-chained record of
state-changing operations. The catalog is a SQLite projection rebuilt from the
tape logs and the audit log, so it is never the only copy of anything. The host
also keeps media-readiness records and fences. A fence is a durable refusal
record written when tape input or output ended in a state the software cannot
prove; the tape is refused for reads, writes and initialization until the fence
clears or an operator releases it.

### The daemon

`rem-daemon` serves catalog queries, write sessions that target a pool or a
pinned tape, and read sessions for a whole object, one file, or a byte range. A
read session can resume after the client restarts: the daemon re-checks the
tape's identity and proves the drive's position before it continues. The daemon
also serves library inspection and robotics, drive stewardship, alarms and live
status. It listens on a Unix socket, and optionally on TCP with mutual TLS
(mTLS), in which client and server both present certificates.

### Operator tools

`rem` is the operator command-line tool and `rem-debug` is the break-glass tool
with wider direct hardware access. Tape initialization runs a chain of identity,
ownership, readiness and data-presence checks before it writes, and suspect
media can be quarantined. `rem` also runs initialization, put and get with
checkpoints, finalization, verification, adoption and catalogless restore
directly against one manually loaded standalone drive. In that mode, called
home mode, it keeps its state under `${XDG_STATE_HOME:-$HOME/.local/state}/rem`
and needs no daemon. Building, inspecting and extracting objects work on local files with no
tape hardware. A separate `rem-recover` binary decrypts and extracts an
encrypted object with no daemon, catalog or configuration file. A plaintext
object is an ordinary pax archive that `bsdtar` can read.

Every command that writes objects to tape opens a write session: `rem put`
through the daemon, standalone `rem put`, and `rem-debug archive write`. A
write session can append to a parity tape that already holds committed
objects.

### Testing

The test suite includes fault-injection (chaos) tests and fuzzing of the format
parsers. Lean and Aeneas proofs cover the parity and format cores; they are in
[`verif/`](../verif).

## What does not work yet

<a id="the-rem-parity-generation-boundary"></a>

### The REM-PARITY publication copy

The bootstrap is the self-describing block at the beginning of a tape, and
`schema_major` is the field in it that names the tape layout. Current software
writes and accepts only `schema_major = 2`, in which the tape's terminal index
is three complete copies, called terminal replicas, separated by typed extents.
The [glossary](reference-glossary.md) defines these terms. The REM-PARITY copy
under `specs/publication/` (1.0.0-draft.2) and its frozen vectors describe
`schema_major = 1`. Current software rejects tapes written by version 0.1.0,
which use `schema_major = 1`, and an implementation written from the published
text cannot read tapes written by current software. The text that matches
current software is in `specs/in-progress/`. The publication copy and its
vectors have not been updated to it yet.

The specifications' concept DOIs are reserved and their first deposits are
still pending. Until the matching text is published, keep the source material
for every tape, and do not describe a tape written by current software as
conforming to the publication copy of REM-PARITY.

### Authorization is coarse

Authorization is a role matrix with no scope below the role. A client is
readonly, operator, orchestrator, admin or system, and a role cannot be narrowed
to particular pools, tapes or objects.

<!-- code-anchor: crates/remanence-api/src/write_session_ingress.rs crates/remanence-api/src/tape_closeout.rs crates/remanence-api/src/daemon_catalog_services.rs proto/layer5.proto @ e4d4f53f433998602c4e2baa56747f68e4e2684e -->
### Some daemon operations are not implemented

Library import and export through the mailslot, library-event streaming, and
write-session restart all return `unimplemented`. So do write sessions that
target a drive; sessions that target a pool or a pinned tape both work.
Write-session RPCs reject caller-supplied `idempotency_key` values. Their replay
detection does not use that field. `Catalog.CloseoutTape` and
`Catalog.FinalizeTape` require an idempotency key for retries.

## Known limitations found in field testing

The October 2026 test sessions, held between 26 and 31 October on an HPE
MSL3040 library with LTO-9 drives, ran the library paths end to end: plaintext
and encrypted writes and reads, multi-object writes, repeated appends, an
overnight soak, and recovery after a killed write, a refused wrong tape, a
retired and re-initialized cartridge and a catalog rebuild. A restore running
beside a write session on another pool completed normally, and a concurrent
restore and append against two pools on two drives ran at an aggregate of about
218 MB/s.

The sessions also covered the following:

- Partial-file restore. Byte ranges read from the start, the middle and the end
  of 4 GiB and 32 GiB objects, and from members of a 40-file object, matched
  their sources.
- Finalization and full verification. A parity tape holding thirty 1 GiB
  objects was finalized, and `rem tape verify-index` read every protected data
  block and parity shard and reported `verified_complete`. Ten of its objects
  were then restored one by one with matching digests. Tapes without parity,
  the largest holding 681 GB, were finalized and verified in the same way.
- Cancelling a verification. One full verification was cancelled during its
  walk and another during its protected pass. Each stopped at the next tape
  command, released the drive without a fence and recorded no verdict.
- Library operations. Robot moves between slots, `rem drive unload`, a changer
  move that waited while a magazine was open and completed once it was closed,
  a drive cleaning with a cleaning cartridge, and read-only diagnostics,
  TapeAlert and drive-stewardship reads of every drive.
- The per-tape log. The last session, on 31 October, repeated the write, read,
  append, killed-write, wrong-tape, catalog-rebuild and finalization legs with
  the per-tape log that current software keeps.

The findings below come from these sessions.

### Robot moves wait while a drive becomes ready

When a drive does not report its cartridge ready, the daemon records a
media-readiness fence. A resumable fence can clear without an operator: the
daemon starts a wait for each one, including at startup, with a 9000 s timeout
and steady polls every 30 s. A second write session that needs the robot waits
on the same operation through its client, and resume calls attach to that
wait. Robot moves stay refused until the fence clears, even if other drive bays
are idle. A release-only fence records an unknown outcome, a terminal drive
error or a reservation conflict, and it stays until an operator runs
`rem tape quarantine release`. A wait that an operator cancelled stays stopped
until a client resumes it explicitly.

Detached verification and manual finalization wait up to 15 minutes from the
first readiness refusal before they stop their attempt. A stopped finalization
attempt reports `FAILED`, keeps the tape's finalizing intent, and resumes under
the same operation ID when the caller retries with the same idempotency key.
`rem tape finalize --wait` exits with the attempt's error summary.

<!-- code-anchor: crates/remanence-api/src/startup_media_readiness.rs crates/remanence-api/src/mount.rs @ e4d4f53f433998602c4e2baa56747f68e4e2684e -->
### A cartridge left unthreaded by a hard kill

A hard kill during an unload can leave the cartridge in the drive unthreaded.
The drive then asks for an initializing command. When inventory shows the
cartridge in the drive, the readiness wait sends one LOAD and records the
attempt so a restart does not repeat it. A stop signal lets an unload already
in progress finish its MOVE home before the daemon exits. After a hard kill,
check `rem top` for a bay shown busy with no I/O and `rem tape quarantine list`
for a `media_initializing` fence. See the
[shutdown guidance](guide-troubleshooting.md#a-drive-stays-not-ready-after-a-daemon-crash).

### Full verification can be watched and cancelled, but not resumed

`rem tape verify-index --tape-uuid <UUID> --detach` starts a full verification
and prints an operation UUID. `rem op watch <UUID>` follows it,
`rem op cancel <UUID>` cancels it, and `rem tape verify-index --result <UUID>`
fetches the verdict. Cancellation waits for the tape command in flight and then
releases the drive. A cancelled verification starts over from the beginning.
The catalog records completed verdicts, and audit replay rebuilds their
timestamps, states and terminal edition digests. Drive allocation belongs to
the caller: the daemon does not interrupt a verification to admit a write or a
restore.

### Catalog rebuild reads the tape logs

`rem rebuild-catalog-from-journals` rebuilds the catalog from the audit log and
the tape logs. (The command keeps its older name; the directory it reads holds
the tape logs.) Its summary includes a `tape logs:` line with the snapshot
count. It leaves the log and audit bytes unchanged and does not clear repair
fences.

### Write throughput is below the drive rate

On the field-test host, the host adapter limits each SCSI command to 1 MiB, and
the daemon's staging ring keeps only a few such commands in flight. Together
these held writes to about 190 MB/s on LTO-9 drives rated at 300 MB/s.

### Two tests are timing-sensitive

`read_pipeline_diag_open_and_close_have_stable_json_schema` in `remanence-api`
and `checkpoints_restore_versions_after_projection_loss_and_full_rebuild` in
`remanence-cli` fail intermittently without any code change. A single CI
failure in either is not by itself evidence of a regression.

<!-- code-anchor: crates/remanence-library/src/lib.rs crates/remanence-cli/src/standalone.rs crates/remanence-library/tests @ 625f7044 -->
## Tested hardware

| Hardware | Evidence and limits |
|---|---|
| QuadStor virtual tape library (VTL) | Library integration and failure-recovery tests. A VTL configuration that exposes a drive without a changer is pending qualification; it does not establish real standalone-drive support. |
| HPE MSL3040 with LTO-9 drives | Physical library reads, writes and recovery testing. The October 2026 sessions covered reads and writes with SHA-256 read-back, encrypted round trips, partial-file restore, recovery after a daemon killed during a write, a refused overwrite of a written tape, retire and rebind, a catalog rebuild, finalization and full verification of tapes with and without parity, cancelled verifications, robot moves, a wait for an open magazine, drive cleaning, and an overnight soak. Physical test time is episodic. |
| HP Ultrium 7 | INQUIRY and VPD (vital product data) captures for discovery and identity. These captures do not establish media read or write coverage. |
| Real hardware below LTO-7 | No testing. That `--generation` accepts L5 and L6 is not a hardware qualification. |
| Autoloader | No testing. |
| Real standalone drive | No testing yet. Model tests and a VTL view do not substitute for a physical standalone drive. |

A hardware report that cannot be reproduced stays open once a
`rem diagnostics capture --out diagnostics.tar` archive is attached. For a
specific standalone drive, add `--device /dev/sgN --allow-device <SERIAL>`.
Include the failing command and its output with the capture.

`DriveMappingUnavailable` means the changer does not report the drive
identities needed to map its drive bays. It is a discovery limitation, and it
does not permit treating those drives as standalone. Unresolved bays block
standalone writes, and an ownership pin cannot bypass that refusal. Report the
changer model and attach the diagnostic capture so that its mapping can be
investigated.

## Maturity

Remanence has been developed against a QuadStor virtual tape library and
field-tested on an HPE MSL3040 with LTO-9 drives. The format and parity cores
are exercised against the virtual library on every test run; time on physical
tape hardware is episodic rather than continuous.

The standalone control path has model-backed integration tests for discovery,
authorization, initialization, checkpointed write and restore, metadata
changes, release, and failure fencing. That is stronger than a parser-only
claim, but it is not broad physical-drive validation. Standalone users should
expect hardware-specific findings and include a `rem diagnostics capture`
archive when they report them.

The formats are the part that has to last the planning horizon of 10 to 20
years. They have specifications, pinned vectors, formal proofs over their cores,
and a clean-room recovery drill: a reader written from the specifications alone,
without the source, though not by an independent organization. The software
around them is younger than the formats it implements. Restore and verify real
material on your own hardware before relying on either.
