# Layer 3c Design: Tape Parity

**Status:** Draft v0.2 for review. Revises v0.1 with structural
simplifications from design-review discussion (May 2026). Sister
documents: `docs/layer3a-design.md` (tape mechanism),
`docs/layer3b-design.md` (tape format). Spec reference:
`docs/spec-v0.3.md` §5 (on-tape format), §9.4 (write verification
policy).

**Changes from v0.1:**

- All on-tape blocks live in the uniform parity-protected data
  area. No more carve-outs for catalog, bootstrap, or object
  headers. Catalog, bootstrap, headers, and object data are all
  parity-protected blocks; some are additionally replicated for
  fast common-case access. (§5, §10)
- Bootstrap is the canonical root of trust, replacing the
  catalog as the carrier of the parity scheme. Bootstrap is
  found *before* anything else and tells the reader how to
  interpret everything that follows. (§5.6, §10)
- Bootstrap blocks are written by the writer at approximate
  fractional positions of the tape, between object boundaries.
  Placement is a writer policy decision; the reader finds them
  by magic-scanning at expected positions. Bootstrap blocks are
  ordinary parity-protected blocks; no special framing or
  alignment to neighborhood boundaries. (§7.3)
- Neighborhoods remain fixed-size physical-tape abstractions
  (16,896 blocks at default). The LBA-to-stripe math stays
  closed-form. Objects span neighborhoods freely; neighborhood
  boundaries are invisible to body formats. (§5.1)
- Catalog no longer carries `ParityScheme` or `ParityGeometry`
  fields. Catalog records what it always did: object entries,
  refresh pointers, per-tape state. Parity-domain information
  lives in the bootstrap. (§10)

---

## 1. Scope

Layer 3c is the **erasure-coded protection layer** for the
on-tape body. It sits between Layer 3a (the SSC primitive set
on `DriveHandle`) and Layer 3b (the pluggable body-format
layer), and wraps the `BlockSink` / `BlockSource` traits 3b
consumes. The body format writes and reads data blocks; the
parity layer transparently interleaves parity blocks, and on
read failure, reconstructs missing data from parity.

### Goals

- Protect against **localized media damage** that defeats LTO's
  built-in inner+outer ECC: medium errors on individual blocks,
  multi-block contiguous damage stripes (servo-track damage,
  dust contamination, edge damage, head-clog moments that
  survived write-verify).
- Stay **format-agnostic.** A new body format inherits parity
  protection without modification. The parity layer wraps the
  sink/source traits; body formats are oblivious.
- **Cover every block uniformly.** Bootstrap, catalog, object
  headers, object data — every on-tape block is parity-protected
  by the same scheme. No special cases.
- Be **transparent on the happy path.** When no damage is
  present, reads incur zero parity-related overhead. Writes
  emit parity blocks at known LBAs but the body format never
  sees them.
- Be **configurable.** The parity scheme parameters (codeword
  size, parity blocks per codeword, interleave factor) are
  per-tape and recorded in the bootstrap. System-wide defaults
  are tuned for the target archive use case (see §11) but every
  parameter is changeable.
- Inherit Layer 3a's audit + dirty-state model. Parity-block
  writes pass through `DriveHandle::write_block` like any other
  block; transport errors flip the same dirty bit.
- Match the spec's 30-year-portability priority. Parity and
  bootstrap blocks carry self-describing magic + metadata so
  a future reader with only the format documentation can
  identify them, skip them on a normal read, and use them on
  a recovery read.

### Non-goals (for this doc)

- **Replacing LTO's built-in protection.** Layer 3c sits above
  LTO's inner+outer Reed-Solomon. The drive's 10⁻²⁰ post-ECC
  BER continues to apply to every block read; 3c handles the
  cases where the drive's ECC gives up entirely.
- **Protection against complete tape destruction.** Fire, full
  delamination, total media failure are out of scope. Those
  are handled by the multi-copy redundancy at the orchestration
  layer.
- **Body-format awareness.** 3c does not know what's in the
  blocks it protects. It treats them as opaque bytes. Bootstrap
  blocks carry their own magic, but 3c doesn't interpret object
  headers, catalog entries, or any body-format structure.
- **Parity scrubbing.** Periodic background reads to detect
  bit rot before it becomes irrecoverable belongs in Layer 5
  operational tooling. The 3c read path supports it (any read
  can trigger recovery), but the scheduling and reporting of
  scrubs is policy.
- **Parity-only re-encoding.** Reading an existing tape and
  rewriting its parity blocks (e.g., to upgrade to a stronger
  scheme) is out of scope. A tape with insufficient protection
  is replaced, not re-paritied.
- **Parity for the LTO-7 → LTO-9 migration tapes.** Migration
  pre-dates 3c. Tapes written without parity remain readable;
  the bootstrap's absence (or its `parity_scheme = none` flag)
  signals no parity coverage.
- **External format authoring.** Same as 3b — the trait surface
  is fixed, no out-of-tree parity schemes.
- **Compression or encryption of parity blocks.** Parity blocks
  are written raw (no zstd, no AES). The body format's
  compression is applied before parity is computed; encryption
  is applied per-block by the drive (Layer 6) and applies
  uniformly to data and parity blocks alike.

---

## 2. Background — what 3a / 3b leave for 3c

Layer 3a exposes the SSC primitive set on `DriveHandle<'a>` as
laid out in `docs/layer3a-design.md` §2. Layer 3c uses
`write_block` / `read_block` / `locate` / `position` /
`write_filemarks`, plus the post-write `WriteOutcome` for byte-
accurate accounting.

Per the user's structural decision on 2026-05-19, the
`BlockSink` / `BlockSource` traits + `DriveHandleSink` /
`DriveHandleSource` adapter newtypes live in
`crates/remanence-library/src/block_io.rs` — not in
`remanence-format`. This makes Layer 3b (`remanence-format`) and
Layer 3c (`remanence-parity`) **true sibling crates**: both
depend on `remanence-library`, neither depends on the other. See
`docs/layer3b-design.md` §4.5 for the trait shapes; the move
landed in 3b's Step 10.2 as a prerequisite for 3c's Step 11.0.

**Where 3c fits.** Layer 3c provides two new sink/source
wrappers — `ParitySink` and `ParitySource` — that wrap an inner
`BlockSink` / `BlockSource` and implement the same trait. The
body format consumes the parity-wrapped sink exactly as it would
consume a `DriveHandleSink` directly; the wrapping is transparent
above and the parity-block writes happen below.

Composition at the daemon level:

```rust
let mut drive_sink   = DriveHandleSink(&mut drive);
let mut parity_sink  = ParitySink::new(&mut drive_sink, scheme)?;

// Write bootstrap copy 0 at LBA 0 (first thing on tape after BOT).
parity_sink.write_bootstrap(BootstrapHints { sequence: 0, ... })?;

let mut writer = format.begin_write(&mut parity_sink, params)?;
writer.write_object(...)?;
writer.finish()?;

// Writer policy decides when to emit further bootstrap copies
// (typically after object boundaries at roughly fractional
// tape positions). See §7.3.
parity_sink.write_bootstrap(BootstrapHints { sequence: 1, ... })?;

// ... more objects, more bootstraps ...

parity_sink.finish()?;
```

Both `DriveHandleSink` and `ParitySink` implement `BlockSink`.
The body format takes `&mut dyn BlockSink` and is oblivious to
which implementation it has, *and* oblivious to the bootstrap
calls happening between its writes.

**Crate boundary.** Layer 3c lives in a new crate,
`crates/remanence-parity`, that depends on **`remanence-library`**
directly (for the `BlockSink` / `BlockSource` traits, the
`DriveHandle` adapter newtypes, and `TapeIoError`) — not on
`remanence-format`. The split keeps the erasure-code dependency
(`reed-solomon-erasure` and its transitive crates) out of
`remanence-format`; body-format authors writing
`rem-format-bareos`-style crates depend on `remanence-format`
without pulling in encoding-library weight.

```
remanence-cli ──┐
                ├─→ remanence-parity ──┐
                ├─→ remanence-format ──┴─→ remanence-library ──→ remanence-scsi
remanence-api ──┘
```

`remanence-format` and `remanence-parity` are siblings —
composition happens at the daemon level (Layer 5).

3c does not own:

- The body format's chunk / index / header layout (3b).
- Cartridge-level lifecycle (2b).
- Encryption (Layer 6 — applied per-block by the drive,
  transparent to 3c).
- The catalog's CBOR schema (3b owns it; 3c doesn't add to it).
- LBA range planning for the tape as a whole — the writer
  appends sequentially; 3c just maintains stripe accounting.

---

## 3. Position in the stack

```
Layer 5  (gRPC API)        ← caller: orchestrator, CLI
Layer 4  (local state)     ← catalog cache, audit log
Layer 3b (tape format)     ← rem-chunked-v1, pax-tar-v1, ...
Layer 3c (tape parity)     ← THIS DOC: ParitySink / ParitySource
Layer 3a (tape mechanism)  ← DriveHandle: rewind/locate/read/write
Layer 2  (identity + ops)  ← LibraryHandle: discover, move, load
Layer 1  (SCSI core)       ← remanence-scsi: CDB builders + sg_io
```

3c is the only layer in the stack with two adapter roles: it
both *consumes* a `BlockSink`/`BlockSource` (the one wrapping
`DriveHandle`) and *provides* a `BlockSink`/`BlockSource` (the
one body formats consume). This is intentional — it's the
property that makes parity transparent to body formats.

---

## 4. Domain model

All types live in `remanence_parity::model` unless noted.

### 4.1 `ParityScheme`

The configuration the writer uses and the bootstrap records.
Once a tape is written with scheme S, reading the tape requires
exactly S. Backward-incompatible scheme changes need a new
scheme ID; old tapes continue to use their original scheme,
recorded per-tape in the bootstrap.

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParityScheme {
    /// Stable identifier. Format: "rs-cauchy-gf256-v1" for the
    /// initial scheme. New parameter ranges or algorithm
    /// changes get new IDs.
    pub id: SchemeId,

    /// Data blocks per stripe (k). Codeword data size.
    pub data_blocks_per_stripe: u16,

    /// Parity blocks per stripe (m). Each stripe survives up
    /// to m erasures.
    pub parity_blocks_per_stripe: u16,

    /// Stripes per neighborhood. Determines how many stripes
    /// are interleaved within one physical neighborhood
    /// region of tape. Higher → better dispersion against
    /// contiguous damage, more memory required during write.
    pub stripes_per_neighborhood: u32,
}

impl ParityScheme {
    /// Total blocks per neighborhood = stripes_per_neighborhood
    /// × (data_blocks_per_stripe + parity_blocks_per_stripe).
    pub fn neighborhood_blocks(&self) -> u64 {
        self.stripes_per_neighborhood as u64
            * (self.data_blocks_per_stripe + self.parity_blocks_per_stripe) as u64
    }

    /// Capacity overhead, as a fraction of usable capacity.
    /// E.g. m=4, k=128 → 4/128 = 0.03125 = 3.125%.
    pub fn overhead_ratio(&self) -> f64 {
        self.parity_blocks_per_stripe as f64 / self.data_blocks_per_stripe as f64
    }

    /// Maximum contiguous damage (in blocks) that one
    /// neighborhood can recover from. Assumes the damage hits
    /// stripe positions roughly uniformly within the
    /// neighborhood (true for damage smaller than one
    /// stripe-row width).
    pub fn contiguous_damage_threshold(&self) -> u64 {
        self.stripes_per_neighborhood as u64
            * self.parity_blocks_per_stripe as u64
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SchemeId(Cow<'static, str>);
```

### 4.2 `StripeAddress`

The result of mapping a physical LBA back to its stripe identity.
Reversible: given a `StripeAddress` and the scheme, the LBA can
be recomputed.

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StripeAddress {
    pub neighborhood: u64,
    pub stripe_index: u32,            // 0..stripes_per_neighborhood
    pub position: StripePosition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StripePosition {
    /// Data block at position 0..k within its stripe.
    Data { index: u16 },
    /// Parity block at position 0..m within its stripe.
    Parity { index: u16 },
}
```

### 4.3 `RecoveryEvent`

Emitted by the parity reader on every recovery attempt. Surfaced
to Layer 5 via the audit hook so operators see them in the
audit log. A tape that produces recovery events should be
flagged for replacement.

```rust
#[derive(Clone, Debug)]
pub struct RecoveryEvent {
    pub stripe: StripeAddress,
    pub lost_blocks: Vec<StripePosition>,
    pub outcome: RecoveryOutcome,
    pub at_lba_requested: u64,
}

#[derive(Clone, Debug)]
pub enum RecoveryOutcome {
    /// Reconstructed successfully from k surviving blocks.
    Recovered,
    /// More than m blocks lost; reconstruction failed. Caller
    /// gets a read error and (typically) falls back to another
    /// copy of the data.
    Unrecoverable { lost_count: u16 },
}
```

### 4.4 `BootstrapHints`

What the writer passes to the parity sink when emitting a
bootstrap block. The sink fills in the rest of the bootstrap
contents (parity scheme, tape UUID it was constructed with,
this bootstrap's sequence number) from its own state.

```rust
#[derive(Clone, Debug)]
pub struct BootstrapHints {
    /// Monotonically increasing sequence number across the
    /// tape's bootstrap copies. Copy 0 is at LBA 0; subsequent
    /// copies get sequence 1, 2, 3, ...
    pub sequence: u32,

    /// Optional hint LBAs for catalog copies. Allows readers
    /// to jump directly to the catalog without scanning. Best-
    /// effort — readers fall back to scanning if the hints
    /// are stale or wrong (e.g., the catalog was rewritten
    /// after this bootstrap).
    pub catalog_hint_lbas: Vec<u64>,
}
```

---

## 5. On-tape layout

### 5.1 The uniform data area

The entire tape, from LBA 0 to end-of-data, is a single uniform
**parity-protected data area**. There are no carve-outs, no
special regions, no zones outside parity coverage. Every block
on tape — bootstrap, catalog, object header, object data, parity
itself — is part of the parity scheme's structure.

The data area is divided into **neighborhoods** of fixed size,
determined by the parity scheme:

- Default scheme: 16,896 blocks per neighborhood (128 stripes ×
  132 blocks/stripe at k=128, m=4).
- At 1 MiB block size: 16.5 GiB per neighborhood.
- An 18 TB LTO-9 tape contains ~1,100 neighborhoods.

Neighborhoods are a purely physical-tape concept. They start
at LBA 0 and continue in fixed-size chunks. Their boundaries
are deterministic from the scheme: neighborhood N occupies LBAs
[N × neighborhood_blocks, (N+1) × neighborhood_blocks).

Importantly, **neighborhoods have nothing to do with the body
format's structure**. Objects, catalog blocks, and bootstrap
blocks live wherever the writer puts them; they often span
neighborhood boundaries. The parity layer interleaves and
encodes blocks based purely on their LBA, without knowing what
they are.

### 5.2 Interleave pattern

Within a neighborhood, blocks are laid out by **row-major
interleave**: the writer emits data block 0 of every stripe
first, then data block 1 of every stripe, and so on, before
emitting parity blocks. Within each row, stripe index varies
fastest.

For default `stripes_per_neighborhood = S = 128`,
`data_blocks_per_stripe = k = 128`,
`parity_blocks_per_stripe = m = 4`:

```
Position within neighborhood:
  0      → stripe 0,  data 0
  1      → stripe 1,  data 0
  2      → stripe 2,  data 0
  ...
  S-1    → stripe S-1, data 0
  S      → stripe 0,  data 1
  S+1    → stripe 1,  data 1
  ...
  S*k-1  → stripe S-1, data k-1
  S*k    → stripe 0,  parity 0
  S*k+1  → stripe 1,  parity 0
  ...
  S*(k+m)-1 → stripe S-1, parity m-1
```

This pattern has three useful properties:

1. **Contiguous damage of N blocks hits stripes roughly
   uniformly.** A contiguous N-block damage region affects
   ceil(N / S) blocks per stripe (when N ≤ S, exactly one
   block per stripe is affected; when N > S, the damage rolls
   over to a second pass through the stripe set). At the
   default S=128, m=4, contiguous damage up to 4×128 = 512
   blocks (~512 MiB) is recoverable.

2. **The mapping LBA ↔ (stripe, position) is computable** with
   integer arithmetic, no table lookup, no per-tape state
   beyond the scheme parameters. Critical for recovery — the
   reader doesn't need a per-stripe index.

3. **Data is written before parity within each neighborhood.**
   Parity blocks live after all the data blocks they protect.
   The writer can compute parity in-stream: by the time the
   writer reaches LBA S*k, all S stripes have their k data
   blocks in memory and parity computation is immediate.

### 5.3 LBA mapping

Given the scheme and a position `p` within a neighborhood
(where `p` is 0..S*(k+m)-1):

```rust
fn position_to_stripe(p: u64, scheme: &ParityScheme) -> StripeAddress {
    let s = scheme.stripes_per_neighborhood as u64;
    let k = scheme.data_blocks_per_stripe as u64;

    let stripe_index = (p % s) as u32;
    let row = p / s;

    let position = if row < k {
        StripePosition::Data { index: row as u16 }
    } else {
        StripePosition::Parity { index: (row - k) as u16 }
    };

    StripeAddress {
        neighborhood: 0,  // caller fills in
        stripe_index,
        position,
    }
}
```

And the reverse, for the writer / recovery reader:

```rust
fn stripe_to_position(addr: &StripeAddress, scheme: &ParityScheme) -> u64 {
    let s = scheme.stripes_per_neighborhood as u64;
    let k = scheme.data_blocks_per_stripe as u64;

    let row = match addr.position {
        StripePosition::Data { index }   => index as u64,
        StripePosition::Parity { index } => k + index as u64,
    };

    row * s + addr.stripe_index as u64
}

fn lba_to_stripe(lba: u64, scheme: &ParityScheme) -> StripeAddress {
    let neighborhood_size = scheme.neighborhood_blocks();
    let neighborhood = lba / neighborhood_size;
    let position_in_neighborhood = lba % neighborhood_size;
    let mut addr = position_to_stripe(position_in_neighborhood, scheme);
    addr.neighborhood = neighborhood;
    addr
}
```

### 5.4 End-of-tape: partial neighborhood

When the writer reaches end-of-tape (drive reports EOM via
`WriteOutcome::end_of_medium`), the current neighborhood is
almost certainly partial — some stripes may have all their
data blocks emitted, some may have only a few, and parity has
not been written for any of the stripes in the partial
neighborhood.

The writer handles this by:

1. Truncating the in-flight neighborhood at the last data block
   that was successfully written.
2. For each stripe in the partial neighborhood:
   - If all k data blocks landed: compute and write parity if
     space remains. If not, leave the stripe unprotected.
   - If fewer than k data blocks landed: pad with zero blocks
     to k, compute parity, write parity if space remains. The
     padding bytes are real tape blocks (consuming LBAs) but
     their content is zero. The bootstrap's `data_area_end_lba`
     records the last real-data LBA so readers know where data
     ends and padding begins.
3. The trailing bootstrap (written after the last real data
   object) records the partial state for downstream readers.

The trade-off: the tail end of the tape may have a few hundred
MiB of unprotected data. Operationally this is acceptable
because (a) the orchestrator's three-copy policy still applies,
and (b) the bootstrap records which LBAs lack parity.

### 5.5 Parity block format

Each parity block on tape is one tape block in size and begins
with a small header followed by the raw parity bytes. The
header lets a forward scanner identify parity blocks and skip
them on a normal read.

```
+--------+--------+--------+--------+--------+--------+--------+--------+
| <8-byte HMAC-derived parity magic; see below>                        |
+--------+--------+--------+--------+--------+--------+--------+--------+
| neighborhood (u64 BE)                                                |
+--------+--------+--------+--------+--------+--------+--------+--------+
| stripe_index (u32 BE) | parity_index (u16 BE) | reserved (u16 BE)   |
+--------+--------+--------+--------+--------+--------+--------+--------+
| crc32_header (u32 BE — CRC of bytes 0..22)                          |
+--------+--------+--------+--------+--------+--------+--------+--------+
| parity_payload_len (u32 BE)                                          |
+--------+--------+--------+--------+--------+--------+--------+--------+
| parity payload (parity_payload_len bytes; raw RS parity)             |
| ...                                                                  |
| (padding to fill the tape block)                                     |
+--------+--------+--------+--------+--------+--------+--------+--------+
```

The magic is **derived per-tape** to avoid collision with user
data that happens to start with the same bytes:

```
parity_magic = HMAC-SHA256(tape_uuid, b"REM\x00PAR\x01")[0..8]
```

The recovery scanner, knowing the tape UUID from the bootstrap,
recomputes the expected magic and compares. User data colliding
with a per-tape-derived magic is genuinely 2⁻⁶⁴.

The CRC covers the header; the parity payload's integrity is
verified by the RS reconstruction itself (if the payload is
corrupt, reconstruction with that parity fails — but we have
m other parity blocks plus k-? data blocks to try).

### 5.6 Bootstrap block format

The bootstrap is the **canonical root of trust** for the tape.
It's the first thing a reader finds, and it tells the reader
everything needed to interpret the rest of the tape — including
the parity scheme used by all other blocks.

Bootstrap blocks live at LBAs chosen by the writer (typically
~5% of tape apart, at object boundaries — see §7.3). They are
ordinary parity-protected blocks: each bootstrap is one block,
written through `ParitySink::write_block`, assigned to a stripe
slot, and protected by parity like any other block.

```
+--------+--------+--------+--------+--------+--------+--------+--------+
| 'R'    | 'E'    | 'M'    | 0x00   | 'B'    | 'O'    | 'O'    | 0x01   |  <- magic (fixed)
+--------+--------+--------+--------+--------+--------+--------+--------+
| schema_major (u16 BE) | schema_minor (u16 BE) | flags (u32 BE)      |
+--------+--------+--------+--------+--------+--------+--------+--------+
| tape_uuid (16 bytes)                                                 |
+--------+--------+--------+--------+--------+--------+--------+--------+
| block_size_bytes (u32 BE) | sequence (u32 BE)                       |
+--------+--------+--------+--------+--------+--------+--------+--------+
| crc32_header (u32 BE — CRC of bytes 0..40)                          |
+--------+--------+--------+--------+--------+--------+--------+--------+
| cbor_payload_len (u32 BE)                                            |
+--------+--------+--------+--------+--------+--------+--------+--------+
| CBOR payload: ParitySchemeRecord + catalog_hint_lbas                 |
| ...                                                                  |
+--------+--------+--------+--------+--------+--------+--------+--------+
| crc32_payload (u32 BE)                                               |
+--------+--------+--------+--------+--------+--------+--------+--------+
| (padding to fill the tape block)                                     |
+--------+--------+--------+--------+--------+--------+--------+--------+
```

Why a **fixed** magic for bootstrap (unlike parity blocks):
the reader needs to find a bootstrap before it knows the tape
UUID, so the magic must be discoverable from spec alone. The
trade-off — user data that happens to contain `REM\x00BOO\x01`
near a known fractional position — is mitigated by the magic
being checked at a *known LBA region* (where the reader is
already looking), and by the header CRC validating the rest.

CBOR payload schema:

```cbor
BootstrapPayload = {
    1: ParitySchemeRecord,            ; the parity scheme for this tape
    2: ?[* uint],                     ; catalog_hint_lbas (best-effort)
    3: ?tstr,                         ; rem software version that wrote this tape
    4: ?tstr,                         ; RFC3339 timestamp of this bootstrap's write
}

ParitySchemeRecord = {
    1: tstr,                          ; scheme ID (e.g. "rs-cauchy-gf256-v1")
    2: uint,                          ; data_blocks_per_stripe (k)
    3: uint,                          ; parity_blocks_per_stripe (m)
    4: uint,                          ; stripes_per_neighborhood (S)
}
```

A `flags` field with bit 0 set indicates this tape has no parity
(written with `--parity none`); all other fields except magic,
schema version, tape UUID, block size, sequence, and header CRC
may be absent. Readers seeing this flag treat the tape as
no-parity and bypass the parity source.

The bootstrap is small — typical CBOR payload is well under
200 bytes; the full block fits in 1 KiB comfortably. The block
itself is padded to the tape's block size (typically 1 MiB), so
the actual on-tape footprint is one block. With ~20 bootstraps
per tape, total bootstrap overhead is ~20 MiB on an 18 TB tape —
0.0001%.

---

## 6. Public API

```rust
// crates/remanence-parity/src/lib.rs

pub use crate::sink::ParitySink;
pub use crate::source::ParitySource;
pub use crate::model::{
    ParityScheme, SchemeId, StripeAddress, StripePosition,
    BootstrapHints, RecoveryEvent, RecoveryOutcome,
};
pub use crate::error::ParityError;

/// Default parity scheme for new tapes. Tuned for the target
/// archive (16 GiB neighborhood, RS(128, 4), 128-way interleave,
/// 3.125% overhead). See §11 for the rationale.
pub fn default_scheme() -> ParityScheme {
    ParityScheme {
        id: SchemeId::new_static("rs-cauchy-gf256-v1"),
        data_blocks_per_stripe: 128,
        parity_blocks_per_stripe: 4,
        stripes_per_neighborhood: 128,
    }
}

/// A more conservative scheme for tapes expected to see harsh
/// storage conditions. RS(64, 6), 64-way interleave, ~6 GiB
/// neighborhoods, 9.4% overhead, ~384 MiB contiguous damage
/// tolerance per neighborhood.
pub fn conservative_scheme() -> ParityScheme {
    ParityScheme {
        id: SchemeId::new_static("rs-cauchy-gf256-v1"),
        data_blocks_per_stripe: 64,
        parity_blocks_per_stripe: 6,
        stripes_per_neighborhood: 64,
    }
}
```

### 6.1 `ParitySink`

Wraps an inner `BlockSink` and inserts parity blocks at the
configured intervals. The body format writes data blocks; the
parity sink emits both data and parity blocks to the inner sink
in the order dictated by the interleave pattern.

```rust
pub struct ParitySink<'a> {
    inner: &'a mut dyn BlockSink,
    scheme: ParityScheme,
    tape_uuid: [u8; 16],
    parity_magic: [u8; 8],  // derived from tape_uuid + scheme
    state: NeighborhoodState,
    audit_hook: Option<Arc<dyn ParityAuditHook>>,
}

impl<'a> ParitySink<'a> {
    /// Construct a new parity sink wrapping `inner`. The starting
    /// LBA must be 0 (the data area begins at BOT).
    pub fn new(
        inner: &'a mut dyn BlockSink,
        scheme: ParityScheme,
        tape_uuid: [u8; 16],
    ) -> Result<Self, ParityError>;

    /// Emit a bootstrap block at the current position. Like any
    /// other block, it goes through the inner sink and is
    /// accounted for in the current stripe's data row. Caller
    /// (Layer 5 write-session manager) decides when to call this.
    pub fn write_bootstrap(&mut self, hints: BootstrapHints)
        -> Result<TapePosition, ParityError>;

    /// Flush any partial neighborhood at the end of writing.
    /// Returns the final LBA of usable data + the unprotected
    /// LBA ranges (if any) for the caller to record in the
    /// catalog or final bootstrap.
    pub fn finish(self) -> Result<FinalGeometry, ParityError>;
}

pub struct FinalGeometry {
    pub data_area_end_lba: u64,
    pub unprotected_ranges: Vec<(u64, u64)>,  // (start_lba, end_lba) pairs
}

impl<'a> BlockSink for ParitySink<'a> {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError>;
    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError>;
    fn position(&mut self) -> Result<TapePosition, TapeIoError>;
}
```

### 6.2 `ParitySource`

Wraps an inner `BlockSource`. On a clean read, passes through
to the inner source. On a `TapeIoError::CheckCondition(MEDIUM_ERROR)`
(or `Transport` error, or any other erasure indication), uses
the parity scheme to identify the affected stripe and attempts
reconstruction.

The parity source is constructed *after* the reader has located
and parsed a bootstrap block, so it knows the parity scheme.

```rust
pub struct ParitySource<'a> {
    inner: &'a mut dyn BlockSource,
    scheme: ParityScheme,
    tape_uuid: [u8; 16],
    parity_magic: [u8; 8],
    cache: StripeCache,
    audit_hook: Option<Arc<dyn ParityAuditHook>>,
}

impl<'a> ParitySource<'a> {
    pub fn new(
        inner: &'a mut dyn BlockSource,
        scheme: ParityScheme,
        tape_uuid: [u8; 16],
    ) -> Result<Self, ParityError>;
}

impl<'a> BlockSource for ParitySource<'a> {
    fn read_block(&mut self, buf: &mut [u8]) -> Result<usize, TapeIoError>;
    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError>;
    fn space(&mut self, count: i64, kind: SpaceKind) -> Result<SpaceResult, TapeIoError>;
    fn position(&mut self) -> Result<TapePosition, TapeIoError>;
}
```

### 6.3 Bootstrap discovery (separate from `ParitySource`)

Finding a bootstrap is a prerequisite to constructing a
`ParitySource` — the source needs the scheme, which only the
bootstrap can provide. So bootstrap discovery uses the *inner*
source directly (no parity reconstruction available yet):

```rust
/// Find a valid bootstrap block on the tape. Scans expected
/// positions; returns the first one that parses and validates.
/// Used at tape-mount time before constructing a ParitySource.
pub fn discover_bootstrap(
    source: &mut dyn BlockSource,
    tape_total_blocks_hint: Option<u64>,
) -> Result<BootstrapPayload, ParityError>;
```

The discovery algorithm (§8.1) tries known fractional positions
in order, locating to each and scanning a few blocks forward
looking for bootstrap magic. The first match that validates by
CRC is returned. If no bootstrap is found at any expected
position, the tape is treated as no-parity (with a warning to
the operator).

### 6.4 Composition with body formats

```rust
// In the daemon's write-session setup:
let mut drive_sink   = DriveHandleSink(&mut drive);
let mut parity_sink  = ParitySink::new(&mut drive_sink, scheme, tape_uuid)?;

// Write the first bootstrap at LBA 0 before any body data.
parity_sink.write_bootstrap(BootstrapHints {
    sequence: 0,
    catalog_hint_lbas: vec![],   // initially empty; refresh later if desired
})?;

let mut writer = format.begin_write(&mut parity_sink, params)?;

for object in objects_to_write {
    writer.write_object(object)?;

    // Periodic bootstrap (writer policy, see §7.3):
    if should_write_bootstrap_after_this_object(&parity_sink) {
        parity_sink.write_bootstrap(BootstrapHints {
            sequence: next_seq(),
            catalog_hint_lbas: known_catalog_lbas(),
        })?;
    }
}

writer.finish()?;

// Final bootstrap after the last object (which typically also contains
// the final catalog).
parity_sink.write_bootstrap(BootstrapHints {
    sequence: final_seq,
    catalog_hint_lbas: final_catalog_lbas(),
})?;

let geometry = parity_sink.finish()?;
```

The body format does not need to know whether its sink is a
`ParitySink` or a `DriveHandleSink`. The bootstrap-writing calls
happen between body-format object writes; the body format is
oblivious.

---

## 7. Write flow

### 7.1 Stripe accounting

At write-session open, the parity sink allocates S stripe buffers
(each holding k blocks worth of data; default 128 MiB per stripe,
16 GiB total). It positions the inner sink at LBA 0 and
initializes the neighborhood counter to 0.

For each call to `ParitySink::write_block(buf)` (regardless of
whether the block is body data, bootstrap, or anything else):

1. Determine the current (row, stripe_index_in_row) from a
   running counter.
2. If row < k (data row):
   - Forward buf to inner.write_block(buf) — the block lands
     on tape at the current LBA.
   - Copy buf into stripe[stripe_index].data[row].
   - Increment counter.
   - If counter mod S == 0: row += 1.
   - If row == k: trigger parity computation (see below).
3. Return WriteOutcome including the post-write position.

When all k data rows are full (k*S data blocks written):
1. For each stripe S in the neighborhood:
   - Pass stripe.data[0..k] to reed-solomon-erasure encoder.
   - Receive m parity blocks.
   - Write each parity block to inner.write_block, with the
     parity-block header (§5.5) prepended.
2. Reset the stripe buffers.
3. Advance counter to the start of the next neighborhood.

The body format and the bootstrap caller are oblivious to this
process — they just call `write_block` and get back a position.

### 7.2 At `ParitySink::finish()`

Handle any partial neighborhood as in §5.4:
- Stripes with full k data blocks: emit their parity.
- Stripes with fewer than k data blocks: pad with zeros, emit
  parity. Record the unprotected range (the padding LBAs) in
  the returned `FinalGeometry`.
- Stripes with no data: skipped entirely; their LBAs are
  unwritten.

Return the final geometry for the caller (typically Layer 5)
to record in the catalog or final bootstrap.

### 7.3 Bootstrap placement policy

The writer (Layer 5's write-session manager, not the parity sink
itself) decides when to emit bootstrap blocks. The policy lives
outside Layer 3c because it depends on operational concerns —
how many bootstrap copies operators want, what fraction of tape
should be uncovered if a chunk of tape is destroyed, etc.

Default policy: **emit a bootstrap at LBA 0, and after each
object boundary if at least (tape_capacity / 20) blocks have
been written since the last bootstrap.** This gives ~20
bootstrap copies on a full tape, each ~900 GB apart on an
18 TB LTO-9, with the first guaranteed at LBA 0.

Why between objects rather than mid-object: objects are
contiguous LBA ranges in the body format's view (a Layer 3b
invariant). Bootstrap blocks are also blocks, but they're not
part of any object. Placing them between objects preserves the
invariant. The parity layer doesn't care either way — bootstrap
blocks are just blocks — but the body format's catalog records
object start_lba/end_lba ranges, and those ranges must be
contiguous to satisfy the body format's expectations.

Why writer policy rather than parity-layer policy: the parity
layer doesn't know what an object is. Layer 5 (which composes
the body format and parity layer at write-session level) knows
object boundaries and can make the call.

The first bootstrap is always at LBA 0 (the first block written
on the tape). This is **the** invariant readers can rely on:
LBA 0 is a bootstrap. Subsequent bootstraps are at writer
discretion.

### 7.4 Performance

At LTO-9's 400 MB/s write rate, parity encoding for one full
neighborhood (S=128, k=128) is roughly 128 RS encodings each
over 128 MiB. The `reed-solomon-erasure` crate's Cauchy GF(2⁸)
encoder runs at ~3 GB/s per core. Total compute is ~16 GiB /
3 GB/s = ~5 seconds per neighborhood, spread across the ~40
seconds it takes the tape to write one neighborhood worth of
data and parity. Single-threaded; no parallelism required. On
a server with multiple cores, parity computation runs in a
background thread and never blocks the write pipeline.

Bootstrap writes are amortized away: one extra block per ~20
GB of data is invisible in any throughput measurement.

---

## 8. Read flow

### 8.1 Bootstrap discovery (tape-mount time)

When a tape is loaded, the first thing the reader does is find
a valid bootstrap. This must happen *before* any
`ParitySource` is constructed, because the source needs the
scheme.

```rust
fn discover_bootstrap(source: &mut dyn BlockSource,
                     tape_total_blocks_hint: Option<u64>)
    -> Result<BootstrapPayload, ParityError>
{
    // Expected bootstrap positions: roughly every 5% of tape,
    // starting at LBA 0.
    let positions = expected_bootstrap_positions(tape_total_blocks_hint);

    for pos in positions {
        if let Ok(bp) = try_read_bootstrap_at(source, pos) {
            return Ok(bp);
        }
    }

    Err(ParityError::NoBootstrapFound)
}

fn try_read_bootstrap_at(source: &mut dyn BlockSource, target_lba: u64)
    -> Result<BootstrapPayload, ParityError>
{
    // The actual bootstrap might be a few blocks past the target,
    // depending on where the previous object boundary fell.
    source.locate(target_lba)?;

    // Scan forward up to N blocks looking for bootstrap magic.
    // N is chosen to cover the typical "next object boundary"
    // distance — 1000 blocks (1 GiB) is generous.
    for _ in 0..MAX_BOOTSTRAP_SCAN_BLOCKS {
        let mut buf = vec![0u8; BLOCK_SIZE];
        match source.read_block(&mut buf) {
            Ok(_) => {
                if has_bootstrap_magic(&buf) {
                    return parse_bootstrap(&buf);
                }
            }
            Err(_) => continue,  // medium error: skip and keep scanning
        }
    }

    Err(ParityError::NoBootstrapAtPosition(target_lba))
}
```

LBA 0 always contains a bootstrap (§7.3 invariant), so discovery
succeeds in the vast majority of cases on the first try. If
LBA 0 is damaged, the scan falls back to ~5%, then ~10%, etc.

### 8.2 Clean read (happy path)

```
ParitySource::read_block(buf):
  1. inner.locate(current_lba)  — if we're not already positioned
  2. inner.read_block(buf)      — returns Ok(bytes) on success
  3. If the block has bootstrap or parity magic in its first 8
     bytes, the caller might be reading a non-data block. The
     parity source doesn't filter these out — callers (body
     format) addresses blocks by LBA and is expected to know
     which LBAs are body-format data. The body format's catalog
     records data LBAs; non-data LBAs aren't requested.
  4. Return Ok(bytes)
```

Zero overhead from the parity layer when no error occurs. The
parity blocks and bootstrap blocks are at LBAs the body format
never asks for — they exist on tape but are never read during
normal operation.

### 8.3 Error-triggered recovery

```
ParitySource::read_block(buf):
  1. Attempt inner.read_block(buf) as above.
  2. On any of:
     - TapeIoError::CheckCondition with sense key MEDIUM_ERROR (0x03)
     - TapeIoError::Transport (rare; usually means cabling/HBA
       issue, but if it happens mid-read of a known-good LBA range,
       recovery is worth trying)
     → fall through to recovery.
     Hardware Error (0x04) and Not Ready (0x02) do **not**
     currently fall through to recovery: the IBM LTO SCSI
     Reference GA32-0928-08 Annex B tables do not identify a
     Hardware Error / Not Ready tuple that is safely equivalent
     to a per-block erasure. They propagate until a real
     hardware-observed tuple is grounded and tested.

  3. Recovery:
     a. Compute StripeAddress for the failed LBA using lba_to_stripe.
     b. Build the list of LBAs for the other (k+m-1) members of
        the same stripe.
     c. Read each member:
        - inner.locate(member_lba)
        - inner.read_block(scratch_buf)
        - On success, store the block in the recovery buffer.
        - On error, record this LBA as an additional erasure and
          continue.
     d. Stop early once k surviving members are collected.
     e. If fewer than k surviving members: emit
        RecoveryEvent::Unrecoverable and return a synthetic
        TapeIoError::CheckCondition that callers (format crate)
        map to their own Unrecoverable variant.
     f. Run reed-solomon-erasure's reconstruct() with the
        surviving members and the indices of the missing ones.
     g. Validate the reconstructed block has the expected size.
     h. Copy the reconstructed block into the caller's buf.
     i. Emit RecoveryEvent::Recovered via audit_hook.
     j. Locate inner back to the position the caller expects to
        be at after the read (one past the failed LBA).
     k. Return Ok(reconstructed_size).
```

Recovery cost: in the worst case, k LOCATEs and k inner reads.
At LTO-9 LOCATE speeds (typically a few seconds for a long
seek, sub-second for short ones within a neighborhood), recovery
of a single block takes 5-30 seconds. This is acceptable for an
error path — it's slow compared to a clean read but orders of
magnitude faster than fetching from another tape copy.

### 8.4 Recovery of bootstrap and catalog blocks

A bootstrap or catalog block that fails to read goes through
the same recovery path as any other block — the parity layer
doesn't know it's special. This works automatically:

- Bootstrap discovery (§8.1) tries multiple positions; if
  the LBA-0 bootstrap is damaged enough to defeat parity, the
  reader proceeds to the 5% bootstrap and so on.
- Catalog blocks that fail to read can be recovered via parity
  if the damage is within scheme limits, or by falling back to
  another catalog copy (recorded in the catalog refresh pointer
  list).
- Object header blocks are recovered via parity, or by falling
  back to the duplicate header copy (recorded in the body
  format's catalog entry).

The defense-in-depth is: parity protection for every block,
plus replication for the structurally most important blocks
(bootstrap, catalog, object headers). A damage event that
defeats parity for a structural block still has a replicated
copy elsewhere on tape to fall back to.

### 8.5 Servo-damage handling

The LTO-4 dust failure mode (Appendix A) presents differently
from a typical MEDIUM_ERROR: the drive may fail before returning
block bytes. The implementation treats IBM-grounded
MEDIUM_ERROR reads as erasures. Hardware Error and Not Ready
conditions are **not** currently treated as erasures because the
project-local IBM LTO SCSI Reference GA32-0928-08 Annex B
tables list those keys as drive-state, cartridge, controller, or
hardware faults rather than target-LBA data erasures. If live
hardware later produces a specific Hardware Error / Not Ready
tuple that is demonstrably per-block-erasure-equivalent, add it
as an exact key+ASC+ASCQ allowlist entry with fixtures before
routing it to recovery.

Once a condition is classified as an erasure, the recovery flow
doesn't care *why* a block is unreadable — it only needs to know
which blocks are unreadable and gather enough surviving stripe
members to reconstruct.

Important subtlety: if the parity blocks for the affected
stripe also live in the servo-damaged region, those parity
blocks are unreadable too. They count as additional erasures.
The interleave pattern (§5.2) helps here — for a damage region
shorter than one row width (S blocks), each stripe loses at
most one member, regardless of whether data or parity. The
math:

- Damage region of N blocks where N < S:
  - Each stripe loses at most 1 block.
  - All stripes recover (m ≥ 1).
- Damage region of N blocks where S ≤ N < 2S:
  - Each stripe loses at most 2 blocks.
  - All stripes recover if m ≥ 2.
- ...
- Damage region of N blocks where (m+1)*S - 1 < N:
  - Some stripes lose more than m blocks.
  - Recovery fails for those stripes.

At defaults (S=128, m=4): damage up to 4*128 - 1 = 511 blocks
(~511 MiB) recoverable; damage of 512+ blocks risks losing some
stripes.

---

## 9. Recovery in detail

### 9.1 Erasure detection

The parity source treats the following as erasures, all handled
by the same recovery path:

| SCSI condition                                | Erasure? | Reason |
|-----------------------------------------------|----------|--------|
| Clean read                                    | No       | Happy path. |
| CHECK_CONDITION sense key 0x03 (MEDIUM_ERROR) | Yes      | LTO ECC gave up. |
| Transport error (timeout, etc.)               | Maybe    | Try once more; if it persists, treat as erasure. |
| CHECK_CONDITION sense key 0x04 (HARDWARE_ERROR) | No     | Per IBM LTO SCSI Reference GA32-0928-08 Annex B Table B.5, Hardware Error ASCs (03/02, 04/03, 10/01, 40/XX, 41/00, 44/00, 51/00, 52/00, 53/00, 53/04, EE/0E, EE/0F) describe drive-level faults (loose tape, cartridge fault, internal target failure, target operating conditions changed). None corresponds to a per-block-erasure equivalent of "servo lost at target LBA". The prior 0x09/0x15/0x3B "positioning ASC" allowlist conflated Medium Error ASCs (Table B.4) with Hardware Error; that was conjecture. Until a real Hardware Error tuple is demonstrably equivalent to a per-block erasure on production hardware, HARDWARE_ERROR propagates to the operator. |
| CHECK_CONDITION sense key 0x02 (NOT_READY)    | No       | Annex B Table B.3 ASCs are drive-state codes (medium not present, becoming ready, load/eject failed); no listed tuple corresponds to "post-LOCATE servo damage at a target LBA". Propagates as a drive error. |
| CHECK_CONDITION sense key 0x05 (ILLEGAL_REQUEST) | No   | Programming error in rem, not media. |
| CHECK_CONDITION sense key 0x07 (DATA_PROTECT) | No       | Encryption or write-protect, not media damage. |

Sense codes are extracted from `TapeIoError::CheckCondition` via
the existing `ScsiCheckCondition` accessor.

### 9.2 Stripe reconstruction

The `reed-solomon-erasure` crate exposes the reconstruction
primitive:

```rust
use reed_solomon_erasure::galois_8::ReedSolomon;

let rs = ReedSolomon::new(k, m)?;  // k=128, m=4 by default

// shards: Vec<Option<Vec<u8>>> with k+m entries
// None for missing blocks, Some(bytes) for surviving ones.
let mut shards: Vec<Option<Vec<u8>>> = build_shards_from_surviving_members(...);

rs.reconstruct(&mut shards)?;
// shards[i] is now Some(_) for all i; missing data blocks have
// been reconstructed.
```

The crate uses Cauchy matrices and SIMD-accelerated finite-field
arithmetic. On modern x86, it sustains ~3 GB/s per core for
encoding and similar for decoding. Decoding cost for one stripe
(128 surviving blocks of 1 MiB each → 1 reconstructed block):
~50 ms.

Total recovery cost per block: dominated by tape I/O (the 128
LOCATEs and reads), not by CPU. Recovery rate for a heavily
damaged region: limited by tape throughput, not encoding.

### 9.3 Recovery cache

The parity source keeps an LRU cache of recently-read stripes:

```rust
struct StripeCache {
    entries: LruCache<StripeId, CachedStripe>,
    max_stripes: usize,  // default 4
}

struct CachedStripe {
    members: Vec<Option<Vec<u8>>>,  // k+m slots, populated on demand
    reconstructed: HashSet<StripePosition>,
}
```

When recovery reads stripe members, they're cached. Subsequent
recovery requests for the same stripe (e.g., a contiguous run
of bad blocks all in the same stripe) reuse cached members.

Cache invalidation:
- On `locate()` to a new neighborhood: keep the cache (still
  valid for the previous neighborhood if the reader comes back).
- Explicit `parity_source.invalidate_cache()`: provided for
  test scenarios; not called in production.

### 9.4 Recovery events

Every recovery — successful or not — emits a `RecoveryEvent` via
the audit hook. The audit hook is the same `Arc<dyn ParityAuditHook>`
the parity source was constructed with; it's wired by the daemon
to the same audit log as Layer 2's `LibraryAuditHook`.

Operators are expected to monitor recovery events. A tape that
produces recovery events is **flagged for replacement** by Layer 5
policy — even though the data is still readable, the trend is
toward more damage. Three copies plus parity is a strong
position; one copy plus parity that's actively being exercised
is a tape that's failing in slow motion.

---

## 10. Catalog integration

### 10.1 No new catalog fields

Unlike v0.1 of this spec, the catalog (Layer 3b's responsibility)
does **not** record parity scheme parameters or geometry. Those
live in the bootstrap (§5.6). The catalog records what it
always recorded:

- Object entries (each with start_lba, end_lba — contiguous
  physical LBA ranges).
- Catalog refresh pointers.
- Encryption parameters (per spec §5.5).
- Schema-version extensibility.

This is a meaningful simplification from v0.1: the catalog has
no parity-related fields. Layer 3c is invisible to the catalog
schema.

### 10.2 Catalog placement on tape

Catalog blocks are body-format-managed (Layer 3b decides when
and where to write them). With the v0.2 uniform-parity model,
catalog blocks are ordinary parity-protected blocks at LBAs
inside the data area. The body format records its own catalog
LBAs.

Catalog replication for fast common-case access (the "triplicate"
property from v0.1) remains a 3b concern. The body format
chooses to write each catalog refresh at three well-separated
LBAs, recorded in the previous catalog's refresh pointer list
and in the bootstrap's `catalog_hint_lbas` field. A reader that
finds a clean copy at any of the hint LBAs proceeds without
parity reconstruction; if all hints fail, parity recovery is
attempted before falling back to another tape copy.

### 10.3 Object header replication

Object final headers are duplicated (per 3b §7.1's "final
header written twice for redundancy") at the body-format level.
The duplicates are at slightly separated LBAs within the same
object, both parity-protected like every other block.

Defense-in-depth: parity catches single-block losses; replication
catches the rare case where parity *also* fails for that block.

### 10.4 The unified picture

Every on-tape block on a parity-protected tape:

- Belongs to exactly one parity stripe (in exactly one
  neighborhood).
- Is recoverable if the damage affecting its stripe stays
  within scheme limits.
- May additionally be replicated at one or more other LBAs
  (bootstrap, catalog, object final headers).

No block is structurally outside the parity coverage. The
"this is special so we protect it differently" case from v0.1
is gone.

---

## 11. Configuration

### 11.1 Default parameters (target archive)

| Parameter                | Default | Rationale |
|--------------------------|---------|-----------|
| Block size               | 1 MiB   | rem-chunked-v1's chunk size; well below LTO-9's 16 MiB cap. |
| Data blocks per stripe (k) | 128   | Stripe data size ~128 MiB; balances codeword math cost vs. dispersion. |
| Parity blocks per stripe (m) | 4   | ~3% overhead; survives 4 erasures per stripe. |
| Stripes per neighborhood (S) | 128 | 128-way interleave defeats contiguous damage up to 512 MiB per neighborhood. |
| Neighborhood size        | 16,896 blocks (~16.5 GiB) | Fits comfortably in production memory (32 GiB peak); good independent-failure-domain granularity. |
| Bootstrap interval       | tape_capacity / 20 (~900 GB) | ~20 bootstrap copies per LTO-9 tape. |
| Capacity overhead (parity)        | 3.125%  | m/k = 4/128. |
| Capacity overhead (bootstrap)     | 0.0001% | ~20 blocks per 18 TB tape, negligible. |
| Per-neighborhood damage tolerance | ~512 MiB | S × m blocks of contiguous loss. |
| Number of neighborhoods per LTO-9 tape | ~1100 | 18 TB / 16.5 GiB. |

These defaults are explicitly chosen for the target archive's
failure profile (servo damage of perhaps 100 MB to a few GB)
and storage environment (controlled, indoor, climate-managed).
See Appendix A for the source data behind these choices.

### 11.2 Conservative scheme

For tapes expected to see harsher conditions (long-term offsite
storage, suspect climate control, older library hardware):

| Parameter                | Conservative | Rationale |
|--------------------------|-------------|-----------|
| Data blocks per stripe (k) | 64        | Smaller stripes; lower codeword cost. |
| Parity blocks per stripe (m) | 6        | Higher recovery threshold per stripe. |
| Stripes per neighborhood (S) | 64       | 64-way interleave. |
| Neighborhood size        | 4,480 blocks (~4.4 GiB) | Smaller neighborhoods; more independent failure domains. |
| Capacity overhead        | 9.4%      | m/k = 6/64. |
| Per-neighborhood damage tolerance | ~384 MiB | S × m. |

### 11.3 Validation

`ParityScheme::validate()` enforces:

- `data_blocks_per_stripe >= 2` (a stripe of 1 data + m parity
  has no advantage over just writing m+1 copies of the data).
- `parity_blocks_per_stripe >= 1` (m=0 is "no parity at all";
  use the `--parity none` flag instead).
- `parity_blocks_per_stripe <= data_blocks_per_stripe` (m > k
  is wasteful — m copies of the data would be smaller).
- `stripes_per_neighborhood >= 1`.
- `data_blocks_per_stripe + parity_blocks_per_stripe <= 255` for
  GF(2⁸) RS. The crate enforces this internally; we double-check
  at validation time for a friendlier error message.
- Total neighborhood blocks fits in u32 (4 G blocks, far more
  than any realistic configuration).

Schemes that fail validation are rejected at write-session open
with `ParityError::InvalidScheme`. The bootstrap reader also
validates on tape mount; a tape recording an invalid scheme is
treated as suspect and the operator is alerted.

### 11.4 Per-tape configuration

The scheme is **per-tape**, recorded in the bootstrap at write
time. Once a tape is written with scheme S, it's read with
scheme S forever. Changing the system-wide default scheme
affects only new tapes; old tapes remain readable with their
original parameters.

Tapes can be written with a non-default scheme by passing an
explicit `ParityScheme` to the write-session creation. The
caller (Layer 5, typically driven by orchestrator policy) is
responsible for choosing the scheme. The CLI exposes `--parity
default`, `--parity conservative`, `--parity none`, and
`--parity custom:k,m,S` for operator control.

### 11.5 No-parity opt-out

A write session can explicitly request `parity = none`. The
bootstrap records this via the no-parity flag bit (§5.6).
Readers handle this transparently — when the bootstrap's
no-parity flag is set, the `ParitySource` is bypassed and the
body format reads directly from the inner source.

Use cases for no-parity:
- Scratch tapes for development and testing.
- Migration tapes that exist as a temporary intermediate.
- Tapes where the orchestrator has determined the data is not
  irreplaceable.

For the production archive workflow, all tapes use the
default scheme. The opt-out is for development.

---

## 12. Error model

```rust
#[derive(Debug, thiserror::Error)]
pub enum ParityError {
    #[error("tape I/O error: {0}")]
    TapeIo(#[from] TapeIoError),

    #[error("invalid parity scheme: {0}")]
    InvalidScheme(String),

    #[error("Reed-Solomon error: {0}")]
    ReedSolomon(reed_solomon_erasure::Error),

    #[error("stripe {stripe:?} unrecoverable: lost {lost_count} blocks (limit is {limit})")]
    Unrecoverable {
        stripe: StripeAddress,
        lost_count: u16,
        limit: u16,
    },

    #[error("invariant violation: {0}")]
    Invariant(&'static str),

    #[error("bootstrap not found anywhere on tape")]
    NoBootstrapFound,

    #[error("no bootstrap found at expected position {0}")]
    NoBootstrapAtPosition(u64),

    #[error("bootstrap parse error: {0}")]
    BootstrapParse(String),

    #[error("parity scheme mismatch: bootstrap says {tape}, reader expects {expected}")]
    SchemeMismatch { tape: String, expected: String },

    #[error("magic mismatch on expected parity block at LBA {lba}: got {got:02x?}")]
    BadParityMagic { lba: u64, got: [u8; 8] },
}
```

Errors map upward:

- `ParityError::Unrecoverable` → surfaces to the format crate as
  a `TapeIoError::CheckCondition` with a synthetic
  unrecoverable-stripe sense buffer; the format wraps it into
  its own `FormatError::Unrecoverable` → Layer 5's gRPC error
  code for unrecoverable reads.
- `ParityError::TapeIo(..)` → propagated through.
- `ParityError::NoBootstrapFound` → tape can't be read at all
  with parity. Layer 5 reports to operator; potentially treats
  tape as no-parity for emergency reads (with strong warning).
- `ParityError::InvalidScheme` → write-session creation fails.
- `ParityError::SchemeMismatch` → bootstrap recorded a scheme
  the reader doesn't support; tape is suspect.

---

## 13. Implementation plan

Each step ends in `cargo fmt + cargo clippy --workspace
--all-targets -- -D warnings + cargo test --workspace + cargo doc
--workspace --no-deps`, all green. Steps are sized to be
commit-worthy individually.

**Status (as of 2026-05-19):** Steps 11.0–11.17 complete and
merged to `main`. Step 11.18 (live smoke on production
MSL3040) and the parity-side of Step 11.17 full hardware round-
trip pending a maintenance window on the chassis. 112 unit +
integration tests pass for the parity crate, plus 1
`#[ignore]`-gated hardware test ready to run when a QuadStor
VTL is reachable.

| Step | Status | Description |
|--|--|--|
| 11.0 | ✅ | Crate skeleton with deps; depends on `remanence-library`, not on `remanence-format` (sibling architecture). |
| 11.1 | ✅ | `ParityScheme`, `SchemeId`, `StripeAddress`, `StripePosition` value types + validation per §11.3. |
| 11.2 | ✅ | LBA ↔ stripe mapping per §5.3; round-trip + interleave-pattern tests. |
| 11.3 | ✅ | Bootstrap block writer/parser per §5.6, with no-parity-omits-scheme + buffer-padding invariants. |
| 11.4 | ✅ | Parity block format per §5.5, HMAC-SHA256(key=tape_uuid, msg=…) magic, CRC over bytes 0..22. |
| 11.5 | ✅ | `ReedSolomonCodec` wrapper around `reed_solomon_erasure::galois_8::ReedSolomon`. |
| 11.6 | ✅ | `ParitySink::new` + data-path write_block with stripe accounting. |
| 11.7 | ✅ | `ParitySink::write_bootstrap` — bootstrap is a regular parity-protected block. |
| 11.8 | ✅ | Parity emission at S×k boundary; row-major output per §5.2. |
| 11.9 | ✅ | `ParitySink::finish` with §5.4 zero-padding strategy. |
| 11.10 | ✅ | `discover_bootstrap` with LBA-0-first + fractional-position fallback per §8.1. |
| 11.11 | ✅ | `ParitySource` happy-path passthrough. |
| 11.12 | ✅ | Recovery path per §8.3 — erasure detection, surviving-member walk, RS reconstruct, post-recovery re-locate. |
| 11.13 | ✅ | LRU stripe cache per §9.3 — measured to reduce reads from ~13 to ~8 on a 2-failure same-stripe scenario. |
| 11.14 | ✅ | `ParityAuditHook` + `RecoveryEvent` emission on both success and failure paths. |
| 11.15 | ✅ | Corruption-injection §14 matrix as 7 integration tests in `tests/recovery_matrix.rs`. |
| 11.16 | ✅ | `ParityConfig` + `parse_parity_arg` for the §11.4 CLI vocabulary. CLI wiring deferred until Layer 5 write-session subcommand exists. |
| 11.17 | ✅ scaffold; 🟡 full hardware round-trip pending | `tests/quadstor_parity.rs` ships the `#[ignore]`-gated test scaffold with the env-var contract. Full ParitySink+DriveHandleSink composition lands when Layer 5 plumbs the daemon write session; until then, the unit + integration tests in `recovery_matrix.rs` cover the parity correctness contract against the public crate surface. |
| 11.18 | 🟡 pending | Live smoke on production MSL3040 — same as 11.17 against a scratch LTO-9 tape. Pending hardware access window. |
| 11.19 | ✅ | Wrap-up: this design-doc status table sync + final journal entry. |

### 13.1 Dependencies on other layers

3c's actual dependency on 3b is narrower than the layer naming
implies. 3c does **not** depend on:

- The body format trait shape (`TapeFormat`, `FileAddressable`,
  `ByteRangeAddressable`, `Verifiable`).
- The format registry (`FormatRegistry`).
- The catalog reader/writer or the catalog CBOR schema.
- `rem-chunked-v1` or any specific body format.

3c **does** depend on:

- `BlockSink` and `BlockSource` traits (3b spec §4.5).
- `DriveHandleSink` / `DriveHandleSource` newtype wrappers that
  adapt `DriveHandle` to those traits.

These two items together correspond to **3b's step 10.2** in the
3b implementation plan. After step 10.2 lands, 3c can be
implemented to completion (steps 11.0-11.18) in parallel with
the rest of 3b (steps 10.3-10.15).

**Structural refactor — landed.** The `BlockSink`,
`BlockSource`, `DriveHandleSink`, and `DriveHandleSource` types
live in `remanence-library::block_io` (landed in 3b Step 10.2,
commit `97749d4`). 3b and 3c are true siblings depending on
`remanence-library`. This subsection was previously marked
"optional" pending the user's call; the call was made
2026-05-18 and the move shipped.

Other layer dependencies:

- 3a: complete (steps 9.0-9.8 merged). No further work required.
- Layer 5 (gRPC API): consumes recovery events via the audit
  hook. Not on 3c's critical path; the audit hook is a trait
  object 3c can be tested against with a mock.

**Implementation ordering options:**

The dependency analysis above allows three possible orderings:

1. **3b before 3c (the original order).** Build 3b fully, then
   3c on top. Conservative; ensures the body format and parity
   layer are tested against each other from the start.

2. **3b skeleton, then 3c, then rest of 3b.** Build 3b steps
   10.0-10.2 (crate skeleton, traits, adapters), then 3c steps
   11.0-11.18 to completion, then return to 3b steps 10.3-10.15.
   Has the advantage that 3c (the simpler-surface layer) is
   complete and well-tested before the larger 3b surface gets
   built on top.

3. **3b and 3c in parallel after the refactor.** After moving
   `BlockSink`/`BlockSource` into `remanence-library` and
   completing 3b steps 10.0-10.1 (crate skeleton + capability
   types only), both 3b and 3c can be developed independently.

Recommendation: option (2) or (3) is generally preferable to
(1). 3c is the simpler artifact (one trait wrapper, one
encoding library, well-bounded surface). Building it first
exercises 3a more thoroughly than 3b would (3c writes thousands
of blocks per neighborhood; 3b mostly writes objects with file
marks), surfacing 3a bugs sooner. 3c also locks in the on-tape
physical layout before the format layer builds on it, which
makes 3b's job easier.

Option (2) has the lowest workflow friction for a single-
developer setup. Option (3) is appropriate if 3b and 3c will be
developed by different people (or different Claude Code
sessions) concurrently.

### 13.2 What's left out of v0.2

- **Parity scrubbing scheduler.** Belongs in Layer 5.
- **Parity-only re-encoding** of existing tapes. Out of scope.
- **Erasure-code variants other than Cauchy GF(2⁸).** The
  scheme ID is extensible; future versions can use GF(2¹⁶) or
  alternative codes (LDPC, etc.) if the math becomes more
  favorable. v0.2 ships only `rs-cauchy-gf256-v1`.

---

## 14. Testing strategy

Five-tier shape:

1. **Math unit tests** (`remanence-parity/src/model.rs`,
   `src/mapping.rs`). LBA ↔ stripe mappings round-trip; scheme
   validation; neighborhood-size math. Property-based via
   `proptest` for the LBA mapping.

2. **Encoder/decoder tests** (`remanence-parity/src/codec.rs`).
   The `reed-solomon-erasure` crate's own tests cover the RS
   math; our tests verify the integration. Tests at small
   stripe sizes for fast iteration; one comprehensive test at
   default parameters.

3. **Bootstrap format tests** (`remanence-parity/src/bootstrap.rs`).
   Round-trip CBOR payloads, validate magic + CRC, exercise
   the no-parity flag path, exercise version checking.

4. **In-memory round-trip** (`remanence-parity/tests/round_trip.rs`).
   Use the in-memory `VecBlockSink` / `VecBlockSource` from
   `remanence_library::block_io` (landed in 3b Step 10.2).
   Write a multi-neighborhood tape's worth of data with
   bootstraps inserted at writer-policy points; parse it back;
   verify every block reads correctly. The "happy path"
   baseline.

5. **Corruption injection** (`remanence-parity/tests/recovery.rs`).
   `CorruptingBlockSource` simulates failure modes. Test matrix:
   - Single-block erasure at various stripe positions.
   - Multi-block contiguous erasure of N blocks for
     N ∈ {1, 50, 128, 256, 512, 513, 1000}.
   - Servo-style (LOCATE fails) vs MEDIUM_ERROR (READ fails)
     erasure types.
   - Erasures that hit data blocks only.
   - Erasures that hit parity blocks only.
   - Erasures that hit bootstrap blocks (and bootstrap discovery
     falls back to next position).
   - Erasures that hit catalog blocks (and parity recovers them).
   - Catastrophic erasures that exceed m: verify graceful
     failure with correct `Unrecoverable` reporting.

6. **Live smoke** (`#[ignore]`-gated). QuadStor (step 11.17) and
   production MSL3040 (step 11.18).

The corruption-injection test suite is the most operationally
important tier. It's the only way to verify recovery semantics
without weeks of waiting for real-world damage. The matrix
should be exercised on every release.

---

## 15. Open questions

### 15.1 Bootstrap discovery scan window

§7.3 mandates a bootstrap at LBA 0; §8.1 falls back to ~5%, ~10%,
etc. if LBA 0 is unreadable. The fallback positions are
approximate ("every ~5% of tape"). With variable bootstrap
placement (writer policy), and with object sizes that can reach
100 GB or more in the target archive (4K masters, compound
archives, multi-clip shoot bundles), the actual bootstrap LBA
following a target position can deviate from that target by up
to one object's worth of blocks.

Two distinct cases for the bootstrap discovery scan window:

**Normal operation:** LBA 0 is intact. The reader reads one
block, gets a valid bootstrap, is done. No scan window matters.
This is the case for every routine tape mount.

**Catastrophic recovery:** LBA 0 is unreadable (damaged tape
start, head-clog at write time, etc.) and the reader needs to
find any other bootstrap on the tape. In this case the scan
window is effectively the **full remaining tape**. The reader
locates to ~5%, reads forward looking for bootstrap magic, then
~10%, then ~15%, and so on — and within each target region, the
forward scan continues until a bootstrap is found or the next
target region is reached. With object sizes up to ~1 TB,
intermediate scan ranges can be hundreds of GB. This is a slow
recovery path (potentially hours of sequential reading), but
it's the catastrophic case — falling back to another tape copy
is the operationally preferred response.

The reader's logic stays simple: try LBA 0 first; if that fails,
fall back to a brute-force scan across expected positions with
no fixed upper bound on per-region scan distance. Operators
running catastrophic recovery should expect it to take time and
should usually prefer multi-copy fallback first.

No code change needed from v0.2 §8.1 — the algorithm already
handles arbitrarily-large object sizes; the `MAX_BOOTSTRAP_SCAN_BLOCKS`
constant should be set generously (or treated as unbounded in
the catastrophic path).

### 15.2 Parallelism in the writer

The proposal in §7 is single-threaded: parity computation
happens inline with the write pipeline. At default parameters
this is well within budget (CPU is not the bottleneck), but
for higher-performance configurations (k=256, m=16) it might
be worth running RS encoding in a background thread.

Lean: defer. Performance is adequate at defaults; revisit if
empirical measurement shows a bottleneck.

### 15.3 Cache warming on read

The recovery cache (§9.3) is populated on demand. Pre-warming
based on access patterns is complex and unclear ROI.

Lean: do nothing. On-demand caching is simple and correct.

### 15.4 Bootstrap magic — fixed vs derived

§5.6 uses a fixed magic `REM\x00BOO\x01` for bootstrap blocks
because the reader needs to find a bootstrap before knowing the
tape UUID. The 2⁻⁶⁴ user-data collision concern is mitigated by
the magic check happening at known LBA regions plus CRC
validation. Open: should the bootstrap magic be derived from
some publicly-known parameter (e.g., the schema spec version)
to reduce collision risk further? Probably not worth the
complexity; the current scheme is fine.

### 15.5 Inter-neighborhood damage

Damage that straddles a neighborhood boundary affects two
neighborhoods. Each is independently recoverable, but the
total damage tolerance for boundary-crossing damage is
effectively halved.

This is a real failure mode worth measuring against real
damage patterns. If empirical observation shows most damage
events are smaller than one neighborhood (highly likely), it's
not an issue.

### 15.6 Recovery read-amplification

A recovery read of one block requires up to k inner reads
(plus k LOCATEs). At default k=128 and worst-case LOCATE
latency, recovery of one block can take 30+ seconds.

This is not a 3c issue per se but worth flagging — the read
performance of a recovery-heavy workload is fundamentally
limited by tape mechanics, and parity makes individual reads
slower in exchange for making them succeed at all.

---

## 16. Out of scope

Already covered in §1 non-goals, but worth restating:

- **No replacement of LTO's built-in ECC.** Layer 3c is in
  addition to the drive's per-block inner+outer Reed-Solomon.
- **No protection against complete tape destruction.** Three-
  copy redundancy at Layer 5 handles that.
- **No format awareness.** 3c does not know what's in the
  blocks it protects; it treats them as opaque bytes.
- **No scrubbing scheduler.** Belongs in Layer 5.
- **No write verification policy.** That's spec §9.4 / Layer 5.
- **No PERSISTENT RESERVE handling.** That's spec §9.3 / Layer 5.
- **No encryption handling.** Encryption is per-block by the
  drive (Layer 6); parity blocks and data blocks are encrypted
  identically and 3c is oblivious.

---

## 17. References

- `docs/spec-v0.3.md` §5, §9.4 — on-tape format, write
  verification policy.
- `docs/layer3a-design.md` — SSC primitive set on `DriveHandle`.
- `docs/layer3b-design.md` — body format trait, `BlockSink` /
  `BlockSource`, catalog schema.
- `docs/pfr-reference.md` — chunk-size rationale, encryption
  posture, integrity model.
- `reed-solomon-erasure` crate documentation:
  <https://docs.rs/reed-solomon-erasure>.
- IBM LTO SCSI Reference GA32-0928-08 — Annex B sense-code
  tables (Table B.3 NOT_READY, Table B.4 MEDIUM_ERROR, Table B.5
  HARDWARE_ERROR) used to ground the §9.1 erasure-detection
  allowlist.
- Backblaze, "Reed-Solomon Coding for Data Backup":
  <https://www.backblaze.com/blog/reed-solomon/>.

---

## Appendix A: The LTO-4 dust incident (motivating case study)

Around 2014-2016, the Archives team ran into a class of failures
on LTO-4 tapes that exposed the limits of the medium's built-in
protection and ultimately motivated this design.

Symptoms observed:

- Specific physical regions of tape became unreadable —
  contiguous block ranges in the middle of otherwise-healthy
  tapes.
- The drive could not LOCATE to LBAs within these regions; it
  returned positioning errors. SPACE past the region worked,
  and blocks on the other side read normally.
- Affected regions ranged from ~50 MB to ~2 GB of contiguous
  tape (estimated from the LBA gaps).
- LTO's built-in inner+outer Reed-Solomon ECC was bypassed
  entirely — the drive's heads could not be positioned to read
  the affected wraps in the affected longitudinal range.

Root cause hypothesis (consistent with all observations but not
formally confirmed): dust contamination damaged the pre-written
servo tracks in localized regions of tape. Without working servo,
the drive could not position the heads at the affected
longitudinal positions, so neither data nor the inner+outer ECC
covering it could be read.

At the time, the team attempted a remediation strategy of
post-hoc Reed-Solomon parity computation: read a tape's full
contents to a disk image, compute parity over the image, store
the parity as a separate file. The approach had problems:

- Computing RS over an 800 GB monolithic codeword took ~1 day
  per tape on the available hardware.
- The data-movement cost (full tape read to disk, full disk
  read to encoder) dominated.
- The remediation was post-hoc rather than inline, so the
  parity didn't exist for tapes already damaged.
- The strategy was abandoned after a few tapes due to
  prohibitive cost.

The lessons informing Layer 3c's design:

1. **The failure mode is positional contiguous damage**, not
   randomly distributed bit errors. The protection scheme
   must defend against contiguous loss, not against scattered
   single-block failure.
2. **Computation must be inline.** Post-hoc encoding requires
   reading the tape, which is exactly what's failing.
   Computing during the original write is essentially free.
3. **Per-stripe RS is dramatically faster than monolithic RS.**
   The LTO-4 attempt used one codeword for the whole tape;
   3c's per-stripe approach uses ~280,000 small codewords,
   each computed independently in milliseconds.
4. **Distribution matters more than overhead.** A naive
   "k consecutive data blocks, m consecutive parity blocks"
   layout would have left the LTO-4 damage entirely
   unrecoverable. The interleave pattern (§5.2) converts
   positional contiguous damage into approximately uniform
   per-stripe damage.
5. **Three copies plus light parity beats one copy plus heavy
   parity.** The three-copy redundancy at the orchestration
   layer means parity's job is "improve the recoverability of
   this copy," not "be the sole defense." 3% overhead with
   distributed parity is the right point in that trade-off
   space for the target archive.

The LTO-4 damage today, written under the v0.2 scheme:

- A 500 MB contiguous damage region (well within the typical
  range observed): fully recovered by per-neighborhood parity.
  No reader-visible impact beyond a logged RecoveryEvent.
- A 2 GB contiguous damage region (the worst observed): some
  stripes would lose more than m=4 blocks; partial recovery
  with structured `Unrecoverable` errors for specific LBA
  ranges. The body format reports unrecoverable byte ranges
  within affected files; the orchestrator falls back to
  another tape copy for those specific ranges.
- A bootstrap block within a damaged region: the bootstrap
  discovery (§8.1) automatically falls back to the next
  bootstrap copy.

The takeaway: 3c protects against the failure mode that actually
happens in this archive, at a cost (3% capacity) that's
operationally trivial. The worst-case damage events remain
reliant on multi-copy fallback — which is exactly what multi-copy
is for.

---

*End of design v0.2. Comments and corrections welcome — please
annotate inline rather than rewriting.*
