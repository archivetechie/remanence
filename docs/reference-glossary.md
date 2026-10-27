# Glossary

This page defines the Remanence terms and the tape and SCSI vocabulary used in
the other documentation. A reader new to Remanence can start with the
[formats and objects](#formats-and-objects) section. Names in `code font` are
literal identifiers, flags, or file names.

<!-- code-anchor: crates/remanence-format/src/model.rs crates/remanence-aead/src/header.rs crates/remanence-aead/src/wrap.rs crates/remanence-aead/src/key_frame.rs crates/remanence-aead/src/xwing.rs crates/remanence-parity/src/lib.rs crates/remanence-state/src/index.rs @ 4dc2a3c6 -->
## Formats and objects

Remanence uses three formats. REM-OBJECT packages files into one stored unit,
REM-ENCRYPT encrypts such a unit, and REM-PARITY lays units out on tape with
recovery data. Terms are grouped in that order: the unit and its encryption
first, then the tape layout.

**REM-OBJECT** — the native stored-object format. One REM-OBJECT object packages
many files, with their paths and checksums, into a single unit that is written
to tape in one pass. Specified by REM-OBJECT Core Format 1.0. The `1.0`/`1.1`
pair that appears on the wire is the stream-schema feature gate
(`REMANENCE.schema_version`), not a pair of documents.

**object / Object** — a lowercase "object" is one REM-OBJECT unit. REM-PARITY
defines an **Object** (capital O) more generally: one opaque string of blocks
that occupies exactly one tape file, whose contents the tape layout does not
interpret. Remanence stores a REM-OBJECT as each Object. Remanence's builders
require a non-empty caller object id, and Remanence gives each object a UUID;
the format itself treats the object identifier as opaque UTF-8 text.

**version** — one committed REM object for a pool, archive path and content.
Newest means the latest `created_at_utc` among versions with a committed copy
in that pool, with object row order breaking ties. It is not a stored version
number or a latest flag. Get-by-path selects newest on the loaded tape;
`get --version` selects an object UUID explicitly.

**representation** — the form in which an object's bytes are stored.
`plaintext` is the readable form (`rem-object-v1`, below). `encrypted` is the
same object sealed by REM-ENCRYPT (`REMO`, below). Each copy of an object has
one representation, recorded in the catalog.

**plaintext_digest** — the SHA-256 checksum of the object in its plain form
(files and manifest together). It is the same for the plaintext and the
encrypted representation of one object, so it identifies the content, and a
reader that has decrypted an object checks the result against it.

**stored_digest** — the SHA-256 checksum of the bytes actually stored on tape,
whichever representation they are. A storage system can check a copy against
it without understanding the format or holding a key.

**rem-object-v1** — the plaintext REM-OBJECT body: a POSIX pax tar archive with
`REMANENCE.*` pax headers, chunk-aligned members, and a trailing CBOR
manifest. Readable with `bsdtar`. (tar is the standard Unix archive format;
pax is its extended-header form, used here to carry extra named fields; CBOR is
a compact binary data format, readable with common libraries.)

**manifest** — the deterministic CBOR member index written as the last
entry of every REM-OBJECT object (`_remanence/manifest.cbor`). It holds each
member's metadata and, for regular files, a checksum. A hardlink refers to
its primary entry, and directories and symlinks have no checksum.

**chunk size / block size** — the fixed transfer unit, 256 KiB by
default. The REM-OBJECT chunk size aligns member data inside an object; the
tape block size is the fixed SCSI block recorded in the bootstrap. In an
encrypted object a **chunk** is also the unit that is encrypted and
authenticated on its own.

**blob wrapper** — an ingest artifact: a dense subtree of non-compliant
files packed into a `.remwrap.tar` member (with a generated
`.remwrap.idx` sibling index) instead of thousands of individual
entries. The convention, the index fields, and the single-file recovery
procedure are described in REM-OBJECT Core Format Appendix E (informative).

**REMO** — the encrypted representation of a REM-OBJECT object: a 128-byte
header (magic `REMO`), HKDF-SHA-256 key derivation, and a
ChaCha20-Poly1305 STREAM over the tar bytes. The accepted representation
is REM-ENCRYPT 1.0 with on-tape `format_version = 2` and an HPKE recipient
key frame. Readers reject every other `format_version`
(`UnsupportedFormatVersion`).

**recipient** — a public key that an object is encrypted for. The matching
private key opens the object. An encrypted object names two to eight
recipients when written, so that more than one key can open it. A recipient's
public key is stored in a `REMR` file and its private key in a `REMP` file; the
byte layouts are in the [CLI reference](reference-cli.md#recipient-key-file-formats).

**recipient epoch** — the identity of one recipient key: a 16-byte epoch id and
an optional text label. Replacing a recipient's key creates a new epoch. A
`REMP` file carries its epoch id, and opening an object selects the key-frame
slot that has the same epoch id.

**DEK (data-encryption key)** — the random key generated for one object, used
to encrypt that object's content. It is never stored in the clear.

**wrap / wrapped key** — to wrap a key is to encrypt it under a recipient's
public key. Each object's DEK is wrapped once per recipient. The wrapped copies
are stored in the key frame, so any one recipient's private key can recover the
DEK.

**REM-ENCRYPT 1.0 HPKE envelope (`format_version = 2`)** — the REMO layout in
which no secret is shared between writer and reader. A fresh DEK is generated
for each object and wrapped for each recipient with HPKE (RFC 9180 Base mode,
HKDF-SHA256, ChaCha20-Poly1305) running the **X-Wing hybrid KEM** (below). The
wrapped keys sit in a **key frame** (wire tag `REMK`, 1-8 recipient slots
accepted by readers) between the header and the metadata frame. Production
sealers require 2-8 distinct, nonzero recipient epochs. `archive build`,
pool-selected `archive write`, and a full `archive reseal` produce it;
`archive extract`/`restore`/`read`/`verify`, the streaming range commands,
and `rem-recover` open it with a matching REMP private key.

**HPKE** — Hybrid Public Key Encryption (RFC 9180), a standard way to encrypt a
small secret, here the DEK, to a public key.

**KEM** — key-encapsulation mechanism: the public-key operation that HPKE uses
to derive a shared secret for one recipient.

**X-Wing hybrid KEM** — the KEM that wraps
each recipient's copy of the DEK: ML-KEM-768 (a post-quantum lattice
scheme, FIPS 203) combined with X25519 (classical elliptic-curve
Diffie-Hellman) by the combiner in `draft-connolly-cfrg-xwing-kem`,
plugged into HPKE as KEM id `0x647a`. Combining the two means an attacker
must break both. A recipient's public key (`REMR`
file) is a 1216-byte serialized X-Wing encapsulation key; the matching
private key (`REMP` file) is a 32-byte seed. Wrap-suite id `0x01` (an
X25519-only KEM) is permanently forbidden and rejected by readers and sealers, so every
REMO object this codebase produces or accepts uses the X-Wing hybrid.

**covering range** — the mapping, computed once by `remanence-aead`,
from a requested plaintext byte range to the smallest span of encrypted
chunks (and their stored ciphertext byte offsets) that must be fetched
and authenticated to serve it. It is what `rem archive covering-range` prints
and what the ranged mode of `extract-stream` consumes. See the
[extract-stream protocol](reference-extract-stream-protocol.md).

**REM-PARITY** — the tape-layout-plus-parity format. A tape holds the
bootstrap, Object tape files, Reed-Solomon parity sidecars, a final ParityMap,
and the terminal suffix (replicas A, B and C separated by extents AB and BC).
Published as the REM-PARITY 1.0 specification.

**tape file** — a run of one or more tape blocks ended by exactly one
filemark. A tape holds a sequence of tape files; the kinds are the bootstrap,
Object, parity sidecar, ParityMap, terminal replica, and separation extent.

**filemark** — a marker the drive writes on tape to end a tape file. The drive
can skip forward or backward over filemarks without reading the data between
them, which is how a reader counts and measures tape files.

**parity** — extra recovery data written alongside the data blocks. With
Reed-Solomon parity, a reader can rebuild damaged blocks within stated limits.
A tape can also be written without parity.

**bootstrap** — the first tape file, a single block at LBA 0 (the first block
on the tape). It states the tape UUID, the block size, and the parity scheme,
and it does not change when Objects are added. It holds no per-Object
information and is written once, not repeated at checkpoints or finalization. On a tape written
without parity the bootstrap carries the no-parity flag and no parity scheme
record; a no-parity bootstrap whose payload carries one is refused (REM-PARITY
Section 8.2, where the error is `BootstrapParse`). The CLI reference gives the
line the tool prints.

**parity sidecar (sidecar)** — a tape file of Reed-Solomon parity shards and
per-block checksums that covers the data blocks of one epoch (below). A reader
uses it to find damaged data blocks and rebuild them.

**stripe / neighborhood** — parity geometry. A stripe is one Reed-Solomon
codeword: k data blocks plus m parity blocks. A neighborhood is S consecutive
stripes whose parity lands in one sidecar.

**epoch (parity epoch)** — one group of data blocks that are protected
together and described by one sidecar. At the default settings a full epoch
covers 16 GiB of Object data. A bare "epoch" in the tape layout means this;
the other uses are **recipient epoch** (above), **write epoch** and **daemon
epoch** (below).

**ParityMap** — the tape file written after the last sidecar, when the tape
has any sidecars. It lists every sidecar and the range of data each protects.
It is indexed by the terminal replicas but is not itself terminal inventory
authority.

**terminal suffix** — the last five tape files of a finished tape, in this
order: terminal replica A, separation extent AB, terminal replica B, separation
extent BC, terminal replica C. Writing the suffix finalizes the tape, and no
Object is written after it.

**terminal replica** — one of the three tape files A, B and C of the terminal
suffix. Each is a complete copy of the tape's index (the terminal inventory),
which lists every tape file before the suffix and the recovery data for every
Object. A reader prefers C, then B, then A, and requires valid copies to agree.

**separation extent** — one of the two spacer tape files, AB and BC, that sit
between the terminal replicas. Each is a header record, zero-filled interior
records, and a footer record, 1 GiB by default. They keep the replicas
physically apart so that one damaged region is unlikely to destroy all three.

**filemark map** — the structural catalog of a tape: which tape file at
which position is an Object, sidecar, BOT bootstrap, or final ParityMap. The
terminal replica payload fixes the complete map of the tape files before the
suffix.

**foreign-format adapter** — an auxiliary, read-only parser linked into a
separate Remanence distribution. Core Remanence owns the normalized reader and
registry contract but ships with an empty registry and no concrete adapters.
See [foreign-format adapters](reference-foreign-format-adapters.md).

<!-- code-anchor: crates/remanence-state/src/config.rs crates/remanence-api/src/pool_write.rs crates/remanence-state/src/index.rs crates/remanence-state/src/checkpoint.rs crates/remanence-parity/src/journal.rs @ 6ed03ef0 -->
## Catalog and daemon

**catalog** — the queryable model of what is on which tape. The durable
truth is the required per-tape journals plus the audit log; the SQLite index
is a rebuildable projection of them.

**tape-file journal** — the Layer 3c append-file
(`<tape-uuid>.remjournal`) recording tape-file entries, parity state, and
checkpoint watermarks for a parity-enabled tape.

**checkpoint journal** — the append-file
(`checkpoints/<hyphenated-uuid>.remcheckpoint`) recording synchronized
checkpoint EOD positions and replayable catalog projections. On parity tapes,
Layer 3c bundles beyond the last checkpoint watermark are kept as orphan
evidence, and the journal refuses appends until they are reconciled with the
physical tail; the checkpointed histories must agree with the tape-file
journal before append resume. SQLite is not commit authority.

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
*native* (an REM-OBJECT object Remanence wrote) or *foreign* (an archive
scanned through a registered auxiliary adapter).

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
is. Write sessions and read-session opens (including cold resumes) return it.

**daemon epoch** — a random value minted once when `rem-daemon` starts
and returned on every `ReadSession`, letting a client detect that it's
now talking to a restarted daemon instance by diffing epochs. (The
request side of this field, `prior_daemon_epoch`, is decoded
but not checked, so a client cannot ask the daemon
to reject a stale-epoch resume.)

**advisory arbitration surface / drive assignment** — a read-only
projection (`GetLiveStatusResponse.drive_assignments`) of the existing
per-`(library_serial, bay)` atomic reservation, exposed for an external
arbitration client to make scheduling decisions against. It cannot gate,
queue, or block a mount — the atomic reservation itself remains the sole
enforcement path, unchanged by this projection existing. `rem top` does
not render it.

**spool** — the daemon's pre-commit staging directory for appends
(default `<state_dir>/spool`), sized from the shared `io_memory_ceiling`
budget (see **I/O memory ceiling** below).

**drive stewardship** — the drive-fleet lifecycle machinery: identity by
drive UUID, health snapshots, TapeAlert polling, cleaning runs, history,
annotation, retirement.

**alarm** — a standing, catalog-persisted condition raised against a
drive, tape, or library resource (for example a repeated cleaning
failure or a fenced tape awaiting release) until an operator
acknowledges or clears it. Listed and acknowledged with `rem alarms`.

<!-- code-anchor: crates/remanence-library/src/handle/tape_io/readiness.rs crates/remanence-library/src/handle/mod.rs crates/remanence-state/src/index.rs crates/remanence-api/src/calibration.rs crates/remanence-state/src/calibration.rs @ 1dd451b2 -->
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
readiness waits default to 2.5h. Unrelated to the read-order **calibration**
state below, despite the shared word.

**calibration (read-order)** — the per-volume state (`Uncalibrated` /
`Calibrated` / `UnsupportedFormat`) tracking whether a durable **wrap map**
is safe to serve for [read-order
planning](architecture-overview.md#read-order-planning). Harvested once per
drive load and invalidated by the **write epoch** below; not to be confused
with **media optimization / conditioning** above.

**write epoch** — a durable, per-volume counter that the media-write fence
advances before the first media-modifying command of a load. A **wrap map**
whose recorded epoch doesn't match the volume's current epoch is stale and
is never served.

**wrap map** — the per-drive-load record of which logical block ends each
tape wrap, harvested via `READ END OF WRAP POSITION` and validated by the
`remanence-order` crate; the input to [read-order
planning](architecture-overview.md#read-order-planning).

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
a time. It is the only path for streaming object reads and writes; there is no
non-pipelined fallback. Full verification reads outside it through its own
strict batch reader.

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

**LTO** — Linear Tape-Open, the cartridge and drive standard (LTO-9 holds about
18 TB per cartridge, uncompressed).

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

**voltag** — volume tag, the shell label read by a changer scanner or asserted
by an operator. See [labels and endpoints](#labels-and-endpoints).

**volume label** — the identifying record traditional tape systems write
first on a cartridge (the ANSI VOL1 record, the LTFS volume label).
Remanence has no separate label structure; the sole BOT bootstrap carries
that role. The recovery index is a separate structure: three terminal replicas
and two separation extents written at the tape's tail during finalization.
These are not repeated bootstrap copies. See **bootstrap** and **terminal replica**.

**BOT / EOM / EOD** — beginning of tape, end of media (the physical end), and
end of data (the position just after the last data written).

**LBA** — logical block address: the SCSI logical position on the tape,
counted from 0 at the beginning of tape. It counts both blocks and filemarks.
It is distinct from the dense ordinal that REM-PARITY assigns to Object data
blocks only.

**st driver / mt** — the Linux kernel tape driver (`/dev/st*`,
`/dev/nst*`) and its command-line tool; the conventional way to operate
a drive. Remanence bypasses it — see
[why-sg-passthrough.md](why-sg-passthrough.md).

**mtx** — the classic command-line medium-changer tool. It names drives
by element address, with no join to the st driver's device files; the
gap is described in [why-sg-passthrough.md](why-sg-passthrough.md).

**SG_IO / sg device** — the Linux ioctl that passes a raw SCSI command (CDB)
to a device through its `/dev/sgN` node and returns status and sense data.
It is Remanence's only transport, for drives and changers alike. A `/dev/sgN`
path is what commands such as `rem-debug tape recovery-report` take for a drive.

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

**CRC** — cyclic redundancy check, a short checksum used to detect damage in a block.

**VTL** — virtual tape library, software that emulates a tape library
chassis, its drives, and its cartridges.

<!-- code-anchor: crates/remanence-api/src/mount_backend.rs crates/remanence-library/src/discovery.rs crates/remanence-state/src/index.rs @ 4dc2a3c6 -->
## Labels and endpoints

**label (voltag)** — the code on a cartridge shell, read by a changer scanner
or a person. It is not the logical tape identity.

**label provenance (voltag_source)** — `changer` when a scanner reads the
label, `operator` when a person asserts it. Neither is an identity trust level;
the bootstrap UUID is the identity in both cases.

**tape UUID** — the UUID in the BOT bootstrap, identifying one logical tape
independently of its label, pool, library or device path.

**endpoint** — a place a cartridge can be mounted: a library bay or standalone
drive. An endpoint is not a device path.

**standalone drive** — a drive proven unclaimed by complete discovery, with no
unresolved changer bays, or pinned as standalone by configuration. A drive
that no changer claims during incomplete discovery has **ownership unknown**;
that alone does not authorize writes.

**DriveKey** — a durable endpoint key: `Bay { library_serial, bay }` or
`Standalone { drive_serial }`. Drive serial comes from VPD page 0x80. Neither
an OS path nor a sentinel bay stands in for a standalone key.

**MountEndpoint** — `Durable(DriveKey)` or `Ephemeral { sg_path }`. Ephemeral
endpoints permit authorized reads only.

**append admission** — proof that the tape's physical suffix permits a write
at the journal-authorized position. Complete data beyond known state produces
`TailBeyondKnownState`; state rows and fence release do not authorize erasure.
