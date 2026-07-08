# Codex Prompt — Chaos Adapter Phase D (TapeAlert / LOG SENSE 0x2E)

Implement Phase D: a production LOG SENSE page-0x2E TapeAlert reader in rem, plus
the chaos TapeAlert provider + L1b tests. Phases A–C are landed (`qschaos`,
`ChaosTransport`/`FaultEngine`, `ModelTransport`). Do not reimplement them.

## Source of truth
- **Design (read fully): `docs/chaos-phase-d-tapealert-design-v0.1.md`** — this
  prompt implements it; the design wins on any conflict.
- Parent: `docs/chaos-adapter-design.md` (TapeAlert provider; Phase D).
- Flag catalogue: `/home/user/quadstor-chaos/quadstor-chaos.md` (the IBM LTO
  TapeAlert table is the source of truth for flag numbers).
- Phase B/C code: `crates/remanence-chaos/src/lib.rs`, `src/model.rs`.

## Part A — production reader (CLI-only; purely additive)

1. **`crates/remanence-scsi/src/log_sense.rs`** (new; template:
   `read_block_limits.rs`). `OPCODE = 0x4D`, `PAGE_TAPE_ALERT = 0x2E`,
   `build_cdb(page_code, alloc_len) -> [u8;10]` (byte2 = `(0x01<<6)|page_code` =
   `0x6E` for TapeAlert current-cumulative; alloc_len BE bytes 7-8), a
   `TapeAlerts { active: BTreeSet<u8> }` result with `is_set`/`active`/optional
   `flag_name`, and `parse_response(buf) -> Result<TapeAlerts, ScsiError>` that
   walks the 4-byte log header (byte0 low6 == 0x2E, length bytes 2-3 BE) then
   per-parameter entries **honoring each param's length byte** (no fixed-stride
   assumption), bounds-checked, `InvalidResponse` on truncation, never panics.
   Register `pub mod log_sense;` in `lib.rs`. Add unit tests: synth→parse round
   trip + truncated-buffer safety.
2. **`crates/remanence-library/src/handle/tape_io/mod.rs`** —
   `DriveHandle::read_tape_alerts() -> Result<TapeAlerts, TapeIoError>` modeled
   on `position()`: `AuditOp::TapeReadAlerts { bay }` (add the variant),
   `build_cdb`, `set_timeout_for(TimeoutClass::TapeStatus)`, `execute_in` into a
   324-byte buffer, **defensive slice to `bytes_transferred.min(len)`**, parse,
   `map_scsi` on transport error / `MalformedResponse` (or new
   `MalformedLogResponse`) on parse failure, `finish_tape_success`/`error`.
3. **`crates/remanence-cli/src/lib.rs`** — `TapeCommand::Alerts(TapeAlertsArgs)`
   (mirror `TapeInitArgs` config/library/bay args); `run_tape_alerts` handler
   mirroring `run_tape_init_hardware` (open library → open_drive →
   read_tape_alerts → JSON via `serde_json::to_string`, flag numbers + names);
   wire into the dispatch match + `validate_before_discovery`;
   `#[cfg(target_os = "linux")]`-gate the hardware handler.

Part A must be **purely additive** — new module, new method, new verb; do not
change existing read/write/parity paths (Phase C had an avoidable parity edit;
don't repeat that).

## Part B — chaos provider

4. **`ModelTransport` (`src/model.rs`)** — honest device. Add
   `tape_alert_flags: BTreeSet<u8>` to `VirtualTape` and `drive_alert_flags:
   HashMap<u16, BTreeSet<u8>>` to `VirtualWorld` (both default empty). Drive
   `execute_in`: add `Some(0x4d) => self.log_sense(buf)` — lock world, resolve
   the loaded tape for the bay, **union** per-bay drive flags with the loaded
   tape's flags, synthesize the 324-byte page (header `2E 00 01 40` then 64×
   `[flag>>8, flag&0xFF, control, 0x01, value]`), copy `min(page,buf)` honoring
   alloc length. Unloaded bay → drive flags only. Add `set_tape_alert(barcode,
   flag)` / `set_drive_alert(bay, flag)` seeders.
5. **`ChaosTransport`/`FaultEngine` (`src/lib.rs`)** — the new data-in seam:
   - Parse `action = { tape_alert = [..] }` → `Vec<u8>` (new
     `tape_alert_spec_for_fault`, mirroring `mutation_spec_for_fault`).
   - In `execute_in` for opcode 0x4D, **post-call**: if a matching `tape_alert`
     fault is armed (reuse `target.tape`/`target.drive` matching + `op =
     log_sense`), parse the returned page and OR-set the armed flags' value
     bytes; if the inner page is absent/short/malformed, **synthesize a clean
     324-byte page first**, then set flags (so it also works over
     `FixtureTransport`). Respect buffer/alloc length.
   - Populate the JSONL `tape_alert` field (add a `tape_alert` member to
     `CommandEvent`; it's currently hardcoded `Value::Null`).
   - TapeAlert faults are **persistent** (scope `tape`/`drive`), not one-shot —
     re-emit on every LOG SENSE; reuse existing scope machinery.

## Part C — L1b tests (`remanence-chaos`, `#[cfg(target_os = "linux")]`)

Drive the **real** `DriveHandle::read_tape_alerts` over
`ChaosTransport<ModelTransport>` (Phase C handle recipe):
1. **Honest device (chaos off)** — clean model → no flags; pre-seeded
   `set_tape_alert` → exactly those (proves reader↔model page round-trip).
2. **MED-07 tape-scoped** — `tape_alert=[7,19]`, `target.tape=<barcode>` → {7,19}
   + JSONL `tape_alert=[7,19]`, seed, `op=log_sense`.
3. **CLN-01 drive-scoped** — `tape_alert=[20]`, `target.drive` → {20} (fires
   regardless of loaded tape).
4. **HW-04 multi-flag** — `[58,16]` → {58,16}.
5. **Persistence** — a second read still reports the flags.

## Constraints
- No root, no `/dev/sg*`, no QuadStor; `cargo test` hermetic.
- **No daemon/proto change** (CLI-only surface). **No RDY-01, no `time_scale`,
  no wear counters** — Phase D2. Alerts are scenario-declared, not wear-derived.
- Defensive, panic-free page parsing on both sides (medium-sourced input).
- Reuse existing engine pieces (0x4D decode, scope, target matching, JSONL slot);
  add only the action parser, merge path, field population, model handler.
- `cargo fmt --check` + `cargo clippy -p remanence-scsi -p remanence-library
  -p remanence-chaos -- -D warnings` clean; `cargo build --release` (harness
  freshness guard). Doc new `pub` items. Commit per `AGENTS.md` (journal +
  report; a test never silently passes).

## Acceptance (design §8)
- `remanence-scsi::log_sense` build/parse + unit tests (round-trip + truncation).
- `DriveHandle::read_tape_alerts` + `rem tape alerts` JSON output.
- Model answers LOG SENSE 0x2E from alert state; ChaosTransport `tape_alert`
  action ORs scenario flags + populates JSONL `tape_alert`.
- L1b honest/MED-07/CLN-01/HW-04/persistence green via the real reader.
- Gates green (paste counts). Report: families now L1b-proven, the new reader,
  D2 deferral (RDY-01 + counters + time_scale).
