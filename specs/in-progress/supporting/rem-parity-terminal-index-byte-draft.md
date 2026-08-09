# REM-PARITY terminal index byte draft

**Status:** in-progress byte candidate for independent review; not frozen.
**Date:** 2026-08-09.
**Architecture:** terminal triple-index replacement with three complete final
replicas and two typed separation extents.
**Implementation anchors:** `terminal_tail.rs`, `tape_index_replica.rs`, and
`index_separation.rs` in `crates/remanence-parity/src/`.

This file defines the candidate bytes for the draft replacement architecture.
The REM-PARITY specification is not frozen, so this is a clean replacement. It
does not preserve the former checkpoint-bootstrap or adjacent `2M+1` snapshot
wire merely for compatibility.

The sole BOT Bootstrap is Object-count independent and contains no Object
recovery rows, including in the no-parity profile. Checkpoint operations do not
write a Bootstrap. Complete Object rows exist only in the streamed A/B/C
replica payloads.

## 1. Terminal grammar

After the final pre-tail parity closeout, a normally finalized tape has exactly:

```text
TapeIndexReplica A       filemark
IndexSeparationExtent AB filemark
TapeIndexReplica B       filemark
IndexSeparationExtent BC filemark
TapeIndexReplica C       filemark
EOD
```

The payload in A, B, and C describes the complete prefix before A. It excludes
all five terminal files. The three payloads are identical. Replica-local
headers and footers differ because their ordinal and location differ.

Unless a field is explicitly inside deterministic CBOR, every fixed-width
integer below is unsigned little-endian; the two-byte slot-length prefix is
also little-endian. Deterministic CBOR uses its own canonical big-endian
argument encoding. All arithmetic is checked in `u64`. Terminal writers,
readers, capacity profiles, and replacement-draft vectors support fixed record
sizes of 256 KiB, 512 KiB, and 1 MiB. An out-of-band read hint chooses one of
those sizes; it does not extend the terminal-tail grammar. The separate BOT
structural walk may still measure a nonconformant or historical tape at another
hinted size without claiming that it decoded a terminal tail. The draft profile
uses partition zero and requires hardware compression disabled.

## 2. Planned terminal layout

The planned layout contains:

- `partition: u32`;
- `block_size: u32`;
- exactly five ordered component tuples; and
- `expected_eod_lba: u64`.

Each component tuple is exactly 32 bytes:

| Offset | Size | Field |
| ---: | ---: | --- |
| `0x00` | 2 | structural kind: `4` replica or `5` separation |
| `0x02` | 2 | one-based ordinal within its kind |
| `0x04` | 4 | trailing filemark count, exactly `1` |
| `0x08` | 8 | planned dense tape-file number |
| `0x10` | 8 | planned logical start LBA |
| `0x18` | 8 | data-record count before the filemark |

The order and ordinals are fixed: `(4,1), (5,1), (4,2), (5,2), (4,3)`.
Tape-file numbers are dense. Logical start positions advance by
`record_count + 1`, where the `1` is the actual trailing filemark. EOD is the
start of C plus C's record count plus one filemark.

The kind, replica/separation ordinal, replica/separation count, component
count, and trailing-filemark count are **bounded topology discriminators**, not
media-size counters. Their canonical on-media widths are the `u16`/`u32`
widths shown here; Rust retains those bounded widths, protobuf transports them
in `uint32`, and CLI JSON renders them as ordinary JSON integers. This is an
explicit exception to the `u64` rule for scale-derived counts and positions.
Tape-file numbers, LBAs, record counts, payload lengths, structural/Object row
counts, and data/protected ordinals remain `u64` end to end (and use decimal
strings on JSON surfaces whose integer-safety contract requires them).

Each 32-byte component tuple is little-endian:

```text
kind:u16 || ordinal:u16 || trailing_filemark_count:u32=1 ||
planned_tape_file_number:u64 || planned_start_lba:u64 || record_count:u64
```

The layout digest is:

```text
SHA256(
  "REM-TERMINAL-TAIL-LAYOUT-V1\0" ||
  partition:u32 || block_size:u32 || component_count:u16=5 ||
  five 32-byte component tuples || expected_eod_lba:u64
)
```

This digest contains only planned on-media facts. A conservative filemark
capacity charge is not an on-media location and is deliberately excluded.

## 3. TapeIndexReplica

### 3.1 Record geometry

Let `S` be structural rows, `R` be Object recovery rows, and `B` be the fixed
record size:

```text
payload_len            = 64*S + 256*R
payload_record_count   = ceil(payload_len / B)
payload_padding_bytes  = payload_record_count*B - payload_len
footer_block_offset    = 1 + payload_record_count
replica_record_count   = 2 + payload_record_count
```

One replica tape file is:

```text
one full header record
payload_record_count full payload records
one full BootstrapFooter record
one trailing filemark
```

The header never shares a record with payload bytes. `BootstrapFooter` is the
inner footer of structural kind 4; it is not a structural kind-2 Bootstrap and
there is no singular bootstrap after C.

The payload is all 64-byte structural slots in tape-file order followed by all
256-byte Object recovery-row slots in matching Object order. A slot is:

```text
encoded_len:u16 || deterministic CBOR || zero padding to the fixed slot size
```

The existing dense-map, scope, ordinal-range, deterministic-CBOR, and exact
map↔Object-row bijection rules apply. Structural kind `4` means
`TapeIndexReplica` and kind `5` means `IndexSeparationExtent`; because the
payload describes only the prefix before A, either terminal kind inside a
replica payload is invalid. A final edition is never an empty geometry-only
profile: structural row 0 must be the kind-2 BOT Bootstrap required by the tape
grammar. For this replacement profile, the reused CBOR row schema decodes
structural tape-file numbers, block counts, data/protected ordinals, epoch IDs,
Object tape-file numbers, stored-block counts, and plaintext manifest
positions/sizes/counts as `u64`. This supersedes the older draft's `u32`
structural file-number wording; the encrypted representation's bounded
key-frame length retains its REM-ENCRYPT `u32` semantic constraint. The payload
digest is:

```text
SHA256("REM-TAPE-INDEX-REPLICA-PAYLOAD-V1\0" || every complete fixed slot)
```

The canonical-map digest remains SHA-256 over the existing canonical CBOR map
projection.

### 3.2 Header and footer common frame

The header and footer each occupy one complete tape record. Their meaningful
frame is the first `0x400` bytes; bytes from `0x400` through the end of the
record are zero. The CRC is CRC-64/XZ over bytes `0x000..0x3F8`.

| Offset | Size | Field |
| ---: | ---: | --- |
| `0x000` | 8 | role magic |
| `0x008` | 2 | schema version `1` |
| `0x00A` | 2 | role: header `1`, footer `2` |
| `0x00C` | 4 | flags: FINAL bit 0 required; other bits zero |
| `0x010` | 16 | tape UUID |
| `0x020` | 16 | nonzero final edition ID |
| `0x030` | 8 | nonzero final edition sequence |
| `0x038` | 2 | replica ordinal `1`, `2`, or `3` |
| `0x03A` | 2 | replica count `3` |
| `0x03C` | 4 | partition `0` |
| `0x040` | 4 | fixed block size |
| `0x044` | 4 | compression mode `0` (disabled) |
| `0x048` | 8 | covered-prefix tape-file count |
| `0x050` | 8 | total data ordinals |
| `0x058` | 8 | highest protected ordinal |
| `0x060` | 8 | structural-row count `S` |
| `0x068` | 8 | Object-row count `R` |
| `0x070` | 8 | payload length |
| `0x078` | 8 | payload record count |
| `0x080` | 8 | replica record count |
| `0x088` | 8 | planned local tape-file number |
| `0x090` | 8 | planned local start LBA |
| `0x098` | 8 | forward footer block offset |
| `0x0A0` | 8 | planned terminal EOD LBA |
| `0x0A8` | 32 | domain-separated payload SHA-256 |
| `0x0C8` | 32 | canonical-map SHA-256 |
| `0x0E8` | 32 | edition digest |
| `0x108` | 32 | terminal-layout digest |
| `0x128` | 32 | replica descriptor digest |
| `0x148` | 160 | five planned 32-byte component tuples |
| `0x1E8` | 8 | reserved zero |
| `0x1F0` | 2 | writer-version byte length |
| `0x1F2` | 2 | write-timestamp byte length |
| `0x1F4` | 4 | reserved zero |
| `0x1F8` | 128 | printable ASCII writer version, then zero padding |
| `0x278` | 64 | valid RFC3339 timestamp, then zero padding |
| `0x2B8` | 32 | footer: complete header-record SHA-256; header: zero |
| `0x2D8` | 8 | footer local observed tape-file number; header: zero |
| `0x2E0` | 8 | footer local observed start LBA; header: zero |
| `0x2E8` | 8 | footer local observed record count; header: zero |
| `0x2F0` | 8 | footer observed footer LBA; header: zero |
| `0x2F8` | 8 | footer backward start delta; header: zero |
| `0x300` | 248 | reserved zero |
| `0x3F8` | 8 | CRC-64/XZ |

Magic derivation is HMAC-SHA256 keyed by the public 16-byte tape UUID,
truncated to its first eight bytes. Labels are exactly:

```text
header: "REM\0TIREP\x01H"
footer: "REM\0TIREP\x01F"
```

These magics classify structures; they do not authenticate them.

### 3.3 Digest domains

The common edition digest is SHA-256 over:

```text
"REM-TAPE-INDEX-EDITION-V1\0"
schema_version:u16
tape_uuid[16]
edition_id[16]
edition_sequence:u64
partition:u32
block_size:u32
compression_mode:u32=0
covered_prefix_tape_file_count:u64
total_data_ordinals:u64
highest_protected_ordinal:u64
structural_row_count:u64
Object_row_count:u64
payload_len:u64
payload_record_count:u64
payload_sha256[32]
canonical_map_sha256[32]
writer_version_len:u64 || writer_version bytes
write_timestamp_len:u64 || write_timestamp bytes
```

The replica-local descriptor digest is SHA-256 over:

```text
"REM-TAPE-INDEX-REPLICA-DESCRIPTOR-V1\0"
edition_digest[32]
terminal_layout_digest[32]
replica_ordinal:u16
replica_count:u16=3
local 32-byte component tuple
footer_block_offset:u64
```

The header and footer repeat the same descriptor digest. A footer therefore
cannot bind another ordinal or location. Cross-replica agreement compares tape,
edition, scope, counts, payload/map digests, block/compression facts, writer
facts, and terminal-layout digest.

## 4. IndexSeparationExtent

### 4.1 Record geometry

The configured nominal extent includes its header and footer. For nominal bytes
`E` and block size `B`:

```text
total_records   = ceil(E / B), required >= 2
footer_offset   = total_records - 1
interior_records = total_records - 2
actual_bytes    = total_records * B
```

The default `E` is exactly 1 GiB. One extent is a full header record, the
zero-filled interior records, a full footer record, and a trailing filemark.
Compression must be proven disabled by the write session. The on-media zero
compression field is an assertion, not that proof.

### 4.2 Header and footer frame

The meaningful frame is `0x200` bytes and the rest of the full record is zero.
CRC-64/XZ covers bytes `0x000..0x1F8`.

| Offset | Size | Field |
| ---: | ---: | --- |
| `0x000` | 8 | role magic |
| `0x008` | 2 | schema version `1` |
| `0x00A` | 2 | role: header `1`, footer `2` |
| `0x00C` | 4 | flags zero |
| `0x010` | 16 | tape UUID |
| `0x020` | 16 | nonzero final edition ID shared with A, B, and C |
| `0x030` | 2 | separation ordinal `1` or `2` |
| `0x032` | 2 | separation count `2` |
| `0x034` | 4 | partition `0` |
| `0x038` | 4 | fixed block size |
| `0x03C` | 4 | compression mode `0` |
| `0x040` | 8 | planned local tape-file number |
| `0x048` | 8 | planned local start LBA |
| `0x050` | 8 | nominal total extent bytes |
| `0x058` | 8 | total record count |
| `0x060` | 8 | footer block offset |
| `0x068` | 8 | zero interior record count |
| `0x070` | 2 | predecessor replica ordinal |
| `0x072` | 2 | successor replica ordinal |
| `0x074` | 4 | fill kind `0` (all-zero interior) |
| `0x078` | 8 | predecessor tape-file number |
| `0x080` | 8 | successor tape-file number |
| `0x088` | 8 | predecessor start LBA |
| `0x090` | 8 | successor start LBA |
| `0x098` | 32 | terminal-layout digest |
| `0x0B8` | 32 | separation descriptor digest |
| `0x0D8` | 8 | planned terminal EOD LBA |
| `0x0E0` | 160 | five planned 32-byte component tuples |
| `0x180` | 32 | footer header-record SHA-256; header zero |
| `0x1A0` | 8 | footer local observed tape-file number; header zero |
| `0x1A8` | 8 | footer local observed start LBA; header zero |
| `0x1B0` | 8 | footer local observed record count; header zero |
| `0x1B8` | 8 | footer observed footer LBA; header zero |
| `0x1C0` | 8 | footer backward start delta; header zero |
| `0x1C8` | 48 | reserved zero |
| `0x1F8` | 8 | CRC-64/XZ |

Magic labels are exactly `"REM\0TISEP\x01H"` and
`"REM\0TISEP\x01F"`, HMAC-derived and truncated as for replicas.

The separation descriptor digest is SHA-256 over:

```text
"REM-INDEX-SEPARATION-DESCRIPTOR-V1\0"
tape_uuid[16]
edition_id[16]
separation_ordinal:u16
separation_count:u16=2
partition:u32
block_size:u32
nominal_extent_bytes:u64
total_records:u64
local component tuple
predecessor component tuple
successor component tuple
terminal_layout_digest[32]
```

## 5. Planned versus locally observed values

All layout locations are committed plans calculated before terminal tape
motion. Future entries in A or B do not prove that the later components exist.

Each footer records the writer's local file/start/count observation and the
header hash. A reader locates the frame from the immutable planned tuple,
checks device-reported post-read positions for the addressed records and
trailing filemark, and cross-checks the footer observation against that tuple.
The footer's tape-file/start/count fields are writer observations, not an
independent device tape-file counter. A valid footer still does not prove its trailing filemark,
synchronous barrier, journal fsync, or host projection. Only reconciliation and
the external authority order advance five-component progress.

For each component, that authority order is: media barrier; exact component
and watermark transition in the parity sink journal; progress fsync in the
checkpoint journal; SQLite projection. Restart compares the two journals with
the immutable final-edition plan before any terminal positioning or write. It
may promote only one exact next, barrier-proved sink-journal transition; it
first completes that transition's sink watermark if needed, then advances the
checkpoint journal and rebuilds SQLite. Equality is idempotent. Every other
skip, regression, or byte disagreement fails before media motion.

After replica C follows that same SQLite progress projection, the writer
fsyncs the sealed checkpoint, retires the matching companion intent, and only
then publishes the final SQLite outcome. The sealed-fsync-to-intent-cleanup
window is therefore separately restartable and tested.

The run-to-completion writer performs one final, read-only position assertion
against planned terminal EOD after the fifth barrier. Terminal component
journal bundles use the edition digest as canonical metadata for A/B/C and the
local separation descriptor digest for AB/BC; all five copy the same
edition-scoped protected and total ordinal watermarks.

The header is written before the streamed payload is replayed. A source replay
failure can therefore leave a header-only or otherwise partial component at
its planned start. Restart classifies that state as torn terminal control,
never as an Object: a proved rewritable start may be rewritten from that
component, while WORM media or an unproved start remains RecoveryRequired with
no further motion.

### 5.1. Irreversible lifecycle invariant

The normative lifecycle is:

```text
Open -> Finalizing -> Finalized
                    \
                     -> RecoveryRequired
```

Acceptance of finalization durably enters `Finalizing` at
`BeforeReplicaA` before any terminal media motion. From that transition onward,
Object admission is permanently false. Finalization is not a pause and no
error, restart, operator action, or degraded acceptance may return the tape to
`Open`.

Successful barriers advance only through `AfterReplicaA`,
`AfterSeparationAb`, `AfterReplicaB`, `AfterSeparationBc`, and
`AfterReplicaC`; ordinary `Finalized`/sealed projection requires
`AfterReplicaC` and the required host persistence order.
`Finalizing(AfterReplicaC)` is valid while the sealed checkpoint or final
SQLite projection remains pending; restart completes those host-only steps
without repeating terminal media motion. A matching sealed checkpoint wins
over a stale companion intent left by interrupted cleanup, while a mismatch
fails closed. A failure or completion-unknown outcome enters or retains
`RecoveryRequired`; that classification is durable in the companion intent
and restart cannot clear it at unchanged progress. A successful successor
component clears it, while `AfterReplicaC` may clear it only in the no-media
host completion path after host-authority alignment. Recovery may
reconcile, rewrite, or append only missing terminal control components at a
proved location under the medium's rewrite policy. It MUST NOT write an Object,
remove the finalization fence, or construct a second terminal triple. A
validated partial replica set may be accepted only as a typed
finalized-degraded result, never as `Open`.

## 6. Decoder order

Readers:

1. classify role magic; matching magic plus malformed content is a typed
   control failure, never an Object fallback;
2. validate record length, CRC, version, role, flags, tape UUID, partition,
   block size, compression, ordinals/counts, reserved bytes, and padding;
3. check every size formula with overflow protection before allocation or seek;
4. reconstruct and validate the five-component plan and its digest;
5. recompute edition/local descriptor digests;
6. validate that the footer's backward delta names the same header start as
   the discovered immutable layout, read that planned header coordinate, and
   require the complete header hash and common descriptor to agree;
7. compare declared locations/counts/filemark with device measurements;
8. for a replica, stream-decode every fixed slot and validate the dense map,
   scope, deterministic CBOR, zero padding, exact map↔Object-row bijection,
   payload digest, and canonical-map digest;
9. for a full separation verification, stream-check every interior byte zero.

The replica's pre-A structural map is locally eligible only when all of these
additional relationships hold:

- a `ParityMap` row, when present, is the final structural row;
- a final `ParityMap` is present if and only if at least one
  `ParitySidecar` row is present;
- when sidecars are present, `highest_protected_ordinal` equals
  `total_data_ordinals`, so finalization left no Object ordinal unprotected;
- `covered_prefix_tape_file_count`, `structural_entry_count`, and replica A's
  planned tape-file number are equal.

Fast inventory tries C, then B, then A. Any one independently valid replica is
sufficient to return the complete inventory with degraded-replica evidence. If
all three are invalid, recovery explicitly scans structural files from BOT; it
never returns an empty inventory merely because the terminal index is missing.

## 7. Capacity terms are separate

Logical LBA layout always counts one actual filemark per component. Capacity
uses independent conservative charges:

```text
replica_charge = replica_record_count + replica_filemark_charge
gap_charge     = total_gap_records + gap_filemark_charge

terminal_close = parity_closeout_charge
               + 3*replica_charge
               + 2*gap_charge
               + safety_allowance
```

No header, footer, record, or filemark appears in more than one term.

## 8. Vector profiles

Full default gaps are exercised by VTL and eventual physical-drive scenarios.
Compact byte vectors use an explicitly named test profile with nominal gap
bytes `E = 3*B`, producing one header, one zero interior record, and one footer.
The compact profile changes only the recorded nominal extent and derived count;
it does not change framing or validation rules.

The multi-object candidate prefix includes the one final pre-A ParityMap row
produced by nonempty sidecar closeout. That retained ParityMap is not part of
the five-component terminal layout or a substitute for A/B/C authority.

HMAC-derived magics, CRC-64/XZ, and SHA-256 detect substitution/corruption and
bind self-consistent structures. Because the tape UUID is public, none of them
is a secret-key authenticity claim.
