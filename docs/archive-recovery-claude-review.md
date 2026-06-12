# Archive Recovery Design Review: Claude

Date: 2026-05-28

Scope: Review of the proposed Remanence recovery abstraction for physical tape
archives, especially whether recovery should be generic across archive formats
instead of BRU-specific.

## Verdict

The generic engine / format-driver split is sound and the trait hierarchy in
`remanence-format/src/driver.rs` is already well-positioned to support it. The
`BruArchiveReader` + `BruVisitor` internal structure is a clean template for
what format drivers should look like. However, the current event model has
three specific gaps that will cause silent correctness failures on the
single-pass recovery path, and `walk_archive` has a hard-abort on unexpected
magic that must become a resync hook before the driver is usable on damaged
tape. None of this requires structural rework; the gaps are additive.

## Recommended Boundary

### What the format driver owns

The driver parses one archive's logical structure. It knows nothing about retry
policy, tape positioning after failure, or what to do with recovered fragments
across multiple passes.

Concrete responsibilities for BRU:

- Physical record to 2048-byte logical block splitting
  (`PhysicalBruBlockSource` today)
- BRU checksum verification per block
- Magic value dispatch: archive header, file header, continuation
- Inline-payload extraction from header blocks
- `dlen` extraction and bounds from continuation blocks
- Resynchronization: given a `BruBlockSource` positioned anywhere, scan forward
  until the next `MAGIC_FILE_HEADER` or `MAGIC_ARCHIVE_HEADER` is found and the
  surrounding checksum is valid. This is purely format knowledge.
- Emitting `DamageRange` events for individual bad blocks, per the existing
  `BruVisitor` model

The driver does not know how many times to retry a bad physical position,
whether to write a sparse extent or a hole placeholder, how to persist a
checkpoint, or how to merge results from two partial passes.

### What the generic recovery engine owns

- Retry loop: on `DamageStatus::ReadError` from
  `PhysicalTapeSource::read_record`, attempt N configurable retries at the same
  LBA before recording it unrecoverable.
- Checkpoint: after each fully resolved file, persist `(physical_lba,
  file_sequence, partial_manifest_so_far)` to a checkpoint file so the pass can
  resume if interrupted.
- Sparse/extent sink: translate a mix of `write_file_data` and `report_damage`
  events into either a sparse file, zero-filled holes, or a sidecar extent list,
  independent of which format produced them.
- Manifest accumulation: collect `NormalizedEntry` and damage ranges per file
  across the full pass; after the pass write a single recovery manifest.
- Post-pass stitch: given two partial manifests, union recovered extents,
  intersect damage ranges, and detect conflicts when both passes delivered
  different bytes for the same file offset.
- Conflict policy: when merging, if overlapping extents differ, flag for human
  review. Do not silently pick one.

## Event And Manifest Model

### Current model is correct but has three silent gaps

#### Gap 1: `write_file_data` carries no file offset

The `ArchiveEventSink::write_file_data(bytes: &[u8])` contract implicitly
requires data events and damage events to partition `[0, size_bytes)` in
monotonically increasing order. If any driver, or a future recovery retry engine
wrapping the driver, emits them out of order, the sink will silently place bytes
at the wrong offset.

This needs to be an explicit contract in the doc at minimum, and probably a
`file_offset: u64` parameter:

```rust
fn write_file_data(&mut self, file_offset: u64, bytes: &[u8]) -> Result<(), FormatError>;
```

Without this, the sparse/extent sink layer cannot be written correctly without
duplicating the offset-tracking logic that already lives inside the driver.

#### Gap 2: no archive-level unknown-file gap event

`DamageRange` always has a `FileId`. When the recovery engine encounters a
physical region where the driver loses sync, such as blocks whose magic is
unrecognized or a region of read errors before any file header is seen, there is
no event to represent it. The sink has no way to know that physical positions
were consumed but not attributed to any file.

Add a second event to `ArchiveEventSink`:

```rust
fn report_archive_gap(&mut self, range: ArchiveGapRange) -> Result<(), FormatError>;
```

`ArchiveGapRange` should carry physical start and end LBA, a `GapCause`
such as unrecognized magic, read error, or resync skip, and raw bytes if
readable.

This is also the natural landing point for a resync notification: the driver
calls `report_archive_gap` before resuming file streaming.

#### Gap 3: `walk_archive` hard-aborts on unexpected magic

In `remanence-bru/src/lib.rs`, an unexpected magic value is a fatal
`Err(FormatError::Parse(...))`. For recovery mode, the driver must instead:

1. Emit `report_archive_gap` for the unrecognized block.
2. Call an internal `try_resync(source)` that scans forward block by block,
   still 2048-byte aligned, until it finds a valid `MAGIC_FILE_HEADER` or
   reaches end of stream.
3. Either resume file streaming or emit a final `report_archive_gap` to
   end-of-stream and return.

This is BRU-specific format knowledge, because BRU defines what constitutes a
valid resync anchor. It belongs in the driver, not the recovery engine.

### Multiple damage ranges per file

Multiple damage ranges are already supported by calling `report_damage` multiple
times. The spec should state the ordering invariant: within a file, data and
damage events are emitted in strictly increasing `file_offset` order and
together exactly cover `[0, size_bytes)` with no overlaps.

### Manifest model for recovery passes

`FormatManifest.adapter_state: Vec<u8>` is opaque today. For stitch to work, it
needs to be interpretable. Options:

- Minimal: document BRU's `adapter_state` schema and build stitch around that
  knowledge. This keeps the trait surface unchanged.
- Better: add
  `fn merge_manifests(&self, a: &FormatManifest, b: &FormatManifest) -> Result<FormatManifest, FormatError>`
  to `ForeignTapeFormat`. The driver can then deserialize and merge its own
  `adapter_state` correctly, and the generic engine stays format-agnostic.

The second option is the right call once stitch becomes a real operation.

## BRU-Specific Notes

Correctly BRU-specific, and should stay in the driver:

- 2048-byte block boundaries and the physical-record multiple-of-2048 invariant
- BRU checksum: four-lane parallel byte-sum in ASCII hex
- The placeholder checksum bytes during checksum computation
- ASCII hex field encoding throughout
- `INLINE_DATA_LEN_OFFSET` and `INLINE_DATA_OFFSET` layout in file headers
- `CONTINUATION_DLEN_OFFSET` and `CONTINUATION_DATA_OFFSET` layout
- `dlen` upper bound of 1792, which is 2048 minus the 256-byte continuation
  header
- The `size_high << 32 | size_low` 64-bit encoding
- Archive-header-first ordering invariant
- Resync anchor definition: a block at a 2048-byte-aligned offset with a valid
  `MAGIC_FILE_HEADER` checksum

Already correctly generic, and should live in the recovery engine:

- Retry count on `TapeIoError`
- Checkpoint and resume
- Sparse file writing
- Multi-pass merge

The decision of whether a damaged file header means "skip to next file" or
"abort the archive" is BRU-specific. BRU has no per-file block count, so the
driver can only resync by magic scan, not by counting blocks forward. A future
format with explicit block counts could skip more precisely.

## Risks

### 1. Silent data placement errors

`write_file_data` without an offset is the most dangerous gap. A bug where the
driver accidentally emits data events and damage events out of order, for
example after retry logic is added, will produce silently misplaced bytes in the
recovered file. Add the `file_offset` parameter before building the recovery
engine.

### 2. Delivering corrupt inline data

In `stream_entry_body`, when `header_block.status != BruBlockStatus::Ok`, the
code emits a damage event and then still delivers the inline bytes. This may be
correct, because the bytes exist on tape and the checksum failed, but the sink
receives data with no direct signal that those specific bytes are suspect, only
a prior damage range. If `file_offset` is added, the damage event and data event
cover the same range and the sink can decide what to do. Without it, the sink
has to infer that relationship from event order.

### 3. `walk_archive` consuming the archive header implicitly

Both `scan` and `stream_all` call `read_archive_header` internally through
`walk_archive`. If an operator calls `scan` first to build a catalog and then
`stream_all` on the same reader, the second call fails with "missing BRU archive
header." The doc says "use a fresh reader," but the type system does not enforce
it. Consider either consuming `self` on first use or adding an explicit
`reset()` on `BruArchiveReader`.

### 4. `FormatManifest` stitch requires out-of-band knowledge

Any merge logic built outside `ForeignTapeFormat::merge_manifests` will need to
know BRU's `adapter_state` binary layout. If `adapter_state` evolves, stitch
code outside the driver could misinterpret old manifests. Lock the
`adapter_state` schema to the `version()` field early.

### 5. `dlen > 1792` is a correctness guard and resync heuristic

If a bit flip corrupts `dlen` to a value less than or equal to 1792 but wrong,
the driver will read N bytes from the wrong offset and emit them as payload.
This is inherent to BRU's lack of a per-file end marker and is not fixable from
the outside. Document it as a known false-clean risk: checksum passes, `dlen` is
in range, but the field is corrupt. The per-block checksum is the real guard.

### 6. Migration SHA-256 requirement

`rem-tar-v1` requires `file_sha256` before writing. A damaged tape recovery may
deliver a file in two partial passes. The SHA-256 must be computed over the
final merged content, not the partial passes. This means either spooling the
entire recovered file to disk before writing or adding a deferred-hash writer
API for `rem-tar-v1`. Do not start the migration pipeline without deciding
which.

## Implementation Plan

Do not start with the recovery engine. Start with the missing event-model pieces
that every later step depends on.

1. Add `file_offset` to `write_file_data`.

   Change `ArchiveEventSink::write_file_data(bytes)` to
   `write_file_data(file_offset: u64, bytes: &[u8])`. Update `StreamVisitor` in
   `remanence-bru` to track and pass the offset. Update `EventSink` in tests.
   This is a breaking change to the trait and should happen before any other
   code depends on it.

2. Add `report_archive_gap` to `ArchiveEventSink`.

   Define `ArchiveGapRange` with physical LBA range, `GapCause`, and optional
   raw bytes. Add the method to the trait with a default no-op so existing
   implementations do not break.

3. Add BRU resync to `walk_archive`.

   Replace the hard-abort on unexpected magic with gap reporting and a
   2048-byte-aligned scan for the next valid `MAGIC_FILE_HEADER` plus passing
   checksum. Add a unit test with a deliberately corrupted file header and
   verify that the file after it is still recovered and archive-gap events are
   emitted.

4. Add a generic recovery engine skeleton.

   Wrap a `ForeignTapeFormat` and a `PhysicalTapeSource` and drive single-pass
   recovery with configurable retry count, checkpoint file, and a
   `RecoveryManifest` that includes per-file extent lists and per-file damage
   ranges.

5. Add a sparse file sink.

   Implement a `RecoverySink` that writes sparse or extent files from
   `write_file_data(offset, bytes)` and records damage ranges into a sidecar
   JSON file. This is pure I/O and format-agnostic.

6. Add `merge_manifests` on `ForeignTapeFormat` and implement it for BRU.

   The BRU implementation should union recovered extents, intersect damage
   ranges by file sequence number, and flag conflicts.

7. Add the migration pipeline last.

   Only after the recovery event model, sink, resync, and merge pieces exist,
   connect recovery output to `rem-tar-v1`. Decide the SHA-256 spooling strategy
   before starting this step.
