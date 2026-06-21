# Code review — chaos Phase D (TapeAlert / LOG SENSE 0x2E), 2026-06-21

**Scope:** the implementation of `docs/prompt-chaos-phase-d.md` /
`docs/chaos-phase-d-tapealert-design-v0.1.md` — commit `bc7ca8a` ("Implement
chaos Phase D TapeAlert"). New `remanence-scsi/src/log_sense.rs` (285 lines);
`DriveHandle::read_tape_alerts` + `AuditOp::TapeReadAlerts` (library + the
state-side audit-tag wiring); `rem tape alerts` CLI; `ModelTransport` LOG SENSE
0x2E + alert state; `ChaosTransport` `tape_alert` action; L1b tests.

**Method:** read the production parser (`log_sense.rs`) and the `DriveHandle`
reader wiring line by line; dispatched a parallel review of the chaos merge,
model handler, and L1b tests that verified claims **against the code** (not the
report); independently confirmed the cross-crate audit-op plumbing.

**Gates (green):** `cargo fmt --check` clean; `cargo clippy -p remanence-scsi
-p remanence-library -p remanence-chaos --all-targets -D warnings` clean (and
codex's report shows full-workspace `clippy -- -D warnings` clean +
`build --release`); tests **scsi 120 / library 259 / chaos 29, 0 failed**; full
workspace green per the report.

## Verdict

**Strong, faithful, production-clean — no Critical/High/Medium.** The new
production reader is exactly the additive, three-piece change the design asked
for, and the parser is a model of defensive, panic-free, length-driven decoding
of medium-sourced input. The chaos provider keeps the Phase-C invariant (model =
honest device, ChaosTransport = fault engine) and the L1b suite drives the
**real** `DriveHandle::read_tape_alerts` path, not the model directly. Phase D's
production delta stayed **purely additive** — the Phase-C lesson (an avoidable
parity edit) was heeded: every cross-crate touch is the new `AuditOp` variant
rippling through its required sites, nothing more.

One Low (latent, unreachable on the live path) + nits. Nothing blocks.

## Production reader — clean (no findings)

- `log_sense.rs`: `build_cdb` correct (byte2 `0x6E` = PC 01b | page 0x2E; alloc
  BE; test pins `[0x4d,0,0x6e,0,0,0,0,0x01,0x44,0]`). `parse_response` is
  fully bounds-checked and **length-driven** (honors each parameter's own length
  byte rather than assuming the 5-byte stride), `checked_add` throughout, every
  index guarded by a prior `> total` check, returns `Truncated`/`InvalidResponse`
  on any malformation — **no `unwrap`, no unchecked indexing, no panic path**.
  Records only flags 1..=64. `synthesize_tape_alert_page` / `set_tape_alert_flag`
  share the same fail-closed walk. Tests cover round-trip, alternate param
  length, truncation-no-panic, wrong-page, set-flag.
- `DriveHandle::read_tape_alerts` (`tape_io/mod.rs`): textbook `position()`
  template — `AuditOp::TapeReadAlerts`, `build_tape_alert_cdb(324)`,
  `set_timeout_for(TimeoutClass::TapeStatus)`, **defensive slice to
  `bytes_transferred.min(buf.len())`** before parse, `MalformedResponse` on parse
  failure, `map_scsi` on transport error, `finish_tape_success/error`. Two
  library unit tests (`..._parses_log_sense` pins CDB `0x4d`/`0x6e`/alloc
  `0x0144`; `..._rejects_short_successful_transfer` → `MalformedResponse` and
  not-dirty).
- Cross-crate audit plumbing is **complete, not half-wired**:
  `remanence-state/adapters.rs` adds `Layer2AuditOpTag::TapeReadAlerts` at all
  four sites (enum, `as_str` → `"tape_read_alerts"`, `From<LibraryAuditOp>`, and
  the bay-detail match arm). `remanence-library/lib.rs` re-exports `TapeAlerts` +
  `flag_name` as a public surface. CLI `rem tape alerts` emits
  `rem.tape.alerts.v1` JSON (flag numbers + names), Linux-gated, mirroring
  `tape init`.

## Findings

### L1 (Low, latent — unreachable on the live path) — sub-canonical alloc_len makes the returned page and the reported flags disagree
`crates/remanence-chaos/src/lib.rs` (`merge_tape_alert_flags`). If a caller
requests an allocation length **smaller than 324** *and* an armed flag's
parameter doesn't fall within those bytes, `set_tape_alert_flag` returns false;
the re-synthesis fallback is guarded by `limit >= TAPE_ALERT_RESPONSE_LEN`, so it
is skipped — the flag never lands in `buf`, yet `TapeAlertDecision.flags` still
reports it to the JSONL event and (conceptually) the caller. The returned bytes
and the reported flags disagree. **Not reachable today:** the only production
caller (`read_tape_alerts`) and the model both use `alloc_len = 324`, and all
tests use 324. **Fix (when convenient):** when the re-synth branch is skipped due
to `limit`, intersect the reported `flags` with what actually got written (or
document that a sub-canonical alloc_len truncates). Low because no live path hits
it.

### Nits (no action required)
- **JSONL order vs reader order:** the `tape_alert` event field is logged in
  spec/array order (HW-04 → `[58,16]`) while `TapeAlerts::active()` returns a
  sorted `BTreeSet` (`[16,58]`). Both are internally consistent and the tests
  assert each correctly; anything cross-checking the two must sort first. A
  one-line comment would help. (Design §4 calls the field "flags set this call,"
  so spec-order is defensible.)
- **Duplicated `log_sense_alloc_len` helper** in `lib.rs` and `model.rs` —
  byte-identical; could hoist to `remanence_scsi::log_sense`. Cosmetic.
- **Redundant page-code re-check** after the `operation == "log_sense"` gate —
  harmless defense-in-depth.

## Verified conformant (subagent claims re-checked against code)

- `tape_alert` action parsed (`tape_alert_spec_for_fault`, mirroring
  `mutation_spec_for_fault`); flags validated 1..=64; empty/all-invalid → no
  decision.
- Post-call OR-merge on opcode 0x4D: preserves existing flags; synthesizes a
  clean 324-byte page when the inner page is absent/short/malformed (so it works
  over `FixtureTransport` and a bare model); never writes past `buf`
  (`limit = buf.len().min(alloc_len).min(324)`).
- JSONL `tape_alert` field now populated (was hardcoded `Value::Null`).
- TapeAlert faults are **persistent, not one-shot, not CHECK CONDITION** —
  filtered out of the pre-call CC path, never touch `fired_once`; a pure
  tape_alert fault has no `check_condition` so no sense fires.
- Targeting reuses `target_matches` — `target.tape` (tape_id/barcode) vs
  `target.drive` (drive_id); both exercised.
- Model `log_sense` handler unions per-bay drive flags with the loaded tape's
  flags, synthesizes the canonical page, copies `min(page, buf, alloc_len)`;
  unloaded bay → drive flags only; no panic on a small buffer.
- **L1b: all five scenarios present and correctly wired**, each driving the real
  `DriveHandle::read_tape_alerts` over `ChaosTransport<ModelTransport>`: honest
  device (clean + pre-seeded), MED-07 tape-scoped `[7,19]` (+ JSONL), CLN-01
  drive-scoped `[20]`, HW-04 multi-flag `[58,16]`, persistence (two reads → two
  events). No scenario weakly asserted or missing.

## Net

Phase D is complete and correct: TapeAlert is now a real, defensively-parsed
production read capability (`rem tape alerts`), and the chaos adapter models and
injects it end-to-end through the genuine reader. The single Low is a latent
sub-canonical-alloc_len reporting mismatch that no caller can reach today; the
rest are cosmetic. Deferred to D2 as designed: RDY-01, wear counters, `time_scale`
(and changer-LUN TapeAlerts to Phase E). Ready.
