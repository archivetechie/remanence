# Codex Prompt — Chaos Adapter Phase B Only

Implement only Phase B of the QuadStor chaos adapter in this Remanence workspace.
Phase A is already implemented in `/home/user/quadstor-chaos` as the `qschaos`
CLI, SQLite state schema, starter scenarios, and JSONL event tooling. Do not
reimplement Phase A here.

## Source Of Truth

- Design: `docs/chaos-adapter-design.md`
- Phase A producer repo: `/home/user/quadstor-chaos`
- Fault catalogue: `/home/user/quadstor-chaos/quadstor-chaos.md`
- Spike result: `/home/user/quadstor-chaos/SPIKE-RESULT.md`
- Original combined prompt: `/home/user/quadstor-chaos/PROMPT-CHAOS-PHASE-AB.md`

Read the design sections for Components 1, 5, Fault engine details, Fidelity
ladder, and Implementation plan Phase B. The seam choice is settled:
`ChaosTransport<T: SgTransport>` wraps Remanence's existing SCSI transport.

## Deliverables

1. Add a new crate `crates/remanence-chaos` and add it to the workspace members.
2. Implement `ChaosTransport<T: SgTransport>` modeled on
   `RecordingTransport<T>` in `crates/remanence-library/src/transport.rs`.
3. Read the armed scenario from the SQLite file in `REM_CHAOS_STATE`, using the
   Phase A schema written by `qschaos`.
4. Implement the L1a scripted fault set over `FixtureTransport`:
   - `MED-05`: after successful READ, mutate the returned buffer while keeping
     GOOD status.
   - `MED-01`: fixed-format CHECK CONDITION on READ.
   - `EOM-01`: fixed-format CHECK CONDITION with EOM bit/residual on WRITE.
   - `RDY-02`: NOT READY sense on TEST UNIT READY/open-style CDBs.
   - `BUS-01`: unit attention sense.
   - `HOST-01`: deferred fixed-format sense using response code `0x71`.
5. Append one JSONL event per intercepted command to the Phase A event log path
   convention: `<REM_CHAOS_STATE>.events.jsonl`.
6. Forward `set_timeout_for` to the inner transport.
7. Add L1a tests that prove command-level behavior with `FixtureTransport`.

## Constraints

- Do not touch QuadStor, `mainlib`, `/dev/sg*`, or anything requiring root.
- Do not implement `ModelTransport`; that is Phase C.
- Do not implement TapeAlert/LOG SENSE stateful models; that is Phase D.
- Do not implement changer/library fault wiring; that is Phase E.
- Do not use `ScsiError::TransportError` for RESERVATION CONFLICT, BUSY, TASK SET
  FULL, or TASK ABORTED. Recommend the target-status shape in notes, but defer
  implementation of RES-/BUS-status coverage.
- Production must remain byte-identical when chaos is disabled. If adding a
  transport-factory helper, make sure `REM_CHAOS_ENABLED` unset returns the inner
  transport untouched.
- The current worktree may contain unrelated local edits. Stage and commit only
  files created or changed for Phase B.

## Acceptance

- `cargo test -p remanence-chaos` passes with no root, no QuadStor, and no tape.
- Chaos disabled forwarding test: CDB log, buffers, and transfer counts match the
  unwrapped `FixtureTransport`.
- MED-05 test: scripted READ returns mutated bytes, GOOD status, and a JSONL
  event with scenario id, fault id, opcode, LBA, seed, and mutation summary.
- EOM-01 test: fixed-format CHECK CONDITION has response code `0x70`, EOM bit,
  ASC/ASCQ from the scenario, and partial bytes transferred.
- RDY-02/BUS-01/HOST-01 tests use fixed-format CHECK CONDITION and exercise the
  same Remanence-visible `ScsiError::CheckCondition` shape.

