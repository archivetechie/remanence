# Codex Prompt — Chaos Adapter Phase C (`ModelTransport` + L1b)

Implement Phase C of the QuadStor chaos adapter in the Remanence workspace:
a stateful virtual tape/changer (`ModelTransport`) and the L1b hermetic test
suite. Phase A (`qschaos` CLI, SQLite schema, JSONL tooling) lives in
`/home/user/quadstor-chaos`; Phase B (`ChaosTransport`, `FaultEngine`, L1a
tests) is landed in `crates/remanence-chaos`. Do not reimplement A or B.

## Source of truth
- **Design (read fully): `docs/chaos-phase-c-modeltransport-design-v0.1.md`** —
  this prompt implements it; the design wins over this prompt on any conflict.
- Parent design: `docs/chaos-adapter-design.md` (Component 2, Phase C, L1b).
- Phase B code: `crates/remanence-chaos/src/lib.rs` (`ChaosTransport`,
  `FaultEngine`, `DeviceCtx`, `maybe_wrap_from_env`).
- Fault catalogue: `/home/user/quadstor-chaos/quadstor-chaos.md`.

## What to build

### 1. `ModelTransport` (in `crates/remanence-chaos`, a new `pub mod model`)
A stateful `SgTransport` over a shared `Arc<Mutex<VirtualWorld>>`, per design §4.
- `VirtualWorld` (tapes, slots, drive_bays bay→loaded-barcode, changer/drive
  serials, element layout) cloned into each per-device transport.
- `VirtualTape` = record list (`Record::Block(Vec<u8>)` | `Record::Filemark`) +
  `capacity_bytes`, `written_bytes`, WP/WORM flags, block_size. Record-oriented
  (one WRITE(6) buffer = one block record); position is a record index.
- `DeviceRole { Changer, Drive { bay } }`; dispatch each `execute_*` on CDB
  opcode by role.
- **Drive handlers** (design §4.1): INQUIRY std + VPD 0x80 (identity), READ BLOCK
  LIMITS, MODE SENSE(6) p0x0F, MODE SELECT(6), WRITE(6)/READ(6) variable, WRITE
  FILEMARKS, SPACE(6/16), LOCATE(16), READ POSITION long, LOAD/UNLOAD, REWIND.
  **Answer the inline READ POSITION (0x34) that follows every WRITE / LOCATE /
  SPACE / WRITE FILEMARKS** — the handle asserts CDB orders like `[0x0A,0x34]`.
- **Changer handlers** (design §4.2): INQUIRY std + VPD 0x80, READ ELEMENT STATUS
  (two-phase header probe then full element pages with PVOLTAG barcodes + drive
  DVCID, exact shape per `crates/remanence-scsi/src/read_element_status.rs`),
  MOVE MEDIUM (move barcode src→dst; if dst is a drive bay set
  `drive_bays[bay]`, clear source — mirror `crates/remanence-library/src/ops.rs`
  `apply_planned_move`).
- **Boundary sense** (design §4.3): fixed-format `0x70/0x71`, byte2 bits
  FILEMARK/EOM/ILI, VALID byte0 bit7, signed INFORMATION bytes 3-6, addl-len
  byte7=24. EOM on WRITE when `written_bytes > capacity_bytes` (key 0x00
  early-warning / 0x0D overflow). FILEMARK on READ (key 0, ASC/ASCQ 00/01).
- **Reuse discovery byte shapes** from the handle-test helpers
  (`crates/remanence-library/src/handle/tests.rs`: `vpd80_response`,
  `lto9_inquiry`/`*_inquiry_response`, `rbl_response`, `mode_sense_response`,
  `rp_long_response`) — do not hand-fabricate (design §4.4). If those helpers are
  `#[cfg(test)]`-private, copy the byte-shape logic into the model (cite the
  source); do not weaken library visibility for this.
- Keep it bounded (design §4.5): no encryption/reservations/extra library
  states. `#[cfg(target_os = "linux")]`-gate any `ScsiError::CheckCondition`
  synthesis (design §6); `ModelTransport` must be `Send`.

### 2. L1b test suite (`crates/remanence-chaos`, `#[cfg(target_os = "linux")]`)
Build `DriveHandle`/`LibraryHandle` over `ChaosTransport<ModelTransport>` using
**only public `remanence-library` API** — `Library::from_captures` →
`open_with(&policy, factory)` → `open_drive(bay, &policy)` (design §3 recipe).
A small chaos-crate test helper may wrap this; **no `remanence-library` change.**
Drive the real parity/format path (design §5): write via `DriveHandleRawSink` +
`ParitySink::new_sidecar_only` (`write_bootstrap` → `begin_object…` →
`write_block×N` → `finish_object` → `finish`); read via `DriveHandleRawSource` +
`ObjectParitySource::open(…, OpenTrust::RequireValidated)`, using
`read_object_payload` with the manifest anchor for digest detection and a
`ParityAuditHook` for recovery events.

Scenarios (design §5, staged drive-first):
1. **Faithful round trip, chaos disabled** — write→read returns identical bytes;
   position/filemark sanity.
2. **MED-05** (pre-seeded loaded tape) → assert `FormatError::ManifestDigestMismatch`
   (or `file_sha256` mismatch) + a JSONL event with seed/LBA/mutation summary.
   **Detection is at the digest layer, NOT the parity layer** (design §2) — do
   not assert RS catches a GOOD-status flip.
3. **EOM** — small `capacity_bytes`, write past it → EOM bit through the real
   fixed-format sense path → success-with-early-warning.
4. **MED-01 RS recovery** — ≤ m shards erased → `RecoveryEvent{Recovered}`;
   > m → `ParityError::Unrecoverable{lost_count, limit}` + `RecoveryEvent{Unrecoverable}`.
5. **Combined** — MED-01 erasure on shard X + MED-05 on a peer shard →
   sidecar CRC-64 mismatch → `ParityError::Unrecoverable` (`recovery.rs:418-426`).
6. **Changer coupling** — pre-seed a slot tape, `LibraryHandle::load(slot, bay)`,
   confirm `drive_bays[bay]` barcode, arm a per-tape MED-05 by `target.tape =
   barcode`, write/read, assert the fault bound to the loaded tape.

## Constraints
- No root, no `/dev/sg*`, no QuadStor, no real tape. `cargo test -p
  remanence-chaos` must pass hermetically.
- **No `remanence-library` change.** The runtime `REM_CHAOS_ENABLED` factory hook
  (design §7) is Phase F — do not implement it here.
- Production builds unchanged; `ModelTransport` is test/chaos-only, never on a
  production path.
- Reuse Phase B (`ChaosTransport`, `FaultEngine`, `DeviceCtx`, the MED-05
  mutation + sense synthesis already there) — do not duplicate the fault engine.
- `cargo fmt --check` clean; `cargo clippy -p remanence-chaos -- -D warnings`
  clean; doc every new `pub` item (`missing_docs = warn`).
- Commit only Phase C files. Follow `AGENTS.md` (journal + report; a test never
  silently passes).

## Acceptance (design §8)
- Hermetic `cargo test -p remanence-chaos` green; fmt + clippy clean.
- Chaos-disabled round trip returns original bytes; coherent SSC CDB sequence.
- MED-05 ⇒ `ManifestDigestMismatch` + JSONL event.
- EOM ⇒ fixed-format EOM/residual ⇒ success-with-early-warning.
- MED-01 ⇒ `Recovered` (≤ m) and `Unrecoverable` (> m) via `ParityAuditHook`;
  combined ⇒ CRC-64 `Unrecoverable`.
- Changer coupling ⇒ `load` binds bay→barcode; per-tape fault fires on the
  loaded tape.
- Report: which catalogue rows are now L1b-proven, the MED-05 digest-vs-parity
  finding confirmed in code, and anything deferred.
