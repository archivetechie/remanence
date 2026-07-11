# Prompt TIO-6 R2a — funnel hardening + BlockRead trait split + delivery types + error precedence (FOUNDATION)

**Status:** pending. **Normative (read FIRST, treat as binding — do NOT inline it,
it is the contract):** `docs/design-tape-io-read-pipeline-v0.1.md` (**FROZEN v0.4**),
§3.2 (delivery types), §3.3 (retyped consumer / `BlockRead`), §4.4 (funnel hardening),
§5.1 (outcome table), §5.6 (error precedence), §9.2 (golden fixture), §10 (the rows
named below). Also read `crates/remanence-library/src/handle/tape_io/mod.rs`
(`read_block_batch`, `write_block_batch`, `write_block_batch_pipelined`),
`crates/remanence-library/src/block_io.rs` (`BlockSource`/`BlockSink`),
`crates/remanence-library/src/handle/tape_io/model.rs` (`ReadBufferHandoff`,
`ReadBatchOutcome`), `crates/remanence-api/src/read_core.rs`.

This is the **FOUNDATION** stage of TIO-6 R2 (design §11.2 prerequisite ordering:
funnel hardening → trait split → …). The submitter + reservoir (R2b) builds ON it.

## Scope — implement exactly what the frozen design specifies
1. **Funnel hardening (§4.4).** In the ONE shared read funnel `read_block_batch`: on the
   filemark and recovered residual paths, cross-check `records_read × block_size ≤
   bytes_transferred` and fail-closed (completion-unknown) on violation; impossible
   residual (current RECOVERED ERROR + VALID nonzero residual + NO terminal flag) is
   fail-closed. **Write side:** extract a shared `validate_residual_claim` helper and
   CALL it (wrap-don't-copy — NEVER a fork) from BOTH `write_block_batch` and
   `write_block_batch_pipelined` at the enumerated sites (recovered-full, residual-partial;
   pipelined `PartialBatchUncommittable` → degrade-to-completion-unknown). PRESERVE the
   write path's EOM/EW device-position arbitration (helper diag-only there). The helper is
   **one-sided**: vacuous / never spuriously fails on stacks reporting no data-out residual
   (`bytes_transferred == dxfer_len`); garbage residual clamps to 0 → completion-unknown.
2. **BlockRead trait split (§3.3).** Introduce a narrow read-only `BlockRead` sub-trait
   (only what the format decoder needs, e.g. `read_block`); the decode-side source type
   implements ONLY `BlockRead` and holds NO drive handle — motion methods absent BY TYPE
   (this is the structural "one command in flight" guarantee). Land the trait + the
   decode-side type shape + a type-level proof the decode path cannot reach a drive method.
   Do NOT retype the whole pipeline (R2b).
3. **Delivery types (§3.2).** Define `ReadDelivery = Handoff(SequencedHandoff) |
   ProofFrontier{through_seq, plan_records_end, proof}` and `SequencedHandoff{seq,
   plan_records_end, position_after, evidence, handoff}` preserving `ReadBatchOutcome`'s
   position evidence (which `from_outcome` currently DISCARDS). Wire `read_buffer_handoff`
   to return that evidence alongside the unchanged `ReadBufferHandoff`. (The submitter/decode
   that USE these are R2b; here: the types + evidence preservation + unit tests.)
4. **Error-precedence accumulator (§5.6).** A replaceable priority-ranked terminal
   accumulator (P0 SCSI root > P1 decode > P2 sender); record-then-close discipline; post-join
   panic → ranked cause; terminal status emitted exactly once after all producers join;
   genuine disconnect short-circuits. Land the mechanism + unit tests (the three-thread wiring
   is R2b).
5. **Doc-minors (frozen v0.4 header).** `reference-configuration.md`: add the four
   reservoir/proof keys (§4.8: `read_reservoir_bytes`, both watermark %,
   `position_check_bytes_ranged`); rewrite the spool row so post-R2 authority is the shared
   ceiling (§4.6), NOT a runtime `MemAvailable` clamp; degenerate configs rejected at load.

## Binding invariants
- **Wrap, don't copy** (additive-bias rule): the write helper is CALLED from both write
  funcs; a near-verbatim copy is a defect even with green tests. Read hardening lives in the
  one read funnel.
- **Never weaken SG:** touch no `DevicePositionProof`/fencing semantics; hardening only ADDS
  fail-closed checks.
- **No landed-write regression:** parity — identical sense fixtures through both write entry
  points ⇒ identical helper decisions; no legitimate landed write outcome is reclassified.
- Golden READ-CDB fixture (§9.2) unchanged (this stage changes types/funnel, not the CDB stream).

## Tests (§10 rows for this stage)
funnel-hardening rows (both read paths + both write funcs, parity, EOM/EW byte-identical,
garbage-resid clamp, vacuous-pass); error-precedence rows (SCSI root recorded LAST still wins;
panic post-join ranked; emitted once after joins; disconnect → teardown, emission skipped);
delivery-type units (evidence preserved); `BlockRead` type-level no-drive-method proof.

## Definition of done (AGENTS.md)
`cargo test --workspace` green, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
socket tests run (sandbox limits reported, never `#[ignore]`d). Summary: files touched, each
§4.4/§5.6 row → test name, and confirmation the write helper is one-sided + reclassifies no
landed outcome. R2b (submitter/reservoir) + the scenario `covers` (`rem.tape.read_pipeline`)
follow.
