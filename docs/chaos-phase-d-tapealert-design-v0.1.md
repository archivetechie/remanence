# Chaos Adapter Phase D — TapeAlert (LOG SENSE 0x2E) provider — design v0.1

**Status:** design approved in discussion (2026-06-21, owner + claude); codex
implementation pending. Devloop: this doc + `prompt-chaos-phase-d.md` hand off to
codex. Refines `docs/chaos-adapter-design.md` (Phase D, TapeAlert provider) with
code-verified seams. Builds on landed Phase B (`ChaosTransport`/`FaultEngine`)
and Phase C (`ModelTransport`).

## 1. Scope

Phase D adds the **TapeAlert** mechanism — the single provider behind every
alert-driven catalogue family. It has two halves:

**A. A new production capability in rem** (no TapeAlert/LOG SENSE reader exists
today — verified: the only `0x4D` literals in the tree are unrelated). A minimal
**CLI-surfaced** LOG SENSE page 0x2E reader.

**B. The chaos side**: `ModelTransport` answers LOG SENSE 0x2E with a
well-formed page; `ChaosTransport` injects the scenario's TapeAlert flags into
that page on a matching `log_sense` command; L1b tests drive the real reader
end-to-end against injected alerts.

**In:** the production reader; page-0x2E synthesis in the model; a `tape_alert`
action in the fault engine + JSONL population; L1b tests for the alert families
MED-07/10/11, CLN-01/02/03, ENV-01/02/03, HW-01..04, FW-01, ENC-04.

**Out (deliberately):**
- **RDY-01** first-load optimization, `time_scale`/delay enforcement, and the
  wear **counters** (dirtiness, GB-written, mount-count, optimized-flag,
  cleaner-use-count) → **Phase D2**. Phase D alerts are **declared by the
  scenario**, not derived from accumulated wear.
- Daemon/gRPC surface for the reader (CLI only; no proto change).
- Changer-LUN (library) TapeAlerts → defer to Phase E (changer faults).
- LIB-* / sense-tuple faults (already Phase B/E).

## 2. The production reader (real feature, chaos-independent)

A drive's TapeAlert page is genuinely useful health telemetry; chaos just
exercises it. Smallest change set (CLI-only), three crates:

### 2.1 `crates/remanence-scsi/src/log_sense.rs` (new)
Mirror `read_block_limits.rs` / `read_position.rs` (the minimal builder
template: `OPCODE` const → `build_cdb` → result struct → `parse_response` →
tests). Register `pub mod log_sense;` in `lib.rs`.

- `pub const OPCODE: u8 = 0x4D;`
- `pub const PAGE_TAPE_ALERT: u8 = 0x2E;`
- `pub fn build_cdb(page_code: u8, alloc_len: u16) -> [u8; 10]` — LOG SENSE(10):
  byte0 `0x4D`; **byte2 = `(0x01 << 6) | page_code`** (PC=01b current-cumulative
  → `0x6E` for TapeAlert); bytes 7-8 = alloc_len BE; rest 0. (Helper
  `build_tape_alert_cdb(alloc_len)` wrapping it is fine.)
- `pub struct TapeAlerts { active: BTreeSet<u8> }` (flag numbers 1..64) with
  `is_set(flag) -> bool`, `active() -> &BTreeSet<u8>`, and an optional
  `flag_name(flag) -> Option<&'static str>` table from the catalogue (§5) for
  human-readable CLI output.
- `pub fn parse_response(buf: &[u8]) -> Result<TapeAlerts, ScsiError>` — walk the
  4-byte log page header (byte0 low-6-bits must == `0x2E`; page length = bytes
  2-3 BE), then iterate parameters: param code (2 bytes BE = flag number),
  control byte, **param length byte**, then `param_length` value bytes; set the
  flag if the value byte is non-zero. **Honor each parameter's own length byte**
  — do not hard-assume a fixed 5-byte stride (defensive against drives that emit
  the alternate form). Bounds-check every step; return
  `ScsiError::InvalidResponse{..}` on truncation, never panic.

Page wire layout (the synthesizer in §3 and this parser must agree): 4-byte
header `2E 00 <len_hi> <len_lo>` (len = 320 = 0x140), then 64 params, each 5
bytes `[flag>>8, flag&0xFF, control, 0x01, value]`. Total 324 bytes.

### 2.2 `crates/remanence-library/src/handle/tape_io/mod.rs`
`pub fn read_tape_alerts(&mut self) -> Result<TapeAlerts, TapeIoError>` on
`impl DriveHandle`, modeled on `position()`:
1. `let op = AuditOp::TapeReadAlerts { bay: self.bay_address };` (add this
   `AuditOp` variant alongside the `Tape*` family).
2. `let cdb = remanence_scsi::log_sense::build_tape_alert_cdb(324);`
3. `self.fire_tape_started(op, &cdb);` `let started = Instant::now();`
4. `self.transport.set_timeout_for(TimeoutClass::TapeStatus);` (5 s; the right
   class for a fast informational read).
5. `execute_in` into a 324-byte buffer; **defensively slice to
   `outcome.bytes_transferred.min(buf.len())`** before parsing.
6. `parse_response` → on parse failure map to `TapeIoError::MalformedResponse`
   (or a new `MalformedLogResponse(String)` mirroring `MalformedModeResponse`);
   on transport error use `map_scsi(e)`.
7. `finish_tape_success(op, started.elapsed())` / `finish_tape_error(op, &e)`.

### 2.3 `crates/remanence-cli/src/lib.rs`
Add `TapeCommand::Alerts(TapeAlertsArgs)` (the `tape` group already exists with
`Init`/`Retire`). `TapeAlertsArgs` mirrors `TapeInitArgs`' `--config` /
`--library` / target-bay args. Handler `run_tape_alerts` mirrors
`run_tape_init_hardware`: `library.open(policy)` → `handle.open_drive(bay,
policy)` → `drive.read_tape_alerts()` → emit JSON (active flag numbers + names)
via `serde_json::to_string`. Wire into the dispatch `match` and
`validate_before_discovery`; `#[cfg(target_os = "linux")]`-gate the hardware
handler (non-Linux returns the standard error string).

## 3. ModelTransport — answer LOG SENSE 0x2E (honest device)

`ModelTransport` is the truthful device: it returns a **well-formed page
reflecting its own alert state**, with no knowledge of faults (faults are
`ChaosTransport`'s job, §4).

- Add `tape_alert_flags: BTreeSet<u8>` to `VirtualTape` (cartridge/media alerts,
  e.g. media-life) and a per-bay `drive_alert_flags: HashMap<u16, BTreeSet<u8>>`
  to `VirtualWorld` (drive alerts, e.g. cleaning-required, hardware). Both
  default empty.
- Drive `execute_in`: add `Some(0x4d) => self.log_sense(buf)`. The handler locks
  the world, resolves the loaded tape for this bay, **unions** the per-bay drive
  flags with the loaded tape's flags (a real drive reports both its own and the
  loaded cartridge's conditions on one page), synthesizes the 324-byte page (§2.1
  layout) with those flags set, and copies `min(page.len(), buf.len())` bytes
  out (honoring the CDB alloc length). On an unloaded bay, emit the page with
  only drive flags.
- Test seeders: `VirtualWorld::set_tape_alert(barcode, flag)` /
  `set_drive_alert(bay, flag)` for pre-seeded alerts (used by the honest-device
  test and any pre-seeded scenario). The **primary** injection path is
  `ChaosTransport` (§4), so most tests leave the model's flags empty and let the
  scenario drive them.

(Changer-LUN LOG SENSE for library alerts is out of scope — Phase E.)

## 4. ChaosTransport — the `tape_alert` fault (the new data-in seam)

Today `ChaosTransport` only returns CHECK CONDITION or mutates an inner READ
buffer; it never fabricates/edits a data-in payload. Phase D adds a **LOG SENSE
merge**, structurally parallel to the MED-05 post-read mutation:

- **Action parse:** `action = { tape_alert = [7, 19] }` → `Vec<u8>` flag numbers
  (new `tape_alert_spec_for_fault`, mirroring `mutation_spec_for_fault`).
- **Trigger:** the engine already decodes `0x4D → "log_sense"` and matches a
  trigger's `op` verbatim, so `trigger = { op = "log_sense" }` already works.
  Targeting uses the existing `target.tape` / `target.drive` matcher (per-tape
  vs per-drive alerts).
- **Merge (post-call, `execute_in` for opcode 0x4D):** after the inner transport
  returns its page, parse it and **set the value byte to 1 for each armed flag's
  parameter** (OR-in, preserving any flags the device already reported). If the
  inner page is absent/too-short/malformed, **synthesize a clean 324-byte page
  first**, then set the flags — so this also works over `FixtureTransport`
  (L1a-style) and a bare model. Respect the caller's buffer/alloc length.
- **JSONL:** populate the existing-but-always-null `tape_alert` event field with
  the flags set this call; add a `tape_alert` member to `CommandEvent`.
- **Scope:** TapeAlert faults are persistent (`scope = "tape"` / `"drive"`) —
  they re-emit on every LOG SENSE — using the existing scope machinery (no
  one-shot). The `fired_once` path is not used.

This keeps the invariant: **the model is the honest device; ChaosTransport is the
fault engine.** A TapeAlert "fault" is the engine setting bits in the device's
page, exactly as MED-05 is the engine flipping bytes in the device's read.

## 5. Catalogue family → TapeAlert flag numbers (source of truth)

From `quadstor-chaos.md` (IBM LTO TapeAlert table; flag *n* = parameter code
*n*). The scenarios and the L1b assertions use these:

| Family | Flags | Scope |
|--|--|--|
| MED-07 media end-of-life / nearing | 7, 19 | tape |
| MED-10 CM / tape-directory corruption | 15, 18, 51 | tape |
| MED-11 tape system-area r/w failure | 52, 53 | tape |
| CLN-01 clean now | 20 | drive |
| CLN-02 clean periodic | 21 | drive |
| CLN-03 expired cleaning cartridge | 22 | drive |
| ENV-01 drive over-temperature | 36 | drive |
| ENV-02 cooling-fan failure | 26 | drive |
| ENV-03 drive voltage / brown-out | 37 | drive |
| HW-01 hardware A (reset to recover) | 30 | drive |
| HW-02 hardware B (POST failure) | 31 | drive |
| HW-03 predictive failure | 38 | drive |
| HW-04 microcode panic / forced eject | 58, 16 | drive |
| FW-01 firmware download failure | 34 | drive |
| ENC-04 encryption policy violation | 61 | drive |

(Reserved/unused on LTO: 24, 27-29, 40-48, 50, 54, 57, 62-64.) A small
`flag_name()` table in `log_sense.rs` covering at least these makes CLI output
and test failures legible.

## 6. L1b test suite (in `remanence-chaos`, `#[cfg(target_os = "linux")]`)

Drive the **real production reader** (`DriveHandle::read_tape_alerts`) over
`ChaosTransport<ModelTransport>` via the Phase C handle recipe:

1. **Honest device (chaos disabled)** — `read_tape_alerts` on a clean model
   returns no active flags; with a pre-seeded `set_tape_alert`, returns exactly
   those. Proves the production reader ↔ model page round-trips byte-correctly.
2. **MED-07 (tape-scoped)** — arm `tape_alert = [7, 19]`, `target.tape =
   <loaded barcode>`; `read_tape_alerts` returns {7,19}; JSONL event has
   `tape_alert = [7,19]`, the seed, and `op = log_sense`.
3. **CLN-01 (drive-scoped)** — arm `tape_alert = [20]`, `target.drive`;
   returns {20}; proves per-drive targeting (fires regardless of loaded tape).
4. **HW-04 multi-flag** — arm `[58, 16]`; returns {58,16}; proves multi-flag
   merge + the family table.
5. **Persistence/scope** — a second `read_tape_alerts` still reports the flags
   (TapeAlert faults are persistent, not one-shot).
6. **rem-scsi unit test** — `build_cdb`/`parse_response` round trip: synthesize a
   page with a known flag set, parse it back, assert equality; plus a truncated
   buffer returns `InvalidResponse`, no panic.

Plus a CLI smoke test if the existing `tape`-verb test harness supports it
(otherwise the `DriveHandle`-level test is the gate).

## 7. Constraints / gotchas

- **No production behavior change beyond the new reader.** The reader is purely
  additive (new module, new method, new CLI verb); existing paths untouched.
  (Contrast Phase C, where an avoidable parity edit slipped in — keep D's
  production delta to the three additive pieces in §2.)
- **Linux-gated** sense/SCSI paths as in Phase C; the reader's hardware handler
  and the L1b tests are `#[cfg(target_os = "linux")]`.
- **Defensive parsing** on both sides (reader and model/engine page handling):
  honor length fields, bounds-check, no panic on a short/edge buffer — the
  reader is medium-sourced input.
- **Reuse, don't duplicate:** the engine already decodes `0x4D`, has scope +
  targeting (`target.tape`/`target.drive`) + the JSONL `tape_alert` slot. Add
  only the action parser, the merge path, and the field population.
- `missing_docs = warn` on `remanence-chaos` (and rem-scsi) — doc new `pub`
  items.

## 8. Acceptance (Phase D)

- New `remanence-scsi::log_sense` with `build_cdb` + `parse_response` + unit
  tests (round-trip + truncation-safe).
- `DriveHandle::read_tape_alerts()` returns active flags; `rem tape alerts`
  prints them as JSON.
- `ModelTransport` answers LOG SENSE 0x2E with a well-formed page from its alert
  state; `ChaosTransport` `tape_alert` action ORs the scenario flags in and
  populates the JSONL `tape_alert` field.
- L1b: honest-device, MED-07 (tape), CLN-01 (drive), HW-04 (multi-flag),
  persistence — all green, driving the real reader.
- `cargo test -p remanence-scsi -p remanence-library -p remanence-chaos`
  green; `cargo fmt --check` + `cargo clippy -p remanence-scsi -p
  remanence-library -p remanence-chaos -- -D warnings` clean. `cargo build
  --release` (harness freshness). Production untouched except the §2 additions.
- Report: which alert families are now L1b-proven, the new production reader, and
  what's deferred to D2 (RDY-01 + counters + time_scale).
