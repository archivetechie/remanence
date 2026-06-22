# Chaos Phase F Rem Hook Implementation Report

Date: 2026-06-22
Author: codex
Source prompt: `docs/prompt-chaos-phase-f-rem-hook.md`
Design: `~/system/docs/design-chaos-harness-l2.md` Section 3

## Summary

Implemented the rem-side Phase F runtime hook at the CLI layer. Local hardware
CLI opens can now route `LinuxSgTransport` through
`ChaosTransport<LinuxSgTransport>` when, and only when, both
`REM_CHAOS_ENABLED` and `REM_CHAOS_ALLOW_REAL` are truthy.

No `remanence-library`, `remanence-api`, or daemon code was changed.

## Implemented

- Added `remanence-chaos` as a normal `remanence-cli` dependency.
- Added `remanence_chaos::ENV_CHAOS_ALLOW_REAL` and
  `remanence_chaos::chaos_real_enabled_from_env()` so the real-hardware
  guardrail has one source of truth.
- Added a CLI-local Linux transport factory that preloads `FaultEngine` at
  factory-build time when both gates are set. Invalid or missing
  `REM_CHAOS_STATE` fails before opening SG devices.
- Routed `rem-debug` discovery through `discover_with` and the chaos-aware
  factory; normal `rem` discovery remains the bare `remanence_library::discover`
  path.
- Routed local CLI hardware opens through `Library::open_with` so subsequent
  `LibraryHandle::open_drive` calls inherit the same factory.
- Added minimal L2 `DeviceCtx` population: `backend = "linux"` and `drive_id`
  derived from the SG path (`/dev/sg0` -> `drive1`).

## Tests

Added no-hardware tests for:

- real-hardware chaos requiring both env gates;
- disabled factory construction without `REM_CHAOS_STATE`;
- enabled real-hardware chaos failing before any device open when state is
  absent;
- Linux device context backend and drive alias derivation.

## Deferred

Daemon-path chaos wrapping remains deferred, as designed. Tape-scoped real
hardware targeting by loaded barcode is also deferred; the Phase F rem hook only
provides backend and drive identity in `DeviceCtx`.

## Verification

Focused checks before the full gate:

```text
cargo check -p remanence-cli --tests
cargo test -p remanence-chaos real_hardware_gate_requires_enabled_and_allow_real -- --nocapture
cargo test -p remanence-cli chaos_ -- --nocapture
```

Full required gates were run after this report was added; see the final summary
for exact command output.
