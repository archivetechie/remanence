# Verification status

This is the concise entry point for Remanence's local formal-verification
estate. Detailed theorem statements live in each `verif/<area>/SPEC.md`; the
broader reviewer-facing inventory lives in `docs/formal-verification-status.md`.

Status date: 2026-07-07.

## Replay gate

Run the full local proof inventory with:

```text
make proof-inventory
```

That command discovers every `verif/*` crate with a `Cargo.toml` and runs:

- `cargo test drift_guard -- --list` to prove the drift guard is a runnable
  test, not just matching text
- `cargo test drift_guard`
- `cargo test`
- `lake build`; every current proof crate must have `lean/lakefile.toml`
- a maintained Lean placeholder scan over `*.lean`, excluding build caches and
  proof-search scratch transcripts

All current proof crates have a `drift_guard` test. Those tests pin selected
production snippets to the proof-facing extraction, so production changes fail
closed and force the extraction/proof boundary to be reviewed.

## Trust model

The trust anchor is the Lean type checker accepting the local proof files with
no local proof placeholders. Leanstral, Claude, and Codex are proof-search or
editing aids only.

The Rust extraction crates are proof-facing models. They deliberately replace
some production concerns, such as IO, allocation, cryptographic primitives,
hardware behavior, and full container traversal, with smaller scalar facts.
Each table row below names the live production surface the proof claims to
mirror and the boundary it does not cross.

## Inventory

| Area | Production target | Proved claim | Deliberately outside proof |
| --- | --- | --- | --- |
| `parity-state` | `remanence-parity::model::{ObjectParityState, ObjectParityStateUpdateRange}` | Object parity classification, update-range completeness, watermark skip safety, monotonicity, and recompute consistency. | Layer-5 scheduling, catalog persistence, physical reconstruction. |
| `parity-capacity` | `crates/remanence-parity/src/capacity.rs` | Sidecar/bootstrap sizing, epoch completion, tape/spool reserve, and capacity-gate order. | Live spool filesystem behavior and IO. |
| `parity-mapping` | `crates/remanence-parity/src/mapping.rs` | Epoch size, coordinate bounds, row-major shape, round trip, and invalid-coordinate rejection. | Sidecar encoding and tape IO. |
| `parity-sidecar-layout` | `crates/remanence-parity/src/sidecar.rs` | Fixed sidecar header/footer/index ranges, CRC windows, block placement, and checked range bounds. | Reed-Solomon recovery, raw shard contents, variable traversal, allocation, and tape IO. |
| `crc64-xz` | `crates/remanence-crc/src/lib.rs` | CRC-64/XZ bit step, table entry, table update, public slice loop, and normative vectors. | Call-site byte-window selection outside the CRC function. |
| `aead-framing` | `crates/remanence-aead/src/{stream,range,inspect}.rs` | Chunk counts, payload/stored sizes, ciphertext offsets, plaintext range validation, range planning, and inspect geometry. | AEAD security, HKDF, hashing, CBOR, allocation, and byte IO. |
| `rao-header` | `crates/remanence-aead/src/header.rs` | Header-core validation, frozen-field emission, parse/serialize round trip, and bad-field rejection. | Exact string/array reconstruction, hashing, encryption, CBOR, and allocation. |
| `rao-metadata` | `crates/remanence-aead/src/metadata.rs` | Metadata-core validation, writer-schema emission, decode/encode round trip, and checked arithmetic failure paths. | Exact digest byte copying, recursive CBOR extension skipping, encryption, hashing, and allocation. |
| `rao-manifest` | `crates/remanence-format/src/{layout,manifest}.rs` | Manifest chunk arithmetic, writer shapes, bounded/fixed-capacity entry round trips, fold/membership bridge, duplicate rejection, and hardlink accumulation. | Production CBOR bytes, real `Vec`/`String` traversal, tar/pax layout, hashing, maps, and arbitrary xattr payloads. |
| `rao-archive` | RAO header/metadata/manifest archive composition | Scalar archive `decode(encode(x)) = x` for validated archive cores and top-level consistency checks. | Exact byte archives, production container internals, path syntax, tar/pax records, hashing, encryption, allocation, and IO. |
| `tape-init` | `crates/remanence-api/src/tape_init.rs::decide_tape_init` | Committed-pool conflict dominance, fail-closed BOT decisions, blank BOT rules, clean no-op, and ordered bootstrap hazards. | BOT reads, catalog projection, bootstrap writes, and hardware orchestration. |
| `pool-selection` | `crates/remanence-api/src/pool_selection.rs` | Fit/completion predicates, leftover arithmetic, and ranking/tie-break order for `CompleteOrFill` and `FillOldest`. | Iterator internals, catalog projection, drive occupancy projection, and caller footprint projection. |

## Next target

The next useful proof work is RAO byte-level expansion. The scalar RAO header,
metadata, manifest, and archive-composition proofs are present; the remaining
strategic value is narrowing the boundary to exact production byte shapes and
production traversal behavior.
