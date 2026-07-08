# Codex prompt — TIO-2: batched paths wired into pool write/read + durable tape fence

**Repo:** `~/remanence`. **Status:** pending (depends on TIO-1 landed).
**Normative design:** `docs/design-tape-io-throughput-v0.1.md` (FROZEN v0.2);
§3.4 (read-side blocker fix), §4.2 (durable fence), §8 (config), §9 (tests).

Deliverables (design §10 TIO-2):

0. Carry-over from TIO-1 diff gate: add the fixed/variable byte-identical
   image test (§9) — serial variable writes vs one batched write produce
   identical model-transport tape images.
1. Switch the pool-write transfer loop and read-core to the TIO-1 batched
   primitives, honoring `tape_io.legacy_single_block` and
   `write_batch_blocks`/`read_batch_blocks`/`position_check_bytes` config.
2. **Read-side MODE SELECT step (panel blocker):** before batch reads, set
   `Fixed{block_size_bytes}` from the tape's bootstrap/catalog row, verify
   via MODE SENSE, fall back to legacy variable-mode if the block size cannot
   be established (§3.4).
3. Partial-batch ⇒ object uncommittable (§3.2(a)4): no object-closing
   filemark, no journal record, session poisoned, durable fence persisted.
4. **Durable tape-I/O fence (§4.2):** extend the quarantine store with a
   tape-I/O scope (tape UUID + barcode); enforce at pool selection, session
   open, and startup reconciliation; release only via the operator
   quarantine flow.
5. Drift tripwire per `position_check_bytes`; mismatch ⇒ poison + durable
   fence + evidence with both positions.
6. Diag: replace the hardcoded `drive_write_per_block_read_position=true`
   with computed `write_batch_blocks`/`effective_batch_blocks`/
   `position_check_bytes`; `position_calls` stays honest.

Verification member (hermetic, required): §9 tests at this layer — fenced
tape refused by selection/open/startup until operator release; partial-batch
uncommittable (assert no filemark CDB, no journal record); read-side mode
setup (unprepared drive refused/re-prepared, block size from tape row);
cross-version stored-image tests (old-code reads new-batch image and vice
versa); tripwire mismatch fences durably. Full `cargo test` for touched
crates + fmt/clippy. Add `rem.tape.batched_io` to `scenario-append` `covers`
(coordinate with `~/system` bindings note in the report — do not edit
~/system). Report files touched, gates, deviations.

---
**Diff gate 2026-07-07 (Claude Fable 5): PASS.** Independent gates: fmt/clippy
clean, FULL `cargo test` green including the daemon unix-socket suites that
were sandbox-blocked for codex. Verified enforcement at all three points
(pool selection `pool_write.rs:3002`, session open write+read
`write_owner.rs:1803/2356` with dedicated refusal test, daemon startup
`lib.rs:108/404` with operator-release guidance in the error). Read-side prep
tested by `prepare_drive_for_read_sets_catalog_fixed_block_size_or_legacy_fallback`
(block size from catalog row, MODE SENSE verify, legacy fallback). TIO-1
carry-overs closed: byte-identical image test
(`no_parity_stored_images_cross_read_between_serial_and_batched_paths`) and
`legacy_single_block` config wiring. Diag fields replaced as designed.
Deviation accepted: `rem.tape.batched_io` covers entry added to ~/system
`scenarios/contracts.toml` by the supervisor (codex correctly did not edit
~/system).
