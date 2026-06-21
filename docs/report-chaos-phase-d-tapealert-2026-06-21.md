# Chaos Phase D TapeAlert Implementation Report

Date: 2026-06-21
Author: codex
Source prompt: `docs/prompt-chaos-phase-d.md`
Design: `docs/chaos-phase-d-tapealert-design-v0.1.md`

## Summary

Implemented Phase D: TapeAlert LOG SENSE page 0x2E is now a production
read-only drive capability, and the chaos adapter can model and inject
TapeAlert flags over the real `DriveHandle::read_tape_alerts` path.

## Production Surface

- Added `remanence_scsi::log_sense` with LOG SENSE(10) CDB builders,
  `TapeAlerts`, a flag-name table, length-driven TapeAlert page parsing,
  canonical page synthesis, and defensive unit tests.
- Added `DriveHandle::read_tape_alerts()` with `AuditOp::TapeReadAlerts`,
  `TimeoutClass::TapeStatus`, bounded transfer slicing, and malformed payload
  mapping to `TapeIoError::MalformedResponse`.
- Added `rem tape alerts --bay ... [--config ...] [--library ...]`, emitting
  `rem.tape.alerts.v1` JSON with flag numbers and names. This is CLI-only; no
  daemon or protobuf surface changed.

## Chaos / Model Surface

- Extended `VirtualTape` and `VirtualWorld` with tape-level and drive-level
  TapeAlert state plus `set_tape_alert` / `set_drive_alert` seeders.
- Added `ModelTransport` LOG SENSE 0x2E support. It returns a canonical
  324-byte TapeAlert page containing the union of loaded-tape and drive flags,
  honoring caller allocation length.
- Added `ChaosTransport` post-call TapeAlert injection for `action = {
  "tape_alert": [...] }`. Matching faults OR flags into successful LOG SENSE
  responses, synthesize a clean page for absent/short/malformed responses, and
  preserve parseable existing flags when expanding a noncanonical page.
- Populated the JSONL `tape_alert` event field with the flags injected for the
  call. TapeAlert faults are persistent and do not enter the pre-call
  CHECK CONDITION path.

## Coverage

L1b coverage now drives the real `DriveHandle::read_tape_alerts` over
`ChaosTransport<ModelTransport>`:

- Honest model: clean page, then pre-seeded tape alerts.
- MED-07 tape-scoped flags `[7, 19]`, including JSONL `tape_alert`.
- CLN-01 drive-scoped flag `[20]`.
- HW-04 multi-flag `[58, 16]`.
- Persistence: repeated LOG SENSE calls re-emit the alert.

Additional unit coverage exercises malformed-inner synthesis and the
parse-valid-but-incomplete page expansion path.

## Deferred

Phase D2 remains deferred as designed: RDY-01 first-load optimization,
wear-derived counters, and `time_scale`/delay behavior. Changer-LUN TapeAlert
handling remains Phase E.

## Verification

Targeted Phase D gate passed:

```text
cargo test -p remanence-scsi -p remanence-library -p remanence-chaos
```

Result: 29 `remanence-chaos` tests, 258 `remanence-library` tests, and 120
`remanence-scsi` tests passed; ignored hardware smoke tests stayed ignored.

Final workspace gates:

```text
$ cargo test
Finished `test` profile [unoptimized + debuginfo] target(s) in 0.11s
test result: ok. 25 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 167 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 29 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 104 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 27 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 120 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 101 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
Doc-tests: all crates ok; remanence_library doctests: 0 passed, 2 ignored
```

```text
$ cargo fmt --check
```

No output; exit 0.

```text
$ cargo clippy -- -D warnings
Checking remanence-scsi v0.0.1 (/home/user/remanence/crates/remanence-scsi)
Checking remanence-library v0.0.1 (/home/user/remanence/crates/remanence-library)
Checking remanence-format-driver v0.0.1 (/home/user/remanence/crates/remanence-format-driver)
Checking remanence-parity v0.0.1 (/home/user/remanence/crates/remanence-parity)
Checking remanence-chaos v0.0.1 (/home/user/remanence/crates/remanence-chaos)
Checking remanence-format v0.0.1 (/home/user/remanence/crates/remanence-format)
Checking remanence-bru v0.0.1 (/home/user/remanence/crates/remanence-bru)
Checking remanence-state v0.0.1 (/home/user/remanence/crates/remanence-state)
Checking remanence-stream v0.0.1 (/home/user/remanence/crates/remanence-stream)
Checking remanence-api v0.0.1 (/home/user/remanence/crates/remanence-api)
Checking remanence-cli v0.0.1 (/home/user/remanence/crates/remanence-cli)
Checking remanence-daemon v0.0.1 (/home/user/remanence/crates/remanence-daemon)
Finished `dev` profile [unoptimized + debuginfo] target(s) in 7.47s
```

```text
$ cargo build --release
Finished `release` profile [optimized] target(s) in 44.68s
```
