# Glossary

Project-internal terms and the tape-industry vocabulary Remanence leans
on. Definitions reflect what the code does today, not aspirations.

<!-- code-anchor: crates/remanence-format/src/model.rs crates/remanence-aead/src/header.rs crates/remanence-parity/src/lib.rs @ 2a20106 -->
## Formats and objects

**RAO** — Rem Archive Object, the native stored-object format. One RAO
object holds many files. Published as the RAO 1.0/1.1 specifications.

**rao-v1** — the plaintext RAO body: a POSIX pax tar archive with
`REMANENCE.*` pax headers, chunk-aligned members, and a trailing CBOR
manifest. Readable with stock `tar`.

**RAO1** — the encrypted representation of an RAO object: a 128-byte
header (magic `RAO1`), HKDF-SHA-256 key derivation, and a
ChaCha20-Poly1305 STREAM over the tar bytes. The accepted representation
is format version 2 with an HPKE recipient key frame.

**format version 1 (reserved)** — a permanently unsupported RAO1 wire
version retained only as a reserved version number. Current parsers reject
it with `UnsupportedFormatVersion`; there is no compatibility reader,
writer, recovery mode, or CLI flag for it.

**format version 2 (HPKE envelope)** — a RAO1 shape with no shared
secret: a fresh per-object **data-encryption key (DEK)** is generated
and wrapped once per recipient with HPKE (RFC 9180, X25519-HKDF-SHA256-
ChaCha20Poly1305) into a **key frame** (wire tag `RAOK`, 1-8 recipient
slots accepted by readers) sitting between the header and metadata frame.
Production sealers require 2-8 distinct recipient epochs. `archive build`,
pool-selected `archive write`, and a full `archive reseal` produce it;
`archive extract`/`restore`/`read`/`verify`, the streaming range commands,
and `rao-recover` open it with a matching RAOP private key.

**covering range** — the mapping, computed once by `remanence-aead`,
from a requested plaintext byte range to the smallest span of AEAD
chunks (and their stored ciphertext byte offsets) that must be fetched
and authenticated to serve it. What `rem archive covering-range` prints
and `extract-stream`'s ranged mode consumes.

**REM-PARITY** — the tape-layout-plus-parity format: bootstrap blocks,
object tape files, Reed-Solomon parity sidecars, and parity maps.
Published as the REM-PARITY 1.0 specification.

**bootstrap** — the self-description block a tape carries at LBA 0 (and
at intervals down the tape): tape UUID, block size, parity scheme,
filemark-map digest. What makes a tape readable without the catalog.

**parity sidecar** — a tape file of Reed-Solomon parity shards covering
the data blocks written since the previous sidecar.

**parity map** — a tape file acting as a directory of sidecar epochs when
that directory outgrows the bootstrap.

**filemark map** — the structural catalog of a tape: which tape file at
which position is an object, sidecar, bootstrap, or parity map.

**stripe / neighborhood** — parity geometry. A stripe is k data blocks
plus m parity blocks; a neighborhood is S consecutive stripes whose
parity lands in one sidecar.

**chunk size / block size** — the fixed transfer unit, 256 KiB by
default. The RAO chunk size aligns member data inside an object; the
tape block size is the fixed SCSI block recorded in the bootstrap.

**manifest** — the deterministic CBOR member index written as the last
entry of every RAO object (`_remanence/manifest.cbor`).

**blob wrapper** — an ingest artifact: a dense subtree of non-compliant
files packed into a `.remwrap.tar` member (with a generated
`.remwrap.idx` sibling index) instead of thousands of individual
entries.

**BRU** — Backup and Restore Utility, a legacy Unix archive format.
Remanence reads BRU/BRU-PE dumps through the `bru` foreign-format driver
for migrating old archives; it never writes them.

<!-- code-anchor: crates/remanence-state/src/config.rs crates/remanence-api/src/pool_write.rs crates/remanence-state/src/index.rs @ 2a20106 -->
## Catalog and daemon

**catalog** — the queryable model of what is on which tape. The durable
truth is per-tape journals plus the audit log; the SQLite index is a
rebuildable projection of them.

**journal** — the append-file (`<tape-uuid>.remjournal`) recording each
tape's committed contents on disk.

**audit log** — append-only daily `.remaudit` segments recording every
state-changing operation and who asked for it.

**tape pool** — an operator-defined group of tapes that writes target.
A write session names a pool; Remanence picks the tape.

**copy class / content class** — pool labels that let an orchestrator
keep redundant copies on disjoint pools (`copy-a` / `copy-b`) and
segregate content kinds.

**watermarks** — per-pool fill fractions: at `watermark_low` (0.92) a
tape becomes a candidate for sealing; `watermark_high` (0.97) caps
usable capacity below end-of-media.

**locator** — the JSON record emitted after a write that pins an object
copy to physical media (tape UUID, position, geometry). The input to
direct reads, exports, and verifies.

**caller object id** — the orchestrator's opaque id for an object,
recorded alongside Remanence's own UUID. Same-content retries with the
same caller id are idempotent; different content under a reused id is a
conflict.

**catalog unit** — one enumerable archive in the catalog, either
*native* (an RAO object Remanence wrote) or *foreign* (a scanned legacy
archive such as a BRU tape).

**operation** — a daemon-tracked long-running action with a UUID,
queryable and cancellable via `rem op`.

**session** — the daemon's write or read transaction: open, append or
read, checkpoint, close. Sessions have idle timeouts and abort paths.
Every open always mints a brand-new session id — sessions never
continue across a client restart; see **cold resume** below for what a
read session offers instead. Write sessions have no restart contract at
all (`recover_session_id` returns unimplemented).

**cold resume / resume target** — a read-only alternative to session
continuation: a client that lost its session id (typically across an
app restart) can reopen with a *resume target* — durable coordinates
`(tape_uuid, object_id, file_id, file_boundary_byte_offset)` it saved
itself — instead of nothing. The daemon still mints a fresh session, but
first re-verifies the mounted tape's identity against `tape_uuid`
(rejecting a swapped cartridge before trusting any position) and
relocates to the exact catalogued file boundary (never mid-file).

**position proof** — a real SCSI READ POSITION result (not an arithmetic
estimate) returned to a caller as evidence of where the drive actually
is. Long-standing on the write side; RM3.1b added it to read-session
opens (including cold resumes) too.

**daemon epoch** — a random value minted once when `rem-daemon` starts
and returned on every `ReadSession`, letting a client detect that it's
now talking to a restarted daemon instance by diffing epochs. (The
request side of this field, `prior_daemon_epoch`, is currently decoded
but never checked against anything — a client can't yet ask the daemon
to reject a stale-epoch resume.)

**advisory arbitration surface / drive assignment** — a read-only
projection (`GetLiveStatusResponse.drive_assignments`) of the existing
per-`(library_serial, bay)` atomic reservation, exposed for an external
arbitration client to make scheduling decisions against. It cannot gate,
queue, or block a mount — the atomic reservation itself remains the sole
enforcement path, unchanged by this projection existing. `rem top` does
not currently render it.

**spool** — the daemon's pre-commit staging directory for appends
(default `<state_dir>/spool`), sized from the shared `io_memory_ceiling`
budget (see **I/O memory ceiling** below).

**drive stewardship** — the drive-fleet lifecycle machinery: identity by
drive UUID, health snapshots, TapeAlert polling, cleaning runs, history,
annotation, retirement.

<!-- code-anchor: crates/remanence-library/src/handle/tape_io/readiness.rs crates/remanence-library/src/handle/mod.rs crates/remanence-state/src/index.rs @ 2a20106 -->
## Safety machinery

**allowlist** — the explicit list of library serials a process may
mutate: `[[libraries]]` in the daemon config, `--allow` on `rem-debug`.
Anything not listed is visible but untouchable.

**derived identity** — a drive bay whose identity could not be read
directly from the device and had to be inferred. Operating one requires
a separate opt-in (`allow_derived_drive_identity`, `--allow-derived`).

**dirty snapshot** — the in-memory library state after an operation
whose completion is uncertain, marked with a cause (`CompletionUnknown`,
`PartialFailure`). Refresh or rescan before trusting it.

**fence / quarantine** — a durable refusal record written when media or
tape I/O ended in an unproven state. Fenced media is refused for writes,
reads, and init until an operator resumes or releases the fence with an
acknowledgement.

**media readiness** — the TEST UNIT READY classification state machine
(ready, becoming-ready, terminal-not-ready, reservation conflict,
transport-unknown, and so on) with durable operation records and the
exit-code taxonomy in the [CLI reference](reference-cli.md#exit-codes).

**destructive-safety gauntlet** — the check chain `rem tape init` runs
before touching a cartridge: identity, ownership, readiness, and
data-presence checks, each with its own override semantics (`--force`
for `RequireForce` decisions, `--clobber-data` only for data-holding
tapes, never in batch).

**media optimization / conditioning** — the one-time calibration pass an
LTO-9 cartridge performs on first load; can exceed an hour and is why
readiness waits default to 2.5h.

**poison** — a fail-closed "don't proceed" mark set after a failed data
command, at two independent scopes. *Transfer poison* discards the rest
of one already-failing `AppendObject` call's queued staging-ring
batches. *Session poison* (`AppendGate`) fails every subsequent append
for the rest of that write session, permanently — the fix is to close
the session and open a new one, not retry. Distinct from a **dirty
snapshot** (a library/robotics-layer concern) and from a **fence** (a
durable, catalog-persisted refusal); see
[troubleshooting](guide-troubleshooting.md#dirty-snapshots-and-completion-unknown).

**staging ring** — the fixed pool of page-aligned buffers
(`tape_io.staging_ring_buffers`, default 4) a write session's producer
thread fills while a submitter issues one blocking tape-write command at
a time. The only tape I/O path — there is no non-pipelined fallback.

**read reservoir / watermark stop-start** — the host-RAM buffer a read
session's submitter fills up to a high watermark
(`tape_io.read_reservoir_high_pct`, default 90%) before parking the
drive, resuming only once the consumer has drained it below the low
watermark (`tape_io.read_reservoir_low_pct`, default 25%). Every resume
from a park forces a fresh **position proof**, not just a periodic one.

**proof-frontier** — for byte-range reads, the rule that data is never
handed to the decoder until a real position proof has "covered" it —
preventing a caller from receiving bytes the drive hasn't actually
confirmed reaching.

**position tripwire** — the periodic real READ POSITION check
(`tape_io.position_check_bytes`, default 1 GiB) during a long write or
read that catches silent arithmetic position drift between checks.

**I/O memory ceiling** (`daemon.io_memory_ceiling`, default 24 GiB) —
the one shared budget every pipeline consumer draws from: the append
spool and every drive's read reservoir. Enforced by a single atomic
permit manager; exceeding it at daemon startup (via `validate_config`)
or at read-pipeline-start (if the reservoir can't fit its minimum
staging pool) is a hard error, not a soft degrade.

<!-- code-anchor: none -->
## Tape and SCSI background

**LTO** — Linear Tape-Open, the cartridge/drive standard (LTO-9 ≈ 18 TB
native per cartridge).

**library / changer** — the robot chassis holding cartridges, drives,
slots, and mail slots. SCSI calls it a medium changer.

**partition** — a mode where one physical chassis presents as several
independent logical libraries; the reason for the allowlist safety
model.

**element address** — the changer's numeric address for any location
(slot, drive bay, IE port, picker), for example `0x0400`.

**drive bay** — the changer element holding a drive, as distinct from
the drive itself.

**IE port / mail slot** — the import/export station where cartridges
enter and leave the chassis.

**voltag** — volume tag, the barcode on a cartridge as reported by the
library's scanner. A label, not an identity.

**BOT / EOM** — beginning of tape / end of media.

**filemark** — the on-tape separator between tape files.

**VPD page 0x80** — the SCSI vital-product-data page carrying a device's
serial number; the basis of all identity joins.

**TUR** — TEST UNIT READY, the SCSI no-op probe whose sense data drives
readiness classification.

**unit attention (UA)** — the SCSI condition a device raises after state
changes (media change, reset); consumed and classified, repeated
identical UAs become terminal.

**TapeAlert** — the standardized drive/media health flag page (LOG SENSE
0x2E) polled by drive stewardship.

**RES** — READ ELEMENT STATUS, the changer inventory command.

**SG_IO / sg device** — the Linux generic SCSI ioctl and the `/dev/sgN`
nodes Remanence issues commands through.

**VTL** — virtual tape library, software emulating a chassis (QuadStor
in this project's development setup).

**Miria** — the commercial archive product whose tape role Remanence is
built to replace.
