# aead-framing formal specification

Target: `verif/aead-framing/src/lib.rs`, a dependency-free extraction of the
pure framing arithmetic in `crates/remanence-aead/src/stream.rs`,
`crates/remanence-aead/src/range.rs`, and
`crates/remanence-aead/src/inspect.rs`.

Parameters and constants:

- `H` = scalar encrypted RAO header length (`128` for v1 and v2)
- `K` = plaintext key-frame length (`0` for v1; variable for v2)
- `F = 16` = completion footer length
- `T = 16` = ChaCha20-Poly1305 tag length
- `C` = AEAD plaintext chunk size
- `M` = encrypted metadata frame length
- `P` = canonical plaintext size

## A1 -- chunk count

For `C > 0`, `P > 0`, and `P % C = 0`, `chunk_count(P, C)` returns
`P / C`. Inputs with zero chunk size, zero plaintext size, or a partial final
chunk are rejected.

## A2 -- payload frame length

For valid chunk-count inputs and no `u64` overflow,
`payload_frame_len(P, C)` returns `P + T * (P / C)`.

## A3 -- stored object size

For valid payload inputs and no `u64` overflow, the extracted v1
`stored_size_from_parts(C, M, P)` returns
`round_up(128 + M + payload_frame_len(P, C) + F, C)`.

## A4 -- ciphertext chunk offset

For no `u64` overflow, the extracted v1 `cipher_offset(M, C, b)` returns
`128 + M + b * (C + T)`.

## A5 -- plaintext range validation

`validate_range(start, len, P)` returns `start + len` exactly when the
half-open plaintext range is valid. Empty ranges may start exactly at `P`.
Non-empty ranges must end at or before `P`. Addition overflow is rejected.

## A6 -- non-empty range plan

For a valid non-empty range and no `u64` overflow, `nonempty_range_plan`
returns the same chunk coverage and stored-byte coverage used by
`open_plaintext_range_with_context`:

- `first_chunk = start / C`
- `last_chunk = (start + len - 1) / C`
- `fetched_chunk_count = last_chunk - first_chunk + 1`
- `stored_range_start = cipher_offset(M, C, first_chunk)`
- `stored_range_len = (cipher_offset(M, C, last_chunk) + C + T)
  - stored_range_start`
- `trim_start = start % C`

## A7 -- keyless inspect geometry

For a geometrically valid stored size, `inspect_geometry` derives the same
chunk count, plaintext size, footer offset, and expected rounded stored size
as `inspect_bytes`.

## A8 -- wrapper and fail-closed edges

The Lean proof also covers small but important edge contracts:

- `expected_stored_size` returns the same value as `stored_size_from_parts`
- `cipher_offset(M, C, 0)` starts payload ciphertext at `H + M`
- zero chunk size is rejected by chunk-size validation and non-empty range
  planning
- empty ranges are rejected by `nonempty_range_plan` after range validation

## A9 -- v2 prefix status (not formally proved)

The Rust extraction and production tests exercise `(H, K)` geometry for v1
`(128, 0)` and v2 `(128, key_frame_len)`, but the current Aeneas-generated
`Funs.lean` signatures still hardwire the v1 `K = 0` path. Therefore v2
geometry is unit-test- and drift-guard-covered only; it is not part of the Lean
proof claim. Follow-up **RAO-V2-FORMAL-PREFIX** must carry `key_frame_len`
through the extracted stored-size, ciphertext-offset, range-plan, and inspect
functions and re-prove the existing success theorems over `(H, K)`.

## Trust anchor

The Lean type checker (`lake build` with zero local placeholders) is the trust
anchor. The Rust `drift_guard` test ties this extraction back to the production
AEAD formulas; if it fires, the extraction and proofs must be re-established.
