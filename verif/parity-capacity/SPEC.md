# parity-capacity formal specification

Target: `verif/parity-capacity/src/lib.rs`, a dependency-free extraction of the
pure arithmetic in `crates/remanence-parity/src/capacity.rs`.

Notation:

- `S` = one sidecar tape file's tape-block cost
- `B` = one bootstrap tape file's tape-block cost
- `F = current_epoch_fill_blocks + projected_object_blocks`
- `E = data_shards_per_epoch`
- `Q = F / E` = full epochs completed by the projected object
- `R = F % E`
- `P = S` when `R != 0`, otherwise `0`

## C1 -- sidecar and bootstrap file sizes

When arithmetic does not overflow:

- `S = (2 * sidecar_index_block_count + 1) + parity_shards_per_epoch + sidecar_filemark_blocks`
- `B = 1 + bootstrap_filemark_blocks`

## C2 -- epoch completion and final partial sidecar

For `E > 0` and `current_epoch_fill_blocks < E`:

- `epochs_completed_by_object = Q`
- `final_partial_sidecar_needed` is true exactly when `R != 0`

## C3 -- tape reserve

When arithmetic does not overflow:

`reserve_after_object_blocks =
object_filemark_blocks
+ pending_completed_sidecars * S
+ Q * S
+ P
+ remaining_bootstrap_count * B
+ safety_margin_blocks`

and

`required_tape_blocks = projected_object_blocks + reserve_after_object_blocks`.

## C4 -- spool reserve

When arithmetic does not overflow:

`required_spool_bytes =
pending_completed_epoch_parity_bytes + Q * (S * block_size_bytes)`.

## C5 -- gate ordering

After invariant and arithmetic checks:

- if `empty_tape_usable_blocks < required_tape_blocks`, the result is
  `ObjectTooLargeForEmptyTape`
- else if `remaining_tape_blocks < required_tape_blocks`, the result is
  `TapeCapacity`
- else if `remaining_spool_bytes < required_spool_bytes`, the result is
  `ParitySpoolCapacity`
- otherwise `evaluate` returns a report with the C1-C4 fields.

## Trust anchor

The Lean type checker (`lake build` with zero local placeholders) is the trust
anchor. The Rust `drift_guard` test ties this extraction back to the production
capacity formulas; if it fires, the extraction and proofs must be re-established.
