# Chaos Adapter Phase C — `ModelTransport` + L1b hermetic tests — design v0.1

**Status:** implemented in `crates/remanence-chaos` (2026-06-21). Devloop:
this doc + `prompt-chaos-phase-c.md` handed off to codex. Refines
`docs/chaos-adapter-design.md` (Components 2, Phase C, Fidelity ladder L1b)
with code-verified seams. Companion to the landed Phase B
(`crates/remanence-chaos`, `ChaosTransport` + `FaultEngine` + L1a tests).

## 1. Scope

**In:**
- `ModelTransport` — a stateful in-memory virtual tape **and** changer that
  implements `SgTransport`, sized to L1b. Lives in `remanence-chaos`.
- The L1b test suite in `remanence-chaos`: a real Remanence write-object →
  read-object workflow driven through `ChaosTransport<ModelTransport>` over a
  genuine `DriveHandle`/`LibraryHandle`, asserting MED-05, EOM, and RS-recovery
  behavior end to end with no hardware, no root, no QuadStor.

**Out (deliberately):**
- LIB-* changer **fault injection** (Phase E). Phase C models MOVE MEDIUM /
  READ ELEMENT STATUS / LOAD-UNLOAD only enough to load a tape and couple a
  per-tape fault by barcode.
- TapeAlert / LOG SENSE stateful provider (Phase D).
- The `REM_CHAOS_ENABLED` **runtime** injection hook into the daemon/CLI factory
  closures (Phase F / L2). See §7 — designed here, deferred.
- Encryption, reservations, persistent-reservation, and detailed library element
  states beyond load/unload. Add per catalogue-row need later.

## 2. Key architectural finding (reframes what L1b asserts)

A MED-05 silent flip on a **GOOD-status** read is **not** seen by the
Reed-Solomon layer. The object block read (`crates/remanence-parity/src/source.rs`,
`BlockSource::read_block`) is a length-checked passthrough; RS reconstruction +
the sidecar CRC-64/XZ run **only** when a read returns an **erasure** —
`is_erasure` accepts only sense key `0x03` (MEDIUM ERROR) or a transport error
(`source.rs:1127-1135`). A silent flip under GOOD carries no erasure signal, so
by coding theory RS cannot locate it. The flip is caught **downstream by the
end-to-end SHA-256**: the rem-tar manifest anchor
(`FormatError::ManifestDigestMismatch`, `crates/remanence-format/src/manifest.rs`)
or the per-entry `file_sha256`.

**Consequence (decision, 2026-06-21):** L1b asserts MED-05 at the **digest
layer**, and exercises Reed-Solomon with **erasure** faults (MED-01) separately.
We do **not** overclaim that the parity layer catches silent corruption. This is
the honest, architecture-faithful framing (it is correct that GOOD status is
trusted and integrity is an end-to-end digest property; RS erasure-codes against
*detected* loss, not silent flips).

| Scenario | Fault | Detection point | Assertion |
|--|--|--|--|
| Silent corruption | MED-05 (mutate READ buf, GOOD, no sense) | format digest | `FormatError::ManifestDigestMismatch` (or `file_sha256` mismatch) |
| Recoverable media error | MED-01 (SK 0x03) ≤ m shards | RS recovery | `RecoveryEvent{outcome: Recovered}` via `ParityAuditHook` |
| Data loss beyond tolerance | MED-01 > m shards | RS recovery | `ParityError::Unrecoverable{lost_count, limit}` + `RecoveryEvent{Unrecoverable}` |
| Reconstruction-integrity guard | MED-01 erasure on shard X + MED-05 on a peer shard | sidecar CRC-64 | recovered shard CRC mismatch → `ParityError::Unrecoverable` (`recovery.rs:418-426`) |

## 3. No `remanence-library` change is needed for Phase C

Verified: `remanence-chaos` already depends on `remanence-library` and can build
the full handle stack over **public** API:

- `Library::from_captures(captures)` / `Library::new(serials)` — `pub`
  (`model.rs:294,365`), re-exported (`lib.rs:49-50`). Synthesizes a `Library`
  value (slots + drive bays + serials) with no hardware.
- `Library::open_with<F>(&policy, factory)` — `pub` (`handle/mod.rs:1695`, **not**
  `#[cfg(test)]`; the `#[cfg(test)]` at `:1803` is a later module). `F: FnMut(&Path)
  -> Result<Box<dyn SgTransport>, IoErrorKind> + 'static`. This is the exact seam
  the in-crate handle tests use; an external crate calls it identically.
- `LibraryHandle::open_drive(bay, &policy)` — `pub` (`handle/mod.rs:805`); reuses
  the factory from `open_with`.
- `AccessPolicy` + `StaticAllowlist`, `IoErrorKind`, `DeviceCaptures` — all `pub`,
  re-exported.

**Recipe the L1b harness uses (no library edit):**

```rust
let library = Library::from_captures(captures);          // models 1 changer + 1 drive bay, known serials
let world = Arc::new(Mutex::new(VirtualWorld::new(...))); // shared tape/changer state
let engine = FaultEngine::from_state_path(state_db)?;     // Phase A SQLite, armed by qschaos
let handle = library.open_with(&policy, {
    let world = world.clone(); let engine = engine.clone();
    move |path| {
        let role = role_for_path(path);                   // changer vs drive bay, from the Library layout
        let model = ModelTransport::new(world.clone(), role);
        let ctx = device_ctx_for(&world, role);           // sets DeviceCtx.barcode from bay->barcode at open time
        Ok(Box::new(ChaosTransport::new(model, engine.clone(), ctx)) as Box<dyn SgTransport>)
    }
})?;
let mut drive = handle.open_drive(bay, &policy)?;         // identity probes answered by the model
```

The handle constructors enforce identity revalidation (INQUIRY + VPD-0x80 must
report the right device type and matching serial); the model answers these from
its configured identity, so the `Library` serials and the `ModelTransport`
identity must agree (test wiring).

## 4. `ModelTransport` architecture

A **shared world**, mirroring how `FaultEngine` is already `Arc<Mutex<…>>`-shared
across the changer and drive `ChaosTransport` instances.

```rust
/// Shared virtual library state; one per L1b world, cloned into each device transport.
pub struct VirtualWorld {
    tapes: HashMap<String /*barcode*/, VirtualTape>,
    slots: Vec<SlotState>,            // address, Option<barcode>
    drive_bays: HashMap<u16 /*bay element addr*/, Option<String /*loaded barcode*/>>,
    changer_serial: String,
    drive_serials: HashMap<u16, String>,
    element_layout: ElementLayout,    // address ranges for transport/storage/ie/data-transfer
}

pub struct VirtualTape {
    records: Vec<Record>,             // a Record is one written block OR a filemark
    capacity_bytes: u64,              // virtual EOM threshold (uncompressed accounting)
    written_bytes: u64,
    write_protected: bool,
    worm: bool,
    block_size: u32,                  // set by MODE SELECT / configure
}
enum Record { Block(Vec<u8>), Filemark }

pub enum DeviceRole { Changer, Drive { bay: u16 } }

pub struct ModelTransport {
    world: Arc<Mutex<VirtualWorld>>,
    role: DeviceRole,
    position: u64,                    // drive: current record index (per open session)
}
impl SgTransport for ModelTransport { /* dispatch on CDB opcode by role */ }
```

**Why a shared world:** loaded-barcode coupling. A MOVE MEDIUM the *changer*
model sees updates `drive_bays[bay]`; the *drive* model then reads/writes the
tape named there. One `Arc<Mutex<VirtualWorld>>` cloned into both device
transports (the changer transport and each drive transport), exactly as the
shared `FaultEngine` is cloned today.

**Record-oriented, not byte-addressable.** Remanence issues *variable-block*
READ(6)/WRITE(6) (transfer length = byte count), but writes one fixed-size block
per WRITE. So the model stores each WRITE buffer as one `Record::Block` at the
current index; READ returns the record at the index; position is a record index.
This matches the SSC logical-object model and is far simpler than byte addressing.

### 4.1 Drive-role handler table (CDB → behavior)

Encodings confirmed in `crates/remanence-scsi` and `handle/tape_io`. The model
must answer the **inline READ POSITION** that follows every WRITE / LOCATE /
SPACE / WRITE FILEMARKS.

| CDB | Opcode | Behavior |
|--|--|--|
| INQUIRY std | `12 00…` | ≥36-byte LTO reply, device_type `0x01` (SequentialAccess). Reuse a captured fixture or a small builder. |
| INQUIRY VPD 0x80 | `12 01 80…` | 4-byte header + ASCII serial = `drive_serials[bay]`. |
| READ BLOCK LIMITS | `05…` | 6-byte reply; bytes 1-3 BE = max block (use a large value), 4-5 = min. |
| MODE SENSE(6) p0x0F | `1A 00 0F…` | 28-byte reply; Block Descriptor Length = 8, block length (bytes 9-11) = tape block_size, page 0x0F with DCE bit per compression state. |
| MODE SELECT(6) | `15 10…` | Parse compression param list; record block_size if the descriptor sets it. |
| WRITE(6) variable | `0A 00 LLL 00` | Append `Record::Block(buf[..len])` at position; advance; `written_bytes += len`; if `written_bytes > capacity_bytes` return EOM (see §4.3). Then answer the inline READ POSITION. |
| READ(6) variable | `08 00 LLL 00` | Return the record at position into `buf`. If it is a `Filemark` → FILEMARK sense (fixed-format, byte2 bit7, key 0, ASC/ASCQ `00/01`). If past EOD → BLANK CHECK / no-data. Advance on data. |
| WRITE FILEMARKS(6) | `10 00 CCC 00` | Append N `Record::Filemark`; advance; inline READ POSITION. |
| SPACE(6/16) | `11 0X ccc…` / `91…` | Move position by `count` of `code` (filemarks/records, signed two's-complement). On early stop at BOT/EOD return the SPACE residual sense (`space_residual_if_early_stop` shape). Inline READ POSITION. |
| LOCATE(16) | `92 00…bbbbbbbb…` | Set position = LBA (record index). Inline READ POSITION. |
| READ POSITION long | `34 06…` | 32-byte reply; byte0 BOP/EOP bits, partition (bytes 4-7), LBA (bytes 8-15) = position. |
| LOAD / UNLOAD | `1B…` | LOAD: mark bay's tape mounted/at BOT. UNLOAD: clear. |
| REWIND | `01…` | position = 0. |

### 4.2 Changer-role handler table

| CDB | Opcode | Behavior |
|--|--|--|
| INQUIRY std | `12 00…` | device_type `0x08` (MediumChanger). |
| INQUIRY VPD 0x80 | `12 01 80…` | serial = `changer_serial`. |
| READ ELEMENT STATUS | `B8…` | Two-phase: answer the 8-byte header probe, then the full element pages. Emit storage pages (FULL + PVOLTAG barcode for occupied slots; FULL=0 empty) and one data-transfer page per drive (FULL + PVOLTAG + SVALID source when loaded; DVCID = drive serial). Exact shape per `read_element_status.rs` parse rules (header `num_elements`/`byte_count` must be exact; page `byte_count` an exact multiple of `desc_len`; 36-byte voltag block). |
| MOVE MEDIUM | `A5…` | Move barcode src→dst; if dst is a drive bay, set `drive_bays[bay]=Some(barcode)` and clear the source slot — mirroring `ops::apply_planned_move` (`crates/remanence-library/src/ops.rs:43-68`). No data phase. |

### 4.3 Boundary sense shapes (must match the consumers exactly)

Fixed-format only (response code `0x70`/`0x71`); byte2 bits FILEMARK(7)/EOM(6)/
ILI(5), VALID byte0 bit7, INFORMATION bytes 3-6 signed BE, additional length
byte7 = 24, ASC byte12, ASCQ byte13. Returned via
`ScsiError::CheckCondition { sense, bytes_transferred }` (Linux-gated — see §6).

- **EOM on WRITE** (`write_eom_signal`, `tape_io/mod.rs`): EOM bit set, key `0x00`
  → early-warning (success-with-EW); key `0x0D` → end-of-medium. Residual in
  INFORMATION. `write_block` turns this into `Ok(WriteOutcome{early_warning})`.
- **FILEMARK on READ** (`read_filemark_signal`): VALID, FILEMARK bit, ILI clear,
  key `0x00`, ASC/ASCQ `00/01`.
- **MED-01 erasure**: MEDIUM ERROR key `0x03`, ASC/ASCQ `11/00` — this is what
  `is_erasure` accepts to trigger RS recovery. (MED-01 is emitted by
  `ChaosTransport`, not the model; the model just needs to not mask it.)

### 4.4 Discovery responses — reuse, don't hand-fabricate

The hidden cost is the open-time identity + `read_config` bytes. Reuse the
per-opcode fixture builders the handle tests already use
(`crates/remanence-library/src/handle/tests.rs`: `vpd80_response`,
`*_inquiry_response`/`lto9_inquiry`, `rbl_response`, `mode_sense_response`,
`rp_long_response`). The model can call equivalents or embed the same byte
shapes. There is **no** monolithic discovery generator — one helper per opcode.

### 4.5 Scope guard

`ModelTransport` is a test double sized to L1b, **not** a conformant SSC/SMC
target. If it starts drifting toward "reimplement QuadStor," stop — that is what
L2 (`mainlib`) is for. Add a CDB handler only when a Phase-C catalogue row needs
it.

## 5. L1b test suite (in `remanence-chaos`, staged drive-first)

All hermetic: no root, no `/dev/sg*`, no QuadStor. Each uses the §3 recipe to
build a `DriveHandle` over `ChaosTransport<ModelTransport>`, then drives the real
parity/format path: write via `DriveHandleRawSink` → `ParitySink::new_sidecar_only`
→ `write_bootstrap` → `begin_object…` → `write_block×N` → `finish_object` →
`finish`; read via `DriveHandleRawSource` → `ObjectParitySource::open(…,
OpenTrust::RequireValidated)` (+ `read_object_payload` with the manifest anchor
for digest detection, and a `ParityAuditHook` for recovery events).

1. **Faithful-device round trip (chaos disabled).** Write an object, read it
   back; bytes identical; CDB-level sanity (position advances, filemarks land).
   Proves the model is a correct device before any fault rides on it.
2. **MED-05 silent corruption** (pre-seeded loaded tape, drive-only). Arm MED-05
   on the object body READ; assert the read pipeline returns
   `FormatError::ManifestDigestMismatch` (or `file_sha256` mismatch), and the
   JSONL event records seed/LBA/mutation summary. Marquee.
3. **EOM early-warning.** Set a small `capacity_bytes`; write past it; assert the
   EOM bit flows through the real fixed-format sense path to
   success-with-early-warning.
4. **MED-01 RS recovery.** Arm MED-01 erasure on ≤ m shards → assert
   `RecoveryEvent{Recovered}`; on > m shards → assert
   `ParityError::Unrecoverable{lost_count, limit}` + `RecoveryEvent{Unrecoverable}`.
5. **Reconstruction-integrity guard (combined).** MED-01 erasure on shard X +
   MED-05 on a peer shard used for reconstruction → sidecar CRC-64 mismatch →
   `ParityError::Unrecoverable` (`recovery.rs:418-426`).
6. **Changer coupling.** Pre-seed a tape in a slot; `LibraryHandle::load(slot,
   bay)` (MOVE MEDIUM → open_drive → drive LOAD); confirm `drive_bays[bay]`
   barcode; arm a per-tape MED-05 by `target.tape = barcode`; write/read and
   assert the fault bound to the loaded tape (proving the bay→barcode coupling).

## 6. Constraints / gotchas for the implementer

- **Linux-gated error variants.** `ScsiError::CheckCondition` / `TransportError`
  are `#[cfg(target_os = "linux")]` (`crates/remanence-scsi/src/error.rs`). All
  sense-bearing model behavior and the L1b suite are therefore Linux-only, like
  the rest of the SCSI path — gate the model's sense synthesis and these tests
  `#[cfg(target_os = "linux")]`. Portable shapes (`InvalidInput`, `Truncated`)
  need no gate.
- **`SgTransport: Send`.** `ModelTransport` must be `Send` — `Arc<Mutex<…>>`
  satisfies it. There's a blanket `impl SgTransport for Box<dyn SgTransport>`,
  so boxing composes.
- **Inline READ POSITION after every mutating op** — easy to miss; the handle
  asserts CDB orders like `[0x0A, 0x34]`, `[0x92, 0x34]`.
- **Identity must agree** between the `Library` model serials and the
  `ModelTransport` identity, or `open_with`/`open_drive` fail with
  `IdentityChanged`.
- **One `FaultEngine` + one `VirtualWorld`** shared across the changer and drive
  transports (separate `ChaosTransport` instances). Don't construct independent
  copies.
- **`missing_docs = warn`** is on for the crate — doc every new `pub` item.

## 7. Optional `remanence-library` runtime hook (designed, DEFERRED to Phase F)

Phase C needs no library change (§3). The only future need is a **runtime**
`REM_CHAOS_ENABLED` path so the daemon/CLI (not just tests) route real opens
through `ChaosTransport` over `LinuxSgTransport` (L2) or `ModelTransport`. Today
the two production factory closures call `LinuxSgTransport::open[_rw]` directly:
`discovery.rs:146-148` and `handle/mod.rs:1682-1686`. The minimal hook wraps
those results:

```rust
// inside each production closure, after constructing the LinuxSgTransport:
let inner = LinuxSgTransport::open_rw(path)?;
let boxed = remanence_chaos::maybe_wrap_from_env(inner, ctx)?; // returns inner untouched if chaos off
Ok(boxed)
```

`remanence-chaos::maybe_wrap_from_env` already exists (`lib.rs:644`). This adds a
`remanence-library → remanence-chaos` dependency, so it must be **feature-gated**
(e.g. `chaos-hook`, off by default) to keep production builds and the dependency
graph clean — or, to avoid the back-dependency entirely, expose a
`set_transport_wrapper(Box<dyn Fn(...)->...>)` injection point in
`remanence-library` that the daemon/CLI populate at startup. Decide at Phase F;
**not part of Phase C.** Recorded here so the seam is known.

## 8. Acceptance (Phase C)

- `cargo test -p remanence-chaos` passes with no root, no QuadStor, no tape.
- Chaos disabled ⇒ write/read round trip through `ChaosTransport<ModelTransport>`
  returns original bytes; CDB log shows a coherent SSC sequence.
- MED-05 ⇒ `FormatError::ManifestDigestMismatch` + JSONL event (seed, LBA,
  mutation summary).
- EOM ⇒ fixed-format CHECK CONDITION with EOM bit/residual maps to
  success-with-early-warning through the real sense path.
- MED-01 ⇒ `Recovered` (≤ m) and `Unrecoverable` (> m) via `ParityAuditHook`;
  combined MED-01+MED-05 ⇒ CRC-64 `Unrecoverable`.
- Changer coupling ⇒ `load` binds bay→barcode; a per-tape fault fires on the
  loaded tape.
- `cargo fmt --check`, `cargo clippy -p remanence-chaos -- -D warnings` clean.
- No `remanence-library` change (the §7 hook is not landed in this phase).
