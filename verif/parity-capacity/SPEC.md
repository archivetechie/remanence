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

The C1-C5 surface remains the compatibility proof for the current live reserve
helper. The following clauses specify the proof-first snapshot-aware helper;
it is intentionally unused by the writer until this proof gate is complete.

## C6 -- shared sidecar index bound

For block size `b`, the sidecar index starts after a 184-byte fixed header and
ends eight bytes before each block boundary. The extracted scalar packer places
all 16-byte parity rows followed by all 8-byte data-CRC rows without allowing a
row to cross a block. The production encoder calls this same scalar helper, so
there is no second layout formula to reconcile. Rust boundary tests compare it
with the retired entry-by-entry reference and pin the shipped 256 KiB profile;
Lean checks the extracted scalar result and composes it into the tape bound.
The maximum complete sidecar uses
`parity_shards_per_epoch` parity rows and `data_shards_per_epoch` data rows.

The sidecar's pre-filemark tape cost is:

`SidecarBody = 2 * index_blocks + parity_shards_per_epoch + 1`

where the final block is the footer locator. Its physical tape-file charge is
`SidecarBody + sidecar_filemark_blocks`.

## C7 -- bounded ParityMap term

For post-closeout sidecar row count `N`, checked allocation-free CBOR bounds are:

- directory bytes `<= 43 + 116 * N`
- complete ParityMap payload bytes `<= 325 + 116 * N`

The constants charge u64-width structural integers, maximum legal diagnostic
strings, maximum CBOR container heads, and every fixed field. The current
encoder must remain below this future-width-safe bound. The allocation-free
constants are a reviewed CBOR derivation guarded by maximum-width encoder
tests. Lean proves the checked `43+116*N` and `325+116*N` arithmetic; it does
not verify the `ciborium` implementation. An external ParityMap uses C8's
control geometry with a 184-byte header, plus its separate filemark.

The capacity profile also derives a physical directory-count ceiling:

`Nmax = empty_tape_usable_blocks /
        (parity_shards_per_epoch + 3 + sidecar_filemark_blocks)`.

A pre-existing committed directory above that ceiling is inconsistent and
rejected. A proposed Object that would cross it proceeds to the ordinary
empty/current-tape capacity gates, so a naturally oversized Object receives
the correct terminal-versus-retry classification instead of `Invariant`.
Before per-Object projection, the kernel evaluates the ParityMap bounds at
`Nmax` and the snapshot fixed-slot/control geometry at conservative counts
`T=C, O=C`. It also requires the maximum complete sidecar and the conservative
sum of that sidecar, the maximum ParityMap, the maximum snapshot, the terminal
bootstrap, their filemarks, and safety allowance to fit the checked closeout
band `C-H`. `H > C`, any arithmetic failure, or a physically impossible close
is a typed unsafe-profile refusal. This is an allocation-free representability
and physical-feasibility check, not a claim that those hypothetical rows are
materialized.

## C8 -- snapshot geometry

For `structural_entry_count = T` and `object_row_count = O`, require `O <= T`.
The exact payload is `64*T + 256*O`. For block size `b`, header size `h`, and
payload size `p`, one replicated control file occupies:

`2 * ceil((h + p) / b) + 1` blocks.

All arithmetic is checked. The final one is the footer locator. The snapshot
uses `h=512`; its trailing filemark is charged separately.

## C9 -- post-Object and post-closeout counts

Before projection, Object rows and sidecar-directory rows are disjoint map
entries, so checked `object_rows_before + sidecar_rows_before <=
structural_entries_before` is required in addition to each individual bound.
Every mapped structural file, including a bootstrap prefix, consumes at least
one physical block, so `structural_entries_before <= C` is also required. There
is no uncounted bootstrap-prefix allowance outside that ceiling.

Let `D = pending_completed_sidecars + Q` and `J = 1` when `R != 0`, else `0`.
Then:

- `object_commit_charge = projected_object_blocks + object_filemark_blocks + D*S`
- `object_rows_after = object_rows_before_object + 1`
- `sidecar_entries_after_closeout = sidecar_entries_before_object + D + J`
- `structural_entries_after_closeout =
  structural_entries_before_object + 1 + D + J + [N != 0]`

The new snapshot and following terminal bootstrap are deliberately excluded
from the embedded map's prefix count. Until the replacement bootstrap byte
contract supplies a codec-derived inline-fit decision, this proof-first kernel
conservatively reserves one external ParityMap for every non-empty directory;
it accepts no caller-supplied inline byte budget.

## C10 -- conservative checked close bound and gates

`CloseBound` is the checked safe upper bound formed from:

- a maximum-layout terminal partial sidecar and filemark, when `J=1`
- the external replicated ParityMap and filemark, when required
- the replicated snapshot and filemark
- one bootstrap block and its filemark
- the safety allowance

`required_tape_blocks = object_commit_charge + CloseBound`.

The empty-tape-impossible gate precedes current-tape retry, which precedes the
separate spool gate. Snapshot-aware spool bytes charge encoded sidecar blocks
before the tape-only filemark. Equality with tape or spool availability
succeeds; every checked overflow fails closed.

This Stage 0 base kernel covers an ordinary Object for which no checkpoint or
geometric policy control is due. Its Object charge is the Object file plus
completed sidecars. The Stage 4 scheduler must add every forced checkpoint or
policy ParityMap/snapshot/bootstrap bundle to `U'` and re-establish the
post-policy close bound before this result is wired into admission; the pure
pool kernel already accepts that complete caller-projected charge.

## Trust anchor

The Lean type checker (`lake build` with zero local placeholders) is the proof
anchor for the extracted arithmetic and branch order. Rust drift tests compare
the proof-facing kernel with production behavior and pin encoder-only facts
outside Lean's scope. The proof inventory also regenerates the checked-in
Aeneas output and requires a byte-for-byte match. If either guard fires, the
extraction and proofs must be re-established.
