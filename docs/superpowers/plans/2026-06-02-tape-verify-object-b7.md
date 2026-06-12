# Tape Verify-Object (B.7 `archive verify`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `rem-debug archive verify` — stream one tape object, hash the payload without restoring it to disk or buffering it, and compare to `--expected-sha256` — by extracting A.9's read path into a shared streamer.

**Architecture:** Refactor `run_archive_read`'s body into `stream_tape_object<W: Write>` (generic over the sink writer); `read` passes a `File`, `verify` passes `std::io::sink()` (discards bytes → no disk, O(block_size) memory). `verify` compares the streamed hash to `--expected-sha256` and prints a `{verified, expected_sha256, actual_sha256}` receipt.

**Tech Stack:** Rust, `clap`, `serde`/`serde_json`, `sha2`; crate `remanence-cli` (`pool_ops.rs`, `lib.rs`). Reuses `remanence-format` streaming, `remanence-library` block I/O, `remanence-state` catalog lookups — all already wired by A.9.

Spec: `docs/tape-verify-object-design-v0.1.md`. Builds on the committed A.9 read path (`pool_ops.rs`). The design was verified with a `cargo check`+clippy-clean skeleton (since removed); this plan recreates that exact code.

**Gates (run before every commit):**
```
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p remanence-cli
```

---

## File Structure

- `crates/remanence-cli/src/pool_ops.rs` — the shared streamer + verify logic (Tasks 1, 2).
- `crates/remanence-cli/src/lib.rs` — CLI wiring for `archive verify`, mirroring `archive read` (Task 3).

No new files.

---

### Task 1: Extract the shared `stream_tape_object<W>` streamer (pure refactor)

Pull `run_archive_read`'s decode→plan→mount→identity→fixed-block→space→stream→finish body into a generic helper, and make `run_archive_read` a thin wrapper. Behaviour-preserving: the existing A.9 unit tests must stay green, and `read`'s observable output is unchanged. There is no new test here — the existing suite is the regression guard (the orchestration itself is hardware, exercised by scenario-A in Task 4).

**Files:**
- Modify: `crates/remanence-cli/src/pool_ops.rs` (insert helper before `run_archive_read` at :720; rewrite `run_archive_read` body :720-879)

- [ ] **Step 1: Add `TapeObjectRef`, `TapeStreamOutcome`, and `stream_tape_object<W>`**

Insert immediately before `/// Run `rem-debug archive read`...` (the doc comment above `pub fn run_archive_read` at :719):

```rust
/// The catalog/library identity of one object to stream (read + verify share it).
struct TapeObjectRef<'a> {
    library: &'a str,
    config: &'a std::path::Path,
    locator_json: &'a str,
}

/// Result of streaming one object's payload through a sink.
struct TapeStreamOutcome {
    object_id: String,
    locator_content_sha256: [u8; 32],
    payload_bytes: u64,
    actual_sha256: [u8; 32],
}

/// Mount, position, and stream one object's payload through `sink_writer`,
/// returning the streamed hash + byte count. Generic over the sink so `read`
/// passes a `File` and `verify` passes `std::io::sink()` (no disk, no whole-
/// object buffering). On any failure it writes an operator message to `err`
/// and returns the process exit code to use.
fn stream_tape_object<W: Write>(
    report: &remanence_library::DiscoveryReport,
    target: &TapeObjectRef<'_>,
    allow: &[String],
    allow_derived: &[String],
    sink_writer: W,
    err: &mut dyn Write,
) -> Result<TapeStreamOutcome, ExitCode> {
    let loc = decode_locator(target.locator_json).map_err(|e| {
        let _ = writeln!(err, "error: locator: {e}");
        ExitCode::from(1)
    })?;

    let mut state_handle = StateHandle::open_from_config_file(target.config).map_err(|e| {
        let _ = writeln!(err, "error: open state: {e}");
        ExitCode::from(1)
    })?;

    let plan = {
        let index = state_handle.catalog_index();
        resolve_object_read_plan(index, &loc).map_err(|e| {
            let _ = writeln!(err, "error: locate object in catalog: {e}");
            ExitCode::from(1)
        })?
    };

    let lib = report.library(target.library).ok_or_else(|| {
        let _ = writeln!(err, "error: no library with serial {:?}", target.library);
        ExitCode::from(2)
    })?;
    let mut policy = StaticAllowlist::new(allow.iter().cloned());
    for s in allow_derived {
        policy = policy.with_derived_allowed(s.clone());
    }
    let mut library_handle = lib.open(&policy).map_err(|e| {
        let _ = writeln!(err, "error: opening library: {e}");
        ExitCode::from(1)
    })?;

    let mut drive = {
        let index = state_handle.catalog_index();
        load_tape_by_uuid(index, &mut library_handle, &policy, &loc.tape_uuid).map_err(|e| {
            let _ = writeln!(err, "error: load tape: {e}");
            ExitCode::from(1)
        })?
    };

    drive.rewind().map_err(|e| {
        let _ = writeln!(err, "error: rewind before verify: {e}");
        ExitCode::from(1)
    })?;
    {
        let mut source = DriveHandleSource(&mut drive);
        verify_tape_identity(&mut source, &loc.tape_uuid).map_err(|e| {
            let _ = writeln!(err, "error: tape identity: {e}");
            ExitCode::from(1)
        })?;
    }

    let current_cfg = drive.read_config().map_err(|e| {
        let _ = writeln!(err, "error: read drive config: {e}");
        ExitCode::from(1)
    })?;
    drive
        .write_config(TapeConfig {
            block_size: BlockSize::Fixed {
                size_bytes: plan.block_size_bytes,
            },
            compression: false,
            max_block_size_bytes: current_cfg.max_block_size_bytes,
            write_protected: current_cfg.write_protected,
            worm: current_cfg.worm,
        })
        .map_err(|e| {
            let _ = writeln!(err, "error: set fixed-block config: {e}");
            ExitCode::from(1)
        })?;

    let mut sink = CapturePayloadSink::new(sink_writer);
    let stream_result = {
        let mut source = DriveHandleSource(&mut drive);
        if let Err(e) = source.space(i64::from(loc.tape_file_number), SpaceKind::Filemarks) {
            let _ = writeln!(err, "error: space to tape file {}: {e}", loc.tape_file_number);
            return Err(ExitCode::from(1));
        }
        stream_rem_tar_object(
            &mut source,
            plan.block_size_bytes as usize,
            plan.block_count,
            &mut sink,
        )
    };
    if let Err(e) = stream_result {
        let _ = writeln!(err, "error: read object: {e}");
        return Err(ExitCode::from(1));
    }

    let (payload_bytes, actual_sha256) = sink.finish().map_err(|e| {
        let _ = writeln!(err, "error: {e}");
        ExitCode::from(1)
    })?;

    Ok(TapeStreamOutcome {
        object_id: loc.object_id,
        locator_content_sha256: loc.content_sha256,
        payload_bytes,
        actual_sha256,
    })
}
```

- [ ] **Step 2: Rewrite `run_archive_read` as a thin wrapper**

Replace the entire body of `pub fn run_archive_read` (from its first `let loc = ...` through the final `}` of the function, :728-879) with:

```rust
    let out_file = match std::fs::File::create(&args.out) {
        Ok(f) => f,
        Err(e) => {
            let _ = writeln!(err, "error: create --out {}: {e}", args.out.display());
            return ExitCode::from(1);
        }
    };

    let target = TapeObjectRef {
        library: &args.library,
        config: &args.config,
        locator_json: &args.locator,
    };
    let outcome = match stream_tape_object(report, &target, allow, allow_derived, out_file, err) {
        Ok(o) => o,
        Err(code) => return code,
    };

    let verified = outcome.actual_sha256 == outcome.locator_content_sha256;
    let receipt = ArchiveReadReceipt {
        object_id: outcome.object_id.clone(),
        bytes_written: outcome.payload_bytes,
        content_sha256: bytes_to_hex(&outcome.actual_sha256),
        verified,
    };
    let line = serde_json::to_string(&receipt).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
    let _ = writeln!(out, "{line}");

    if verified {
        ExitCode::SUCCESS
    } else {
        let _ = writeln!(
            err,
            "error: content_sha256 mismatch (tape payload vs locator)"
        );
        ExitCode::from(1)
    }
```

Keep the `pub fn run_archive_read(report, args, allow, allow_derived, out, err) -> ExitCode` signature unchanged. (`--out` is now created before mounting; this also fixes the earlier "mount before validating --out" note from the A.9 review.)

- [ ] **Step 3: Verify the refactor preserved behaviour + types**

Run: `cargo test -p remanence-cli`
Expected: PASS — all existing A.9 unit tests (`hex_to_bytes_*`, `decode_locator_*`, `plan_from_records_*`, `capture_payload_sink_*`, `debug_cli_parses_archive_read`) still green; the crate compiles with `run_archive_read` delegating to `stream_tape_object`.

- [ ] **Step 4: Commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/remanence-cli/src/pool_ops.rs
git commit -m "Extract stream_tape_object<W> shared by archive read (B.7 prep)"
```

---

### Task 2: `build_verify_receipt` + `run_archive_verify`

**Files:**
- Modify: `crates/remanence-cli/src/pool_ops.rs`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `pool_ops.rs`:

```rust
#[test]
fn build_verify_receipt_matches_and_mismatches() {
    let a = [0xABu8; 32];
    let b = [0xCDu8; 32];

    let ok = super::build_verify_receipt(a, a);
    assert!(ok.verified);
    assert_eq!(ok.expected_sha256, ok.actual_sha256);
    assert_eq!(ok.expected_sha256, super::bytes_to_hex(&a));

    let bad = super::build_verify_receipt(a, b);
    assert!(!bad.verified);
    assert_eq!(bad.expected_sha256, super::bytes_to_hex(&a));
    assert_eq!(bad.actual_sha256, super::bytes_to_hex(&b));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p remanence-cli build_verify_receipt_matches_and_mismatches`
Expected: FAIL to compile — `build_verify_receipt` / `ArchiveVerifyReceipt` do not exist.

- [ ] **Step 3: Implement the args struct, receipt, pure helper, and runner**

Add to `pool_ops.rs` (next to `run_archive_read`):

```rust
/// Arguments for `rem-debug archive verify`.
pub struct ArchiveVerifyArgs {
    /// Library serial to allow.
    pub library: String,
    /// Canonical locator JSON emitted by `archive write --json`.
    pub locator: String,
    /// Expected payload SHA-256, hex (the catalog's recorded asset hash).
    pub expected_sha256: String,
    /// Path to config file.
    pub config: PathBuf,
}

/// One-line JSON receipt printed by `archive verify`.
#[derive(serde::Serialize)]
struct ArchiveVerifyReceipt {
    verified: bool,
    expected_sha256: String,
    actual_sha256: String,
}

/// Pure: build the verify receipt from expected vs streamed hash.
fn build_verify_receipt(expected: [u8; 32], actual: [u8; 32]) -> ArchiveVerifyReceipt {
    ArchiveVerifyReceipt {
        verified: expected == actual,
        expected_sha256: bytes_to_hex(&expected),
        actual_sha256: bytes_to_hex(&actual),
    }
}

/// Run `rem-debug archive verify`: stream the object, hash the payload, and
/// compare to `--expected-sha256`. No `--out`; uses `std::io::sink()` so
/// nothing is written to disk and only one block + the hash live in memory.
pub fn run_archive_verify(
    report: &remanence_library::DiscoveryReport,
    args: &ArchiveVerifyArgs,
    allow: &[String],
    allow_derived: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let expected: [u8; 32] = match hex_to_bytes(&args.expected_sha256).and_then(|v| {
        <[u8; 32]>::try_from(v.as_slice())
            .map_err(|_| format!("expected-sha256 must be 32 bytes, got {}", v.len()))
    }) {
        Ok(e) => e,
        Err(e) => {
            let _ = writeln!(err, "error: --expected-sha256: {e}");
            return ExitCode::from(1);
        }
    };

    let target = TapeObjectRef {
        library: &args.library,
        config: &args.config,
        locator_json: &args.locator,
    };
    let outcome = match stream_tape_object(report, &target, allow, allow_derived, std::io::sink(), err)
    {
        Ok(o) => o,
        Err(code) => return code,
    };

    let receipt = build_verify_receipt(expected, outcome.actual_sha256);
    let verified = receipt.verified;
    let line = serde_json::to_string(&receipt).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
    let _ = writeln!(out, "{line}");

    if verified {
        ExitCode::SUCCESS
    } else {
        let _ = writeln!(
            err,
            "error: sha256 mismatch (tape payload vs --expected-sha256)"
        );
        ExitCode::from(1)
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p remanence-cli build_verify_receipt_matches_and_mismatches`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/remanence-cli/src/pool_ops.rs
git commit -m "Add run_archive_verify (streaming sha256 verify via io::sink)"
```

(`run_archive_verify` is `pub` but not yet dispatched, so it is not dead code in the lib; Task 3 wires it.)

---

### Task 3: Wire `rem-debug archive verify` into the CLI

Mirror the `archive read` wiring at every site.

**Files:**
- Modify: `crates/remanence-cli/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `lib.rs` (next to `debug_cli_parses_archive_read`):

```rust
#[test]
fn debug_cli_parses_archive_verify() {
    let cli = DebugCli::parse_from([
        "rem-debug",
        "--allow",
        "LIB",
        "archive",
        "verify",
        "--library",
        "LIB",
        "--locator",
        "{}",
        "--expected-sha256",
        "00",
        "--config",
        "/tmp/config.toml",
    ]);
    assert!(matches!(
        cli.command,
        Command::Archive {
            command: RemArchiveCommand::Verify(_)
        }
    ));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p remanence-cli debug_cli_parses_archive_verify`
Expected: FAIL to compile — `RemArchiveCommand::Verify` does not exist.

- [ ] **Step 3: Add the clap args + enum variants + From impls**

In `enum RemArchiveCommand`, after the `Read(RemArchiveReadArgs)` variant:
```rust
    /// Verify one object on tape by streaming + hashing, no restore to disk.
    Verify(RemArchiveVerifyArgs),
```

After `struct RemArchiveReadArgs { … }`, add:
```rust
/// Arguments for `rem archive verify`.
#[derive(Args, Debug)]
struct RemArchiveVerifyArgs {
    /// Library serial for the physical tape library.
    #[arg(long, value_name = "SERIAL")]
    library: String,

    /// Canonical locator JSON emitted by `archive write --json`.
    #[arg(long, value_name = "JSON")]
    locator: String,

    /// Expected payload SHA-256 (hex) to compare the tape bytes against.
    #[arg(long, value_name = "HEX")]
    expected_sha256: String,

    /// Path to `/etc/rem/config.toml`.
    #[arg(long, value_name = "PATH", default_value = "/etc/rem/config.toml")]
    config: PathBuf,
}
```

In `impl From<RemArchiveCommand> for ArchiveCommand`, after the `Read` arm:
```rust
            RemArchiveCommand::Verify(args) => Self::Verify(args.into()),
```

After `impl From<RemArchiveReadArgs> for ArchiveReadArgs { … }`, add:
```rust
impl From<RemArchiveVerifyArgs> for ArchiveVerifyArgs {
    fn from(value: RemArchiveVerifyArgs) -> Self {
        Self {
            library: value.library,
            locator: value.locator,
            expected_sha256: value.expected_sha256,
            config: value.config,
        }
    }
}
```

In `enum ArchiveCommand`, after `Read(ArchiveReadArgs)`:
```rust
    /// Verify one object on tape by streaming + hashing, no restore to disk.
    Verify(ArchiveVerifyArgs),
```

After `struct ArchiveReadArgs { … }`, add:
```rust
/// Arguments for the shared `archive verify` command (post-From transform).
#[derive(Args, Debug)]
struct ArchiveVerifyArgs {
    /// Library serial for the physical tape library.
    #[arg(long, value_name = "SERIAL")]
    library: String,

    /// Canonical locator JSON emitted by `archive write --json`.
    #[arg(long, value_name = "JSON")]
    locator: String,

    /// Expected payload SHA-256 (hex) to compare the tape bytes against.
    #[arg(long, value_name = "HEX")]
    expected_sha256: String,

    /// Path to `/etc/rem/config.toml`.
    #[arg(long, value_name = "PATH", default_value = "/etc/rem/config.toml")]
    config: PathBuf,
}
```

- [ ] **Step 4: Add the `ArchiveCommand` method arms**

- `tape_target()` — after the `Self::Read(args) => Some(...)` arm:
  ```rust
            Self::Verify(args) => Some(args.library.as_str()),
  ```
- `is_dump_command()` — change `Self::Write(_) | Self::Read(_) => false,` to:
  ```rust
            Self::Write(_) | Self::Read(_) | Self::Verify(_) => false,
  ```
- `source()` — after the `Self::Read(_) => panic!(...)` arm:
  ```rust
            Self::Verify(_) => panic!("ArchiveCommand::Verify has no dump/tape source"),
  ```
- `format()` — after the `Self::Read(_) => panic!(...)` arm:
  ```rust
            Self::Verify(_) => panic!("ArchiveCommand::Verify has no format"),
  ```

- [ ] **Step 5: Dispatch `Verify` + add the unreachable arms**

In the `Command::Archive { command } => { … }` block, after the `if let ArchiveCommand::Read(args) = &command { … }` block:
```rust
            if let ArchiveCommand::Verify(args) = &command {
                return pool_ops::run_archive_verify(
                    &report,
                    &pool_ops::ArchiveVerifyArgs {
                        library: args.library.clone(),
                        locator: args.locator.clone(),
                        expected_sha256: args.expected_sha256.clone(),
                        config: args.config.clone(),
                    },
                    &allow,
                    &allow_derived,
                    out,
                    err,
                );
            }
```

In `run_archive_dump_command` and `run_archive_tape_with_drive`, next to each `ArchiveCommand::Read(_) => { unreachable!(...) }`:
```rust
        ArchiveCommand::Verify(_) => {
            unreachable!("archive verify dispatched before the archive handler")
        }
```

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo test -p remanence-cli debug_cli_parses_archive_verify`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/remanence-cli/src/lib.rs
git commit -m "Wire rem-debug archive verify command (B.7)"
```

---

### Task 4: Full verification + integration

**Files:** (verification only)

- [ ] **Step 1: Full-workspace gates**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
Expected: all pass. Confirms the refactor didn't regress A.9 and the verify surface is clean.

- [ ] **Step 2: Hardware round-trip (manual, requires the akash QuadStor fixture)**

```bash
cargo build --release -p remanence-cli
sudo setcap cap_sys_rawio+ep target/release/rem-debug
# Write a known file (A.5), capture the locator + its sha256:
LOC=$(target/release/rem-debug archive write --library 7CBAD9CF74 --allow 7CBAD9CF74 \
  --file /etc/hostname --pool scenario-a --config /var/lib/replica/rem/config.toml --json)
SHA=$(sha256sum /etc/hostname | cut -d' ' -f1)
# Verify the tape bytes hash to the expected sha (B.7), no --out:
target/release/rem-debug archive verify --library 7CBAD9CF74 --allow 7CBAD9CF74 \
  --locator "$LOC" --expected-sha256 "$SHA" --config /var/lib/replica/rem/config.toml
# Expect a receipt with "verified":true and matching hashes; exit 0.
# Negative check: a wrong expected hash must report verified:false and exit non-zero.
target/release/rem-debug archive verify --library 7CBAD9CF74 --allow 7CBAD9CF74 \
  --locator "$LOC" --expected-sha256 "$(printf '%064d' 0)" --config /var/lib/replica/rem/config.toml; echo "exit=$?"
```
Expected: first verify prints `"verified":true` (exit 0); the wrong-hash verify prints `"verified":false` (exit 1).

- [ ] **Step 3: Record the result in the journal**

Append a dated entry noting the B.7 hardware result (or that it is pending hardware access).

---

### System-side follow-up (OUT of this plan's scope — `~/system`, not remanence)

To run scenario B.7 through the harness, the seam `rem.tape.verify_object` must flip from `Stub` to a real adapter. This lives in `~/system`, **not** Codex's scope here. Sketch:
- `~/system/bindings.toml`: set `"rem.tape.verify_object" = "Real(cli-subprocess)"`.
- `~/system/harness/seams/rem.py`: add `_real_verify_object(locator, expected_sha256)` that shells `rem-debug archive verify --locator <json> --expected-sha256 <hex> …`, parses the stdout receipt, and returns `{"verified": ..., "expected_sha256": ..., "actual_sha256": ...}`.

---

## Self-Review

**Spec coverage** (against `docs/tape-verify-object-design-v0.1.md`):
- Shared `stream_tape_object<W>` refactor → Task 1. ✓
- `read` thin wrapper, behaviour unchanged → Task 1 Step 2. ✓
- `verify` via `io::sink()` (no disk, no buffer) → Task 2 (`std::io::sink()`). ✓
- Compare to `--expected-sha256`; receipt `{verified, expected_sha256, actual_sha256}` → Task 2. ✓
- CLI surface `archive verify`, rem-debug-gated → Task 3 (`tape_target → Some(library)`). ✓
- Always-print receipt + exit 0/1 → Task 2. ✓
- `--expected-sha256` decode error path → Task 2. ✓
- Testing (pure `build_verify_receipt` + parse + manual hardware) → Tasks 2, 3, 4. ✓
- Out of scope (recovery, multi-file, daemon, harness flip) → not implemented; harness flip noted as system-side. ✓

**Placeholder scan:** none. Every code step shows full code; commands have expected output. Task 1 has no new test by design (pure refactor guarded by the existing A.9 suite + scenario-A in Task 4) — stated explicitly, not a gap.

**Type consistency:** `TapeObjectRef`/`TapeStreamOutcome`/`stream_tape_object<W>` signatures are identical in Task 1's definition and Task 2's call. `ArchiveVerifyArgs` fields (`library`, `locator`, `expected_sha256`, `config`) match across pool_ops (Task 2), the lib.rs `From` (Task 3 Step 3), and the dispatch mapping (Task 3 Step 5). `build_verify_receipt(expected: [u8;32], actual: [u8;32]) -> ArchiveVerifyReceipt` matches its test (Task 2 Step 1). `run_archive_read`'s wrapper uses `outcome.payload_bytes`/`outcome.actual_sha256`/`outcome.object_id`/`outcome.locator_content_sha256` as defined in `TapeStreamOutcome`.

**Verified against code:** this is the same code that passed `cargo check -p remanence-cli` + clippy clean on 2026-06-02 as the design skeleton (clippy `too_many_arguments` already resolved via `TapeObjectRef`). All A.9 anchor sites (`RemArchiveCommand`/`ArchiveCommand` variants, `tape_target`/`is_dump_command`/`source`/`format`, dispatch, the two `unreachable!` handlers) confirmed present in the committed `lib.rs`.
