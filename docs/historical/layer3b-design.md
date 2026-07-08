# Layer 3b: tape format — design

**Status:** draft v0.1, 2026-05-18.

**Scope of this document:** the implementation-level design for
Layer 3b — rem's pluggable body-format layer plus the fixed
catalog reader/writer plus the `rem-chunked-v1` built-in format.
The spec (`docs/spec-v0.3.md` §5, §9.1, §9.2) defines *what* must
exist; this document is *how*.

---

## 1. Scope

Layer 3b sits on top of [Layer 3a](layer3a-design.md) and
provides the abstraction every format implementor codes against.
The split is:

```
Layer 5  (gRPC API)       ← caller code: orchestrator, CLI
Layer 4  (local state)    ← catalog cache files, audit log
Layer 3b (tape format)    ← THIS DOC: TapeFormat trait, catalog, formats
Layer 3a (tape mechanism) ← DriveHandle: rewind/locate/space/read/write/...
Layer 2  (identity + ops) ← LibraryHandle: discover, move, load/unload
Layer 1  (SCSI core)      ← remanence-scsi: CDB builders + sg_io
```

### Goals

- Make rem the kind of system where adding a new body format means
  implementing one trait, registering one factory, and recompiling.
- Land a default format (`rem-chunked-v1`) that supports the full
  Tier 0 + Tier 1 + Tier 2 + `VERIFIABLE` + `METADATA_PRESERVING`
  capability set so PFR (Per-File-Recovery) and partial-file
  recovery work out of the box. See `docs/pfr-reference.md`.
- Land the fixed catalog reader/writer that every tape format
  shares — the bootstrap layer that lets a future tape reader
  identify what's on a tape with zero out-of-band context.
- Keep the registry boundary at compile-time. No plugin loaders,
  no dynamic library boundaries, no scripting hooks. The trust
  surface of a long-lived daemon with hardware access is too big
  to widen for the convenience of out-of-tree formats.

### Non-goals (for this doc)

- Layer 5 streaming, batching, and the gRPC API — those compose
  3b primitives.
- The on-disk catalog cache (Layer 4) — Layer 3b reads/writes the
  catalog *block on tape*; Layer 4 caches the parsed result.
- `pax-tar-v1` implementation. Sketched in §8 and signposted in
  the Step 10.x plan, but the initial 3b shipping target is
  `rem-chunked-v1` + the catalog + the trait shape. `pax-tar-v1`
  follows.
- Format authoring guide / external-format docs. Once the trait
  shape lands and `rem-chunked-v1` exercises it, a follow-up
  document covers how third parties write
  `rem-format-bareos`-style crates.

---

## 2. Background — what 3a leaves for 3b

Layer 3a gives 3b a `DriveHandle<'a>` whose surface, after the
Step 9.x work landed, is exactly the SSC primitive set:

```rust
// Positioning
pub fn rewind(&mut self) -> Result<(), TapeIoError>;
pub fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError>;
pub fn space(&mut self, count: i64, kind: SpaceKind) -> Result<SpaceResult, TapeIoError>;
pub fn position(&mut self) -> Result<TapePosition, TapeIoError>;

// Data
pub fn read_block(&mut self, buf: &mut [u8]) -> Result<usize, TapeIoError>;
pub fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError>;
pub fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError>;

// Session config
pub fn read_config(&mut self) -> Result<TapeConfig, TapeIoError>;
pub fn write_config(&mut self, cfg: TapeConfig) -> Result<(), TapeIoError>;
```

3a does not own:

- Block grouping into objects (3b decides what an "object" is on tape).
- File mark placement (3b chooses where to write file marks).
- Block-level interpretation (3b decides what each block contains:
  chunk, header, index entry, ...).
- Catalog reading or writing (3b's responsibility — 3a has no
  notion of a catalog).

3b consumes these primitives and never reaches around them to
Layer 1 or 2. The trait `TapeFormat` is parametrised over a
`BlockSink` / `BlockSource` adapter (see §4.5) so that:

- Production code wires `BlockSink`/`BlockSource` to a
  `DriveHandle`.
- Unit tests wire them to an in-memory `Vec<Vec<u8>>` and verify
  the on-tape byte layout without touching SG_IO.
- Cross-format byte-shape comparison ("does my format read what
  the old version wrote?") becomes a fixture test, not a
  hardware test.

---

## 3. Crate boundary

A new crate, `crates/remanence-format`, depends on
`remanence-library` (for `DriveHandle` + `TapeIoError` +
`TapePosition` re-exports **and** the shared
`BlockSink` / `BlockSource` traits — see §4.5) and on
`remanence-scsi` only indirectly.

```
remanence-cli ──┐
                ├─→ remanence-format ──→ remanence-library ──→ remanence-scsi
remanence-api ──┘   ├─→ remanence-parity ──┘
                       (Layer 3c — sibling crate; see layer3c-design-v0.2.md)
```

**Sibling-not-stacked**: `remanence-format` (Layer 3b) and
`remanence-parity` (Layer 3c) are **true siblings**. Both
depend on `remanence-library`; neither depends on the other.
The composition happens at the daemon level — Layer 5 wraps a
`DriveHandleSink` in a `ParitySink` and hands the parity sink to
the format's `begin_write`. The format is oblivious to whether
its sink is paritied. See `docs/layer3c-design-v0.2.md` §3 for
the read-path mirror.

### Why a separate crate

- **Trait surface is the public API for format authors.** Keeping
  it in its own crate makes the dependency graph for
  `rem-format-bareos`-style crates obvious: depend on
  `remanence-format`, implement `TapeFormat`, register at
  `main`-time.
- **Catalog reader/writer is reusable.** A future read-only tool
  (e.g. a "what's on this tape" inspector that runs without the
  daemon) wants the catalog parser without pulling in Layer 2
  policy / Layer 5 gRPC.
- **`zstd-seekable` and other format-side dependencies don't
  leak into the library crate.** rem-chunked-v1's CBOR + zstd +
  SHA-256 dependency set has no business in Layer 2.

### Workspace dependency additions

Initial set (refined as steps land):

- `ciborium` (CBOR codec; current Rust CBOR ecosystem leader,
  serde-compatible, no_std-capable).
- `ciborium-io` (Read/Write traits for the codec).
- `sha2` (SHA-256 for the per-chunk checksum).
- `zstd` (zstd compression). The `seekable` variant is
  not required because rem-chunked-v1 writes one chunk per tape
  block — the standard zstd codec produces independent frames
  per chunk already; the seekable-format wire layout (skippable
  frames, EOF seek table) is *not* what's on tape (per
  `docs/pfr-reference.md` §6.4).
- `uuid` (object IDs, file IDs as UUIDs are an acceptable
  Tier 1 file-identity scheme).
- `bitflags` (capability flags).
- `thiserror` (error enum; already in workspace).

No new system deps. No `pkg-config` requirements (the `zstd` crate
ships its own statically-linked C library by default; the
`pkg-config` feature is opt-in).

---

## 4. Domain model — trait shape

### 4.1 `Capabilities`

A `bitflags!` declaration listing every capability a format may
advertise. The full set is fixed by Remanence — adding a new
capability is a major-version event for the trait crate.

```rust
bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Capabilities: u32 {
        // Tier hierarchy
        const TIER_0 = 1 << 0;  // mandatory; every format
        const TIER_1 = 1 << 1;  // FileAddressable
        const TIER_2 = 1 << 2;  // ByteRangeAddressable

        // Orthogonal
        const VERIFIABLE          = 1 << 8;
        const APPENDABLE          = 1 << 9;
        const RESUMABLE_WRITE     = 1 << 10;
        const SPARSE_PRESERVING   = 1 << 11;
        const METADATA_PRESERVING = 1 << 12;
        const ENCRYPTED           = 1 << 13;
        const COMPRESSED          = 1 << 14;
    }
}
```

A format author cannot lie about capabilities at runtime: the
`TapeFormat` trait's `as_*()` upcast methods enforce that any
flag set in `capabilities()` is backed by `Some(...)` on the
matching upcast, and that any `None` upcast clears its flag (see
§4.3). Layer 4 verifies this at registration time, before any
tape motion.

### 4.2 `FormatId`

A short, immutable, ASCII-only identifier that lives on the
tape's catalog block. Format IDs are forever — once a single
tape exists with `id="rem-chunked-v1"` the daemon must be able to
decode it indefinitely. New format versions get new IDs
(`rem-chunked-v2`); rem-chunked-v1 is never deprecated in code.

```rust
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FormatId(Cow<'static, str>);
```

Backed by `Cow<'static, str>` so in-tree formats can use
`FormatId::new_static("rem-chunked-v1")` without allocation, and
catalog deserialisation can carry an owned `String` when reading
an external format ID.

### 4.3 `TapeFormat` — Tier 0

```rust
pub trait TapeFormat: Send + Sync {
    /// Stable identifier written into the tape catalog. Match
    /// against this string to find the factory at registry lookup.
    fn id(&self) -> &FormatId;

    /// Capabilities the format advertises. Verified at registry
    /// registration: every set flag must have a matching
    /// `as_*()` returning `Some`.
    fn capabilities(&self) -> Capabilities;

    /// Begin a write session: returns a writer that takes
    /// objects one at a time and emits tape blocks to the sink.
    /// The sink writes those blocks via DriveHandle in the
    /// production path; tests substitute an in-memory sink.
    fn begin_write<'a>(
        &self,
        sink: &'a mut dyn BlockSink,
        params: WriteParams,
    ) -> Result<Box<dyn ObjectWriter + 'a>, FormatError>;

    /// Begin a read session: returns a reader that the caller
    /// drives in object order. For Tier 1/2 access the caller
    /// uses the upcast methods below instead.
    fn begin_read<'a>(
        &self,
        source: &'a mut dyn BlockSource,
    ) -> Result<Box<dyn ObjectReader + 'a>, FormatError>;

    // Capability upcasts. Default impl returns None; format
    // authors override the methods they implement. The
    // registry's verify-on-register step enforces consistency
    // between these and the capability flags.
    fn as_file_addressable(&self) -> Option<&dyn FileAddressable> { None }
    fn as_byte_range_addressable(&self) -> Option<&dyn ByteRangeAddressable> { None }
    fn as_verifiable(&self) -> Option<&dyn Verifiable> { None }
    // ... one method per orthogonal capability that has its own trait
}
```

Tier 0 obligations from spec §5.3:

1. Parseable from any start-of-file-mark position without
   external state. ⇒ Format header magic at byte 0 of every
   object.
2. Tolerates concatenation. ⇒ No "next object pointer" fields
   inside an object header; each object stands alone.
3. Capability flags consistent with upcasts. ⇒ Enforced by
   `FormatRegistry::register`.

### 4.4 `FileAddressable` and `ByteRangeAddressable` (Tier 1/2)

```rust
pub trait FileAddressable: Send + Sync {
    /// Position the underlying source at the start LBA of the
    /// requested file and return a reader over the file's bytes.
    /// `object` is the catalog-derived view (see below) — it
    /// carries `start_lba` / `end_lba` plus the format-specific
    /// options the reader needs (e.g. `rem-chunked-v1`'s
    /// `chunk_size` for the per-chunk math).
    fn open_file<'a>(
        &self,
        source: &'a mut dyn BlockSource,
        object: &ObjectInfo,
        file_id: &FileId,
    ) -> Result<Box<dyn Read + 'a>, FormatError>;
}

pub trait ByteRangeAddressable: FileAddressable {
    /// Read a half-open byte range [start, end) within a file.
    /// `start` and `end` are file-relative; the format does the
    /// chunk-boundary math.
    fn read_byte_range<'a>(
        &self,
        source: &'a mut dyn BlockSource,
        object: &ObjectInfo,
        file_id: &FileId,
        start: u64,
        end: u64,
    ) -> Result<Box<dyn Read + 'a>, FormatError>;

    /// The smallest byte range the format can locate without
    /// reading + discarding intra-chunk bytes, **for the given
    /// object**. For `rem-chunked-v1` this is the
    /// `chunk_size` recorded in the object header (configurable
    /// per write session, see §7.2). For a non-chunked format it
    /// would be the file size. Codex 20:55 idref=a0d30ae3 caught
    /// the earlier `&self`-only signature that could not return
    /// per-object data without external context.
    fn seek_granularity(&self, object: &ObjectInfo) -> u64;
}
```

`ObjectInfo` is the format-agnostic view of an object the catalog
records and a `TapeFormat::object_info(...)` accessor builds from
the format-specific header on demand:

```rust
pub struct ObjectInfo {
    pub object_uuid: [u8; 16],
    pub caller_object_id: String,
    pub format_id: FormatId,
    pub start_lba: u64,
    pub end_lba: u64,        // exclusive
    pub length_bytes: u64,
    pub format_options: BTreeMap<String, CborValue>,
}
```

`format_options` carries whatever the format-specific header
captured (for `rem-chunked-v1`: `chunk_size`,
`compression`, `compression_level`). Layer 5 keeps the
`ObjectInfo` for every queryable object in its in-memory catalog
and hands it back on every read call so format methods always
have the per-object context they need.

`FileId` is intentionally opaque:

```rust
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FileId(pub Vec<u8>);
```

A format chooses its file-identity scheme (UUID per file,
`(path, sequence)`, opaque blob, ...) and the catalog stores
whatever the format puts there. The caller (Layer 5) treats
`FileId` as a token — it's whatever the format gave back from
the most recent `list_files()`. Path-only identity is forbidden
by spec §5.3 Tier 1 obligation 2.

### 4.5 `BlockSink` and `BlockSource`

The trait surface that adapts `DriveHandle` for format AND
parity-layer use. Both traits **live in
`remanence-library::block_io`** so that `remanence-format`
(Layer 3b) and `remanence-parity` (Layer 3c) are true siblings —
neither depends on the other, both depend on `remanence-library`.

```rust
// In remanence-library/src/block_io.rs:
pub trait BlockSink {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError>;
    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError>;
    fn position(&mut self) -> Result<TapePosition, TapeIoError>;
}

pub trait BlockSource {
    fn read_block(&mut self, buf: &mut [u8]) -> Result<usize, TapeIoError>;
    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError>;
    fn space(&mut self, count: i64, kind: SpaceKind) -> Result<SpaceResult, TapeIoError>;
    fn position(&mut self) -> Result<TapePosition, TapeIoError>;
}
```

The traits return `TapeIoError` directly rather than a
format-specific error type. Each higher-layer crate's error type
wraps `TapeIoError` via `#[from]`:

```rust
// remanence-format
pub enum FormatError {
    #[error("tape I/O: {0}")]
    TapeIo(#[from] TapeIoError),
    // ... format-specific variants
}
```

#### Adapter: `DriveHandleSink` and `DriveHandleSource`

Two newtype wrappers in `remanence-library::block_io` adapt
`DriveHandle<'a>` to the traits:

```rust
pub struct DriveHandleSink<'a, 'b>(pub &'a mut DriveHandle<'b>);
pub struct DriveHandleSource<'a, 'b>(pub &'a mut DriveHandle<'b>);
```

Callers wrap explicitly at every call site:

```rust
let mut sink = DriveHandleSink(&mut drive);
format.begin_write(&mut sink, params)?;
```

Picking a newtype over a blanket-impl-via-trampoline-trait keeps
the dep graph + orphan rule simple, and makes the adapter
explicit. Easy to extend (the wrapper can log, time, fault-inject
without touching DriveHandle).

#### In-memory test fixtures

`remanence-library::block_io` also exposes `VecBlockSink` and
`VecBlockSource` — fixtures that back the traits with
`Vec<Vec<u8>>` and expose the captured call sequence for
assertions. Format and parity tests use these directly without
defining their own.

Step 10.2 (which lands as a prerequisite for Layer 3c per the
user's structural directive on 2026-05-19) ships these traits +
adapters + fixtures together.

---

## 5. The catalog (fixed format)

The catalog is the only on-tape structure rem itself defines. It
lives at file mark 0 of every tape, with a refresh copy every K
objects and a final copy at end-of-data. Spec §5.2 lists the
contents; this section pins the wire format.

### 5.1 Catalog envelope (multi-block)

A 200,000-object catalog is ~50–100 MiB per spec §5.2, but the
LTO maximum logical block size is **16 MiB on LTO-9, 8 MiB on
LTO-7/8, 1 MiB on LTO-6** (`docs/layer3a-design.md` §6.1; codex
20:55 idref=a0d30ae3 High catch). One catalog therefore spans
multiple tape blocks. The envelope is a two-level structure:

- **Header block** (one tape block at the start, always fits):
  carries magic + version + total payload size + payload block
  count. Reader uses it to know how many blocks to read.
- **Payload blocks** (one or more): the CBOR-encoded
  `CatalogPayload` is split into payload-block-sized chunks and
  written as consecutive tape blocks. Each payload block carries
  a small per-block header (sequence number + crc32 of its
  bytes) so a torn write is detectable mid-stream.

**Header-block layout** (fits in one tape block; padded out to
the configured tape block size):

```
+--------+--------+--------+--------+--------+--------+--------+--------+
| 'R'    | 'E'    | 'M'    | 0x00   | 'C'    | 'A'    | 'T'    | 0x01   |  <- magic
+--------+--------+--------+--------+--------+--------+--------+--------+
| major_version (u16 BE) | minor_version (u16 BE) | flags (u32 BE)    |
+--------+--------+--------+--------+--------+--------+--------+--------+
| payload_len (u64 BE — total bytes across all payload blocks)         |
+--------+--------+--------+--------+--------+--------+--------+--------+
| payload_block_count (u32 BE) | payload_block_size (u32 BE)           |
+--------+--------+--------+--------+--------+--------+--------+--------+
| crc32_header (u32 BE — CRC of bytes 0..28)                           |
+--------+--------+--------+--------+--------+--------+--------+--------+
```

**Payload-block layout** (one per tape block, padded out):

```
+--------+--------+--------+--------+--------+--------+--------+--------+
| 'R'    | 'E'    | 'M'    | 0x00   | 'P'    | 'L'    | 'D'    | seq8   |  <- magic + seq8 (low 8 bits of seq)
+--------+--------+--------+--------+--------+--------+--------+--------+
| seq (u32 BE — full sequence number, starting at 0)                   |
+--------+--------+--------+--------+--------+--------+--------+--------+
| this_block_payload_len (u32 BE)  | crc32_payload (u32 BE)            |
+--------+--------+--------+--------+--------+--------+--------+--------+
| CBOR payload slice (this_block_payload_len bytes)                    |
| ...                                                                  |
+--------+--------+--------+--------+--------+--------+--------+--------+
```

- **Magic on header block**: `b"REM\x00CAT\x01"`. The version
  byte at offset 7 is duplicated by `major_version` in the next
  field as a defense-in-depth against same-magic-different-major
  scenarios.
- **Magic on payload block**: `b"REM\x00PLD<seq8>"` — distinct
  from header magic so a reader can't mistake them, and the seq
  low byte is in the magic so a single-block stream-spot check
  hints at sequence.
- **payload_block_size**: the writer's per-block payload
  capacity, recorded so a future reader can verify continuity
  without round-tripping through the drive's MODE SENSE.
- **payload_block_count × payload_block_size ≥ payload_len**:
  the final block may carry a short payload slice (≤
  `payload_block_size`); intermediate blocks are full.
- **crc32_payload (per-block)**: CRC of just this block's
  payload slice. Catches torn-write / bit-rot of individual
  blocks. The full-payload integrity also verifies via the CBOR
  reader's own end-of-stream check.

CBOR payload schema is unchanged from §5.2 — the reassembled
byte stream across all payload blocks decodes as a single
`CatalogPayload`. Reader: read header block, validate magic +
header CRC, allocate a `Vec` of `payload_len` bytes, then read
`payload_block_count` payload blocks in sequence, concatenate
their payload slices, CBOR-decode the result.

### 5.2 CBOR payload schema

```cbor
CatalogPayload = {
    1: TapeIdentity,
    2: [* ObjectEntry],
    3: ?EncryptionParameters,    ; optional; omitted on unencrypted tapes
    4: ?[* CatalogRefreshPointer], ; optional; only set in the final catalog
    5: ?ExtensibilityMap,         ; major-version-incompatible extensions
}

TapeIdentity = {
    1: bytes .size 16,            ; tape UUID (UUIDv4, 16 bytes — codex 20:55
                                  ;   idref=a0d30ae3 Low/Medium: was 8 bytes,
                                  ;   inconsistent with §11.2 + the uuid crate)
    2: tstr,                      ; barcode (operator-readable)
    3: uint,                      ; generation (LTO-9 = 9)
    4: tstr,                      ; write timestamp (RFC3339)
    5: tstr,                      ; rem schema version (e.g. "1.0")
    6: tstr,                      ; rem software version (e.g. "0.0.1")
}

ObjectEntry = {
    1: bytes .size 16,            ; object UUID (rem-assigned)
    2: tstr,                      ; caller-supplied object ID
    3: tstr,                      ; format ID (e.g. "rem-chunked-v1")
    4: uint,                      ; start LBA on tape
    5: uint,                      ; end LBA on tape (exclusive)
    6: uint,                      ; length in bytes
    7: uint,                      ; format capabilities snapshot (Capabilities bits)
    8: ?bytes .size 32,           ; SHA-256 of object body (VERIFIABLE only)
    9: ?bytes,                    ; optional per-file LBA index (FileAddressable only;
                                  ;   CBOR-encoded per format's choice)
    10: ?{ * tstr => any },       ; format-supplied metadata blob
}
```

CBOR field tags rather than names → smaller catalogs + stable
forward compatibility. The schema is fixed; adding fields means
new tag numbers, never re-using old ones. (CBOR tagged-key
convention rather than tstr-key matches `ciborium`'s ergonomics
and is half the byte count.)

### 5.3 Reader / writer surface

```rust
pub struct CatalogReader;
pub struct CatalogWriter;

impl CatalogReader {
    pub fn read<R: Read>(reader: R) -> Result<CatalogPayload, CatalogError>;
}

impl CatalogWriter {
    pub fn write<W: Write>(payload: &CatalogPayload, writer: W) -> Result<(), CatalogError>;
}
```

The reader/writer are purely byte-oriented; they don't know
about tapes. Layer 3b composes them with a `BlockSink`/
`BlockSource` to write the catalog at the right tape positions.

### 5.4 Catalog placement on tape

```
[BOT]
    [catalog block 0 — initial, just object[0] entry pre-write]
    FM 1
    [object 1 body — opaque-to-3b format]
    FM 2
    ...
    FM K
    [catalog refresh — entries for objects 1..K]
    FM K+1
    [object K+1 body]
    ...
    [catalog final — every object entry, written twice]
    FM N
    [EOD]
```

`K` is configurable (default: 256 objects or 1 hour, whichever
comes first). Refreshes give bounded data loss on a torn write —
if the tape gets pulled mid-write at object 700 but the last
refresh covered up to 512, a future reader can still parse the
first 512 objects.

The final catalog is written twice. Both copies must round-trip
identical; the reader treats the second as authoritative iff the
first fails crc32 + cbor parse.

---

## 6. Format registry

```rust
pub struct FormatRegistry {
    formats: HashMap<FormatId, Arc<dyn TapeFormat>>,
}

impl FormatRegistry {
    pub fn new() -> Self;

    /// Add a format. Verifies that capability flags and
    /// as_*() upcasts agree. Returns `Err` if a format with
    /// the same id is already registered or if the verification
    /// fails. The latter is a programming error in the format
    /// implementation; the daemon refuses to start.
    pub fn register(&mut self, format: Arc<dyn TapeFormat>) -> Result<(), RegistryError>;

    /// Look up a format by id. Returns `None` if not registered;
    /// caller surfaces that as `UnknownFormat` (per spec §5.4)
    /// and refuses to touch the tape.
    pub fn get(&self, id: &FormatId) -> Option<Arc<dyn TapeFormat>>;
}
```

The daemon constructs the registry once at startup, registers
every in-tree format explicitly, registers any external formats
in dependency order, and hands the registry to Layer 5. Layer 5
queries it on every tape mount to resolve format IDs to
implementations.

External formats are added by depending on `remanence-format` in
the daemon's Cargo.toml and adding the `register` call in
`main`. There is no dynamic-loading path.

---

## 7. `rem-chunked-v1` — the default format

This section pins the wire format. Spec §9.1 marks the detailed
schema as "best decided when the format crate begins
implementation"; this is that decision.

### 7.1 On-tape layout (append-only)

Each `rem-chunked-v1` object on tape is a strictly append-only
sequence of blocks (codex 20:55 idref=a0d30ae3 High catch on the
earlier draft, which contradicted itself by mixing
seek-back-and-rewrite with the no-overwrite constraint):

```
+----+----+----+----+----+----+----+----+
| partial_header (one tape block)       |   <- OBJ magic + partial=true,
+----+----+----+----+----+----+----+----+      counts unknown yet
| chunk[0] (one tape block)             |
| chunk[1] (one tape block)             |
| ...                                   |
| chunk[N-1] (one tape block)           |
+----+----+----+----+----+----+----+----+
| chunk_index (one or more tape blocks; |   <- IDX magic + kind=chunks
|   multi-block envelope; §5.1 shape)   |
+----+----+----+----+----+----+----+----+
| file_index (one or more tape blocks;  |   <- IDX magic + kind=files
|   multi-block envelope)               |
+----+----+----+----+----+----+----+----+
| final_header (one tape block)         |   <- OBJ magic + partial=false,
|                                       |      complete counts + indexes' LBAs
+----+----+----+----+----+----+----+----+
[file mark separating this object from the next]
```

The layout is **append-only**: no seek-back-and-rewrite. The
final header is written last, after every chunk and index block;
it contains the chunk count, file count, body SHA-256, and the
LBA of the first block of each index. The partial header at
block 0 lets a forward scanner identify the start of an object
without external state (spec §5.3 Tier 0 obligation 1).

**One chunk per tape block** is the invariant that makes PFR
work without read-amplification (per `docs/pfr-reference.md`
§4 + §7). Encrypted tapes need this for correctness, not just
performance: per-block AES-256-GCM means each tape block has its
own authentication tag.

File marks are not used inside an object — they separate
objects (per spec §5.1 layout). Layer 3b writes one file mark
between each object and after the last one before the closing
catalog.

#### How readers locate blocks

- **Catalog-driven reads (the normal case)**: the catalog records
  `start_lba` and `end_lba` per object. The final header is
  always at `end_lba - 1`. A reader LOCATEs there, parses the
  final header, and learns the LBAs of the file index, chunk
  index, and first chunk in one round-trip.
- **Catalog-corrupt recovery (spec §5.3 Tier 1 obligation 4)**:
  a forward scan from the object's first block reads tape
  blocks one at a time and identifies each by its first 8 bytes:
  - `REM\x00OBJ\x01` → object header (partial or final per the
    flag in the CBOR payload).
  - `REM\x00IDX\x01` → index block (chunk index or file index
    per the `kind` field).
  - Anything else → a chunk. The scanner records chunk LBAs
    until it hits the first IDX magic, then consumes the index
    envelope, then expects more IDX or the final OBJ block.
  Magic collision with raw user data is 2⁻⁶⁴ per chunk — fine
  for a recovery path that's already operating on a damaged
  catalog. Each header / index block also carries its own crc32
  so a false-magic chunk is rejected at parse time.

### 7.2 Object header (partial and final)

Same single-block envelope shape as a small catalog payload
(magic + version header + payload_len + crc32 + CBOR payload,
all fitting in one tape block). Two variants distinguished by
the `partial` field:

```cbor
ChunkedObjectHeader = {
    1: bytes .size 16,             ; object UUID
    2: tstr,                       ; caller-supplied object ID
    3: uint,                       ; chunk_size in bytes (default 1 MiB)
    4: { * tstr => any },          ; format-level options (see below)
    5: bool,                       ; partial — true on the first block of the object,
                                   ;   false on the final block. Reader prefers final.
    6: ?uint,                      ; chunk_count       (required when partial=false; omitted on partial)
    7: ?uint,                      ; file_count        (required when partial=false; omitted on partial)
    8: ?bytes .size 32,            ; body_sha256       (required when partial=false)
    9: ?uint,                      ; first_chunk_lba   (required when partial=false)
    10: ?uint,                     ; chunk_index_lba   (required when partial=false)
    11: ?uint,                     ; file_index_lba    (required when partial=false)
    12: ?ObjectMetadata,           ; optional METADATA_PRESERVING blob (may appear on
                                   ;   either header; partial header carries the caller-
                                   ;   supplied bits, final mirrors them)
}

ObjectMetadata = {
    1: tstr,                       ; writer hostname
    2: tstr,                       ; writer user (POSIX uid + username)
    3: tstr,                       ; write timestamp (RFC3339 ns precision)
    4: { * tstr => any },          ; caller-supplied tags
}
```

Magic for the chunked object header: `b"REM\x00OBJ\x01"` (same
shape as catalog magic; different middle 4 bytes so a stray
catalog block can't be mistaken for an object header). Format
options at field 4:

- `compression`: one of `"none"`, `"zstd"`. Default `"zstd"`
  for PFR-friendliness per `docs/pfr-reference.md` §6.3.
- `compression_level`: u8, default 3. zstd levels 1..22 are
  legal; anything above 9 hits CPU before tape line rate so
  rem caps the default low.

The **partial header** is what a reader sees if a writer was
interrupted mid-object — chunk_count / file_count / body_sha256
are unknown and absent from the CBOR payload. The reader either
gives up (no `is_complete` invariant) or initiates the
spec §5.3 obligation 4 forward-scan recovery to enumerate what
made it to tape.

### 7.3 File index block

Tier 1 per-file LBA index. One entry per file in the object:

```cbor
FileIndex = [* FileIndexEntry]

FileIndexEntry = {
    1: bytes,                      ; file ID (opaque per format choice;
                                   ;   rem-chunked-v1 uses 16-byte UUIDs)
    2: tstr,                       ; original path
    3: uint,                       ; file size in bytes
    4: uint,                       ; first_chunk_within_object
    5: uint,                       ; chunk_count for this file
    6: ?FileMetadata,              ; optional METADATA_PRESERVING blob
}
```

The file index is written **after** all chunks and the chunk
index, just before the final header (§7.1). Its LBA is recorded
in the final header's `file_index_lba` field. The index uses
the same multi-block envelope as the catalog (§5.1) — header
block carrying total payload size + block count, then payload
blocks each carrying a slice of the CBOR-encoded `FileIndex`.
Tier 1 reads do:

1. LOCATE to `end_lba - 1`, read final_header → learn
   `file_index_lba`.
2. LOCATE to `file_index_lba`, read the envelope (header block +
   payload blocks).
3. CBOR-decode into `FileIndex`.
4. Binary-search for the requested `FileId`.

### 7.4 Chunk index block

Tier 2 per-chunk LBA index. One entry per chunk in the object:

```cbor
ChunkIndex = [* ChunkIndexEntry]

ChunkIndexEntry = {
    1: uint,                       ; chunk index within object (0-based)
    2: uint,                       ; tape LBA of this chunk
    3: uint,                       ; uncompressed_size (bytes; equal to chunk_size
                                   ;   except possibly the last chunk of a file)
    4: uint,                       ; compressed_size (bytes; equals uncompressed when
                                   ;   compression = "none")
    5: bytes .size 32,             ; SHA-256 of uncompressed chunk bytes
}
```

Cost estimate: ~80 bytes per entry → ~80 KB per GB of data at
1 MiB chunks. Acceptable per spec §5.3 Tier 2 obligation 5.

The chunk index uses the same multi-block envelope as the
file index and the catalog. Its LBA is recorded in the final
header's `chunk_index_lba` field. Tier 2 reads LOCATE to
`end_lba - 1` to get the final header, LOCATE to
`chunk_index_lba` to read the envelope, then LOCATE to the
specific chunk's LBA from the parsed index.

### 7.5 Chunk encoding

Each chunk is a single tape block. The block payload is:

- If `compression = "none"`: raw uncompressed file bytes.
- If `compression = "zstd"`: one independently-decodable zstd
  frame (no shared dictionary, no inter-frame state).

A chunk's *uncompressed* size is at most `chunk_size` (the
object-header field). The last chunk of a file may be shorter
(spec §5.5 short-last-chunk math); this case is recorded in the
`ChunkIndexEntry::uncompressed_size` field, not inferred.

### 7.6 Capability claims

`rem-chunked-v1` advertises:

```
TIER_0 | TIER_1 | TIER_2 | VERIFIABLE | METADATA_PRESERVING | COMPRESSED
```

`COMPRESSED` is set because the format supports
format-level compression (zstd) and uses it by default. Codex
20:55 idref=a0d30ae3 caught the earlier omission: the default
compression option is `"zstd"` (§7.9) so the format always
advertises `COMPRESSED` capability even though an individual
object may be written with `compression = "none"`.

**Semantics of Capabilities** (codex catch follow-up): the flags
on `TapeFormat::capabilities()` describe what the **format** is
capable of, NOT what an individual object actually uses. A
`COMPRESSED`-capable format can write uncompressed objects (and
record `compression = "none"` in the object header); a
`METADATA_PRESERVING`-capable format can write objects with no
metadata blobs if the caller didn't supply any. The catalog's
`ObjectEntry.format_capabilities_snapshot` records the format's
flag set at the time of write so a future reader can verify
its own format version still supports the on-tape data, even if
the running format implementation has since gained new flags.
Per-object choices (compression on/off, metadata present/absent)
live in the object's format-specific header, not in the
capability flags.

Notably **not** `APPENDABLE` (rem-chunked-v1 closes the object
on `finish_object`; the chunk index is final at that point) —
deferred to a future format revision if there's demand.

`VERIFIABLE` ⇔ the per-chunk SHA-256 fields exist and the reader
verifies on read. `METADATA_PRESERVING` ⇔ the optional metadata
blobs are non-empty for caller-supplied metadata. `COMPRESSED` ⇔
the format can wrap chunk bytes in a compression layer (zstd in
v1).

### 7.7 Write flow (append-only)

The writer streams from start to finish; no seek-back, no
in-place rewrite (LTO drives reject overwriting once you've
appended past a block — codex 20:55 idref=a0d30ae3 High
flagged the earlier draft for proposing both at once):

```
1. begin_write(sink, params {chunk_size, compression, ...})
     -> write partial_header to sink (magic, UUID, caller-id,
        chunk_size, compression options, partial=true)
     -> record object_start_lba = sink.position().lba
2. for each file:
     writer.start_file(FileMetadata)
     for each chunk_in_file:
       encode chunk (zstd-compress if requested)
       write tape block via sink.write_block
       record (chunk_index, lba, sizes, sha256) in chunk_index buffer
     writer.end_file() -> FileIndexEntry (records first_chunk_lba)
3. writer.finish_object()
     -> chunk_index_lba = sink.position().lba
        write chunk_index envelope (one or more tape blocks)
     -> file_index_lba = sink.position().lba
        write file_index envelope (one or more tape blocks)
     -> write final_header to sink (magic, UUID, caller-id,
        chunk_size, compression options, partial=false,
        chunk_count, file_count, body_sha256, first_chunk_lba,
        chunk_index_lba, file_index_lba)
     -> sink.write_filemarks(1)
```

Because the writer captures the post-write `position().lba`
before each section, every LBA recorded in the final header is
exactly what the drive committed. No seek-back is needed.

### 7.8 Read flow

For all three tiers the catalog gives the reader `start_lba` and
`end_lba` of the object. The final header is always at
`end_lba - 1` (one block before the file mark).

**Tier 0 — sequential read of the whole object:**

```
1. format.begin_read(source, ObjectInfo {start_lba, end_lba, ...})
2. internally:
     LOCATE source to end_lba - 1
     read final_header  (one block) -> chunk_count, first_chunk_lba,
                                       chunk_index_lba, body_sha256
     LOCATE to first_chunk_lba
     stream chunk_count blocks, decompressing each, concatenating
3. reader.into_byte_stream() -> impl Read
```

**Tier 1 — single-file read:**

```
1. format.as_file_addressable().open_file(
       source, ObjectInfo, file_id) -> impl Read
2. internally:
     LOCATE to end_lba - 1; read final_header
     LOCATE to file_index_lba; read multi-block file index
     binary-search for file_id -> FileIndexEntry
     LOCATE to entry.first_chunk_lba
     stream entry.chunk_count blocks, decompress, concatenate
```

**Tier 2 — byte-range read:**

```
1. format.as_byte_range_addressable().read_byte_range(
       source, ObjectInfo, file_id, [u_start, u_end))
2. internally:
     LOCATE to end_lba - 1; read final_header
     LOCATE to chunk_index_lba; read multi-block chunk index
       (cached in memory for the rest of the session)
     LOCATE to file_index_lba; read multi-block file index;
       find FileId -> first_chunk_within_object + chunk_count
     compute (first_chunk_within_file, last_chunk_within_file,
              head_drop, tail_drop) per pfr-reference.md §4
     LOCATE to chunk_index[first_abs_chunk].lba
     stream the covering chunks, decompress, head/tail-trim
```

The Tier 0 / Tier 1 readers also cache the chunk index and file
index on the `ObjectReader` so a follow-up call (e.g.
`read_byte_range` after `open_file`) on the same object doesn't
re-LOCATE to the indexes.

#### Catalog-corrupt recovery (forward scan)

If the catalog is unreadable, the reader cannot trust
`end_lba`. The recovery path (spec §5.3 Tier 1 obligation 4)
reads from the object's first block and uses the magic-byte
discrimination from §7.1 to recognise chunks vs index blocks vs
the final header. The partial header tells the recovery scanner
the chunk_size + compression options; the chunk count emerges
from the count of non-magic blocks before the first IDX magic;
the file index emerges from the second IDX envelope.

### 7.9 Recommended defaults

| Parameter        | Default        | Rationale                                                |
|------------------|----------------|----------------------------------------------------------|
| chunk_size       | 1 MiB          | `docs/pfr-reference.md` §5 — sweet spot                  |
| compression      | `"zstd"`       | `docs/pfr-reference.md` §6.3 — PFR-friendly              |
| compression_level| 3              | LTO line rate vs CPU; raise only for unusual workloads   |
| hardware compression | OFF (via TapeConfig.compression=false) | Avoid double-compression; vendor-variable bytes |

---

## 8. `pax-tar-v1` — deferred

Tier 0 + Tier 1 (sidecar index in rem's catalog). The sketch:

- On-tape body is exactly a `pax`-format tar stream. Decoded by
  any `tar` implementation.
- Tier 1 index lives in the catalog block's
  `ObjectEntry::per_file_lba_index` field — a CBOR array of
  `{file_id, path, tar_header_lba, file_data_lba, size}`
  entries.
- No Tier 2 — pax tar has no native chunk boundaries; chunked
  random-access would require a wrapper format, which is what
  `rem-chunked-v1` already is.
- Capabilities: `TIER_0 | TIER_1 | METADATA_PRESERVING`
  (pax extension records carry xattrs).

Implementation lands in a follow-up step (10.10 or later); not
in the initial 3b shipping target.

---

## 9. Error model

```rust
#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    #[error("tape I/O error: {0}")]
    TapeIo(#[from] TapeIoError),

    #[error("catalog parse error: {0}")]
    Catalog(CatalogError),

    #[error("unknown format id: {0}")]
    UnknownFormat(FormatId),

    #[error("magic mismatch at block {at_lba}: expected {expected:02x?}, got {got:02x?}")]
    MagicMismatch { at_lba: u64, expected: [u8; 8], got: [u8; 8] },

    #[error("crc32 mismatch: header says {expected:#x}, computed {got:#x}")]
    CrcMismatch { expected: u32, got: u32 },

    #[error("CBOR decode error: {0}")]
    Cbor(String),

    #[error("file not found in object: {0:?}")]
    FileNotFound(FileId),

    #[error("byte range out of bounds: file is {file_size} bytes, requested [{start}, {end})")]
    ByteRangeOutOfBounds { file_size: u64, start: u64, end: u64 },

    #[error("capability not supported by this format: {capability}")]
    Unsupported { capability: &'static str },

    #[error("corrupt header at lba {at_lba}: {detail}")]
    CorruptHeader { at_lba: u64, detail: String },
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("magic bytes do not match REM CAT")]
    BadMagic,

    #[error("unsupported catalog version {major}.{minor}")]
    UnsupportedVersion { major: u16, minor: u16 },

    #[error("payload length {claimed} exceeds buffer length {actual}")]
    PayloadTooLarge { claimed: u32, actual: usize },

    #[error("crc32 mismatch: header says {expected:#x}, computed {got:#x}")]
    CrcMismatch { expected: u32, got: u32 },

    #[error("CBOR decode error: {0}")]
    Cbor(String),
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("format id already registered: {0}")]
    Duplicate(FormatId),

    #[error("format declares capability {cap} but as_{cap_snake}() returns None")]
    CapabilityMismatch { cap: &'static str, cap_snake: &'static str },
}
```

Layer 5 maps these to gRPC status codes; Layer 4 caches the
catalog parse result so a CBOR parse failure at mount time is
visible to operators via the audit log without re-reading the
tape.

---

## 10. Implementation plan

Each step ends in `cargo fmt + cargo clippy --workspace
--all-targets -- -D warnings + cargo test --workspace + cargo doc
--workspace --no-deps`, all green.

| Step | Description |
|--|--|
| 10.0 | Crate skeleton: new `crates/remanence-format` with `Cargo.toml` (ciborium, sha2, zstd, uuid, bitflags, thiserror), `lib.rs` exporting the public module tree (capabilities, formatid, error, registry, blocksink, blocksource, catalog, formats). Empty stub for each public item. Workspace `Cargo.toml` updated. |
| 10.1 | `Capabilities` bitflags + `FormatId` type + `FormatError` / `CatalogError` / `RegistryError` enums. Unit tests for capability set operations and FormatId equality / hashing / static-vs-owned variants. |
| 10.2 | `BlockSink` / `BlockSource` traits + `DriveHandleSink` / `DriveHandleSource` newtype wrappers + `VecBlockSink` / `VecBlockSource` test fixtures. **Lives in `remanence-library::block_io`** so Layer 3b and Layer 3c are true siblings (user structural decision 2026-05-19; landed ahead of the rest of 3b as a prerequisite for 3c work). |
| 10.3 | `TapeFormat` / `FileAddressable` / `ByteRangeAddressable` / Verifiable trait definitions. No implementations yet; just the trait shape + doc comments + a `panic!()`-bodied test that compiles. |
| 10.4 | Catalog envelope (magic + header + crc32) reader/writer. CRC32 via the `crc` crate. Pure byte tests: round-trip arbitrary payloads, detect magic mismatch, detect crc mismatch, reject oversized payload_len. |
| 10.5 | Catalog CBOR schema + serde-via-ciborium round-trip. `CatalogPayload`, `TapeIdentity`, `ObjectEntry`, etc. Fixture tests against canned CBOR bytes pin the wire format. |
| 10.6 | `FormatRegistry` + `register` / `get`. Capability-vs-upcast consistency check. Tests: register dup-id rejected; register with mismatched capability rejected; lookup returns the right Arc. |
| 10.7 | `rem-chunked-v1` Tier 0: object header (partial + final), chunk encoding (raw + zstd via the `zstd` crate's API), `begin_write` / `finish_object`, `begin_read`. Tests against in-memory `BlockSink`/`BlockSource` round-trip a 3-file object. |
| 10.8 | `rem-chunked-v1` Tier 1: file index block, `as_file_addressable`, `open_file`. Tests verify a Tier 1 read pulls only the LBAs it needs (instrumented BlockSource log). |
| 10.9 | `rem-chunked-v1` Tier 2: chunk index block, `as_byte_range_addressable`, `read_byte_range` + `seek_granularity`. Tests verify the `pfr-reference.md` §4.2 / §4.3 worked examples. |
| 10.10 | `rem-chunked-v1` `VERIFIABLE`: per-chunk SHA-256 enforcement on read. Tests verify a flipped bit in a chunk is caught. |
| 10.11 | `rem-chunked-v1` `METADATA_PRESERVING`: ObjectMetadata + FileMetadata fields populated and surfaced on read. Tests verify round-trip of xattrs / acls / ns-mtime. |
| 10.12 | Integration test against the production catalog placement: write 5 small objects, periodic catalog refresh at K=2, final catalog at EOD, verify all three catalog blocks parse and agree. In-memory only; live-tape smoke deferred. |
| 10.13 | `pax-tar-v1` — Tier 0 + Tier 1 with sidecar index in rem's catalog. Out of initial 3b shipping; signposted here for the format registry to track. |
| 10.14 | QuadStor live-smoke integration test (`#[ignore]`-gated like `quadstor_smoke.rs`) — write a 10-object `rem-chunked-v1` tape, dismount, re-mount, verify catalog parse + Tier 2 random-access read. |
| 10.15 | Wrap-up: design-doc sync, journal final entry, status table update. |

Layer 4 work (on-disk catalog cache) can start in parallel after
step 10.6; Layer 5 (gRPC streaming) needs at least step 10.9
(Tier 2) before its byte-range endpoint is testable.

---

## 11. Open questions

### 11.1 zstd vs zstd-seekable on tape

Settled in §3 / §7.5: plain `zstd` (one frame per chunk = one
tape block). The seekable wire layout (skippable frame + EOF
seek table) is a file-format convenience; on tape the chunk
index in our format header serves the same purpose with less
overhead. Per `docs/pfr-reference.md` §6.4.

### 11.2 Object UUID generation

UUIDv4 (random 128-bit). Avoids the wall-clock dependency of v1
and the namespace decisions of v3/v5. Collision-free at any
realistic tape count.

### 11.3 Catalog refresh cadence

K=256 objects or 1 hour, whichever comes first, by default.
Operator-configurable per write session. Tighter cadence is
strictly safer but slower; the trade-off is recoverable-data-on-
torn-write vs write throughput. Empirical tuning open.

### 11.4 File-index in catalog vs in object

For `rem-chunked-v1` the file index lives as a dedicated
multi-block envelope on tape (§7.3), with its LBA recorded in
the final header — NOT inside the object header (codex 20:55
catch on the earlier wording). It can additionally be mirrored
into the catalog (`ObjectEntry.per_file_lba_index` field) when
the daemon wants the file-list query to land without touching
the object on tape at all.

For `pax-tar-v1` the file index lives only in the catalog
(sidecar index per §8) because the body is opaque tar bytes
with no place to write rem's index.

Settled as "always on tape if FileAddressable, optionally
mirrored in the catalog". `rem-chunked-v1` writes the on-tape
index unconditionally and mirrors to catalog when the write
session sets `WriteParams.mirror_index_to_catalog = true`
(default true so file-list queries are sub-second);
`pax-tar-v1` writes only the catalog copy.

### 11.5 `APPENDABLE` capability

Not in `rem-chunked-v1`'s capability set. The format's "final
header" pattern (§7.7 option b) doesn't admit append-after-close
without a more elaborate sentinel-rewrite scheme. Deferred to
`rem-chunked-v2` if demand appears.

### 11.6 Encryption integration

Spec §5.5 + `pfr-reference.md` §7: LTO hardware encryption is
per-block AES-256-GCM. rem-chunked-v1's one-chunk-per-block
invariant is exactly what encryption needs to keep PFR working.
The format header's compression option doesn't interact with
encryption (encrypt-then-compress doesn't help; compress-then-
encrypt is what we do).

Whether to wire `SECURITY PROTOCOL IN/OUT` (the SCSI commands
for key set-up) lives in Layer 3a, not 3b. The 3b format only
records the wrapped-DEK reference in the catalog
(`EncryptionParameters` field).

---

## 12. References

- `docs/spec-v0.3.md` §3.2, §5, §9.1, §9.2 — architectural
  position, on-tape format, open-questions for the catalog
  schema.
- `docs/pfr-reference.md` — full PFR mechanics, chunk-size
  rationale, compression posture, encryption interaction.
- `docs/layer3a-design.md` — the layer this depends on.
- IBM LTO SCSI Reference GA32-0928-08 — Layer 3a's SSC primitive
  documentation; 3b only consumes 3a's surface.
