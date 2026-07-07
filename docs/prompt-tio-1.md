# Codex prompt — TIO-1: batched tape I/O core (remanence-library)

**Repo:** `~/remanence`. **Status:** pending.
**Normative design:** `docs/design-tape-io-throughput-v0.1.md` (FROZEN v0.2)
— read it fully first; §3 (L1), §4 (L2), §8 (config), §9 (tests) govern.
This milestone touches only `remanence-library` (+ `remanence-scsi` CDB
builders as needed); callers stay on the existing single-block paths.

Deliverables (design §10 TIO-1):

1. `write_block_batch` / `read_block_batch` on `DriveHandle`: FIXED=1
   WRITE(6)/READ(6), N records per CDB (§3.1). No per-record READ POSITION.
2. Sense decode per §3.2: fixed-mode `records_transferred` from the sense
   INFORMATION field (VALID required, bounds 0..=batch); EW/EOM arbitration
   by READ POSITION delta; **deferred sense (0x71/0x73) ⇒ completion-unknown
   everywhere** — tighten `is_fixed_format`/`write_eom_signal`/
   `ili_signed_information`/`space_residual_if_early_stop` accordingly;
   transport-unknown ⇒ dirty fence, NO follow-up data-path CDBs (§3.2(b)).
3. `DevicePositionProof` / `ComputedPosition` types (§4) carried by
   `WriteOutcome`, `WriteFilemarksOutcome`, locate/space results; arithmetic
   tracker (`+records` per clean batch, `+1` per filemark, re-seed points per
   §4; invalidate on ambiguity).
4. sg reserved-buffer handling (§3.3): `SG_SET_RESERVED_SIZE` to the batch
   size at open, query achievable, clamp, and expose the effective batch for
   diag logging.
5. `legacy_single_block` path (§8): preserves the exact shipped command
   stream (variable-mode single-record WRITE/READ incl. per-block RP).
6. Read batch sizing never crosses a tape-file boundary; FILEMARK+VALID+
   residual decode establishes `records_read` (§3.1); SILI stays 0.

Verification member (hermetic, required): the §9 model-transport tests that
belong to this layer — CDB accounting (⌈N/B⌉ WRITEs, RP counts), legacy
command-stream equivalence, fixed/variable byte-identical images, residual
decode matrix incl. VALID=0 / out-of-range / deferred-0x71 regressions,
transport-unknown asserts NO follow-up RP, filemark arithmetic +1,
DevicePositionProof-only construction (compile-fail or API-shape test),
read filemark backstop. `cargo fmt --check`, `clippy --all-targets -D
warnings`, `cargo test -p remanence-library -p remanence-scsi`. Report files
touched, gate summary, deviations with rationale.
