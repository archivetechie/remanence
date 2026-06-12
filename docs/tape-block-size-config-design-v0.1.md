# Configurable tape block size (fix the 4 KiB write-granularity bottleneck) Design v0.1

Status: design decision, prompted by **Scenario I** on the akash fixture: multi-drive
admission works, but write granularity is the bottleneck — tapes are provisioned
with a **4 KiB** block size, so every object is chunked into 4 KiB tape blocks and
`block_write_calls` explodes. Fix: make the tape block size configurable, default it
to **256 KiB**, record it at tape init, reinit scratch tapes, and rerun Scenario I —
`selected_block_size_bytes` should jump to 262144 and `block_write_calls` collapse.

## Root cause (verified against the repo)

The format and parity layers already default to 256 KiB
(`remanence-format` `DEFAULT_CHUNK_SIZE = 256*1024`; `remanence-parity`
`DEFAULT_SCHEME_BLOCK_SIZE_BYTES = 256*1024`), but **provisioning records 4 KiB**:
- `rem tape init`'s fresh-tape path returns `(new_uuid, DEFAULT_TAPE_INIT_BLOCK_SIZE
  = 4096, ParityConfig::None)` (`remanence-cli/src/lib.rs:2851, :3571`).
- That value is written into the tape's BOT bootstrap + catalog via
  `TapeInitGeometry { block_size_bytes, parity }`.
- The live writer faithfully **follows the recorded catalog geometry**:
  `pool_write.rs:586` `let block_size = selected.block_size;` → `:1285`
  `options.chunk_size = block_size as usize;`.
- `TapePoolConfig` has **no** block-size field (`config.rs`), so there's nothing to
  configure it from.

So the daemon is correct (it honors recorded geometry); the bug is the
provisioning/config seam baking in 4 KiB.

## Design

**1. Pool config (the normal source).** Add to `TapePoolConfig`:
```rust
    #[serde(default = "default_tape_block_size", rename = "block_size",
            deserialize_with = "deserialize_byte_size")]
    pub block_size_bytes: u64,   // default 256 KiB
```
(`fn default_tape_block_size() -> u64 { 256 * 1024 }`; mirrors `min_object_size_bytes`'s
`deserialize_byte_size` so config can write `block_size = "256KiB"`.) `validate_config`
checks each pool's `block_size_bytes`: `> 0`, **multiple of 512**, and `<= 16 MiB`
(`16*1024*1024`, comfortably below the `0xFF_FFFF` SSC cap and any LTO drive max;
also `<= u32::MAX` since the on-tape/catalog field is `u32`).

**2. `rem tape init --block-size <SIZE>` override.** Block-size resolution at init,
in precedence order: the `--block-size` CLI flag → the block_size of the pool the
tape is being initialized into (the existing barcode→pool / `--pool` resolution) →
the global default (256 KiB). Same validation as config (positive, 512-multiple,
capped) applied to the resolved value.

**3. Replace the hardcode.** The fresh-init path uses the **resolved** block size
(`u64 → u32`, validated in range) instead of `DEFAULT_TAPE_INIT_BLOCK_SIZE = 4096`.
The idempotent-reuse path is unchanged (it reuses the BOT-recorded geometry — see
backward compatibility). Remove or repurpose the `4096` constant.

**4. Record it.** The resolved block size flows into `TapeInitGeometry.block_size_bytes`
→ the BOT bootstrap + the catalog `tapes.block_size` (existing path; only the value
changes). The write path then picks it up automatically (`selected.block_size`).

**5. Reinit scratch tapes** (operator step): recycle the scratch pool's tapes so
they come up with `block_size = 262144`. Rerun Scenario I to confirm.

## Backward compatibility

Reads use the **recorded** catalog/BOT block size, so existing 4 KiB tapes stay
readable unchanged. Only *new* tape init records 256 KiB; new writes to freshly
re-initialized tapes use the large geometry. No data migration.

## Parity note (relationship, kept out of scope)

The parity scheme is **derived from** block size (`default_scheme_for_block_size(bytes)`
→ stripes sized for a ~512 MiB tolerance), so there is no separate "parity
compatibility" constraint to validate — the scheme adapts to whatever block size is
recorded. Today fresh init records `ParityConfig::None` (no parity sidecars on new
tapes). **Whether fresh tapes should get a derived parity scheme by default is a
separate policy decision** the user has not raised here; this patch leaves the parity
mode exactly as it is and only changes the block size. (Flagged for a later call.)

## Scope

**IN:** `TapePoolConfig.block_size_bytes` (default 256 KiB) + `validate_config`
checks; `rem tape init --block-size` override + the resolution precedence; replacing
the hardcoded 4096 in the fresh-init path with the resolved value; recording it into
BOT/catalog geometry; the `u64→u32` conversion with range validation. **OUT:** the
live **drive READ BLOCK LIMITS** validation (needs new SCSI plumbing — `read_block_limits`
is parsed but never issued to a transport today; 256 KiB is safely within LTO limits,
so this is a documented near-follow-up); enabling parity-by-default on fresh tapes
(separate policy); reducing the inline `READ POSITION` per data block (the user's
flagged separate second-order optimization); migrating existing 4 KiB tapes.

## Acceptance criteria

1. **Config:** `block_size = "256KiB"` in `[[tape_pools]]` parses to `262144`; absent
   → defaults to `262144`; `validate_config` rejects `0`, a non-512-multiple (e.g.
   `1000`), and an over-cap value (`> 16 MiB`).
2. **Tape init:** a fresh `rem tape init` (no override) for a pool with the default
   records `block_size_bytes = 262144` (BOT + catalog); `--block-size 512KiB` overrides
   to `524288`; an invalid `--block-size` is rejected before writing the tape.
3. **Regression:** existing tape-init / write / read tests stay green; the
   idempotent-reuse path still reuses the recorded geometry (a 4 KiB tape re-`init`'d
   no-op stays 4 KiB and readable).
4. **Hardware (human-run, OUT of Codex scope):** reinit the scratch pool, rerun
   Scenario I → `selected_block_size_bytes = 262144` and `block_write_calls` collapses
   (≈ 64× fewer for a given object vs 4 KiB).
- Gates: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test`.

## §verification — Rust design verification

**Light verification (no skeleton), per the skill's "skip if mechanical / no new
types" rule.** This adds no new structs/traits/lifetimes/async — a `TapePoolConfig`
field that mirrors the existing `min_object_size_bytes` (`u64` + `deserialize_byte_size`
+ a `default_*` fn), a CLI `--block-size` arg, and a resolved `u64→u32` value threaded
through the existing `TapeInitGeometry`/BOT/catalog path. The risk areas are
value-level, not type-level: the serde attrs (copied verbatim from `min_object_size`),
the `u64→u32` range check, and the resolution precedence — all covered by the
acceptance tests. `BlockLimits { max_block_length: u32, min_block_length: u16 }` is
noted for the deferred drive-limits follow-up.
