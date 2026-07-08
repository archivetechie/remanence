# Layer 3c Revision Proposal: Filemark-Aware Parity Epochs

> **⚠️ SUPERSEDED (May 2026).** This proposal has been fully
> folded into `layer3c-design.md` (now v0.3.1), which is the
> single authoritative Layer 3c spec. Every decision here lives
> there — including the object-commit model (this doc's §6.4),
> which was integrated into `layer3c-design.md` §7.2.1 and
> extended to model object parity-protection lag. This file is
> retained only as a record of the design discussion that
> produced the epoch model; do not implement from it or treat it
> as active. Review and implementation should use
> `layer3c-design.md`.

**Status:** SUPERSEDED design proposal (was v0.3). Folded into
`layer3c-design.md` v0.3.1. Originally targeted a Layer 3c
revision (then-current: `layer3c-design-v0.2.md`). Companion to
`rem-tar-v1-design.md` v0.8.1 §2.1, §5.1, §16.14. Sister docs:
`docs/layer3a-design.md`, `docs/layer3b-design.md`,
`docs/spec-v0.3.md`.

**Changes from v0.2 (implementation-readiness review).** The
sidecar lifecycle and recovery contracts are now nailed down so
an implementation can't recreate old bugs: sidecar capacity
reservation (§6.1); sidecar/bootstrap writes bypass epoch
accumulation (§6.2, §8.1); "pending epoch" = completed-but-
deferred, never flush the partial epoch at object close (§6.3);
object commit state model incl. sidecars + catalog (§6.4);
sidecar redundancy DECIDED as not-replicated/rely-on-three-copy
(§7); final partial epoch zero-pad-for-RS rule (§9.1);
catalog-less recovery via digest + scan-reconstruct, Option B
(§10); `finish_object` no longer flushes the body's final block
— that's rem-tar's `BodyBlockWriter::finish_after_tar_eof()`
(§8.1, matches rem-tar-v1 v0.8.1 #4).

**Changes from v0.1 (review fixes):** sidecar shard layout
corrected (§5, review #6); bootstrap protection Option B (§10,
review #8); filemark-map persistence via 3b `catalog_tape_files`
(§3, review #7).

This document proposes the changes 3c needs to support
per-object tape filemarks while keeping Reed-Solomon parity
sensible. It does not rewrite the whole 3c spec; it captures
the design decision and the deltas, so the decision can be
reviewed before the full v0.3 rewrite of `layer3c-design.md`.

---

## 1. Why this revision

3c v0.2 models the whole tape as a **uniform parity-protected
data area**: `ParitySink` wraps a `BlockSink` and inserts
parity blocks inline into the same physical block stream, with
a closed-form LBA→stripe mapping and no carve-outs (v0.2 §5.1,
§5.2, §6.1).

rem-tar-v1 originally accepted this (v0.6: "no filemarks inside
the parity body"). But that choice sacrificed an operationally
valuable property the *initial* Remanence spec wanted: **a tape
as a sequence of pax tar archives separated by filemarks**,
navigable with standard `mt`/`tar`, where each archive is
independently readable.

A design review of rem-tar-v1 v0.6 showed we can keep
per-archive filemarks AND keep parity sensible — but not with
the "pure physical-LBA inline interleaver" model unchanged.
rem-tar-v1 v0.7 adopts the filemark-aware model and depends on
3c implementing it. This document specifies the 3c side.

### The constraint that forces the change

A tape filemark is **not a data block**. It is a SCSI
separator/positioning marker. It is not a fixed-size byte
payload that can be a Reed-Solomon shard. SCSI logical
positioning counts both records and filemarks between BOT and
a target, so filemarks affect positioning but cannot be fed
into an RS stripe.

3c v0.2's inline interleave assumes a gap-free physical block
stream it fully controls. Introduce per-object filemarks into
that stream and the closed-form LBA→stripe math breaks: the
filemarks aren't blocks, they shift physical positions, and a
stripe that "spans" a filemark has no clean physical
contiguity.

### Two bad resolutions (rejected)

1. **Flush parity at every filemark.** Make each object
   boundary a parity-neighborhood boundary: pad and close the
   neighborhood, write parity, write the filemark, start fresh.
   Rejected: small objects waste huge padding; many small
   objects destroy interleaving; the orchestrator would have to
   batch objects to fill neighborhoods — pushing erasure-code
   geometry up into the wrong layer.

2. **No filemarks at all** (rem-tar-v1 v0.6 Option 1).
   Rejected: loses standard tape navigation and the
   independently-readable-archive property — the whole reason
   for the change.

### The chosen resolution

**Decouple "what parity protects" from "physical tape
structure."** Parity is computed over a logical sequence of
data records (`ParityDataOrdinal`) that skips filemarks.
Parity neighborhoods ("epochs") span across object filemarks.
Filemarks are physical separators, never shards, and never
flush a neighborhood. Completed parity epochs are written as
**sidecar tape files** at object boundaries — never injected
inside an object archive.

Result: clean per-object pax tar archives with standard tape
navigation, AND Reed-Solomon parity that accumulates freely
across objects of any size without staging.

---

## 2. Address model (replaces v0.2 §5.1, §5.3)

Three address spaces. (rem-tar-v1 §2.1 describes the same model
from the body-format side.)

**`TapePosition`** — physical tape address, expressed as
`(tape_file_number, block_within_file)`, plus a filemark map
for translation. The physical tape is a sequence of
filemark-delimited tape files: object archives interspersed
with parity-epoch sidecar files.

**`ParityDataOrdinal`** — a logical sequence numbering only the
protected **data** records, in order, across object archives,
skipping filemarks and skipping parity sidecar blocks. Object
A's data blocks get ordinals 0..a; the filemark after A gets no
ordinal; object B's data blocks continue at a+1. Reed-Solomon
stripes are defined over `ParityDataOrdinal`.

**`BodyLba`** *(per-object)* — the block stream within one
object archive, starting at 0 for each object, paired with the
object's `tape_file_number`. This is what rem-tar-v1 stores in
the catalog. 3c maps `(tape_file_number, body_lba)` →
`ParityDataOrdinal` and → `TapePosition`.

```
TapePosition:      | obj A | FM | obj B | FM | parity sidecar | FM | obj C | FM | ...
ParityDataOrdinal:  0..a         a+1..b       (not data)            b+1..c
BodyLba (per obj):  0..a         0..(b-a)                           0..(c-b)
```

The closed-form LBA→stripe math of v0.2 §5.3 is **replaced** by
a `ParityDataOrdinal`→stripe mapping (still closed-form in the
ordinal space) plus a durable **filemark map** that translates
ordinal ↔ physical position. The filemark map is the new
structural element; it's small (one entry per tape file).

---

## 3. The filemark map

A durable structure recording, for each tape file on the
medium:

```
FilemarkMapEntry {
    tape_file_number: u32,
    kind: TapeFileKind,           // ObjectArchive | ParityEpochSidecar | Bootstrap
    first_parity_data_ordinal: u64,   // for ObjectArchive: ordinal of its first data block
                                      // (sidecars/bootstrap: not in the ordinal space)
    data_block_count: u64,        // # of data blocks (ObjectArchive only)
    physical_start_hint: u64,     // SCSI block address hint for fast LOCATE
}
```

The map lets 3c answer the two questions it needs:

- Read path: given `(tape_file_number, body_lba)`, find the
  physical position (seek to the tape file via `mt fsf` or a
  LOCATE hint, then `body_lba` blocks in).
- Recovery path: given a `ParityDataOrdinal`, find which tape
  file and offset hold each stripe peer, and which sidecar
  holds the parity shards.

Where the map lives:
- Authoritatively in the **Layer 3b/Layer 4 catalog**, in a
  dedicated `catalog_tape_files` table (specified in the 3b
  follow-up, review #7). Object tape files' ordinals are
  derivable from per-object rows (each object's
  `tape_file_number` + `data_block_count` + a running ordinal
  sum), but **sidecar and bootstrap tape files are not** — they
  have no file rows yet occupy tape-file numbers and (for
  sidecars) define ordinal→parity ranges. So the map is only
  fully reconstructable if sidecars and bootstraps are
  catalogued explicitly, which `catalog_tape_files` does.
- A compact **digest** of the filemark map in each bootstrap
  tape file (v0.2 §5.6). The digest validates a map but does
  not provide one; a catalog-less reader reconstructs the map
  by scanning tape files (identifying each by magic) and
  validates the reconstruction against the digest. This is
  decided as Option B in §10 (no longer an open question).

This replaces v0.2's assumption (§10.1 "no new catalog
fields"): the filemark map must be persisted. It's small, but
it is **not** "just derivable" from object rows alone — hence
the explicit `catalog_tape_files` table.

---

## 4. Parity epochs (replaces v0.2 §5.1 neighborhoods)

An **epoch** is the new name for a parity neighborhood, defined
over `ParityDataOrdinal` rather than physical LBA:

- Geometry: same RS parameters as v0.2 (`k` data shards, `m`
  parity shards, `S` stripes per neighborhood), but the
  default is now **block-size-aware**: at the rem-tar-v1
  default of 256 KiB blocks, use `S=512, m=4, k=128` to retain
  ~512 MiB contiguous-loss tolerance (512 × 4 × 256 KiB). This
  replaces v0.2's `S=128` (which assumed 1 MiB blocks and gives
  only ~128 MiB at 256 KiB). See rem-tar-v1 §6.1.
- An epoch accumulates data blocks by `ParityDataOrdinal`
  across as many object archives as it takes to fill `S × k`
  data shards. Object boundaries (filemarks) are invisible to
  the epoch — a stripe can have shards in object A and object
  B.
- When an epoch fills, its `S × m` parity shards are emitted as
  a **parity-epoch sidecar tape file** at the next object
  boundary (§5).

The neighborhood byte-size is unchanged from v0.2 (S × (k+m) ×
block_size ≈ 16.5 GiB at the new geometry), so writer memory
footprint is the same; there are just more, smaller blocks per
epoch.

---

## 5. Parity-epoch sidecar tape files (new; replaces v0.2 §5.2 interleave)

Parity is **no longer interleaved inline**. Instead:

- As data blocks flow through `ParitySink`, 3c accumulates them
  into the current epoch's RS computation (by ordinal),
  **passing the data blocks through unchanged** to the inner
  sink so the object archive stays a clean pax tar stream.
- When an epoch completes (or at object close with a pending
  epoch — see §6 spool), 3c writes a **parity-epoch sidecar**:
  its own filemark-delimited tape file containing the epoch's
  parity shards plus a header.

### Sidecar format

Two options; **the raw fixed-block sidecar is recommended** for
recovery simplicity, with the pax-wrapped variant available if
the "every tape file is tar-inspectable" property is wanted.

**Option A — raw fixed-block sidecar (recommended):**

A critical constraint (review #6): an RS parity shard for a
`chunk_size` data shard is itself **`chunk_size` bytes**. You
cannot put per-shard metadata (stripe index, shard index, CRC)
*inside* a shard's block — there's no room. So all per-shard
metadata lives in a **header block (or blocks)** at the front
of the sidecar, and the parity shard blocks are full raw shard
bytes, nothing else:

```
[block 0..H-1: sidecar header + shard index table]
   block 0:
     magic "REMPAR01" (8 bytes)
     tape_uuid (16)
     epoch_id (8)
     scheme params (k, m, S, block_size)
     protected_ordinal_start, protected_ordinal_end_exclusive
       (half-open [start, end_exclusive); matches the catalog
       column names and Rust slicing — review #15)
     logical_shard_count   (= S × k; the padded width used for
       the RS math — equals real count except in the final
       partial epoch, §9.1)
     real_data_shard_count (= end_exclusive − start; how many
       data shards are REAL vs implicit-zero padding, §9.1)
     parity_block_count P
     shard_index_block_count H
     header_crc64
   blocks 1..H-1 (shard index table; spills here if it
     doesn't fit in block 0):
     for each parity shard i in 0..P:
       (stripe_index, shard_index_within_stripe, shard_crc64)
     entries packed; the table is itself CRC-protected per block

[blocks H..H+P-1: parity shards]
   each block: exactly chunk_size bytes of one RS parity shard,
   and NOTHING else (no inline header, no inline CRC). The
   shard's CRC and stripe/shard identity are in the index table
   above. Recovery reads a shard block as a full raw shard.
```

The shard index table is small: 3 fields × P shards. At the
default `S=512, m=4` an epoch has S×m = 2048 parity shards;
at ~16 bytes/entry that's ~32 KiB, well under one 256 KiB
header block — so typically `H=1` and the index fits in block
0's tail. Large schemes spill the table across a few header
blocks (`H>1`), accounted for by `shard_index_block_count`.

This is the fix for the "impossible as written" v0.1 layout
that tried to put `stripe_index, shard_index, parity bytes,
block_crc64` all in one block: the parity bytes alone fill the
block.

**Option B — pax-wrapped sidecar:**
A minimal pax tar archive (so it's a valid tar tape file)
containing:
```
_remanence/parity/epoch-000123.cbor   (header + full shard index table)
_remanence/parity/epoch-000123.bin    (raw parity shards, chunk_size each)
```
Same separation: metadata in the `.cbor`, full raw shards in
the `.bin`. The `.bin` shards are still chunk_size-aligned so
recovery can address them by index.

Trade-off: Option A is simpler and faster for the recovery
tool to parse and needs no tar layer; Option B keeps the
"all tape files inspect with standard tools" story but makes
the recovery tool depend on tar+CBOR parsing exactly when the
tape is already damaged. **Recommendation: Option A**, because
recovery should depend on as little machinery as possible. The
sidecar header magic makes it identifiable on a forensic scan.

### Why sidecars don't break tar navigation

A user seeking to object N's tape file with `mt fsf N` skips
over any sidecar tape files (they're just other tape files).
Plain `tar xf` of object N's tape file never encounters parity
bytes, because they're not in N's tape file at all. The parity
sidecars are consulted only by a Remanence-aware reader during
recovery.

---

## 6. The large-archive wrinkle: parity spool (mechanism decided; thresholds deferred)

A single huge object (e.g., 1 TiB) may fill several epochs
before its closing filemark. We cannot write a parity sidecar
*inside* the object's tape file (that would corrupt the pax tar
stream). So pending parity must wait until the object's
filemark.

**Recommended approach: deferred parity sidecars with disk
spool.** As epochs complete mid-object, compute their parity
and spool the parity payload to local disk. When the object
closes and its filemark is written, emit the pending parity
sidecar tape files. This stages only **parity** (~3.125% at
k=128,m=4 — ~32 GiB for a 1 TiB object), not archive data, so
it doesn't reintroduce the "batch whole archives" clunkiness.

Rejected alternatives:
- *Parity as tar members inside the object* — makes parity
  rem-tar-specific and can't protect the middle of one huge
  file entry. No.
- *Split giant objects into multiple pax archives* — preserves
  filemarks but complicates restore semantics and breaks the
  clean "one object = one pax archive" rule. No.

**This is an open question deferred to implementation** (per
the rem-tar-v1 §16.14 operator decision). The guardrails it
will need, mirroring rem-tar-v1 §9.2's file-hashing spool:
- Free-space check before spooling parity for a large object.
- Configurable spool directory and a max-spool-bytes cap.
- Crash-recovery semantics: if the writer dies mid-object with
  spooled parity, the partial object is unrecoverable anyway
  (rem-tar-v1 has no resumable write — §11 "RESUMABLE_WRITE:
  not supported"), so the spool is simply discarded on the
  retry. The retry rewrites the whole object from the start.

The spool sizing is bounded and predictable (≈ object_size ×
m/k), which makes the free-space check straightforward.

### 6.1 Sidecar capacity reservation (review #8)

Deferring parity to sidecars introduces a problem inline parity
never had: **the tape can fill with object data and leave no
room for the parity sidecars that protect it.** Inline parity
consumed its space as it went; sidecars consume it later, so
the writer must reserve for it in advance.

Normative rule:

```
ParitySink maintains a running `required_sidecar_reserve`:
  reserve = bytes for all pending completed-epoch sidecars
          + bytes for the current partial epoch's sidecar at finish (§6.3/§11)
          + bytes for the required bootstrap copies still to write (§10)
          + filemark/control overhead

Before begin_object() (and before accepting more object data
that would start a new epoch), ParitySink checks:
  remaining_tape_capacity - reserve  >=  projected_object_size

If that does not hold, ParitySink refuses to start/continue the
object (returns CapacityReserveExceeded) so Layer 5 closes the
tape cleanly (write pending sidecars + final bootstrap) before
EOM, rather than discovering at EOM that parity won't fit.
```

The reserve is computable: parity per epoch is `S × m ×
block_size`; pending epochs are known; bootstrap copy count and
size are fixed by config (§10). The projection uses the
object's declared size (Layer 5 knows it pre-write) plus a
margin for headers/manifest/alignment.

### 6.2 Sidecar/bootstrap writes bypass epoch accumulation (review #14)

Stated in the API (§8.1) and repeated here as a layout
invariant: when 3c writes a parity-epoch sidecar or a bootstrap
tape file, those blocks are forwarded to the inner sink **only**
— they are not assigned `ParityDataOrdinal` and not fed into RS
epoch accumulation. Only object-archive blocks (written via
`ParitySink::write_block` between `begin_object`/`finish_object`)
get ordinals and contribute to epochs. An implementation that
routed sidecar writes through the same accumulation path would
silently corrupt the next epoch's parity with sidecar bytes.

### 6.3 "Pending epoch" means completed-but-deferred, not partial (review #10)

The sidecar-emission rule at object close is precise, to avoid
accidentally recreating the rejected "flush parity at every
filemark" behavior:

```
At finish_object():
  emit sidecars ONLY for epochs that have COMPLETED (filled
  S × k data shards) and whose parity was deferred/spooled
  during this object.

  DO NOT flush or emit the current PARTIAL epoch merely because
  an object closed. The partial epoch keeps accumulating into
  the next object.

At finish() (tape close only):
  close the current partial epoch per the final-epoch rule (§11).
```

So a tiny object that doesn't complete an epoch produces **no**
sidecar at its close; the epoch spans into the next object. A
huge object that completes several epochs produces those
sidecars at its close (from spool, §6). This is what lets small
and large objects coexist without per-object parity padding.

### 6.4 Object commit semantics (review #9)

An object isn't "safely archived" the moment its tar bytes and
filemark are on tape — its completed-epoch sidecars and the
catalog rows must also be committed. Without an explicit commit
boundary, a tar object could exist on tape that the catalog
treats as parity-protected when its sidecars actually failed to
write.

Object commit state model (Layer 5 owns the transitions):

```
ObjectWrittenUnprotected   — tar file + filemark on tape;
                             completed-epoch sidecars NOT yet written
ObjectSidecarsPending      — sidecars being emitted (finish_object)
ObjectParityProtected      — all completed-epoch sidecars on tape
ObjectCatalogCommitted     — catalog_objects + catalog_files +
                             catalog_tape_files (object AND its sidecars)
                             rows committed in one DB transaction
```

Normative: **an object write is successful only at
`ObjectCatalogCommitted`.** If sidecar writing fails between
states, Layer 5 treats the object as not-successfully-written
and (since rem-tar-v1 has no resumable write) rewrites it on the
next attempt; the orphaned tar bytes on the failed tape are
ignored (no catalog rows point at them). The
`catalog_tape_files` rows for the object and its sidecars are
inserted in the same transaction as the object/file rows, so
the catalog never shows an object as protected without its
sidecar rows present.

---

## 7. Sidecar redundancy (DECIDED: not replicated in v1)

A parity-epoch sidecar is a single point of failure for the
data region its epoch protects: lose the sidecar tape file to a
damaged region, and that epoch's data on *this tape* has no
parity backing.

Options considered:
1. **Replicate each sidecar** at two distant tape positions.
   Doubles parity overhead to ~6.25% but makes a single damaged
   region unable to take out both a data span and its parity.
2. **Parity-on-parity**: protect sidecars with a lighter RS
   code. Recursive, more complex.
3. **Don't replicate; rely on the three-copy archive policy.**

**Decision (v1): option 3.** This is now normative, not open:

```
v1 parity-epoch sidecars are NOT themselves replicated or
parity-protected.

A damaged sidecar degrades its epoch to no-parity ON THIS TAPE.
The epoch's object data is still present and tar-extractable;
only the per-tape parity recovery for that epoch is lost.

When recovery of damaged DATA in such an epoch is needed,
Layer 5 MUST fall back to another of the archive's tape copies
(Remanence policy: three copies per archive, one encrypted).
```

Rationale: Remanence's threat model already assumes three
copies per archive. Per-tape sidecar replication would
duplicate, at 2× parity overhead, protection the multi-copy
policy already provides across tapes. The failure mode it
guards against — a single servo-damage event taking out both a
data span and its co-located sidecar on one tape — is exactly
what the other two copies cover.

**Caveat that bounds this decision:** option 3 is correct only
while every archive genuinely has multiple copies. If a
single-copy tape tier is ever introduced (cost-driven cold
storage, etc.), sidecar redundancy must be revisited for that
tier — option 1 or 2 — because there is no other copy to fall
back to. The v1 default assumes the three-copy invariant holds;
Layer 5 SHOULD refuse to treat a single-copy tape as fully
recoverable under this scheme.

---

## 8. API changes (replaces v0.2 §6.1, §6.2)

The `ParitySink`/`ParitySource` interface changes from
"inline-interleave wrapper" to "per-object body stream +
epoch/sidecar manager."

### 8.1 `ParitySink`

```rust
pub struct ParitySink<'a> {
    inner: &'a mut dyn BlockSink,          // the raw DriveHandle sink
    scheme: ParityScheme,
    tape_uuid: [u8; 16],
    epoch_state: EpochState,               // accumulates RS by ParityDataOrdinal
    spool: ParitySpool,                     // disk spool for pending epoch parity (§6)
    filemark_map: FilemarkMapBuilder,       // records tape files as they're written
    audit_hook: Option<Arc<dyn ParityAuditHook>>,
}

impl<'a> ParitySink<'a> {
    pub fn new(
        inner: &'a mut dyn BlockSink,
        scheme: ParityScheme,
        tape_uuid: [u8; 16],
        spool_config: SpoolConfig,          // dir, max bytes (§6)
    ) -> Result<Self, ParityError>;

    /// Begin a new object archive. Returns the tape_file_number
    /// assigned to it. rem-tar-v1 writes the object's blocks via
    /// the BlockSink impl below; they accumulate into the current
    /// epoch by ParityDataOrdinal AND pass through to `inner`.
    pub fn begin_object(&mut self) -> Result<u32, ParityError>;

    /// Finish the current object. PRECONDITION: the body format
    /// has already flushed its final (zero-filled) block via
    /// BodyBlockWriter::finish_after_tar_eof() — see rem-tar-v1
    /// §9.1.1 and review v0.8.1 #4. finish_object() does NOT hold
    /// or flush any partial tar buffer; it asserts the object's
    /// block stream ended on a block boundary, writes the
    /// terminating filemark, and emits any parity-epoch sidecar
    /// tape files whose epochs completed during this object
    /// (including spooled ones, §6). Returns the object's geometry
    /// AND any sidecar tape-file records produced, so Layer 5 can
    /// commit catalog_tape_files rows atomically (§9, object
    /// commit semantics).
    pub fn finish_object(&mut self) -> Result<ObjectCloseResult, ParityError>;

    /// Emit a bootstrap tape file at the current position (writer
    /// policy; carries the filemark-map digest, §10). Bootstrap
    /// blocks are NOT assigned ParityDataOrdinal and NOT included
    /// in RS epochs (review #14); they are written to `inner`
    /// directly and protected by replication (§10), not parity.
    pub fn write_bootstrap(&mut self, hints: BootstrapHints)
        -> Result<u32, ParityError>;   // returns the bootstrap's tape_file_number

    /// Flush at end of tape: close the final partial epoch per the
    /// final-epoch rule (§11), emit its sidecar, write the final
    /// bootstrap, return the complete filemark map for catalog
    /// recording. Also returns whether the sidecar capacity
    /// reserve was honored (§8 capacity reservation).
    pub fn finish(self) -> Result<TapeGeometry, ParityError>;
}

pub struct ObjectCloseResult {
    pub object_geometry: ObjectGeometry,
    /// Sidecar tape files emitted at this object boundary (epochs
    /// that completed during this object). Each carries its
    /// catalog_tape_files row data. May be empty.
    pub sidecars_emitted: Vec<SidecarTapeFile>,
}

pub struct ObjectGeometry {
    pub tape_file_number: u32,
    pub data_block_count: u64,        // every fixed block in the object tape file
    pub first_parity_data_ordinal: u64,
}

pub struct TapeGeometry {
    pub filemark_map: FilemarkMap,
    pub total_tape_files: u32,
    pub total_data_ordinals: u64,
}

// The BlockSink impl writes OBJECT data blocks within the current
// object. CRITICAL (review #14): only object-archive blocks
// written through this path are assigned ParityDataOrdinal and
// accumulated into RS epochs. Sidecar and bootstrap blocks are
// written by internal helpers that forward to `inner` directly
// and are explicitly NOT given an ordinal and NOT fed into epoch
// accumulation — otherwise sidecar bytes would corrupt the next
// epoch's parity. This separation is an invariant, not an
// implementation detail.
impl<'a> BlockSink for ParitySink<'a> {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, FormatError>;
    fn position(&mut self) -> Result<TapePosition, FormatError>;
    // write_filemarks is NOT called by the body format; filemarks
    // are managed by begin_object/finish_object so 3c controls the
    // epoch/sidecar interaction with object boundaries.
}
```

Key differences from v0.2:
- `begin_object` / `finish_object` bracket each object so 3c
  knows object boundaries (it needs them to place sidecars and
  to assign `tape_file_number` + per-object BodyLba).
- The body format no longer calls `write_filemarks`; 3c writes
  the per-object filemark in `finish_object` — *after* the body
  format has flushed its own final block (review #4).
- `write_block` passes object data through to `inner` unchanged
  (clean pax stream) while accumulating RS state — no inline
  parity insertion.
- Parity and bootstrap tape files are written via internal
  helpers that bypass ordinal assignment and epoch accumulation
  (review #14).
- Parity is emitted as sidecar tape files in `finish_object`
  and `finish`, not interleaved.

### 8.2 `ParitySource`

```rust
pub struct ParitySource<'a> {
    inner: &'a mut dyn BlockSource,
    scheme: ParityScheme,
    tape_uuid: [u8; 16],
    filemark_map: FilemarkMap,             // from catalog or bootstrap
    cache: StripeCache,
    audit_hook: Option<Arc<dyn ParityAuditHook>>,
}

impl<'a> ParitySource<'a> {
    pub fn new(
        inner: &'a mut dyn BlockSource,
        scheme: ParityScheme,
        tape_uuid: [u8; 16],
        filemark_map: FilemarkMap,
    ) -> Result<Self, ParityError>;

    /// Position to (tape_file_number, body_lba) within an object
    /// archive. Resolves via the filemark map to a physical seek.
    /// Within an object there are no parity blocks to skip.
    pub fn locate_in_object(&mut self, tape_file_number: u32, body_lba: u64)
        -> Result<TapePosition, ParityError>;

    /// Forced erasure recovery for the clean-read-but-CRC-failed
    /// case (rem-tar-v1 §13.2). Treats the block at this address
    /// as an erasure: resolve to ParityDataOrdinal, locate stripe
    /// peers (possibly across filemarks in other objects) and the
    /// parity shards in the relevant sidecar, reconstruct.
    pub fn recover_block_at(&mut self, tape_file_number: u32, body_lba: u64)
        -> Result<Vec<u8>, ParityError>;
}

impl<'a> BlockSource for ParitySource<'a> {
    fn read_block(&mut self, buf: &mut [u8]) -> Result<usize, FormatError>;
    fn position(&mut self) -> Result<TapePosition, FormatError>;
    // ... space/seek as before, but operating within an object archive
}
```

Key differences from v0.2:
- Reads within an object archive are pure passthrough (no
  parity blocks interleaved to skip), so a clean read of an
  object's tape file IS a valid tar stream.
- `locate_in_object(tape_file_number, body_lba)` replaces the
  v0.2 `locate(lba)` over a global physical LBA.
- `recover_block_at` (the new forced-erasure API rem-tar-v1
  §13.2 requires) takes the per-object address and does the
  ordinal/stripe/sidecar resolution internally.

---

## 9. Recovery flow (replaces v0.2 §8.3, §9.2)

Suppose object B has a bad data block at `(tape_file_B,
body_lba_x)`, detected either by a SCSI MEDIUM_ERROR or by a
clean-read CRC mismatch (rem-tar-v1 calls `recover_block_at`).

```
1. Map (tape_file_B, body_lba_x) → ParityDataOrdinal O
   (via the filemark map: B's first_parity_data_ordinal + body_lba_x).
2. Compute epoch_id, stripe_index, shard_index from O
   (closed-form in ordinal space).
3. Locate the stripe's other data shards by their ordinals.
   These may live in object A, B, C... across filemarks — the
   filemark map translates each peer ordinal → (tape_file, body_lba)
   → physical position.
4. Locate the epoch's parity shards in the parity-epoch sidecar
   tape file (filemark map: which tape file is this epoch's sidecar).
5. RS-reconstruct the missing block.
6. Return reconstructed bytes.
```

The filemark is irrelevant to the RS math — it only affects the
physical seek path, handled by the filemark map. This is the
core elegance of the ordinal model: parity correctness is
defined entirely in `ParityDataOrdinal` space, and physical
tape structure (filemarks, sidecar placement) is a separate
translation concern.

### 9.1 Final partial epoch (review #11) — normative

At tape close (`ParitySink::finish()`), the current epoch is
almost always partial — it has fewer than `S × k` data shards
because the tape filled or the write session ended mid-epoch.
This is the ordinal-space analogue of v0.2 §5.4's partial-
neighborhood rule. Normative handling:

```
At finish():
1. Let D = number of real data shards accumulated in the
   current partial epoch (D < S × k).
2. Logically zero-pad the epoch to S × k data shards: the
   missing (S × k − D) shards are treated as all-zero blocks
   for the RS computation. They are NOT written to tape — they
   are implicit zeros, known to exist from the recorded count.
3. Compute the S × m parity shards over the (real + implicit-
   zero) data shards.
4. Write the final parity-epoch sidecar tape file. Its header
   records:
     - protected_ordinal_start
     - protected_ordinal_end_exclusive   (= start + D; the
       REAL data ordinals only)
     - logical_shard_count = S × k        (the padded width used
       for the RS math)
     - real_data_shard_count = D
   so a reader knows which shards are real data and which are
   implicit zeros, and can reconstruct correctly by supplying
   zero blocks for the padded positions.
5. Write the final bootstrap copy (§10).
```

A reader recovering a block in the final epoch reconstructs
using the real shards it can read plus implicit-zero shards for
the padded positions, exactly as the writer computed parity.
The `real_data_shard_count` is the authority on where real data
ends; everything at ordinals ≥ `protected_ordinal_end_exclusive`
within the padded width is implicit zero, never on tape.

This makes the final epoch fully protected without writing
zero blocks to tape and without leaving the tail of a tape
unprotected.

---

## 10. Bootstrap interaction and protection (touches v0.2 §5.6, §8.1)

The bootstrap block (v0.2's root of trust at the front of the
tape) gains the **filemark-map digest** so a catalog-less
reader can navigate tape files and perform recovery. The
bootstrap remains its own structure; under the per-object-
filemark model it is naturally its own tape file (or a small
set of blocks before the first object's filemark).

### Bootstrap protection: not parity-protected, but replicated (review #8)

v0.2 said *every* block, including bootstrap, lived in the
uniform parity-protected area. The epoch model breaks that
assumption: `ParityDataOrdinal` counts only object data
records and skips sidecars and bootstrap tape files — so
bootstrap blocks are **not** parity-protected by the epoch
scheme. v0.1 of this revision left that implicit, implying
both protected and unprotected. It must be explicit. Two
options:

```
Option A: Bootstrap tape files are included in ParityDataOrdinal
          and protected like object data.
Option B: Bootstrap tape files are NOT parity-protected, but are
          replicated at fixed/expected positions and validated by
          CRC/hash.
```

**Decision: Option B.** Bootstrap is *not* parity-protected; it
is replicated and CRC/hash-validated instead. Reasoning:

- **Chicken-and-egg.** The bootstrap is what tells a reader the
  parity scheme. You cannot use parity to recover the very
  thing that defines the parity scheme — at discovery time the
  reader hasn't parsed any scheme yet (v0.2 §8.1, §6.3). So
  bootstrap recovery must not depend on parity.
- **Replication is the right tool for a tiny, critical, must-
  find-first structure.** The bootstrap is small (scheme +
  filemark-map digest). The writer places **multiple copies**
  at known fractional tape positions (e.g. BOT, ~1/3, ~2/3,
  near-EOD), each self-validating by CRC/hash. A reader scans
  expected positions for bootstrap magic and takes the first
  copy that validates. Losing some copies to damage is fine as
  long as one validates.
- **Consistent with the discovery model** already in v0.2 §8.1
  (magic-scan at expected positions) — Option B is essentially
  "make that discovery model the protection model too," rather
  than bolting bootstrap into an ordinal stream it logically
  precedes.

So: bootstrap and parity-sidecar tape files are both outside
`ParityDataOrdinal`. Sidecars are protected by the three-copy
archive policy (§7); bootstraps are protected by intra-tape
replication (here). Both are recorded in `catalog_tape_files`
(3b follow-up) and identifiable by magic on a forensic scan.

Bootstrap discovery (v0.2 §8.1, §6.3) is otherwise unchanged: a
catalog-less reader scans for bootstrap magic, parses the
scheme, then constructs a `ParitySource`.

### Catalog-less recovery needs the map, not just a digest (review #12)

A subtlety v0.1 glossed: it said the bootstrap carries a
"filemark-map digest" so a catalog-less reader can navigate and
recover. But a **digest can only validate a map, not provide
one.** If the catalog is gone and the bootstrap holds only a
digest, the reader has nothing to navigate with. Three options:

```
Option A: Bootstrap contains the FULL filemark map (or pointers
          to dedicated map tape files).
Option B: Bootstrap contains only a digest; the reader
          RECONSTRUCTS the map by scanning every tape file,
          identifying object/sidecar/bootstrap files by magic
          and pax headers, then validates the reconstructed map
          against the digest.
Option C: Catalog-less parity recovery is not guaranteed; plain
          tar extraction by filemark still works, but parity
          recovery requires the catalog.
```

**Decision: Option B.** The bootstrap carries a compact digest
(a hash of the canonical filemark map plus the map's entry
count and total tape-file count). For catalog-less recovery the
reader:

```
1. Find and validate a bootstrap copy (scheme + map digest).
2. Scan the tape file by file (mt fsf), classifying each by its
   leading magic / pax global header:
     - object archive  → REMANENCE.format_id global pax header
     - parity sidecar  → "REMPAR01" magic (§5)
     - bootstrap       → bootstrap magic
   recording (tape_file_number, kind, block_count, and for
   objects the data-block count → ordinals; for sidecars the
   protected_ordinal range from the sidecar header).
3. Assemble the reconstructed filemark map.
4. Verify it against the bootstrap's digest. If it matches, the
   map is trustworthy and parity recovery proceeds; if not, the
   reader reports the tape as structurally inconsistent.
```

This keeps bootstraps small (just a digest, replicated cheaply)
while preserving the "the tape itself is self-describing" goal:
every tape file is identifiable by magic, so the map is always
reconstructable by a forensic scan, and the digest proves the
reconstruction is complete and correct. Option A (full map in
bootstrap) is rejected because the map grows with tape-file
count and would bloat every replicated bootstrap copy; Option C
is rejected because losing the catalog should not lose parity
recovery, given the self-describing goal.

The cross-doc reconciliation in rem-tar-v1 §16.15 still holds:
the bootstrap (not a tar index or MAM) is the root of trust;
the per-object manifests + catalog carry the indexes; and the
filemark map is catalogued (`catalog_tape_files`, §3) with the
scan-reconstruct path as the catalog-less fallback.

---

## 11. What stays the same from v0.2

- The Reed-Solomon core math (encoding, erasure reconstruction,
  `ParityScheme`, `RecoveryEvent`, the recovery cache).
- The bootstrap-as-root-of-trust model (§5.6) and discovery
  (§8.1), plus the filemark-map digest + scan-reconstruct
  addition.
- The configuration approach (§11): default/conservative
  schemes, per-tape config, no-parity opt-out — with the
  geometry defaults now block-size-aware (§4).
- The damage model and goals (§1, §2): servo-damage tolerance
  in the 100 MB–2 GB range, which the `S=512, m=4` geometry at
  256 KiB satisfies (~512 MiB).
- The error model (§12), extended with the forced-erasure
  recovery entry point.

## 12. What changes from v0.2 (summary)

| v0.2 | This revision |
|--|--|
| Uniform physical-LBA data area | Per-object tape files + parity sidecar tape files |
| Inline parity interleave (§5.2) | Parity in separate sidecar tape files |
| Closed-form physical LBA→stripe (§5.3) | ParityDataOrdinal→stripe + filemark map |
| No filemarks in data area | Per-object filemarks; epochs span them |
| `S=128` (1 MiB-block assumption) | Block-size-aware; `S=512` at 256 KiB |
| `ParitySink` inline wrapper | `begin_object`/`finish_object` + epoch/spool manager |
| `ParitySource::locate(lba)` | `locate_in_object(tape_file, body_lba)` |
| (no forced-erasure API) | `recover_block_at(tape_file, body_lba)` |
| "No new catalog fields" (§10.1) | Filemark map persisted (catalog + bootstrap digest) |
| Parity flushes implicit | Parity spool for large objects (§6, deferred details) |

## 13. Open questions for the full v0.3 rewrite

Resolved in this proposal (no longer open):
- **Sidecar shard format (§5, review #6)** — per-shard metadata
  in header/index block(s), parity shard blocks are full raw
  `chunk_size` bytes.
- **Bootstrap protection (§10, review #8)** — Option B: not
  parity-protected (chicken-and-egg with scheme discovery);
  replicated at known positions and CRC/hash-validated.
- **Sidecar capacity reservation (§6.1, review #8b)** — writer
  maintains a `required_sidecar_reserve` and refuses to start an
  object that would leave no room for pending parity.
- **Object commit semantics (§6.4, review #9)** — explicit
  state model; an object is successful only at
  `ObjectCatalogCommitted` (tar + filemark + sidecars + catalog
  rows, the last in one transaction).
- **"Pending epoch" disambiguation (§6.3, review #10)** — at
  object close, emit sidecars only for *completed* epochs; never
  flush the current partial epoch merely because an object
  closed.
- **Final partial epoch (§9.1, review #11)** — normative
  zero-pad-to-`S×k`-for-the-RS-math rule; sidecar header records
  `real_data_shard_count` so readers reconstruct with implicit
  zeros.
- **Catalog-less recovery / bootstrap map (§10, review #12)** —
  Option B: bootstrap carries a digest; reader scan-reconstructs
  the map and validates against the digest.
- **Sidecar redundancy (§7, review #13)** — DECIDED: not
  replicated in v1; a damaged sidecar degrades that epoch to
  no-parity on that tape; Layer 5 falls back to another copy.
  Bounded by the three-copy invariant.
- **Sidecar/bootstrap writes bypass epoch accumulation (§6.2,
  §8.1, review #14)** — invariant stated in the API.

Still open for the full v0.3 rewrite:

1. **Parity spool guardrails (§6)** — free-space, cap, crash
   semantics. The shape is fixed (spool parity not data; reserve
   per §6.1); the exact thresholds and config surface are an
   implementation detail to finalize in v0.3.
2. **In-bootstrap digest format detail (§3, §10)** — the exact
   canonical encoding of the filemark-map digest and the
   bootstrap copy count/positions. The mechanism is decided
   (digest + scan-reconstruct); the byte-level format is the
   remaining detail.

---

*End of 3c epoch-revision proposal v0.3. This is a design
decision document, not the full 3c spec rewrite. On approval,
fold these deltas into `layer3c-design.md` as v0.3. Comments
and corrections welcome — please annotate inline.*
