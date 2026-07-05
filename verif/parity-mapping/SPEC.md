# parity-mapping formal specification

Target: `verif/parity-mapping/src/lib.rs`, a dependency-free extraction of the
pure arithmetic in `crates/remanence-parity/src/mapping.rs`.

Notation:

- `S` = `stripes_per_neighborhood`
- `k` = `data_blocks_per_stripe`
- `E = S * k` = object-data shards per parity epoch
- `n` = global object-data ordinal

## M1 -- epoch size

For `S > 0`, `k > 0`, and `S * k < 2^64`,
`data_shards_per_epoch` returns `E = S * k`.

## M2 -- produced coordinates are in bounds

For any ordinal `n` under a valid scheme:

- `stripe_index < S`
- `data_index < k`

## M3 -- row-major shape

For any ordinal `n` under a valid scheme:

- `epoch = n / E`
- `offset = n % E`
- `stripe_index = offset % S`
- `data_index = offset / S`

## M4 -- data-coordinate round trip

For any ordinal `n` under a valid scheme:

`stripe_data_to_ordinal (ordinal_to_stripe n) = n`.

## M5 -- rejection behavior

`stripe_data_to_ordinal` rejects parity coordinates and data coordinates whose
stripe or data index is outside the scheme.

## Trust anchor

The Lean type checker (`lake build` with zero target placeholders) is the trust
anchor. The Rust `drift_guard` test ties this extraction back to the production
mapping formulas; if it fires, the extraction and proofs must be re-established.
