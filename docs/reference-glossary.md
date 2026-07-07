# Glossary

Project-internal terms and the tape-industry vocabulary Remanence leans
on. Definitions reflect what the code does today, not aspirations.

<!-- code-anchor: crates/remanence-format/src/model.rs crates/remanence-aead/src/header.rs crates/remanence-parity/src/lib.rs @ 7fb10f8 -->
## Formats and objects

**RAO** — Rem Archive Object, the native stored-object format. One RAO
object holds many files. Published as the RAO 1.0/1.1 specifications.

**rao-v1** — the plaintext RAO body: a POSIX pax tar archive with
`REMANENCE.*` pax headers, chunk-aligned members, and a trailing CBOR
manifest. Readable with stock `tar`.

**RAO1** — the encrypted representation of an RAO object: a 128-byte
header (magic `RAO1`), HKDF-SHA-256 key derivation, and a
ChaCha20-Poly1305 STREAM over the tar bytes.

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

<!-- code-anchor: crates/remanence-state/src/config.rs crates/remanence-api/src/pool_write.rs crates/remanence-state/src/index.rs @ 7fb10f8 -->
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

**spool** — the daemon's pre-commit staging directory for appends
(default `<state_dir>/spool`).

**drive stewardship** — the drive-fleet lifecycle machinery: identity by
drive UUID, health snapshots, TapeAlert polling, cleaning runs, history,
annotation, retirement.

<!-- code-anchor: crates/remanence-library/src/handle/tape_io/readiness.rs crates/remanence-library/src/handle/mod.rs crates/remanence-state/src/index.rs @ 7fb10f8 -->
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
