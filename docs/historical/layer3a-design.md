# Layer 3a Design: Tape Mechanism

**Status:** Draft for review. Sister documents: `docs/layer2-design.md`
(Layer 2a — discovery), `docs/layer2b-design.md` (Layer 2b — state-
changing changer + drive ops), `docs/layer2c-design.md` (Layer 2c —
hot-plug watcher). Spec reference: `docs/spec-v0.3.md` §3.2 (layering),
§5 (on-tape format — sets the constraints Layer 3a must satisfy).

---

## 1. Scope

Layer 3a is the **block-level tape I/O layer**. It exposes raw read,
write, and positioning primitives against a loaded tape — the
mechanism the body-format layer (3b) builds on top of.

### Goals

- Provide a small, audited set of SCSI tape (SSC) operations:
  `REWIND`, `LOCATE`, `READ POSITION`, `SPACE`, `READ`, `WRITE`,
  `WRITE FILEMARKS`, `MODE SELECT` / `MODE SENSE` for block-size and
  compression configuration.
- Stay **format-agnostic**. This layer pushes and pulls bytes at LBAs;
  it does not know what an object, file, chunk, or catalog block is.
  Those are Layer 3b concepts.
- Inherit Layer 2b's audit + dirty-state model: every state-changing
  CDB fires `Started` / `Finished` events, and completion-unknown
  failures mark the drive's snapshot dirty with cause
  `CompletionUnknown`. CHECK CONDITION leaves it clean.
- Inherit Layer 2's per-operation-class SCSI timeouts. Add the new
  classes Layer 3a needs (positioning, block I/O).
- Single CDB per call. **No batching, no pipelining, no internal
  request queue.** Streaming and progress reporting at this layer
  mean "the caller calls `write_block` 1000 times in a loop"; rate
  matching and bigger-picture streaming live in Layer 5.

### Non-goals (for this doc)

- **On-tape format awareness.** No catalog blocks, no chunk indexes,
  no file boundaries, no compression / encryption parameter
  serialisation. All those live in Layer 3b.
- **Cartridge-level lifecycle.** Loading, unloading, moving, and the
  composed `load` / `unload` / `import` / `export` operations are
  Layer 2b; they stay there.
- **PERSISTENT RESERVE.** Per `docs/spec-v0.3.md` §9.3 the lean is
  "minimal" — per-drive reservation during an active write or read
  session — but the exact reservation parameters land with the
  session lifecycle in Layer 5, not here.
- **Encryption.** SECURITY PROTOCOL IN/OUT and the KEK / DEK key
  flow are Layer 6.
- **Tape verification.** Read-back-and-checksum-on-write is a Layer
  3b or Layer 5 policy decision per spec §9.4; Layer 3a only
  provides the read/write primitives that make verification
  possible.
- **High-level cancellation semantics.** Per spec v0.3 §7.3,
  cancellation is best-effort at safe points; Layer 3a's
  contribution is "one CDB per call, atomic completion or transport
  error." Composed cancellation is Layer 5.

---

## 2. Background — what 2a/2b leave for 3a

Layer 2a's `discover()` returns a `DiscoveryReport`. Layer 2b's
`LibraryHandle::open_drive(bay, policy)` returns a `DriveHandle`
that:

- Holds a `Box<dyn SgTransport>` connected to the drive's `/dev/sgN`.
- Carries the parent library's audit hook.
- Already supports SSC `LOAD` / `UNLOAD` / lock / unlock via
  `execute_none` no-data CDBs.

Layer 3a does not maintain a process-local "bay busy" registry. Direct
callers that open drive handles themselves must serialize access to a
bay; production exclusivity is enforced by Layer 5 drive/session
reservations. This keeps the low-level handle usable for composed
flows that reopen a drive after the prior handle has been dropped.

Layer 3a's question is: **what does a `DriveHandle` need to add so
that, against a *loaded* tape, the caller can read and write byte
streams at arbitrary LBAs?** The constraints set by Layer 3b
(§3.3 of this doc) plus the PFR reference doc
(`docs/pfr-reference.md`) give us a precise answer.

**Where the code lives.** Earlier drafts of this doc proposed a
separate `remanence-tape` crate wrapping `DriveHandle` via a new
`TapeIoHandle<'a>::from_drive(drive)` adapter. Codex review
`93f242da` (High) correctly observed that this can't work as
written: `DriveHandle`'s `transport` and `audit_hook` fields are
private, and `LibraryHandle::mark_dirty` is also private. An
external crate has no way to issue arbitrary CDBs through the
drive, emit audit events on the parent's hook, or update the
snapshot dirty-state. Exposing these as a public API surface
just to satisfy a crate-split aesthetic would widen the trust
boundary unnecessarily.

The first repair pass moved Layer 3a inside `remanence-library`
as a sibling module `src/tape_io/`. Codex review `97997d71`
(High) then caught a second-order problem with that structure:
**a sibling module still cannot see fields private to
`crate::handle`** (Rust modules grant visibility upwards via
`pub`, not downwards to siblings), AND `DriveHandle` does not
carry a reference to the parent `LibraryHandle`'s dirty-state
fields. The existing Layer 2b code at `handle.rs:1308-1315`
explicitly notes this limitation for direct
`DriveHandle::load`/`unload` calls.

So: **Layer 3a lives inside `remanence-library`, with `tape_io`
as a child module of `handle`**. Two structural changes from the
status quo:

1. `src/handle.rs` (current single file, ~4500 lines) becomes a
   directory: `src/handle/mod.rs` (the existing content) plus
   `src/handle/tape_io.rs` (Layer 3a methods on `DriveHandle`).
   Rust child modules can see all of their parent's private
   items, so `tape_io.rs` has direct access to the private fields
   of `LibraryHandle` and `DriveHandle`.

2. `DriveHandle<'a>` gains a new borrowed field: a mutable
   reference to a small `DirtyState` struct factored out of
   `LibraryHandle`. With this, the existing Layer 2b
   `DriveHandle::load`/`unload` direct-call error paths can
   finally flip the parent dirty bit instead of relying on the
   caller to consume the audit event's `dirty: bool` signal —
   closing the TODO at the cited handle.rs lines. Layer 3a
   methods use the same path.

The new field on `DriveHandle`:

```rust
pub struct DriveHandle<'a> {
    // ... existing fields ...
    /// Borrowed reference to the parent LibraryHandle's dirty
    /// state. Flipped on transport-level errors with cause
    /// `CompletionUnknown`; cleared on a clean refresh (which
    /// only LibraryHandle itself does).
    dirty: &'a mut DirtyState,
}

// Existing private fields on LibraryHandle re-grouped under
// a small struct so DriveHandle can borrow them atomically.
struct DirtyState {
    is_dirty: bool,
    dirty_cause: Option<DirtyCause>,
}
```

The `mark_dirty` / `clear_dirty` helpers move to `impl DirtyState`
(or stay as inherent helpers on `LibraryHandle` and a parallel
`pub(super) fn` on `DirtyState` — either works).

Module + file structure:

- `src/handle/mod.rs` — `LibraryHandle`, `DriveHandle`,
  `DirtyState`, the existing impl blocks for Layer 2b.
- `src/handle/tape_io.rs` — `impl DriveHandle<'a> { /* layer 3a
  methods */ }`. Plus the `TapeIoError` type.
- `src/handle/tape_io/model.rs` — value types (`TapePosition`,
  `BlockSize`, `SpaceKind`, `SpaceResult`, `WriteOutcome`,
  `TapeConfig`).

CDB builders continue to live in `remanence-scsi` (Layer 1), as
the existing `move_medium.rs` / `load_unload.rs` pattern.

`crate::lib.rs` re-exports the Layer 3a value types under the
existing `pub use handle::...` pattern.

---

## 3. SCSI commands in scope

| Operation | Opcode | CDB length | Direction | Timeout class | Notes |
|--|--|--|--|--|--|
| `REWIND` | `0x01` | 6 | none | `Rewind` (600 s) | BOT seek. Minutes from EOT on full LTO-9. |
| `LOCATE(16)` | `0x92` | 16 | none | `Positioning` (300 s) | 64-bit LBA addressing. The form Layer 3a uses. |
| `READ POSITION` | `0x34` | 10 | in | `TapeStatus` (5 s) | Query current LBA + EOP / BOP / BPEW / at-filemark flags. Layer 3a uses **service action 6** (long form, 32-byte response, 64-bit LBA). Action 0 is short form (20 bytes, 32-bit LBA); action 1 is short-extended (32 bytes, 32-bit LBA + file/set numbers); action 8 is the extended form that takes an alloc-length field. For actions 0/1/6 the alloc-length field in the CDB must be 0. |
| `SPACE(6)` | `0x11` | 6 | none | `Positioning` (300 s) | Relative skip by blocks / filemarks / EOD. **24-bit signed** count in two's complement, range `[-8_388_608, 8_388_607]` ≈ ±8 TiB at 1 MiB blocks. Used for short skips. |
| `SPACE(16)` | `0x91` | 16 | none | `Positioning` (300 s) | Same operation with a 64-bit signed count. **Required** for full-tape relative positioning on LTO-9 (18 TB native ≈ 18 M blocks at 1 MiB, exceeds SPACE(6)'s range). Both forms ship per codex 93f242da. |
| `WRITE FILEMARKS(6)` | `0x10` | 6 | none | `WriteFilemarks` (120 s) | Writes N file marks. The IMMED bit is **not** set by default — Layer 3a wants synchronous completion so the audit log reflects what actually hit media. |
| ~~`WRITE FILEMARKS(16)`~~ | ~~`0x80`~~ | — | — | — | **Dropped.** Earlier draft treated `0x80` as a SPACE(16)-style 64-bit count CDB; codex review cb91b17b correctly noted that's not the SSC layout. The 16-byte form (opcode `0x80`) is an explicit-LBA-addressed staged-write CDB with partition + logical-object-identifier + 24-bit transfer length + FCS/LCS flags — designed for staged-write workflows rem doesn't use. WRITE FILEMARKS(6)'s 24-bit count covers any plausible rem use case (16 M file marks). |
| `READ(6)` | `0x08` | 6 | in | `TapeIo` (60 s) | Variable-block read by default; fixed-block via MODE SELECT. 24-bit transfer length covers up to 16 MiB per block, which exceeds every LTO generation's per-block cap. |
| `WRITE(6)` | `0x0A` | 6 | out | `TapeIo` (60 s) | Variable- or fixed-block write. Same 24-bit transfer-length range as READ(6). |
| ~~`READ(16)` / `WRITE(16)`~~ | ~~`0x88` / `0x8A`~~ | — | — | — | **Dropped.** Earlier draft listed these. T10 opcode tables mark READ(16) / WRITE(16) as *optional* for sequential-access devices, but **HPE LTO drives don't implement them** (only `0x08` / `0x0A`), and there's no use case where rem needs the 32-bit transfer length the long forms would provide — READ(6) / WRITE(6)'s 24-bit length covers any LTO block size (max 16 MiB). Removed during Step 9.0 implementation. |
| `MODE SELECT(6)` | `0x15` | 6 | out | `ModeConfig` (5 s) | Configure block size, compression, density. Used at write-session open to set fixed-vs-variable + compression off. |
| `MODE SENSE(6)` | `0x1A` | 6 | in | `ModeConfig` (5 s) | Query the current configuration. Used at session open as a sanity check. |
| `LOG SENSE` | `0x4D` | 10 | in | `ReadElementStatus` (60 s, reused) | TapeAlert, error counters, lifetime metrics. Hands off to the orchestrator. |

Exact CDB byte layouts go in the implementing crate (`remanence-scsi`)
as one builder module per command, following the existing
`load_unload.rs` / `move_medium.rs` pattern.

### 3.1 New `TimeoutClass` variants

```rust
pub enum TimeoutClass {
    // ... existing Layer 2 variants ...
    /// Block-level READ / WRITE on a loaded tape. 60 s per CDB.
    /// Block reads at LTO-9 line rate are sub-second; the budget
    /// covers retries and drive-side buffering hiccups.
    TapeIo,
    /// WRITE FILEMARKS (forces sync to media unless IMMED is set;
    /// we don't set IMMED). 120 s.
    WriteFilemarks,
    /// LOCATE / SPACE / REWIND — anything that moves the heads
    /// without reading or writing user data. 300 s covers the
    /// LTO-9 worst case (~100 s BOT → EOT) plus retries.
    Positioning,
    /// REWIND specifically — a full-tape return to BOT from
    /// arbitrary position. 600 s; the most pessimistic case among
    /// positioning operations.
    Rewind,
    /// MODE SELECT / MODE SENSE — config CDBs the drive applies
    /// immediately. 5 s.
    ModeConfig,
    /// READ POSITION — short config read, sub-100 ms typical. 5 s.
    TapeStatus,
}
```

### 3.2 New transport-trait methods + enriched return type

`SgTransport` (in `remanence-library::transport`) gains a new
data-direction method **and** the existing `execute_in` /
`execute_none` are widened to return a richer transfer outcome.
Layer 2b's `execute_none` consumers are unaffected (they discard
the outcome by pattern matching), but Layer 3a needs the residual-
bytes + decoded-sense information that the bare `Result<(),
ScsiError>` cannot carry.

```rust
/// Outcome of a successful or short SCSI data-transfer command.
/// Returned alongside `Ok` even when the drive flagged conditions
/// like ILI or EOM in sense data — the transfer is not a hard
/// error in those cases, just informational.
pub struct TransferOutcome {
    /// Bytes actually transferred (read into `buf` for execute_in;
    /// written to the device for execute_out). May be less than
    /// `buf.len()` on ILI / EOM / early-warning.
    pub bytes_transferred: u32,
    /// Sense bytes the drive returned, parsed into the key fields
    /// the data path cares about. Some when the drive set sense
    /// without going CHECK CONDITION (the "deferred" / informational
    /// sense path that SSC drives use for EOM warnings, etc.).
    pub sense: Option<SenseInfo>,
}

pub struct SenseInfo {
    pub key: u8,
    pub asc: u8,
    pub ascq: u8,
    /// INFORMATION field (8 bytes) if the VALID bit is set. For
    /// READ with ILI, this is the actual block size on tape; for
    /// WRITE with early-warning, it is the residual blocks count.
    pub information: Option<u64>,
    /// True iff the drive set the ILI bit (block size mismatch).
    pub ili: bool,
    /// True iff the drive set the EOM bit (logical end-of-medium,
    /// near-EOM warning).
    pub eom: bool,
    /// True iff the drive set the FILEMARK bit (the operation
    /// hit a file mark instead of completing as requested).
    pub filemark: bool,
}

/// Issue the CDB with `SG_DXFER_TO_DEV` — write `buf` to the device
/// as the command's data-out phase. Used by Layer 3a's `WRITE` and
/// `MODE SELECT`. The data-direction split is mechanical: a
/// transport that only implements `execute_in` + `execute_none`
/// cannot emit a WRITE CDB.
fn execute_out(
    &mut self,
    cdb: &[u8],
    buf: &[u8],
) -> Result<TransferOutcome, ScsiError>;

/// (Existing) Widened to return `TransferOutcome`. Pre-existing
/// callers (Layer 2a discovery) pattern-match on the result and
/// only consume `.bytes_transferred` when they care.
fn execute_in(
    &mut self,
    cdb: &[u8],
    buf: &mut [u8],
) -> Result<TransferOutcome, ScsiError>;
```

The `TransferOutcome` change is the meat of codex 93f242da's
Medium finding: short reads and writes need byte-accurate
accounting AND decoded sense for the caller to construct a
correct `WriteOutcome` or `ReadBufferTooSmall { actual, provided }`.
The existing SG_IO error path keeps sense bytes; this work just
plumbs them through into a typed struct so callers don't have to
re-parse on every error path.

Implementations:

- `LinuxSgTransport` (existing) — `execute_in`/`execute_out` parse
  the SG_IO `sb_len_wr` / `info` / `resid` fields. The SG_IO
  `resid` field gives bytes-NOT-transferred; `bytes_transferred =
  buf.len() - resid`. Sense bytes from `sb_buffer` decode into
  `SenseInfo` if `sb_len_wr > 0`.
- `FixtureTransport` (existing test transport) — gains
  `with_write_expectation(...)` + return-value carrying
  `TransferOutcome` so unit tests can drive the short-write / ILI
  / EOM code paths without hardware.
- `RecordingTransport` (existing test transport) — captures
  outbound writes alongside the existing inbound reads.

### 3.3 Why these commands, in this order

`docs/pfr-reference.md` §2 sets the read-side requirements: LBA-based
seek (`LOCATE(16)`), block reads (`READ`), position query
(`READ POSITION`). `docs/spec-v0.3.md` §5 sets the write-side:
chunked writes (`WRITE`) interleaved with file marks
(`WRITE FILEMARKS`) at the tape-layout boundaries (catalog block,
periodic refresh, end-of-data). `SPACE` is the relative-positioning
companion to `LOCATE` and is needed for fast forward-skip when the
target LBA is known to be close to the current position. `REWIND`
and `MODE SELECT/SENSE` are session-bracket operations: open a write
session by rewinding to BOT and setting block size; close it by
writing file marks and rewinding (or leaving the tape at EOD —
Layer 5's call).

Encryption (`SECURITY PROTOCOL IN/OUT`) is intentionally absent
here — it belongs to Layer 6 and bolts on without Layer 3a needing
to know.

---

## 4. Domain model

All types live in `remanence_library::handle::tape_io::model`
unless noted (re-exported at the crate root via
`pub use handle::tape_io::model::{...}`).

### 4.1 `TapePosition`

The output of `READ POSITION`. Identifies where the tape head sits
right now.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TapePosition {
    /// Logical block address (the durable, portable identifier).
    /// `READ POSITION` long-form (service action 6) returns 8 bytes;
    /// short-form (service action 0) returns 4 bytes that we
    /// zero-extend.
    pub lba: u64,
    /// Partition number. Always 0 on the rem-managed deployment;
    /// LTFS uses partition 1 for its index, which is one reason rem
    /// does not use LTFS.
    pub partition: u8,
    /// True iff the head is at the beginning of the partition.
    pub beginning_of_partition: bool,
    /// True iff the head is at logical end-of-partition (past the
    /// last written block).
    pub end_of_partition: bool,
    /// True iff the head sits at a file-mark boundary. Surfaces the
    /// `BPEW` (block position end-of-warning) bit for the
    /// orchestrator's near-EOM handling — rem itself does not
    /// auto-retire near-EOM tapes (Layer 5 policy).
    pub block_position_end_of_warning: bool,
}
```

### 4.2 `BlockSize`

How Layer 3a addresses the variable-vs-fixed block choice:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockSize {
    /// Variable-block mode. Each WRITE accepts a buffer of any size
    /// (subject to the drive's hardware cap, typically 16 MiB on
    /// LTO-9). Each READ asks the drive for "the next block,
    /// whatever its size, up to my buffer." This is the default and
    /// matches how `tar` and PAX archives talk to tape.
    Variable,
    /// Fixed-block mode. Every read and write is a multiple of
    /// `size_bytes`. The drive enforces it; mismatched buffers
    /// return CHECK CONDITION. Useful for formats with a uniform
    /// chunk size — `rem-chunked-v1` may opt into this if/when the
    /// format spec calls for it.
    Fixed { size_bytes: u32 },
}
```

### 4.3 `SpaceKind`

SSC-5 defines four `SPACE` motion modes, but **IBM LTO drives
only implement CODE 0 (Blocks), 1 (Filemarks), and 3
(End of Data)** per IBM LTO SCSI Reference Tables 145-146.
`SequentialFilemarks` (CODE 2) is listed as Reserved and the
drive returns `INVALID FIELD IN CDB`. The enum still carries the
variant so the type matches SSC vocabulary, but
`DriveHandle::space` rejects `SequentialFilemarks` at the API
boundary (codex 20:00 catch) with
`TapeIoError::InvalidRequest(InvalidInput)` before any CDB goes
out. Callers wanting "advance to next file mark" should issue
`space(1, Filemarks)`.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaceKind {
    /// Skip N blocks forward (positive) or backward (negative).
    Blocks,
    /// Skip N file marks forward / backward.
    Filemarks,
    /// Reserved on IBM LTO — rejected by `DriveHandle::space`.
    /// Retained for SSC vocabulary parity only.
    SequentialFilemarks,
    /// Space to End-of-Data. Count is ignored.
    EndOfData,
}
```

### 4.4 `SpaceResult`

`SPACE` can stop short of the requested count if it hits a file
mark / EOD / BOP / EOM. Callers need to know what happened, not
just "ok / error."

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpaceResult {
    /// Signed number of units the tape actually moved. Negative
    /// means backward. Always between `-count` and `+count`.
    pub units_traversed: i64,
    /// True iff `SPACE` stopped because it hit a file mark or
    /// EOD (mid-traversal). Caller often wants to know.
    pub stopped_at_boundary: bool,
    /// Position immediately after the SPACE, queried via an
    /// inline READ POSITION (Layer 3a always issues this so the
    /// caller doesn't have to). Lets the caller turn a relative
    /// skip into an absolute LBA for the next read.
    pub position_after: TapePosition,
}
```

### 4.5 `WriteOutcome`

`WRITE` similarly can stop short of the requested buffer if the
drive hits EOM (end-of-medium) or early-warning. Caller needs
byte-accurate accounting.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteOutcome {
    /// Bytes actually committed to media. May be less than the
    /// buffer length when the drive reports an "early warning"
    /// state in sense data and stops writing.
    pub bytes_written: u32,
    /// True iff sense data indicated approaching end-of-medium.
    pub early_warning: bool,
    /// True iff the drive reported end-of-medium reached.
    pub end_of_medium: bool,
    /// Position immediately after the write (from an inline
    /// READ POSITION). Lets the caller learn the LBA of the
    /// block it just wrote without a second round-trip.
    pub position_after: TapePosition,
}
```

`bytes_written == buf.len() && !early_warning && !end_of_medium` is
the happy path.

For sequential writers that already maintain their own cursor, Layer
3a also exposes the same WRITE path without the post-write `READ
POSITION`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteUnpositionedOutcome {
    pub bytes_written: u32,
    pub early_warning: bool,
    pub end_of_medium: bool,
}
```

`write_block_unpositioned` uses the same validation, audit, dirty-state,
position-known latch, EOM/early-warning handling, and block-too-large
mapping as `write_block`; it only omits the success-path position query.
Layer 3c's raw fixed-block sink seeds its physical cursor with one
`position()` call and advances that cursor after clean one-block writes.

---

## 5. Public API

Layer 3a adds methods directly to the existing `DriveHandle<'a>`
from Layer 2b. The same handle that already exposes `load()` /
`unload()` / `lock_removal()` (Layer 2b) gains the tape I/O
methods below.

```rust
// crates/remanence-library/src/handle/mod.rs holds the existing
// `impl DriveHandle` for Layer 2b. The Layer 3a methods below live
// in crates/remanence-library/src/handle/tape_io.rs as a child
// module of `handle`, so they can see DriveHandle's private fields
// (transport, audit_hook, the new DirtyState borrow from §2).

impl<'a> DriveHandle<'a> {
    // --- existing Layer 2b methods: load(), unload(), ... ---

    // --- positioning ---

    /// Issue REWIND. Tape moves to BOT, partition 0.
    pub fn rewind(&mut self) -> Result<(), TapeIoError>;

    /// Issue LOCATE(16) with the given LBA. Heads seek directly to
    /// that block. Sub-minute typical, ~100s worst case.
    pub fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError>;

    /// Issue SPACE for relative motion. Returns what the drive
    /// actually did, plus the post-space position. Internally
    /// chooses `SPACE(6)` for counts that fit in 24-bit signed
    /// (±8_388_607 blocks, ~±8 TiB at 1 MiB) and `SPACE(16)`
    /// otherwise — see §3 SCSI table and §11.2 below.
    pub fn space(
        &mut self,
        count: i64,
        kind: SpaceKind,
    ) -> Result<SpaceResult, TapeIoError>;

    /// Issue READ POSITION long-form (service action 6). Returns
    /// the full position including BPEW and file-number; Layer 3a
    /// does not expose the short form because the long form is
    /// only a few bytes more on the wire and the BPEW signal is
    /// useful to callers.
    pub fn position(&mut self) -> Result<TapePosition, TapeIoError>;

    // --- data ---

    /// Issue READ. Returns the number of bytes the drive delivered
    /// (the block size for that block in variable mode; multiple
    /// of `Fixed::size_bytes` in fixed mode). On
    /// `TapeIoError::ReadBufferTooSmall`, **the block has been
    /// consumed** — the drive advanced past it. Caller must
    /// `space(-1, Blocks)` to back up before retrying with a
    /// larger buffer.
    pub fn read_block(&mut self, buf: &mut [u8]) -> Result<usize, TapeIoError>;

    /// Issue WRITE with the full `buf`. Most writes go through this
    /// happy path. Returns the post-write position so the caller
    /// can record where the block landed.
    pub fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError>;

    /// Issue WRITE with the full `buf` but skip the post-write
    /// READ POSITION on success. Intended for sequential writers
    /// that already track physical cursor state.
    pub fn write_block_unpositioned(
        &mut self,
        buf: &[u8],
    ) -> Result<WriteUnpositionedOutcome, TapeIoError>;

    /// Issue WRITE FILEMARKS. `count` is the number of file marks.
    /// IMMED is **not** set — the call returns only after the marks
    /// are committed to media. Use after each object, periodic
    /// catalog refresh, and at tape close.
    ///
    /// Returns a [`WriteFilemarksOutcome`] (mirroring
    /// [`WriteOutcome`]) carrying `early_warning`, `end_of_medium`,
    /// and the post-mark `position_after`. Near-EOM WRITE FILEMARKS
    /// per IBM §4.8 raises CHECK CONDITION with NO SENSE + EOM bit
    /// even though the marks are committed; rem surfaces this as
    /// `Ok` with `early_warning = true` (codex 20:17 idref=6e9b56d9
    /// catch). Sense key 0x0D VOLUME OVERFLOW escalates to
    /// `end_of_medium = true`.
    pub fn write_filemarks(
        &mut self,
        count: u32,
    ) -> Result<WriteFilemarksOutcome, TapeIoError>;

    // --- configuration ---

    /// Read the current block-size + compression configuration via
    /// MODE SENSE page 0x10 (device-configuration) + page 0x0F
    /// (data-compression), and the drive's reported maximum logical
    /// block size via the **READ BLOCK LIMITS** response (IBM LTO
    /// SCSI Reference §5.2.17.1 / Table 78). Two CDBs, not one:
    /// MODE SENSE pages do **not** carry the per-block size limit.
    /// IBM Table 78 documents the reported `MAXIMUM BLOCK LENGTH
    /// LIMIT` field as `0x80_0000` (8 MiB) on LTO-9, with Note 15
    /// saying larger no-encryption block lengths *may* be accepted
    /// but are not reported in the response; §4.11 gives
    /// `0xFF_FFFF` (16 MiB - 1) as the supported unencrypted
    /// maximum. `read_config()` stores the reported RBL value
    /// verbatim — callers wanting the higher supported cap should
    /// not infer it from `max_block_size_bytes` alone.

    /// Write the block-size + compression configuration via
    /// MODE SELECT. Called at write-session open to set the format's
    /// chosen mode. The default `TapeConfig` is
    /// `BlockSize::Variable, compression = false`.
    pub fn write_config(&mut self, cfg: TapeConfig) -> Result<(), TapeIoError>;
}
```

### 5.1 `TapeConfig`

```rust
pub struct TapeConfig {
    pub block_size: BlockSize,
    /// Toggle the drive's hardware compression. `false` is the
    /// rem-chunked-v1 default — see `docs/pfr-reference.md` §6.3.
    pub compression: bool,
    /// The drive-reported maximum logical block size. Set by
    /// `read_config()`; ignored by `write_config()` (the drive's
    /// own limit always wins).
    pub max_block_size_bytes: u32,
}
```

### 5.2 Dirty-state interaction (in-crate version)

Because Layer 3a lives in the same crate as Layer 2b, the
dirty-state update is direct: `TapeIoError::Transport` (from any
of the data-path methods) calls a `pub(crate)` helper that flips
the parent `LibraryHandle`'s snapshot to dirty with cause
`CompletionUnknown`. No Layer 5 coordination is needed for the
basic case; Layer 5 is still free to react to the error if it
wants additional handling (audit-log decoration, alerting, etc.).

Drive position is tracked separately from the changer snapshot.
After any completion-unknown drive transport failure, `DriveHandle`
marks its current tape position unknown and refuses destructive
`write_block` / `write_filemarks` calls until `position()`,
`locate()`, `rewind()`, or `space()` succeeds. `refresh()` /
`rescan()` reconcile the changer inventory; they do not by themselves
prove where the tape head is.

Audit hooks still run synchronously, but the shared drive state
recovers a poisoned mutex guard if a hook panics; one bad hook call
must not permanently brick the handle family.

The original §8.1 lean — "Layer 3a does not mark dirty; the
caller (Layer 5) does" — is **reversed by this design change**.
With the in-crate option, the dirty marking happens at the
lowest layer that can do it safely.

---

## 6. Block-size handling

LTO defaults to **variable-block** mode out of the factory. Each
WRITE call writes one block of whatever size the buffer is; each
READ returns one block of whatever size is on tape (truncated if
the host buffer is too small — sense data signals it).

Fixed-block mode is opt-in via MODE SELECT. In fixed mode, the
WRITE buffer must be a multiple of the configured size; READ
returns multiples of the configured size.

Layer 3a defaults to variable-block. Rationale:

- Matches how POSIX tar / PAX archives talk to tape. Tapes written
  in variable-block mode are readable by `dd if=/dev/nst0` with
  no further configuration — the 30-year-portability story (spec
  §1.2 priority 3) depends on this.
- Layer 3b's `rem-chunked-v1` writes uniform 1 MiB chunks. In
  variable mode, the drive just sees a stream of 1 MiB write CDBs;
  no MODE SELECT needed. The on-tape layout is the same as
  fixed-block mode for this case.
- Avoids a per-tape mode-state-vs-actual-write mismatch hazard. If
  the host changes its mind about block size mid-tape after MODE
  SELECT, the drive throws CHECK CONDITION mid-write — not what
  you want.

A format that genuinely wants fixed-block enforcement (e.g.
streaming uncompressed video at a known frame size) can call
`write_config` at session open. Layer 3a does not second-guess.

### 6.1 Block-size cap

The IBM LTO SCSI Reference documents the maximum logical block size
per generation. LTO-9: 16 MiB; LTO-7/8: 8 MiB; LTO-6: 1 MiB.
`write_block(buf)` does **not** enforce the cap at the library
layer — the drive returns CHECK CONDITION if the buffer exceeds
its limit. After a successful `read_config()`, Layer 3a maps the
drive's INVALID FIELD IN CDB sense to structured
`TapeIoError::BlockTooLarge` with the cached READ BLOCK LIMITS
maximum. Format-layer caps (rem-chunked-v1 default 1 MiB) sit well
below every generation's limit and never see this error in production.

---

## 7. Streaming, progress, cancellation

### 7.1 Streaming

Layer 3a does not stream. `write_block` and `read_block` are
synchronous, one-CDB-per-call operations. The caller composes
them into a streaming pattern:

```rust
let mut total = 0u64;
for chunk in object_chunks {
    let outcome = tape.write_block(chunk)?;
    total += outcome.bytes_written as u64;
    if outcome.early_warning { /* notify orchestrator */ }
    if outcome.end_of_medium { break; }
}
```

Async streaming wrappers (e.g. an `AsyncWrite`-adapter that pipes
through `write_block`) belong in Layer 5. Mixing async at this
layer would force a tokio dependency on every consumer.

Layer 3c's fixed-block `RawTapeSink` is the exception to the
progress-query pattern: it writes many uniform blocks in a tight
sequential stream, so it calls `write_block_unpositioned`, seeds its
cursor once with `position()`, and increments by one block after each
clean write. Synchronous filemark barriers still use positioned
`write_filemarks`, so catalog/durability boundaries re-anchor the
cursor with the drive's reported position.

### 7.2 Progress

Each `write_block` returns `WriteOutcome { bytes_written,
position_after }`. The caller can publish progress events (Layer 5's
gRPC stream) without an extra round-trip — `position_after` is the
LBA of the next-block-to-write.

For very large objects (many GiB), the caller can publish progress
after every N blocks rather than every block; that's a Layer 5
batching decision.

### 7.3 Cancellation

Per `docs/spec-v0.3.md` §7.3, cancellation is best-effort at safe
points. Layer 3a's contribution to the safe-point model:

- **Between CDBs is a safe point.** If the consumer requests
  cancellation after the latest `write_block` returns and before
  the next call, the operation can stop cleanly. No tape motion
  is in flight; the caller writes file marks + rewinds (or not)
  and reports `Succeeded` with however many blocks made it.
- **Inside a CDB is not a safe point.** Once `write_block` has
  issued the WRITE CDB and is waiting on `SG_IO`, there is no
  way to interrupt the drive. The CDB completes or transport-
  errors; Layer 3a reports either, and the caller maps it to
  `CompletedAfterCancel` or `CompletionUnknown` per spec §7.3.

Layer 3a does not expose its own cancel API. The caller (Layer 5)
decides whether to issue the next CDB; Layer 3a happily blocks
forever inside one if it has to.

---

## 8. Error model

`TapeIoError` is a structured enum that surfaces what the drive
actually said, with Layer 2b's dirty-state vocabulary preserved.

```rust
#[derive(Debug, thiserror::Error)]
pub enum TapeIoError {
    /// The drive returned CHECK CONDITION with sense data we could
    /// parse. Physical state is known; this is **not** a dirty
    /// signal. Caller should consult sense_key + ASC/ASCQ.
    #[error("drive rejected the command: {0}")]
    CheckCondition(ScsiCheckCondition),

    /// SG_IO transport-level failure (timeout, kernel I/O error,
    /// driver-level disconnect). Completion is **unknown** —
    /// snapshot must be considered dirty with
    /// `DirtyCause::CompletionUnknown`. Caller should refresh /
    /// rescan before re-issuing.
    #[error("transport error (completion unknown): {0}")]
    Transport(TransportError),

    /// MODE SENSE / MODE SELECT returned bytes we couldn't parse —
    /// the drive is reporting a mode page we don't recognise, or
    /// a malformed page. Distinct from CheckCondition because the
    /// CDB succeeded; the payload was wrong.
    #[error("malformed MODE response: {0}")]
    MalformedModeResponse(String),

    /// A tape-operation adapter failed above the SCSI command layer
    /// and needs to preserve an owned diagnostic string. This is not
    /// a completion-unknown dirty signal; true transport failures use
    /// `Transport`.
    #[error("tape operation failed: {0}")]
    OperationFailed(String),

    /// WRITE buffer exceeded the drive's per-block limit (sense
    /// key 5 / ASC 0x24 / ASCQ 0x00 — INVALID FIELD IN CDB);
    /// `limit` is the READ BLOCK LIMITS value cached by
    /// `read_config()`. Caller should chunk smaller.
    #[error("write block exceeds drive limit: {requested} > {limit}")]
    BlockTooLarge { requested: u32, limit: u32 },

    /// READ buffer was too small for the block on tape. The
    /// drive set ILI; **the block has been consumed** (the head
    /// advanced past it) per IBM LTO SCSI Reference §4.12.1 /
    /// Table 17. The caller MUST `space(-1, Blocks)` to back up
    /// before retrying with a larger buffer, or the next read
    /// will skip the block entirely. Sense INFORMATION carries
    /// `requested - actual` in two's-complement; layer 3a
    /// computes `actual = requested - signed_information`.
    #[error("read buffer too small for block: needed {actual}, got {provided}")]
    ReadBufferTooSmall { actual: u32, provided: u32 },

    /// READ encountered and consumed a filemark boundary instead of
    /// returning data.
    #[error("read encountered filemark")]
    FilemarkEncountered,

    /// The drive is not loaded with a tape. SCSI returns
    /// NOT READY; we map it to a distinct variant because the
    /// recovery action ("call Layer 2b's load()") is different
    /// from any other CHECK CONDITION.
    #[error("drive has no medium loaded")]
    NoMedium,

    /// Cartridge is write-protected (tape's physical switch).
    #[error("medium is write-protected")]
    WriteProtected,

    /// Sense key 0x0D — DATA PROTECT — drive refused write for a
    /// reason other than the WP switch (encryption mismatch,
    /// WORM violation, etc.). Caller reads sense for specifics.
    #[error("data protect: {0}")]
    DataProtect(ScsiCheckCondition),
}
```

`ScsiCheckCondition` is the existing `remanence_scsi::ScsiError`
variant; `TransportError` is its sibling. The Layer 2b
`completion_unknown(&ScsiError)` helper applies unchanged:
`TapeIoError::Transport` always marks the drive snapshot dirty
with cause `CompletionUnknown`.

### 8.1 Dirty-state interaction

Now that Layer 3a is in-crate with Layer 2b (see §2), transport-
error dirty marking happens directly. A `TapeIoError::Transport`
from any data-path method invokes a `pub(crate)` helper that
flips the parent `LibraryHandle`'s snapshot dirty with cause
`CompletionUnknown`. No Layer 5 coordination needed for the
basic case.

The original draft of this section leaned toward "Layer 5 marks
dirty" because Layer 3a was going to be in a separate crate
without access to `LibraryHandle::mark_dirty`. Codex review
93f242da showed that crate split was unworkable; this section
gets the simpler answer.

---

## 9. Implementation plan

Each step ends in `cargo fmt + cargo clippy --workspace --all-targets
-- -D warnings + cargo test --workspace + cargo doc --workspace
--no-deps`, all green. Steps are sized to be commit-worthy
individually.

**Status (as of 2026-05-18): Steps 9.0–9.8 complete and merged to
`main`. Step 9.9 (live smoke on production MSL3040) pending until
the chassis is available for a scratch-tape run.** 303 unit/
integration tests pass, plus 2 `#[ignore]`-gated hardware tests
ready to run when a QuadStor VTL or LTO drive is reachable.

| Step | Status | Description |
|--|--|--|
| 9.0 | ✅ | (Prereq) Layer 1: CDB builders. New modules in `remanence-scsi`: `rewind.rs`, `locate.rs`, `space.rs`, `read_position.rs`, `read_write.rs`, `write_filemarks.rs`, `mode.rs`. Each follows the `move_medium.rs` shape — `pub fn build_cdb(...) -> [u8; N]` + unit tests against fixture bytes. |
| 9.1 | ✅ | Module restructure: `src/handle.rs` → `src/handle/mod.rs` + new `src/handle/tape_io.rs` (empty skeleton, just a placeholder `impl DriveHandle`). Factor `DirtyState` out of `LibraryHandle` as a private struct so `DriveHandle<'a>` can borrow it. Plumb a `&'a mut DirtyState` through `LibraryHandle::open_drive` into `DriveHandle`. **As a side benefit**, this fixes the existing TODO at `handle.rs:1308-1315`: direct `DriveHandle::load`/`unload` transport errors now flip the parent dirty bit instead of relying on the caller. Existing Layer 2b tests must continue to pass. |
| 9.2 | ✅ | Transport extension: `TransferOutcome` + `SenseInfo` types; `execute_in` widened to return `Result<TransferOutcome, ScsiError>`; new `execute_out`; `LinuxSgTransport` decode of SG_IO `resid` + `sb_buffer` for both directions; test-transport plumbing. New `TimeoutClass` variants from §3.1. Existing Layer 2b call sites updated to ignore the new outcome fields (the only consumer that cares is Layer 3a). |
| 9.3 | ✅ | Value types in `src/handle/tape_io/model.rs` from §4 (`TapePosition`, `BlockSize`, `SpaceKind`, `SpaceResult`, `WriteOutcome`, `TapeConfig`). `TapeIoError` in `src/handle/tape_io.rs`. Crate root re-exports. |
| 9.4 | ✅ | `DriveHandle::position` + `rewind` — the simplest pair (one in-data CDB + one no-data CDB) to wire the transport plumbing end-to-end. Audit events emitted via the existing `audit_hook`. Layer 3a CDB builders for these two come from step 9.0. |
| 9.5 | ✅ | `DriveHandle::locate` + `space` — the positioning suite. `SpaceResult` requires an inline `READ POSITION` after each `SPACE`. SPACE(6) for short skips and SPACE(16) for long ones (the runtime selection is internal to `space()`). `SequentialFilemarks` rejected at the API boundary per IBM LTO support table. |
| 9.6 | ✅ | `DriveHandle::read_block` — the read data path. Buffer-too-small handling per §8. ILI sense decoding from `ScsiError::CheckCondition`'s sense INFORMATION field per IBM §4.12.1/Table 17 (positive = short read = `Ok(actual)`, negative = block-larger = `ReadBufferTooSmall`). |
| 9.7 | ✅ | `DriveHandle::write_block` + `write_filemarks` — the write data path. EOM / early-warning sense handling per §4.5. Transport errors here flip the parent dirty bit via the `DirtyState` reference from step 9.1. Includes Step 9.7-prereq: `sg_io::execute_out` (SG_DXFER_TO_DEV) + READ BLOCK LIMITS CDB builder. |
| 9.7b | ✅ | `DriveHandle::read_config` + `write_config`. `read_config()` issues two CDBs: MODE SENSE(6) page 0x0F for block-size (from block descriptor) + compression (DCE bit), and **READ BLOCK LIMITS** (§5.2.17.1 / Table 78) for `max_block_size_bytes` — MODE SENSE pages don't carry the per-block size limit. `write_config()` issues MODE SELECT(6) with PF=1, SP=0 only. |
| 9.8 | ✅ | Integration test against QuadStor: 2 `#[ignore]`-gated tests in `crates/remanence-library/tests/quadstor_smoke.rs`. `quadstor_basic_smoke` exercises REWIND + READ POSITION + READ BLOCK LIMITS + MODE SENSE without touching cartridge state. `quadstor_write_read_round_trip` is the 100×1 MiB destructive variant (gated by `REM_QUADSTOR_WRITE_LOOP=1` for defense-in-depth). |
| 9.9 | 🟡 pending | Live smoke on production MSL3040: same as 9.8 against a scratch LTO-9 tape. Captures real CDB / sense response pairs for the fixture corpus. Documented in `JOURNAL`. |

Layer 3b implementation can start now — 9.0–9.8 cover everything
3b needs from 3a. Step 9.9 stays pending until a maintenance
window opens on the production chassis.

---

## 10. Testing strategy

Same three-tier shape as the rest of the project:

1. **CDB-builder unit tests** (`remanence-scsi/src/<cmd>.rs`).
   Synthetic CDB byte arrays compared against the expected layout.
   No I/O, fastest tier, runs on every `cargo test`.
2. **Mock-transport tests** (`crates/remanence-library/src/handle/tape_io.rs`
   `#[cfg(test)]` mod). Use `FixtureTransport` from Layer 2 to feed
   canned CDB responses into `DriveHandle::{rewind, locate, space,
   position, read_block, write_block, write_filemarks, read_config,
   write_config}` and assert the resulting values. Cover happy path +
   each `TapeIoError` variant. The `LibraryHandle` is constructed with
   a fixture transport too so the dirty-state flip on transport errors
   can be observed via the existing `LibraryHandle::is_dirty()` /
   `dirty_cause()` API.
3. **Live smoke** (`#[ignore]`-gated integration tests).
   - QuadStor VTL on akash. Tightest feedback loop; the dev fixture.
   - Production MSL3040 with a scratch LTO-9 tape. Highest signal,
     slowest, requires an operator window.

The fixture corpus grows here: every live-smoke run captures the
CDB byte stream + drive responses for inclusion in the unit-test
fixtures, the same way Layer 1 has been built up.

---

## 11. Open questions

### 11.1 ~~Dirty-state hook into `LibraryHandle`~~ — resolved

Now that Layer 3a is in-crate with Layer 2b (codex 93f242da), the
dirty-marking happens directly via a `pub(crate)` helper. No
external callback or Layer 5 coordination needed. See §8.1.

### 11.2 ~~`SPACE` long-form usage~~ — resolved (codex 93f242da)

Earlier draft asked whether `SPACE(16)` was needed; the math was
wrong twice in a row. The count field in `SPACE(6)` is 24-bit
signed two's-complement, range `[-8_388_608, 8_388_607]` ≈ ±8 TiB
at 1 MiB. LTO-9 native is 18 TB ≈ 18 M blocks at 1 MiB, so a
full-tape relative skip still exceeds SPACE(6)'s range. **Both
`SPACE(6)` and `SPACE(16)` ship.** The `space()` method internally
chooses based on the requested count via the
`scsi::space::fits_in_space6()` helper.

### 11.3 ~~`READ POSITION` short vs long form~~ — resolved

The doc had `position()` taking an implicit short/long parameter
("pass `true` for long form"). Codex correctly noticed the
parameter wasn't in the signature. **`position()` is always
long-form** (service action 6). The few extra bytes of response
include BPEW (useful for near-EOM signalling) and file-number;
no caller is harmed by the extra data.

### 11.4 Hardware compression default

`docs/pfr-reference.md` §6.3 recommends hardware compression OFF
when format-level zstd-seekable is in use, because compressing
already-compressed data is wasted CPU. Should Layer 3a's
`TapeConfig` default be `compression: false`, or should it inherit
whatever the drive's last MODE SELECT set? Lean: explicit
`compression: false` default for new tapes; honour the drive's
current state for tapes loaded from elsewhere. Layer 5 calls
`write_config` at write-session open.

### 11.5 Variable-block READ buffer size — resolved (codex 19:45)

`read_block(buf)` with variable-block mode: the caller has to know
the maximum possible block size to size `buf`. Three options were
considered:
- Caller passes a hard-coded max (e.g. 16 MiB on LTO-9) every
  time — wasteful for small-block formats, and **wrong** for
  encrypted-cartridge cases where the drive reports
  `0x80_0000` (8 MiB) instead of `0xFF_FFFF` per IBM §4.11.
- Layer 3a reads the drive's reported max via **READ BLOCK LIMITS**
  (§5.2.17.1 / Table 78) when the caller invokes `read_config()`
  at session open, stores it in `TapeConfig::max_block_size_bytes`,
  and stashes it on `DriveHandle` for later `BlockTooLarge` errors.
- Layer 3a accepts a smaller buffer and re-reads on truncation
  by growing — complex and rarely needed.

Lean: option 2, sourced from READ BLOCK LIMITS (**not** MODE SENSE
— MODE SENSE pages don't carry the per-block size limit). The
handle stashes the drive-reported max from `read_config()`; the
caller uses the returned `TapeConfig::max_block_size_bytes`;
writers in chunked-format mode size their buffers to the chunk size
plus a small margin.

### 11.6 ~~Crate name~~ — resolved (codex 93f242da / 97997d71)

The spec's `crates/remanence-tape` line implied a separate crate.
Codex 93f242da's High finding showed that an external crate
cannot drive the I/O without exposing `DriveHandle`'s private
fields or duplicating audit/dirty bookkeeping. A second-round
codex review (97997d71) further showed that even a sibling
in-crate module (`src/tape_io/`) couldn't see the private fields
of `crate::handle`, AND `DriveHandle` doesn't carry a reference
to the parent's dirty state — so the supposed `pub(crate)`
dirty-flip helper was unreachable.

Resolved: **Layer 3a lives as a child module of `handle`**,
specifically `src/handle/tape_io.rs`, with the existing
`src/handle.rs` becoming `src/handle/mod.rs`. Rust grants
visibility down the module tree, so `tape_io` sees `handle`'s
private fields. Additionally, `DriveHandle` is extended with a
`&'a mut DirtyState` borrow that lets it flip the parent's
dirty bit directly — this is a small, contained API change that
also closes the existing TODO in Layer 2b at handle.rs:1308-1315.
Spec §3.2 updated.

§2 of this doc explains the trade-off and the second-round fix.

---

## 12. Out of scope

Already covered in §1 non-goals, but worth re-stating:

- **No on-tape format awareness.** Layer 3b owns that.
- **No cartridge lifecycle.** Layer 2b owns that.
- **No encryption.** Layer 6 owns that.
- **No write verification policy.** Layer 5 owns that.
- **No async streaming, no progress events.** Layer 5 owns that.
- **No reservation management.** Layer 5 owns that, per spec §9.3.
- **No batching across CDBs.** One CDB per call; no internal queue.
- **No Windows backend yet.** Spec §2.2 says architecturally portable
  but no Windows transport in tree. `LinuxSgTransport::execute_out`
  is the only impl at first.

---

*End of design v0.1. Comments and corrections welcome — please
annotate inline rather than rewriting.*
