# Format Drivers and Streaming Boundaries

**Status:** design note, 2026-05-28.

**Scope:** clarifies what it means for Remanence to support
pluggable body and legacy archive formats. This note does not replace
`docs/spec-v0.4.md`; it tightens the implementation model behind the
planned format registry, especially for legacy readers such as BRU.

## 1. Summary

Adding a new archive format to Remanence should remain simple at the
registration boundary:

```rust
registry.register(Box::new(BruFormat::new()));
```

But the registered value cannot be only a declarative format
specification. It must be an executable format driver: a state machine
that knows how to scan, validate, index, and stream that format through
Remanence's storage surfaces.

A schema can describe fields. It cannot, by itself, safely answer:

- how many bytes to request from the tape drive;
- how to split physical tape records into logical format records;
- when a file payload begins and ends;
- how to preserve forward-only streaming and backpressure;
- how to report checksum damage without converting all damage to fatal
  errors;
- how to build a file index for later partial restore;
- whether random file reads are possible or only sequential restore is
  supported.

The design should therefore distinguish a format descriptor from a
format driver.

Implementation note: the current CLI exposes this distinction through a
generic archive command rather than a BRU-specific top-level command:

```text
rem archive probe --format bru --dump file.bru
rem archive scan --format bru --dump file.bru
rem archive restore --format bru --dump file.bru --dest restored/
rem archive recover --format bru --dump file.bru --dest recovered/

rem archive probe --format bru --tape <library-serial> --bay 0x0100 --rewind
rem archive scan --format bru --tape <library-serial> --bay 0x0100 --rewind
rem archive restore --format bru --tape <library-serial> --bay 0x0100 --dest restored/ --rewind
rem archive recover --format bru --tape <library-serial> --bay 0x0100 --dest recovered/ --rewind
```

The `--format` flag selects the driver. The source flags select the
streaming surface. Dump files bypass library discovery. Tape sources
open the library and drive, and therefore still require the usual
`--allow <library-serial>` safety gate.

`restore` is the normal strict filesystem materialization path. `recover`
is the sparse partial-file path: it writes readable bytes at explicit file
offsets, tracks checksum-failed bytes as suspect ranges, records missing
or unreadable file ranges, and appends JSONL records to
`<dest>/.remanence/recovery.jsonl`.

For positive physical-path testing on a scratch QuadStor cartridge,
the CLI also has a development-only seeding helper:

```text
rem dev write-dump-to-tape --dump file.bru --tape <library-serial> --bay 0x0100 \
  --i-understand-this-overwrites-the-loaded-tape
```

It writes a byte-stream BRU dump as variable tape records, writes one
filemark, and rewinds. This helper is intentionally destructive and is
not a migration/import path.

## 2. Problem

The current design language sometimes implies that a new format can be
"dropped in" as something like a class instance containing the format
spec. That is true only for the registry mechanics. It is not true for
the full storage behavior.

Remanence already has at least two materially different read surfaces:

- Native Remanence objects are read through object-local `BlockSource`
  implementations. For example, `rem-tar-v1` reads fixed-size object
  blocks and streams tar entries into callbacks.
- Legacy or foreign tapes may require physical tape access first:
  variable-size records, filemarks, drive block-size configuration, and
  format-specific logical block splitting.

BRU makes the distinction concrete. The BRU logical format uses 2048-byte
internal blocks, but a physical tape read may return a larger I/O buffer
such as 1 MiB. A BRU driver must read the physical record safely, split
it into 2048-byte BRU blocks, verify each BRU checksum, and then stream
file payload ranges. A declarative field map is not enough.

## 3. Decision

The format registry should register drivers, not just specs.

A driver has two parts:

1. A descriptor: stable metadata that Remanence can inspect without
   opening a stream.
2. Executable read/write implementations: the streaming state machines
   for the format.

Sketch:

```rust
trait FormatDescriptor {
    fn id(&self) -> &'static str;
    fn capabilities(&self) -> FormatCapabilities;
    fn source_requirement(&self) -> SourceRequirement;
}

enum SourceRequirement {
    ObjectBlocks,
    PhysicalTapeRecords,
    ByteStreamDump,
    ObjectBytes,
}
```

Then split native object formats from foreign tape formats:

```rust
trait NativeBodyFormat: FormatDescriptor {
    fn open_object_reader(
        &self,
        source: &mut dyn BlockSource,
        metadata: &ObjectFormatMetadata,
    ) -> Result<Box<dyn ArchiveReader>, FormatError>;

    fn open_object_writer(
        &self,
        sink: &mut dyn BlockSink,
        manifest: &FormatManifest,
    ) -> Result<Box<dyn ArchiveWriter>, FormatError>;
}

trait ForeignTapeFormat: FormatDescriptor {
    fn probe(
        &self,
        source: &mut dyn PhysicalTapeSource,
    ) -> Result<ProbeResult, FormatError>;

    fn open_tape_reader(
        &self,
        source: &mut dyn PhysicalTapeSource,
        probe: &ProbeResult,
    ) -> Result<Box<dyn ArchiveReader>, FormatError>;
}
```

The names are illustrative. The important point is the separation of
source requirements and streaming mechanics.

`PhysicalTapeSource` is the small read-side abstraction in
`remanence-library` for this purpose. It should not force legacy format
drivers to depend on Layer 3c. The current Layer 3c `RawTapeSource` is a
useful reference point, but it includes parity-specific responsibilities
and should not become the public format driver contract without review.

## 4. Archive Reader Shape

Every readable format should expose a normalized archive reader:

```rust
trait ArchiveReader {
    fn scan(
        &mut self,
        sink: &mut dyn EntryCatalogSink,
    ) -> Result<ScanReport, FormatError>;

    fn stream_all(
        &mut self,
        sink: &mut dyn ArchiveEventSink,
    ) -> Result<StreamReport, FormatError>;

    fn stream_file(
        &mut self,
        file_id: &FileId,
        sink: &mut dyn FileDataSink,
    ) -> Result<FileStreamReport, FormatError>;
}
```

`stream_file` should be capability-gated. Some legacy formats may only
support `scan` plus sequential `stream_all` until an index has been
built. That is acceptable and should be explicit in `FormatCapabilities`.

The sinks should support explicit data offsets and both file-scoped and
archive-scoped damage/provenance events:

```rust
trait FileDataSink {
    fn write_data(&mut self, bytes: &[u8]) -> Result<(), FormatError>;
    fn report_damage(&mut self, range: DamageRange) -> Result<(), FormatError>;
}

trait ArchiveEventSink {
    fn begin_entry(&mut self, entry: &NormalizedEntry) -> Result<(), FormatError>;
    fn write_file_data(
        &mut self,
        file_offset: u64,
        bytes: &[u8],
    ) -> Result<(), FormatError>;
    fn report_damage(&mut self, range: &DamageRange) -> Result<(), FormatError>;
    fn report_archive_gap(&mut self, range: &ArchiveGapRange) -> Result<(), FormatError>;
    fn end_entry(&mut self, entry: &NormalizedEntry) -> Result<(), FormatError>;
}
```

This avoids forcing every checksum failure or missing segment into a
fatal error. For legacy recovery, "partially restored with known damaged
ranges" is often more useful than "restore failed".

`file_offset` is required for sparse recovery. A sink must not have to
infer placement from append order because a single file may have multiple
missing ranges and later passes may fill them out of order.

`DamageRange` is file-scoped: it says which bytes of a known file are
checksum-failed, missing, unreadable, or unsupported. `ArchiveGapRange`
is archive-scoped: it says source bytes were skipped or unreadable but
the driver cannot attribute them to a file range. This distinction is
important when a physical tape read fails before a file header is known,
or when a driver loses sync and scans forward to the next valid format
boundary.

Recovery sinks should keep clean recovered ranges separate from suspect
ranges. For BRU, a block with a checksum failure can still have bytes on
tape; those bytes may be useful for inspection or later stitching, but
they must not be counted as clean recovery coverage.

## 5. Capability Model

Format capabilities should be explicit. Suggested flags:

- `catalog_scan`: the driver can enumerate entries without restoring all
  bytes to a filesystem.
- `sequential_restore`: the driver can stream entries in archive order.
- `indexed_file_restore`: the driver can seek to a file after an index or
  manifest exists.
- `range_read`: the driver can stream a byte range within one file.
- `write`: the driver can create new objects in this format.
- `verify`: the driver can validate format-level checksums.
- `damage_events`: the driver can surface damaged file ranges without
  aborting the whole stream.
- `metadata_preserving`: the driver can preserve mode, mtime, owners,
  links, or other format-specific metadata.

These capabilities should describe what the driver can actually do on
the provided source, not what the format could theoretically support.

## 6. Native vs Foreign Formats

Native Remanence formats should normally use `BlockSource` and
`BlockSink`. They operate inside object tape files and let Layer 3c own
physical filemarks, sidecars, and parity.

Examples:

- `rem-tar-v1`: native body format, object-local fixed-block streaming.
- future native body formats: same object-local contract.

Foreign tape formats should normally use raw physical tape access during
initial read or migration. They may have their own physical record
framing, filemark conventions, or logical block sizes.

Examples:

- `remanence-tar-legacy`: read-only legacy tar tapes.
- `remanence-bru`: read-only BRU/BRU-PE tapes.

Foreign formats can still produce normalized `ArchiveReader` events. They
just should not be forced through the native object `BlockSource` surface
before their physical framing has been decoded.

The shared physical source contract should expose only what legacy
readers need: configure/read tape records where applicable, observe
filemarks, locate or space when supported, and report physical position
hints. Parity recovery, sidecar discovery, and Remanence object-local
block recovery stay in Layer 3c.

## 7. BRU Implications

A BRU driver should be a `ForeignTapeFormat`, not merely a native
`BlockSource` parser.

It needs at least three internal layers:

1. Physical source adapter:
   read tape records or dump-file bytes with correct buffering.
2. BRU block splitter:
   produce 2048-byte logical BRU blocks with checksum status and source
   offsets.
3. BRU archive reader:
   parse archive headers, file headers, continuation blocks, and damage
   ranges; emit normalized archive/file events.

The driver must not assume that a SCSI tape read of 2048 bytes is safe.
If the physical tape record is larger than the host buffer, the drive may
consume the record and report an incorrect-length condition. BRU's
logical 2048-byte block size is not necessarily the physical tape record
size.

The normalized BRU reader should preserve:

- archive label and archive id;
- entry path and raw path bytes if needed;
- file size and type;
- source physical/logical offsets where available;
- per-file damage ranges for checksum failures, missing data, and read
  errors;
- archive gap events for unattributed skipped or unreadable source
  regions;
- unsupported entry types as explicit skipped/unsupported events.

## 8. Migration Into `rem-tar-v1`

Reading a legacy format and writing `rem-tar-v1` are separate operations.

The legacy driver should produce normalized file streams. A migration
pipeline can then feed those streams into the native Remanence writer.
This separation keeps the BRU driver read-only and keeps `rem-tar-v1`
responsible for Remanence-native object layout.

One important constraint: the current streaming `rem-tar-v1` writer
requires each file's size and SHA-256 before writing. BRU gives file
size in the header, but not SHA-256. Therefore migration requires one of:

- a first pass that scans and hashes before writing;
- temporary spooling while hashing;
- or a future writer API that can finalize file hashes after streaming.

This should be treated as an import-pipeline decision, not as a BRU
format-driver responsibility.

## 9. Indexing

The format layer should allow an adapter-owned opaque index:

```rust
struct FormatManifest {
    format_id: String,
    entries: Vec<NormalizedEntry>,
    adapter_state: Vec<u8>,
}
```

For native formats, `adapter_state` may be a compact manifest. For
foreign formats, it may include source offsets, block numbers, filemark
positions, or format-specific checksums needed for later restore.

The state should be opaque to Layer 4 and Layer 5 except for normalized
fields required by user-facing APIs. The adapter that creates the state
is responsible for interpreting it.

## 10. Catalog Integration

The Layer 5 catalog should not force foreign archives to masquerade as
native Remanence objects. The agreed shape is additive:

- keep `Catalog.EnumerateObjects` as the native Remanence object hot
  path for Sutradhara and other orchestrators;
- add a cross-source `CatalogUnit` surface for discovery and migration;
- represent native objects as catalog units that reference the existing
  `ObjectRecord`;
- represent foreign BRU/tar archives as catalog units with `format_id`,
  scan confidence, entry counts, and driver-owned opaque `adapter_state`;
- return normalized entries and unattributed archive gaps from
  `ArchiveReader::scan()` for `ListEntriesInUnit`;
- keep physical locators such as BRU block offsets, filemarks, resync
  state, and tar byte ranges inside `adapter_state`, not as stable Layer
  4 columns or public API fields.

This keeps foreign tapes queryable without weakening the meaning of a
native Remanence object. A later per-entry cache is allowed, but it is
derived state: delete-and-rebuild, never authoritative, and still opaque
with respect to driver-private locator details.

## 11. Required Design Updates

To make the model elegant, update the Remanence design in these places:

1. Registry language:
   replace "drop in a format spec" with "register a format driver".
2. Layer 3b format model:
   split descriptor metadata from executable reader/writer behavior.
3. Source abstraction:
   distinguish `ObjectBlocks`, `PhysicalTapeRecords`, and dump byte
   streams. Define the physical read abstraction outside Layer 3c, or
   pass it through a daemon facade, so legacy format drivers do not
   depend on parity internals.
4. Legacy-format section:
   classify `remanence-bru` and `remanence-tar-legacy` as foreign
   read-only formats with physical-source adapters.
5. Capability model:
   make sequential restore, indexed restore, range reads, writes, verify,
   and damage events explicit.
6. Streaming callbacks:
   standardize normalized archive/file event sinks, including explicit
   file offsets, non-fatal file damage, and archive gap reporting.
7. Migration path:
   document that importing a foreign format into `rem-tar-v1` is a
   pipeline, not the same thing as reading that foreign format.
8. Catalog integration:
   keep native object APIs stable and add `CatalogUnit` as a parallel
   native/foreign discovery surface; do not expose driver-private
   physical locator details in Layer 4 or Layer 5.

## 12. Non-Goals

This note does not require a dynamic plugin loader. Formats can remain
compile-time registered Rust implementations.

This note does not make Layer 3c body-format-aware. Layer 3c should
continue to operate on object-local blocks and physical filemark maps.

This note does not require every legacy reader to support random file
access on day one. Sequential scan plus normalized catalog emission is a
valid first milestone.
