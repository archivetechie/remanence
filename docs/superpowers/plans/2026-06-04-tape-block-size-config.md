# Configurable tape block size Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the tape block size configurable (pool config + `rem tape init --block-size`), default it to 256 KiB, validate it, and record it at tape init — so freshly-initialized tapes write at 256 KiB instead of the hardcoded 4 KiB.

**Architecture:** Add `block_size_bytes` to `TapePoolConfig` (default 256 KiB); resolve `--block-size` → pool → default at `rem tape init`; replace the `4096` fresh-init hardcode with the resolved value, which flows through the existing `TapeInitGeometry` → BOT → catalog → writer path.

**Tech Stack:** Rust, `remanence-state` (config), `remanence-cli` (tape init), `remanence-api` (`TapeInitGeometry`).

---

## Context the implementer needs

- **Design:** `docs/tape-block-size-config-design-v0.1.md`. Root cause: `rem tape init` records 4 KiB; the writer follows recorded geometry.
- **Mirror this pattern** (`remanence-state/src/config.rs`): `min_object_size_bytes: u64` with `#[serde(default, rename = "min_object_size", deserialize_with = "deserialize_byte_size")]` (`:116`); `deserialize_byte_size -> u64` (`:463`) accepts `"256KiB"`/ints; `validate_config` is where per-pool checks live (`require_absolute`, the watermark band check at `:401`).
- **The hardcode** (`remanence-cli/src/lib.rs`): `const DEFAULT_TAPE_INIT_BLOCK_SIZE: u32 = 4096;` (`:2851`); the fresh-init helper returns `(*Uuid::new_v4().as_bytes(), DEFAULT_TAPE_INIT_BLOCK_SIZE, ParityConfig::None)` (`:3571`); the idempotent-reuse branch returns the BOT-recorded `geometry.block_size_bytes` (keep that branch).
- **The geometry** (`remanence-api/src/tape_init.rs`): `TapeInitGeometry { block_size_bytes: u32, parity: ParityConfig }` → BOT bootstrap + catalog `tapes.block_size`. The writer reads it back (`pool_write.rs:586` `selected.block_size` → `:1285` `chunk_size`).
- **No drive READ BLOCK LIMITS check** today (deferred — see design); validation here is static.

## File Structure

- **Modify** `crates/remanence-state/src/config.rs` — `TapePoolConfig.block_size_bytes` + `default_tape_block_size` + a `pub fn validate_block_size(bytes: u64) -> Result<(), StateError>` + `validate_config` call + tests.
- **Modify** `crates/remanence-cli/src/lib.rs` — `--block-size` arg on `tape init`; resolve precedence; replace the `4096` hardcode with the resolved+validated `u32`.

---

## Task 1: Pool config field + validation

**Files:** `crates/remanence-state/src/config.rs`

- [ ] **Step 1:** Add to `TapePoolConfig` (after `min_object_size_bytes`):
```rust
    /// Fixed tape block size for tapes provisioned into this pool. Recorded at
    /// `rem tape init` into the BOT + catalog; the writer chunks objects at this
    /// size. Default 256 KiB (matches the rem-tar-v1 + parity defaults).
    #[serde(default = "default_tape_block_size", rename = "block_size",
            deserialize_with = "deserialize_byte_size")]
    pub block_size_bytes: u64,
```
and:
```rust
fn default_tape_block_size() -> u64 { 256 * 1024 }

/// Validate a tape block size: positive, a multiple of 512, and within the
/// catalog/on-tape u32 range (capped at 16 MiB — comfortably inside LTO limits).
pub fn validate_block_size(bytes: u64) -> Result<(), StateError> {
    const MAX: u64 = 16 * 1024 * 1024;
    if bytes == 0 || bytes % 512 != 0 || bytes > MAX {
        return Err(StateError::ConfigInvalid(format!(
            "tape block_size {bytes} must be a positive multiple of 512 and <= {MAX} bytes"
        )));
    }
    Ok(())
}
```

- [ ] **Step 2:** In `validate_config`, for each pool call `validate_block_size(pool.block_size_bytes)?;`.

- [ ] **Step 3: Tests** (config `mod tests`): `block_size = "256KiB"` → `262144`; absent → `262144` (default); `validate_config` rejects `block_size = 0`, `block_size = 1000` (not a 512-multiple), and `block_size = "32MiB"` (over cap). (Use the existing `valid_config()`/`parse_config_toml` helpers; base `valid_config()` must omit `block_size` so the default path is covered.)

- [ ] **Step 4:** `cargo test -p remanence-state config` (expect PASS). Commit: `git commit -am "tape block size: configurable per-pool block_size (default 256 KiB) + validation"`.

---

## Task 2: `rem tape init --block-size` + replace the hardcode

**Files:** `crates/remanence-cli/src/lib.rs`

- [ ] **Step 1:** Add `--block-size <SIZE>` to the `tape init` subcommand args (`Option<String>` parsed as a byte size — reuse the same human-size parsing the config uses, or accept a plain integer; if a parse helper isn't exported, accept `u64` bytes and document `--block-size 262144`). 

- [ ] **Step 2:** Resolve the block size at init, in precedence order:
```rust
let resolved_block_size: u64 = match cli_block_size_override {
    Some(bytes) => bytes,
    None => pool_for_tape           // the pool this tape is being initialized into
        .map(|p| p.block_size_bytes) // (existing barcode->pool / --pool resolution)
        .unwrap_or_else(remanence_state::default_tape_block_size_pub_or_256k),
};
remanence_state::validate_block_size(resolved_block_size)
    .map_err(/* -> cli error, exit before writing the tape */)?;
let block_size_u32 = u32::try_from(resolved_block_size).expect("validated <= 16 MiB");
```
(If `pool_for_tape` isn't readily available at the init call site, default to 256 KiB and rely on `--block-size`/pool config; match the existing pool-resolution the command already does. The validated value is `<= 16 MiB` so the `u32` conversion cannot overflow.)

- [ ] **Step 3:** In the fresh-init helper (`:3571`), replace `DEFAULT_TAPE_INIT_BLOCK_SIZE` with `block_size_u32` (thread it in). Keep the idempotent-reuse branch returning `geometry.block_size_bytes` unchanged. Remove the now-unused `DEFAULT_TAPE_INIT_BLOCK_SIZE` constant (`:2851`) if nothing else uses it (the `:3571` site was the consumer).

- [ ] **Step 4: Test:** a CLI/unit test (or doc-level) that the resolved block size for a default pool is `262144` and that `--block-size`/config override it; an invalid `--block-size` (e.g. `1000`) errors before any tape write. (If the init path is hard to unit-test without hardware, assert the resolution+validation helper directly.)

- [ ] **Step 5:** `cargo build --workspace`. Commit: `git commit -am "tape block size: rem tape init resolves pool/override block size, records it (was hardcoded 4 KiB)"`.

---

## Task 3: Gates

- [ ] **Step 1:** `cargo fmt --all`.
- [ ] **Step 2:** `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] **Step 3:** `cargo test --workspace` (expect PASS — config tests + the existing tape-init/write/read regression; the idempotent-reuse path still reuses recorded geometry).
- [ ] **Step 4:** Commit: `git commit -am "tape block size: gates"`.
- [ ] **Step 5 (human-run, OUT of Codex scope — note in journal):** reinit the scratch pool's tapes, rerun Scenario I → `selected_block_size_bytes = 262144`, `block_write_calls` collapses.

---

## Self-Review (completed during planning)

**Spec coverage:** `TapePoolConfig.block_size_bytes` + default + `validate_block_size` + `validate_config` (Task 1) ✓; `--block-size` + precedence (override→pool→default) + replace the 4096 hardcode + `u64→u32` (Task 2) ✓; record into BOT/catalog (automatic via the unchanged `TapeInitGeometry` path — only the value changes) ✓; tests + gates + the Scenario I rerun note (Tasks 1,2,3) ✓. OUT (drive READ BLOCK LIMITS, parity-by-default, READ POSITION reduction, migration) untouched.

**Placeholder scan:** the field/serde/default/validate code is given in full (mirroring `min_object_size_bytes`); the CLI resolution has a concrete precedence + a fallback note where the exact pool-resolution call site is the implementer's to wire (it already exists in the command). No invented APIs — `deserialize_byte_size`, `TapeInitGeometry`, `geometry.block_size_bytes`, `pool_write` chunk_size are all confirmed.

**Type consistency:** config `block_size_bytes: u64` (matches `deserialize_byte_size -> u64` and `min_object_size_bytes`); `validate_block_size(u64)`; resolved `u64` → validated → `u32` for `TapeInitGeometry.block_size_bytes: u32`; the writer's `selected.block_size` (catalog `u64` → `u32`) is unchanged. The cap (16 MiB) guarantees the `u32` conversion is infallible.
