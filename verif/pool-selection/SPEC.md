# pool-selection ranking formal specification

Target: `crates/remanence-api/src/pool_selection.rs`, specifically the pure
ranking kernel used by `CompleteOrFill` and `FillOldest`.

The production policy is intentionally pure, but the implementation uses
slices, `Vec`, iterator adapters, tuple `min_by_key`, trait objects, and `Arc`.
This verification extraction proves the stable arithmetic and pairwise ranking
kernel those iterators consume. UUIDs are modeled as ordered `u64`s because the
production tuple key uses tape UUID only as a deterministic final tie-break.
The Rust `drift_guard` test ties the extraction back to production snippets.

## P1 -- fit predicate

`fits(candidate, P)` is true exactly when `usable_bytes - used_bytes` is defined
and at least the projected footprint `P`.

## P2 -- completion predicate

`completes_tape(candidate, P)` uses saturating addition and is true exactly when
`used_bytes.saturating_add(P) >= low_bytes`.

## P3 -- leftover arithmetic

`leftover_after_write(candidate, P)` is the production saturating expression:
`usable_bytes.saturating_sub(used_bytes).saturating_sub(P)`.

## P4 -- CompleteOrFill completing-rank key

Among fitting candidates that complete the tape, `CompleteOrFill` ranks by:

- lowest leftover after write
- already-loaded before not-loaded
- lowest barcode order
- lowest tape UUID

## P5 -- CompleteOrFill fill-rank key

If no fitting candidate completes the tape, `CompleteOrFill` ranks by:

- already-loaded before not-loaded
- lowest barcode order
- lowest tape UUID

## P6 -- FillOldest rank key

`FillOldest` ranks fitting candidates by:

- lowest barcode order
- already-loaded before not-loaded
- lowest tape UUID

## Trust anchor

The Lean type checker (`lake build` with zero local placeholders) is the trust
anchor. The Rust `drift_guard` test ties this extraction back to
`crates/remanence-api/src/pool_selection.rs`; if it fires, the extraction and
proofs must be re-established.
