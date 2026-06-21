# Chaos Phase E Implementation Report - Changer / Library Faults

Date: 2026-06-21

Implemented `docs/prompt-chaos-phase-e.md` against
`docs/chaos-phase-e-changer-faults-design-v0.1.md`.

## Implemented

- Added `accessible` to `DriveBay`, `Slot`, and `IePort`, populated from the
  already-parsed READ ELEMENT STATUS ACCESS/EXCEPT bits as `access && !except`.
  This is intentionally a Rust model/API-internal addition; protobuf projection
  and CLI JSON schemas were not changed.
- Added changer identity to `DeviceCtx` via `library_id`, and taught chaos
  target matching to honor `target.library`. `target.slot` remains a
  READ ELEMENT STATUS mutator selector, not a context match key.
- Added LIB default sense rows and operation gates for the Phase E catalogue:
  LIB-01, LIB-02, LIB-04, LIB-06, LIB-07, LIB-08, LIB-10, and LIB-11.
- Added defensive READ ELEMENT STATUS byte mutators in `remanence-scsi`:
  `blank_element_voltag` and `set_element_exception`. Both walk the RES framing
  with bounds checks and return `false` on malformed/truncated/missing data.
- Added chaos post-call READ ELEMENT STATUS mutation for LIB-05 and LIB-09,
  with JSONL `element_status` summaries.
- Added the LIB-03 pending-unit-attention primitive: after a successful
  MOVE MEDIUM, the next changer command returns SK6 28/00 once.
- Extended `ModelTransport` with slot `full/access/except` state and seeders
  for inaccessible slots and full slots with unreadable volume tags.
- Added Linux L1b coverage over a real `LibraryHandle` /
  `ChaosTransport<ModelTransport>` stack for honest changer behavior, LIB-01,
  LIB-03, LIB-05, LIB-08 on MOVE and RES, LIB-09, and LIB-11.

## Deferrals

Unchanged from the Phase E design: LIB-07 offline state, LIB-10 latency /
`time_scale`, changer-LUN TapeAlert, and nonexistent LIB-12 remain deferred.

## Verification

Focused checks before the full gate:

- `cargo check -p remanence-scsi -p remanence-library -p remanence-chaos --tests`
- `cargo test -p remanence-chaos lib08 -- --nocapture`
- `cargo test -p remanence-chaos -- --nocapture`

Full required gates were run after this report was added; see the commit/final
summary for exact command output.
