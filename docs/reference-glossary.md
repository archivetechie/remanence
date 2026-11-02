# Glossary

This page defines the Remanence terms and the tape and SCSI vocabulary used in the other
documentation, one entry per term. It is a reference for every reader. A newcomer should
read [Concepts](concepts.md) first: it introduces the core terms in reading order, with the
reasons behind them, and the entries here point to it for the longer story. Names in
`code font` are literal identifiers, flags or file names.

The terms are grouped in five sections. The first four are the terms an operator meets:
formats and objects, the catalog and the daemon, safety and operations, and labels and
endpoints. Then come the tape and SCSI background, and last the implementation terms that
matter to integrators and contributors.

<!-- code-anchor: crates/remanence-format/src/model.rs crates/remanence-aead/src/header.rs crates/remanence-aead/src/wrap.rs crates/remanence-aead/src/key_frame.rs crates/remanence-aead/src/xwing.rs crates/remanence-parity/src/lib.rs crates/remanence-parity/src/authority/identity.rs crates/remanence-state/src/index.rs crates/remanence-library/src/handle/tape_io/model.rs @ 625f7044 -->
## Formats and objects

Remanence uses three formats. REM-OBJECT packages files into one stored unit, REM-ENCRYPT
encrypts such a unit, and REM-PARITY lays units out on tape with recovery data. The entries
below follow that order: the unit and its encryption first, then the tape layout.

### object

The unit Remanence stores: one REM-OBJECT archive holding one or more files, with their paths
and checksums, written to tape as exactly one tape file. Remanence gives each object a UUID,
and its builders require a non-empty caller object id; the format itself treats the object
identifier as opaque UTF-8 text. REM-PARITY uses the word **Object** (capital O) more
generally, for one opaque string of blocks that occupies exactly one tape file, whose
contents the tape layout does not interpret; Remanence stores a REM-OBJECT object as each
Object. In prose these pages write "object". See [Concepts](concepts.md#3-what-remanence-stores-objects-bundles-and-copies).

### REM-OBJECT

The native stored-object format, specified by REM-OBJECT Core Format 1.0. One REM-OBJECT
object packages many files into a single unit that is written to tape in one pass. The
`1.0`/`1.1` pair that appears on the wire is the stream-schema feature gate
(`REMANENCE.schema_version`), not a pair of documents.

### rem-object-v1

The plaintext REM-OBJECT body: a POSIX pax tar archive with `REMANENCE.*` pax headers,
chunk-aligned members and a trailing CBOR manifest, readable with `bsdtar`. Tar is the
standard Unix archive format; pax is its extended-header form, used here to carry extra named
fields; CBOR is a compact binary data format that common libraries read.

### manifest

The deterministic CBOR member index written as the last entry of every REM-OBJECT object
(`_remanence/manifest.cbor`). It holds each member's metadata and, for regular files, a
checksum. A hardlink refers to its primary entry; directories and symlinks have no checksum.

<!-- code-anchor: crates/remanence-cli/src/archive_ingest.rs crates/remanence-stream/src/lib.rs @ f04a0951 -->
### extended attribute (xattr)

A named value that a filesystem stores with a file, beside its contents, such as
`user.mime_type`. The part of the name before the first dot is its namespace: on Linux,
`user.`, `security.`, `trusted.` or `system.`. Remanence captures only `user.*` attributes,
stores them in the object's manifest and applies them on restore. It reports the names of all
other attributes at ingest and never applies them. See
[Extended attributes and file metadata](reference-extended-attributes.md) for the behaviour,
and [Why Remanence keeps only user extended attributes](why-user-xattrs-only.md) for the
reasons.

### bundle

An object built to hold many files, usually small ones, so that they share one tape file, one
filemark and one row in the tape's index. Every file in a bundle keeps its own name and
checksum and can be restored on its own. `rem archive build` makes one, and library-mode
`rem put --stored-object` writes it; ordinary `rem put` writes one object per file. See
[Object sizing](guide-object-sizing.md).

### version

One committed object for a pool, archive path and content. The newest version is the one with
the latest `created_at_utc` among versions with a committed copy in that pool, with object row
order breaking ties. It is not a stored version number or a latest flag. Get-by-path selects
the newest version on the loaded tape; `get --version` selects an object UUID explicitly.

### representation

The form in which an object's bytes are stored. `plaintext` is the readable form
(`rem-object-v1`). `encrypted` is the same object sealed by REM-ENCRYPT (`REMO`). Each copy of
an object has one representation, recorded in the catalog.

### plaintext_digest

The SHA-256 checksum of the object in its plain form, files and manifest together. It is the
same for the plaintext and the encrypted representation of one object, so it identifies the
content, and a reader that has decrypted an object checks the result against it.

### stored_digest

The SHA-256 checksum of the bytes actually stored on tape, whichever representation they are.
A storage system can check a copy against it without understanding the format or holding a
key.

### chunk size and block size

The fixed transfer unit, 256 KiB by default. The REM-OBJECT chunk size aligns member data
inside an object; the tape block size is the fixed SCSI block recorded in the bootstrap. In an
encrypted object a **chunk** is also the unit that is encrypted and authenticated on its own.

### blob wrapper

An ingest artifact: a dense subtree of non-compliant files packed into one `.remwrap.tar`
member, with a generated `.remwrap.idx` index beside it, instead of thousands of individual
entries. The convention, the index fields and the single-file recovery procedure are in
REM-OBJECT Core Format Appendix E (informative).

### REMO

The encrypted representation of a REM-OBJECT object: a 128-byte header (magic `REMO`),
HKDF-SHA-256 key derivation, and a ChaCha20-Poly1305 STREAM over the tar bytes. The accepted
representation is REM-ENCRYPT 1.0 with on-tape `format_version = 2` and an HPKE recipient key
frame. Readers reject every other `format_version` (`UnsupportedFormatVersion`).

### recipient

A public key that an object is encrypted for; the matching private key opens the object.
Remanence's sealing tools name two to eight recipients for each object, so that more than one
key can open it. A recipient's public key is stored in a `REMR` file and its private key in a
`REMP` file; the byte layouts are in the
[CLI reference](reference-cli.md#recipient-key-file-formats).

### recipient epoch

The identity of one recipient key: a 16-byte epoch id and an optional text label. Replacing a
recipient's key creates a new epoch. A `REMP` file carries its epoch id, and opening an object
selects the key-frame slot with the same epoch id.

### DEK (data-encryption key)

The random key generated for one object and used to encrypt that object's content. It is never
stored in the clear.

### wrap and wrapped key

To wrap a key is to encrypt it under a recipient's public key. Each object's DEK is wrapped
once per recipient. The wrapped copies are stored in the key frame, so any one recipient's
private key can recover the DEK.

### REM-ENCRYPT 1.0 HPKE envelope (format_version = 2)

The REMO layout in which no secret is shared between writer and reader. A fresh DEK is
generated for each object and wrapped for each recipient with HPKE (RFC 9180 Base mode,
HKDF-SHA256, ChaCha20-Poly1305) running the X-Wing hybrid KEM. The wrapped keys sit in a
**key frame** (wire tag `REMK`; readers accept 1 to 8 recipient slots) between the header and
the metadata frame. Production sealers require 2 to 8 distinct, nonzero recipient epochs.
`archive build`, pool-selected `archive write` and a full `archive reseal` produce it;
`archive extract`, `restore`, `read` and `verify`, the streaming range commands and
`rem-recover` open it with a matching REMP private key.

### HPKE

Hybrid Public Key Encryption (RFC 9180): a standard way to encrypt a small secret, here the
DEK, to a public key.

### KEM

Key-encapsulation mechanism: the public-key operation that HPKE uses to derive a shared secret
for one recipient.

### X-Wing hybrid KEM

The KEM that wraps each recipient's copy of the DEK: ML-KEM-768 (a post-quantum lattice scheme,
FIPS 203) combined with X25519 (classical elliptic-curve Diffie-Hellman) by the combiner in
`draft-connolly-cfrg-xwing-kem`, used in HPKE as KEM id `0x647a`. Combining the two means an
attacker must break both. A recipient's public key (`REMR` file) is a 1216-byte serialized
X-Wing encapsulation key; the matching private key (`REMP` file) is a 32-byte seed. Wrap-suite
id `0x01` (an X25519-only KEM) is permanently forbidden and rejected by readers and sealers, so
every REMO object this codebase produces or accepts uses the X-Wing hybrid.

### covering range

The mapping, computed by `remanence-aead`, from a requested plaintext byte range to the
smallest span of encrypted chunks, with their stored ciphertext byte offsets, that must be
fetched and authenticated to serve it. `rem archive covering-range` prints it, and the ranged
mode of `extract-stream` consumes it. See the
[extract-stream protocol](reference-extract-stream-protocol.md).

### REM-PARITY

The tape-layout-plus-parity format, published as the REM-PARITY 1.0 specification. A tape
holds the bootstrap, object tape files, Reed-Solomon parity sidecars, a final ParityMap, and
the terminal suffix (replicas A, B and C separated by extents AB and BC).

### tape file

A run of one or more tape blocks ended by exactly one filemark. A tape holds a sequence of
tape files, numbered from 0; the kinds are the bootstrap, object, parity sidecar, ParityMap,
terminal replica and separation extent. Every object is one tape file, but sidecars and the
other kinds are tape files that are not objects.

### filemark

A marker the drive writes on tape to end a tape file. The drive can skip forward or backward
over filemarks without reading the data between them, which is how a reader counts and
measures tape files.

### parity

Extra recovery data written alongside the data blocks. With Reed-Solomon parity a reader can
rebuild damaged blocks within stated limits. A tape can also be written without parity.

### protected

Covered by a committed parity sidecar. Data is committed as soon as its commit record is
durable, and protected only once a sidecar covers it, which can be later; see
[committed](#commit-and-committed).

### bootstrap

The first tape file, a single block at LBA 0 (the first block on the tape). It states the tape
UUID, the block size and the parity scheme, and it does not change when objects are added. It
holds no per-object information and is written once, not repeated at checkpoints or
finalization. On a tape written without parity the bootstrap carries the no-parity flag and no
parity scheme record; a no-parity bootstrap whose payload carries one is refused (REM-PARITY
Section 8.2, where the error is `BootstrapParse`). The CLI reference gives the line the tool
prints.

### parity sidecar (sidecar)

A tape file of Reed-Solomon parity shards and per-block checksums that covers the data blocks
of one epoch. A reader uses it to find damaged data blocks and rebuild them.

### stripe and neighborhood

Parity geometry. A stripe is one Reed-Solomon codeword: k data blocks plus m parity blocks. A
neighborhood is S consecutive stripes whose parity lands in one sidecar.

### epoch (parity epoch)

One group of data blocks that are protected
together and described by one sidecar. At the default settings a full epoch
covers 16 GiB of Object data. A bare "epoch" in the tape layout means this;
the other uses are **recipient epoch** (above), **write epoch** and **daemon
epoch** (below).

### ParityMap

The tape file written after the last sidecar, when the tape has any sidecars. It lists every
sidecar and the range of data each protects. The terminal replicas index it, but it is not
itself the terminal inventory's authority.

### finalization and finalize

The operation that closes a tape: after the last object Remanence writes any remaining parity
and, on a tape with parity, the ParityMap, then the terminal suffix. Finalization starts when a commit takes the tape past
the pool's low watermark, when the drive reports early warning, when no queued object fits in
the space left, or when an operator runs `rem tape finalize`. Once it starts, the tape accepts
no more objects; while it runs the tape is reported as `Finalizing`, and an interrupted
finalization resumes from the last component it proved. See
[Concepts](concepts.md#6-parity-finalization-and-the-sealed-tape).

### sealed

The state of a tape whose finalization has completed: all three terminal replicas are on tape
and the tape log records the tape as sealed. Finalize is the operation; sealed is the state it
leaves. A sealed tape accepts no further objects.

### terminal suffix

The last five tape files of a finished tape, in this order: terminal replica A, separation
extent AB, terminal replica B, separation extent BC, terminal replica C. Writing the suffix
finalizes the tape, and no object is written after it.

### terminal replica

One of the three tape files A, B and C of the terminal suffix. Each is a complete copy of the
tape's index (the terminal inventory), which lists every tape file before the suffix and the
recovery data for every object. A reader prefers C, then B, then A, and requires valid copies
to agree.

### separation extent

One of the two spacer tape files, AB and BC, between the terminal replicas. Each is a header
record, zero-filled interior records and a footer record, 1 GiB by default. They keep the
replicas physically apart, so that one damaged region is unlikely to destroy all three.

### edition

The immutable facts that terminal replicas A, B and C share: every field of the replica frame
that REM-PARITY classes as common to the edition, such as the edition id and sequence, the
covered tape-file count and the payload digest. Two replicas of one edition differ only in
their replica-local fields.

### edition digest

A SHA-256 over an edition's shared facts (REM-PARITY Section 10.4). Replicas of the same
edition agree on it. A full verification records the edition digest it verified, which
`rem catalog tapes --json` shows as `last_verified_edition_digest`.

### filemark map

The structural catalog of a tape: which tape file at which position is an object, a sidecar,
the bootstrap or the final ParityMap. The terminal replica payload fixes the complete map of
the tape files before the suffix.

### foreign-format adapter

An auxiliary, read-only parser linked into a separate Remanence distribution. Core Remanence
owns the normalized reader and registry contract but ships with an empty registry and no
concrete adapters. See [foreign-format adapters](reference-foreign-format-adapters.md).

<!-- code-anchor: crates/remanence-state/src/config.rs crates/remanence-state/src/index.rs crates/remanence-state/src/tape_log/projection.rs crates/remanence-state/src/tape_log/store.rs crates/remanence-state/src/authority_layout.rs crates/remanence-api/src/write_owner/checkpoint.rs @ 625f7044 -->
## Catalog and daemon

### tape log

The append-only file in which the host records, for one tape, its commit records, tape-file
rows, object and member payloads, parity state, physical cursor and finalization progress. It
is the host's authority for that tape, kept for tapes with or without parity, at
`tapes/<uuid>.remtape` under the configured `[journal]` directory (the UUID in lowercase,
without hyphens). The catalog is a replayable projection of its validated closed groups of
records. Some command names keep an older word for these files: `rebuild-catalog-from-journals`
and `tape import-journals`. See [Concepts](concepts.md#5-the-tape-log-the-audit-log-and-the-catalog).

### tape incarnation

A number in the tape log that identifies one life of the log for a tape UUID. It is always 1:
initializing a cartridge again gives it a new tape UUID, and so a new log, rather than a second
incarnation. Append receipts report `tape_incarnation` with the `tape_log_sequence` of their
commit.

### audit log

The hash-chained, append-only record of every state-changing operation and who asked for it,
kept as daily `.remaudit` segments. Operator decisions such as retiring a tape or moving a
label are recorded here, and catalog rebuild replays them.

### catalog

The queryable model of what is on which tape, kept in the SQLite file `rem-state.sqlite`. The
durable truth is the tape logs plus the audit log; the catalog is a rebuildable
[projection](#projection) of them, and `rem rebuild-catalog-from-journals` rebuilds it.

### projection

A store derived entirely from other records, which can be rebuilt from them. The catalog is a
projection of the tape logs and the audit log. Each tape's catalog rows carry a **projection
mark**, the tape-log sequence they have been projected through; it speeds up incremental
projection but does not prove the rows below it.

### barrier

A synchronous WRITE FILEMARKS with a count of zero, which returns only when the drive has
written its buffer to the medium, followed by a READ POSITION that must report the same
position. A barrier proves that the data is on the medium and that the tape is where Remanence
believes it is. The commit record is written after a successful barrier. See
[Concepts](concepts.md#4-writing-sessions-barriers-checkpoints-and-commits).

### commit and committed

An object is **committed** once the tape log holds a durable commit record for it, written
after a successful barrier. Committed means the data is durably on tape and the host has a
durable record of where; it does not mean parity covers it yet (see [protected](#protected)).
The catalog shows committed copies after it projects the commit record.

### checkpoint

One barrier and one commit record for a batch of objects. The daemon checkpoints when the
pending batch reaches the `checkpoint_max_bytes`, `checkpoint_max_objects` or
`checkpoint_max_age_seconds` limit in the
[configuration](reference-configuration.md#daemon-required), when the client calls
`CheckpointSession`, and when the session closes.

### copy

One object as stored on one tape: a row of the catalog's `object_copies` table with a status.
A copy is `committed` when its commit record is durable, and becomes `missing` when its tape is
retired. Writing an object to two pools gives it copies on two tapes.

### tape pool

An operator-defined group of tapes that writes target. A write session names a pool;
Remanence picks the tape.

### copy class and content class

Pool labels (`copy_class`, `content_class`) that let an orchestrator keep redundant copies on
disjoint pools (`copy-a`, `copy-b`) and separate kinds of content.

### watermarks

Per-pool fill fractions. At `watermark_low` (0.92) a tape becomes a candidate for sealing;
`watermark_high` (0.97) caps usable capacity below the end of the medium.

### tape states

The lifecycle state the catalog records for a tape. `ready` accepts writes. `Finalizing`
(reported while finalization runs) and `sealed` accept no more objects. `recovery_required`
means completion or repair needs an explicit recovery before ordinary use: for example a
finalization component that failed, an adopted tape whose tail after the bootstrap is not
exactly a filemark and end of data, or a tape whose log tail was repaired. `retired` is the
terminal state that [`rem tape retire`](tape-identity-lifecycle-explainer.md) sets. Any state
other than `ready` is unwritable.

### locator

The JSON record emitted after a write that pins an object copy to physical media: tape UUID,
position and geometry. It is the input to direct reads, exports and verifies.

### caller object id

The orchestrator's opaque id for an object, recorded alongside Remanence's own UUID.
Same-content retries with the same caller id are idempotent; different content under a reused
id is a conflict.

### catalog unit

One enumerable archive in the catalog, either *native* (a REM-OBJECT object Remanence wrote)
or *foreign* (an archive scanned through a registered adapter).

### operation

A daemon-tracked long-running action with a UUID, which `rem op` can query and cancel.

### session

The daemon's write or read transaction: open, append or
read, checkpoint, close. Sessions have idle timeouts and abort paths.
Every open always mints a brand-new session id — sessions never
continue across a client restart; see **cold resume** below for what a
read session offers instead. Write sessions have no restart contract at
all (`recover_session_id` returns unimplemented).

### authority marker

The file `authority-format` in the state directory, which names the layout of the host's
durable state (the tape logs and the audit log). Only an empty state directory, or the
declared reset, may install it, and only under the exclusive state lock. A state directory in
an older layout is refused, and `rem rebuild-catalog-from-journals` requires the marker and
never installs it.

### declared reset

The offline state reset that an authority-layout change requires, run as `rem reset-catalog`
(the same operation as `rem-debug catalog reset`). It archives the audit log and the tape logs
under `reset-archives` in the state directory, starts a new catalog that keeps only the tapes
named with `--preserve-tape-voltag`, and installs the current authority marker. See the [CLI reference](reference-cli.md#destructive-state-reset).

### spool

The daemon's pre-commit staging directory for appends (default `<state_dir>/spool`), sized
from the shared `io_memory_ceiling` budget (see [I/O memory ceiling](#io-memory-ceiling)).

### drive stewardship

The drive-fleet lifecycle machinery: identity by drive UUID, health snapshots, TapeAlert
polling, cleaning runs, history, annotation and retirement.

### alarm

A standing, catalog-persisted condition raised against a drive, tape or library resource (for
example a repeated cleaning failure, or a fenced tape awaiting release) until an operator
acknowledges or clears it. `rem alarms` lists and acknowledges alarms.

<!-- code-anchor: crates/remanence-state/src/fence.rs crates/remanence-api/src/readiness_wait.rs crates/remanence-api/src/startup_media_readiness.rs crates/remanence-api/src/pool_write/model.rs crates/remanence-api/src/tape_init.rs @ 625f7044 -->
## Safety and operations

### allowlist

The explicit list of library serials a process may change: `[[libraries]]` in the daemon
configuration, `--allow` on `rem-debug`. Anything not listed is visible but untouchable.

### derived identity

A drive bay whose identity could not be read directly from the device and had to be inferred.
Operating one requires a separate opt-in (`allow_derived_drive_identity`, `--allow-derived`).

### fence

A durable refusal recorded against a tape or a drive after an operation on it ended in a state
Remanence could not prove. It is written to the audit log and the catalog and survives
restarts. Fenced media is refused for writes and initialization, and the daemon also refuses
read sessions on it, until an operator resumes or releases the fence with an acknowledgement.
Other media stay available. There are two kinds: the [tape-I/O fence](#tape-io-fence) and the
[media-readiness fence](#media-readiness-fence). See [Concepts](concepts.md#9-fences).

### quarantine

The record behind a fence, named by its **quarantine id**. The `rem tape quarantine` commands
list, show and release fences by that id; quarantine and fence name the same record.

### tape-I/O fence

A fence (quarantine id beginning `tioq-`) recording tape input or output that ended unproved:
a failed checkpoint barrier, a partial batch, a transfer error, an uncertain finalization step,
or a tape log that ends before the point the catalog has already projected. `rem catalog tapes
--json`, `rem top` and `rem daemon health` show it, and `rem tape quarantine release <ID> --ack
<TEXT>` releases it. On a standalone drive reads stay available while it is active.

### media-readiness fence

A fence (quarantine id beginning `mrq-`) recording a readiness wait that was interrupted or
ended without a clear answer. A wait that was still progressing (loading, becoming ready, busy,
unit attention) is resumable, by the daemon or with `rem tape wait-ready --resume`. One that
ended unknown, in a terminal error or in a reservation conflict needs an operator release.
`rem tape quarantine list` shows these fences.

### eject guard

The medium-removal prevention that Remanence sets on a drive while a command runs. An
interrupted command can leave it set; `rem tape release` clears it on a standalone drive. It is
not a fence, and clearing it leaves fences in place.

### media readiness

The TEST UNIT READY classification state machine (ready, becoming ready, terminal not-ready,
reservation conflict, transport unknown, and so on), with durable operation records and the
exit codes in the [CLI reference](reference-cli.md#exit-codes).

### destructive-safety gauntlet

The chain of checks `rem tape init` runs before touching a cartridge: identity, ownership,
readiness and data presence, each with its own override rules (`--force` for `require-force`
decisions, `--clobber-data` only for tapes that hold data, never in batch).

### media optimization and conditioning

The one-time calibration pass an LTO-9 cartridge performs on its first load. It can take more
than an hour, which is why readiness waits default to 2.5 h. It is unrelated to the read-order
[calibration](#calibration-read-order) state, despite the shared word.

### WORM and NotWorm

WORM (write once, read many) cartridges cannot be overwritten. Ordinary object writes require a
cartridge the drive positively reports as rewritable (`NotWorm`); WORM media and media whose
WORM state is unknown are refused before the drive mode changes, because restart recovery may
need to replace an uncommitted tail. See [Concepts](concepts.md#7-worm-media).

### append admission

Proof that the tape's physical suffix permits a write
at the tape-log-authorized position. Complete data beyond known state produces
`TailBeyondKnownState`; state rows and fence release do not authorize erasure.

<!-- code-anchor: crates/remanence-api/src/engine/drive_adapter.rs crates/remanence-library/src/discovery.rs crates/remanence-cli/src/standalone.rs crates/remanence-state/src/index.rs @ 625f7044 -->
## Labels and endpoints

### label (voltag)

The code on a cartridge shell, read by a changer scanner (the barcode) or written and asserted
by a person (an operator label on a standalone cartridge). It is not the logical tape
identity.

### label provenance (voltag_source)

`changer` when a scanner read the label, `operator` when a person asserted it. Neither is an
identity trust level; the tape UUID in the bootstrap is the identity in both cases.

### tape UUID

The UUID in the BOT bootstrap, identifying one logical tape independently of its label, pool,
library or device path. See [Tape identity and the retire operation](tape-identity-lifecycle-explainer.md).

### endpoint

A place a cartridge can be loaded: a library drive bay or a standalone drive. An endpoint is
not a device path.

### standalone drive

A drive proven unclaimed by complete discovery, with no unresolved changer bays, or pinned as
standalone by configuration. A drive that no changer claims during incomplete discovery has
**ownership unknown**, which alone does not authorize writes. `rem` operates a standalone drive
directly, without a daemon.

### home mode

The standalone arrangement in which no `/etc/rem/config.toml` exists and `rem setup` has saved
a drive serial (`[home] drive_serial`) in the home configuration under
`${XDG_STATE_HOME:-$HOME/.local/state}/rem`. In home mode `rem put`, `rem get` and the
`rem tape` commands operate that drive without further flags. See the
[CLI routing table](reference-cli.md#standalone-drives).

### library mode

Operation of a changer and its drives through `rem-daemon`, which owns them; `rem` talks to the
daemon over gRPC.

### DriveKey

A durable endpoint key: `Bay { library_serial, bay }` or `Standalone { drive_serial }`. The
drive serial comes from VPD page 0x80. Neither an OS path nor a sentinel bay stands in for a
standalone key.

### MountEndpoint

`Durable(DriveKey)` or `Ephemeral { sg_path }`. Ephemeral endpoints permit authorized reads
only.

<!-- code-anchor: none -->
## Tape and SCSI background

### LTO

Linear Tape-Open, the cartridge and drive standard. An LTO-9 cartridge holds about 18 TB,
uncompressed.

### library and changer

The robot chassis holding cartridges, drives, slots and mail slots. SCSI calls it a medium
changer.

### partition

A mode in which one physical chassis presents itself as several independent logical libraries.
It is the reason for the allowlist safety model.

### element address

The changer's numeric address for any location (slot, drive bay, IE port, picker), for example
`0x0400`.

### drive bay

The changer element that holds a drive, as distinct from the drive itself.

### IE port and mail slot

The import/export station where cartridges enter and leave the chassis.

### voltag

Volume tag, the shell label read by a changer scanner or asserted by an operator. See
[labels and endpoints](#labels-and-endpoints).

### volume label

The identifying record traditional tape systems write first on a cartridge (the ANSI VOL1
record, the LTFS volume label). Remanence has no separate label structure; the single BOT
bootstrap has that role. The recovery index is a separate structure: three terminal replicas
and two separation extents written at the tape's tail during finalization. They are not
repeated bootstrap copies. See [bootstrap](#bootstrap) and [terminal replica](#terminal-replica).

### BOT, EOM and EOD

Beginning of tape, end of media (the physical end), and end of data (the position just after
the last data written).

### LBA

Logical block address: the SCSI logical position on the tape, counted from 0 at the beginning
of tape. It counts both blocks and filemarks. It is distinct from the dense ordinal that
REM-PARITY assigns to object data blocks only.

### st driver and mt

The Linux kernel tape driver (`/dev/st*`, `/dev/nst*`) and its command-line tool, the
conventional way to operate a drive. Remanence bypasses them; see
[why-sg-passthrough.md](why-sg-passthrough.md).

### mtx

The classic command-line medium-changer tool. It names drives by element address, with no join
to the st driver's device files; the gap is described in
[why-sg-passthrough.md](why-sg-passthrough.md).

### SG_IO and sg device

The Linux ioctl that passes a raw SCSI command (CDB, command descriptor block) to a device
through its `/dev/sgN` node and returns status and sense data. It is Remanence's only transport,
for drives and changers alike. A `/dev/sgN` path is what commands such as
`rem-debug tape recovery-report` take for a drive.

### VPD page 0x80

The SCSI vital-product-data page that carries a device's serial number; the basis of all
identity joins.

### TUR

TEST UNIT READY, the SCSI probe whose sense data drives readiness classification.

### unit attention (UA)

The SCSI condition a device raises after a state change (media change, reset). Remanence
consumes and classifies it; repeated identical unit attentions become terminal.

### TapeAlert

The standardized drive and media health flag page (LOG SENSE `0x2E`), polled by drive
stewardship.

### RES

READ ELEMENT STATUS, the changer inventory command.

### CRC

Cyclic redundancy check, a short checksum used to detect damage in a block.

### VTL

Virtual tape library: software that emulates a tape library chassis, its drives and its
cartridges.

<!-- code-anchor: crates/remanence-library/src/handle/tape_io/readiness.rs crates/remanence-library/src/handle/mod.rs crates/remanence-api/src/calibration.rs crates/remanence-state/src/calibration.rs crates/remanence-api/src/write_session_ingress.rs crates/remanence-api/src/read_session_service.rs @ 625f7044 -->
## Implementation terms

These terms describe internals that integrators and contributors meet in the API and the source.
An operator rarely needs them.

### position proof

A real SCSI READ POSITION result (not an arithmetic estimate) returned to a caller as evidence
of where the drive is. Write sessions and read-session opens, including cold resumes, return
it.

### cold resume and resume target

A read-only alternative to session continuation. A client that lost its session id, typically
across an application restart, can reopen with a *resume target*: durable coordinates
`(tape_uuid, object_id, file_id, file_boundary_byte_offset)` it saved itself. The daemon still
mints a fresh session, but first re-verifies the loaded tape's identity against `tape_uuid`,
rejecting a swapped cartridge before trusting any position, and relocates to the exact
catalogued file boundary, never mid-file.

### daemon epoch

A random value minted once when `rem-daemon` starts and returned on every read session, so that
a client can detect, by comparing epochs, that it is talking to a restarted daemon. The request
side of this field, `prior_daemon_epoch`, is decoded but not checked, so a client cannot ask the
daemon to reject a stale-epoch resume.

### advisory arbitration surface (drive assignment)

A read-only projection (`GetLiveStatusResponse.drive_assignments`) of the atomic reservation
held per library serial and drive bay, exposed for an external scheduling client. It cannot
gate, queue or block a load; the atomic reservation remains the only enforcement path.
`rem top` does not render it.

### dirty snapshot

The in-memory library state after an operation whose completion is uncertain, marked with a
cause (`CompletionUnknown`, `PartialFailure`). Refresh or rescan before trusting it.

### poison

A fail-closed "do not proceed" mark set after a failed data command, at two independent scopes.
*Transfer poison* discards the rest of one already-failing `AppendObject` call's queued
staging-ring batches. *Session poison* (`AppendGate`) fails every later append for the rest of
that write session; the fix is to close the session and open a new one, not to retry. It is
distinct from a [dirty snapshot](#dirty-snapshot), a library and robotics concern, and from a
[fence](#fence), a durable catalog refusal; see
[troubleshooting](guide-troubleshooting.md#dirty-snapshots-and-completion-unknown).

### staging ring

The fixed pool of page-aligned buffers (`tape_io.staging_ring_buffers`, default 4) that a write
session's producer thread fills while a submitter issues one blocking tape-write command at a
time. It is the only path for streaming object reads and writes; there is no non-pipelined
fallback. Full verification reads outside it, through its own strict batch reader.

### read reservoir and watermark stop-start

The host-memory buffer that a read session's submitter fills up to a high watermark
(`tape_io.read_reservoir_high_pct`, default 90 %) before it parks the drive, resuming only once
the consumer has drained it below the low watermark (`tape_io.read_reservoir_low_pct`, default
25 %). Every resume from a park forces a fresh [position proof](#position-proof).

### proof-frontier

For byte-range reads, the rule that data is never handed to the decoder until a real position
proof has covered it, so that a caller cannot receive bytes the drive has not confirmed
reaching.

### position tripwire

The periodic real READ POSITION check (`tape_io.position_check_bytes`, default 1 GiB) during a
long write or read, which catches arithmetic position drift between checks.

### I/O memory ceiling

`daemon.io_memory_ceiling`, default 24 GiB: the one shared budget that every pipeline consumer
draws from, the append spool and every drive's read reservoir. A single atomic permit manager
enforces it. Exceeding it at daemon start-up (in `validate_config`) or when a read pipeline
starts (if the reservoir cannot fit its minimum staging pool) is a hard error, not a gradual
slowdown.

### calibration (read-order)

The per-volume state (`Uncalibrated`, `Calibrated`, `UnsupportedFormat`) that tracks whether a
durable [wrap map](#wrap-map) is safe to serve for
[read-order planning](architecture-overview.md#read-order-planning). It is harvested once per
drive load and invalidated by the [write epoch](#write-epoch). Not to be confused with
[media optimization](#media-optimization-and-conditioning).

### write epoch

A durable, per-volume counter that the media-write fence advances before the first
media-modifying command of a load. A wrap map whose recorded epoch does not match the volume's
current epoch is stale and is never served.

### wrap map

The per-load record of which logical block ends each tape wrap, harvested with READ END OF WRAP
POSITION and validated by the `remanence-order` crate. It is the input to
[read-order planning](architecture-overview.md#read-order-planning).
