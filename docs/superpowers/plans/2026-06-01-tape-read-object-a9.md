# Tape Read-Object (A.9 `archive read`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement `rem-debug archive read` — restore one object's payload bytes from tape given the canonical A.5 locator, closing the scenario-A spine (A.9 → A.10 byte-equal).

**Architecture:** A break-glass CLI command in `crates/remanence-cli/src/pool_ops.rs`, mirroring `archive write`. It decodes the locator, resolves the read length from the catalog, mounts the cartridge via the existing `load_tape_by_uuid` bridge, positions with `DriveHandleSource::locate`, streams the `rem-tar-v1` object through `stream_rem_tar_object` into a sink that extracts the single non-manifest payload entry and SHA-256-verifies it against the locator, then writes it to `--out` and prints a JSON receipt. One read path covers no-parity and parity (healthy) tapes — parity is a sidecar, not interleaved.

**Tech Stack:** Rust, `clap`, `serde`/`serde_json`, `sha2`; crates `remanence-cli`, `remanence-format` (`stream_rem_tar_object`, `RemTarEntrySink`, `MANIFEST_PATH`), `remanence-library` (`DriveHandleSource`, `BlockSource`, `load_tape_by_uuid`), `remanence-state` (`CatalogIndex` lookups), `remanence-api` (`verify_tape_identity`, `TapeUuid`).

Spec: `docs/tape-read-object-design-v0.1.md`. A `cargo check`-verified skeleton already exists in `crates/remanence-cli/src/pool_ops.rs` (section "archive read (A.9) — design-verification skeleton"); this plan fills its `todo!()` bodies and wires the CLI. The `run_archive_read` body is already written; Tasks 2–5 implement the helpers it calls.

**Gates (run before every commit):**
```
cargo fmt --all
cargo clippy -p remanence-cli --all-targets -- -D warnings
cargo test -p remanence-cli
```
Full-workspace gates run in Task 6.

---

## File Structure

- `crates/remanence-cli/src/lib.rs` — CLI surface: the `RemArchiveReadArgs` clap struct, the shared `ArchiveReadArgs` variant payload, the `From` impls, the `ArchiveCommand::Read` variant + its match arms, and the dispatch branch. (Task 1.)
- `crates/remanence-cli/src/pool_ops.rs` — the command logic: `hex_to_bytes`, `decode_locator`, `plan_from_records` + `resolve_object_read_plan`, `CapturePayloadSink`, and the already-present `run_archive_read`. (Tasks 2–5.)
- `crates/remanence-state/src/lib.rs` — ensure `NativeObjectCopyRecord` + `TapeFileRecord` are re-exported (Task 4, if not already).

No new files.

---

### Task 1: Wire `rem-debug archive read` into the CLI

Mirror every `archive write` site with a `Read` analogue so the command parses and dispatches to `pool_ops::run_archive_read`. `run_archive_read`'s helper bodies are still `todo!()` after this task — that is fine; the command compiles and routes, and the parse test does not execute it.

**Files:**
- Modify: `crates/remanence-cli/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `crates/remanence-cli/src/lib.rs`:

```rust
#[test]
fn debug_cli_parses_archive_read() {
    let cli = DebugCli::parse_from([
        "rem-debug",
        "--allow",
        "LIB",
        "archive",
        "read",
        "--library",
        "LIB",
        "--locator",
        "{}",
        "--out",
        "/tmp/restored.bin",
        "--config",
        "/tmp/config.toml",
    ]);
    assert!(matches!(
        cli.command,
        Command::Archive {
            command: RemArchiveCommand::Read(_)
        }
    ));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p remanence-cli debug_cli_parses_archive_read`
Expected: FAIL to compile — `RemArchiveCommand::Read` does not exist yet.

- [ ] **Step 3: Add the `rem`-facing clap args + enum variant**

In `crates/remanence-cli/src/lib.rs`, add a variant to `enum RemArchiveCommand` (after the `Write(RemArchiveWriteArgs)` variant):

```rust
    /// Read one object back from tape by locator and write it to --out.
    Read(RemArchiveReadArgs),
```

And add the args struct next to `RemArchiveWriteArgs`:

```rust
/// Arguments for `rem archive read`.
#[derive(Args, Debug)]
struct RemArchiveReadArgs {
    /// Library serial for the physical tape library.
    #[arg(long, value_name = "SERIAL")]
    library: String,

    /// Canonical locator JSON emitted by `archive write --json`.
    #[arg(long, value_name = "JSON")]
    locator: String,

    /// Destination path for the restored payload bytes.
    #[arg(long, value_name = "PATH")]
    out: PathBuf,

    /// Path to `/etc/rem/config.toml`.
    #[arg(long, value_name = "PATH", default_value = "/etc/rem/config.toml")]
    config: PathBuf,
}
```

- [ ] **Step 4: Add the shared variant payload + `From` impls**

Add a variant to `enum ArchiveCommand` (after `Write(ArchiveWriteArgs)`):

```rust
    /// Read one object back from tape by locator and write it to --out.
    Read(ArchiveReadArgs),
```

Add the shared args struct next to `ArchiveWriteArgs`:

```rust
/// Arguments for the shared `archive read` command (post-From transform).
#[derive(Args, Debug)]
struct ArchiveReadArgs {
    /// Library serial for the physical tape library.
    #[arg(long, value_name = "SERIAL")]
    library: String,

    /// Canonical locator JSON emitted by `archive write --json`.
    #[arg(long, value_name = "JSON")]
    locator: String,

    /// Destination path for the restored payload bytes.
    #[arg(long, value_name = "PATH")]
    out: PathBuf,

    /// Path to `/etc/rem/config.toml`.
    #[arg(long, value_name = "PATH", default_value = "/etc/rem/config.toml")]
    config: PathBuf,
}
```

In `impl From<RemArchiveCommand> for ArchiveCommand`, add the arm after the `Write` arm:

```rust
            RemArchiveCommand::Read(args) => Self::Read(args.into()),
```

Add the `From` impl next to `impl From<RemArchiveWriteArgs> for ArchiveWriteArgs`:

```rust
impl From<RemArchiveReadArgs> for ArchiveReadArgs {
    fn from(value: RemArchiveReadArgs) -> Self {
        Self {
            library: value.library,
            locator: value.locator,
            out: value.out,
            config: value.config,
        }
    }
}
```

- [ ] **Step 5: Add the `ArchiveCommand` method arms**

`archive read` is a direct tape op, so gate it on `rem-debug` like `write`. In `impl ArchiveCommand`:

- `tape_target()` — add before the `_ =>` arm:
  ```rust
            Self::Read(args) => Some(args.library.as_str()),
  ```
- `is_dump_command()` — change `Self::Write(_) => false,` to:
  ```rust
            Self::Write(_) | Self::Read(_) => false,
  ```
  (Critical: `Read` must be matched here, otherwise the `_ => self.source()...` arm calls `source()` on `Read` and panics.)
- `source()` — add before the closing `}` of the match (it has no `_` arm):
  ```rust
            Self::Read(_) => panic!("ArchiveCommand::Read has no dump/tape source"),
  ```
- `format()` — likewise:
  ```rust
            Self::Read(_) => panic!("ArchiveCommand::Read has no format"),
  ```

- [ ] **Step 6: Dispatch `Read` to `pool_ops::run_archive_read`**

In `run_with_mode`'s `Command::Archive { command } => { ... }` block, add this *before* the final `return run_archive_tape_command(...)` (alongside the existing `is_pool_write_command()` / `ArchiveCommand::Write` branch):

```rust
            if let ArchiveCommand::Read(args) = &command {
                return pool_ops::run_archive_read(
                    &report,
                    &pool_ops::ArchiveReadArgs {
                        library: args.library.clone(),
                        locator: args.locator.clone(),
                        out: args.out.clone(),
                        config: args.config.clone(),
                    },
                    &allow,
                    &allow_derived,
                    out,
                    err,
                );
            }
```

Then add the unreachable arms in `run_archive_tape_command`'s two inner matches (the dump handler and the tape handler), next to each `ArchiveCommand::Write(_) => unreachable!(...)`:

```rust
        ArchiveCommand::Read(_) => {
            unreachable!("archive read dispatched before the archive handler")
        }
```

Finally, remove `#[allow(dead_code)]` from `pub fn run_archive_read` in `pool_ops.rs` (it is now reached).

- [ ] **Step 7: Run the test + gates**

Run: `cargo test -p remanence-cli debug_cli_parses_archive_read`
Expected: PASS.
Run: `cargo build -p remanence-cli` — expected: compiles (helper `todo!()`s remain, but nothing executes them).

- [ ] **Step 8: Commit**

```bash
cargo fmt --all
cargo clippy -p remanence-cli --all-targets -- -D warnings
git add crates/remanence-cli/src/lib.rs crates/remanence-cli/src/pool_ops.rs
git commit -m "Wire rem-debug archive read command (A.9)"
```

---

### Task 2: `hex_to_bytes`

**Files:**
- Modify: `crates/remanence-cli/src/pool_ops.rs` (replace the `hex_to_bytes` `todo!()`)

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `pool_ops.rs`:

```rust
#[test]
fn hex_to_bytes_decodes_and_rejects() {
    assert_eq!(super::hex_to_bytes("00ffab").unwrap(), vec![0x00, 0xff, 0xab]);
    assert_eq!(super::hex_to_bytes("").unwrap(), Vec::<u8>::new());
    assert!(super::hex_to_bytes("abc").is_err()); // odd length
    assert!(super::hex_to_bytes("zz").is_err()); // non-hex
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p remanence-cli hex_to_bytes_decodes_and_rejects`
Expected: FAIL — `hex_to_bytes` is `todo!()` (panics).

- [ ] **Step 3: Implement**

Replace the body of `hex_to_bytes` in `pool_ops.rs`:

```rust
fn hex_to_bytes(s: &str) -> Result<Vec<u8>, String> {
    if s.len() % 2 != 0 {
        return Err(format!("hex string has odd length {}", s.len()));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|e| format!("invalid hex byte at offset {i}: {e}"))
        })
        .collect()
}
```

Remove the `#[allow(dead_code, unused_variables)]` line above `fn hex_to_bytes`.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p remanence-cli hex_to_bytes_decodes_and_rejects`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
cargo clippy -p remanence-cli --all-targets -- -D warnings
git add crates/remanence-cli/src/pool_ops.rs
git commit -m "Implement hex_to_bytes for archive read"
```

---

### Task 3: `decode_locator`

**Files:**
- Modify: `crates/remanence-cli/src/pool_ops.rs` (replace the `decode_locator` `todo!()`)

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `pool_ops.rs`:

```rust
#[test]
fn decode_locator_parses_and_validates() {
    let raw = r#"{
        "tape_uuid":"000102030405060708090a0b0c0d0e0f",
        "tape_file_number":1,
        "first_body_lba":1,
        "object_id":"11111111-1111-1111-1111-111111111111",
        "caller_object_id":"c-1",
        "content_sha256":"0000000000000000000000000000000000000000000000000000000000000000",
        "pool_id":"scenario-a",
        "body_format":"rem-tar-v1"
    }"#;
    let loc = super::decode_locator(raw).expect("decode");
    assert_eq!(loc.tape_uuid, [0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15]);
    assert_eq!(loc.tape_file_number, 1);
    assert_eq!(loc.first_body_lba, 1);
    assert_eq!(loc.object_id, "11111111-1111-1111-1111-111111111111");
    assert_eq!(loc.content_sha256, [0u8; 32]);

    // Wrong-length tape_uuid is rejected.
    let bad = raw.replace("000102030405060708090a0b0c0d0e0f", "00ff");
    assert!(super::decode_locator(&bad).is_err());

    // Malformed JSON is rejected.
    assert!(super::decode_locator("{ not json").is_err());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p remanence-cli decode_locator_parses_and_validates`
Expected: FAIL — `decode_locator` is `todo!()`.

- [ ] **Step 3: Implement**

Replace the body of `decode_locator` in `pool_ops.rs`:

```rust
fn decode_locator(raw: &str) -> Result<DecodedLocator, String> {
    let loc: ObjectLocator =
        serde_json::from_str(raw).map_err(|e| format!("parse locator json: {e}"))?;

    let tape_uuid_bytes = hex_to_bytes(&loc.tape_uuid)?;
    let tape_uuid: TapeUuid = tape_uuid_bytes
        .as_slice()
        .try_into()
        .map_err(|_| format!("tape_uuid must be 16 bytes, got {}", tape_uuid_bytes.len()))?;

    let sha_bytes = hex_to_bytes(&loc.content_sha256)?;
    let content_sha256: [u8; 32] = sha_bytes
        .as_slice()
        .try_into()
        .map_err(|_| format!("content_sha256 must be 32 bytes, got {}", sha_bytes.len()))?;

    Ok(DecodedLocator {
        tape_uuid,
        object_id: loc.object_id,
        tape_file_number: loc.tape_file_number,
        first_body_lba: loc.first_body_lba,
        content_sha256,
    })
}
```

Remove the `#[allow(dead_code, unused_variables)]` above `fn decode_locator`, and remove `#[allow(dead_code)]` from `struct ObjectLocator` and `struct DecodedLocator` (now used).

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p remanence-cli decode_locator_parses_and_validates`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
cargo clippy -p remanence-cli --all-targets -- -D warnings
git add crates/remanence-cli/src/pool_ops.rs
git commit -m "Implement decode_locator for archive read"
```

---

### Task 4: `plan_from_records` (pure) + `resolve_object_read_plan`

Split the catalog logic into a pure function (`plan_from_records`) testable with hand-built records, and a thin index-calling wrapper (`resolve_object_read_plan`, already present as a `todo!()`).

**Files:**
- Modify: `crates/remanence-cli/src/pool_ops.rs`
- Modify: `crates/remanence-state/src/lib.rs` (re-export, if needed)

- [ ] **Step 1: Ensure the record types are re-exported**

Run: `grep -n "NativeObjectCopyRecord\|TapeFileRecord" crates/remanence-state/src/lib.rs`
If either is absent from the `pub use` list, add to the existing `pub use index::{...}` re-export block in `crates/remanence-state/src/lib.rs`:

```rust
pub use index::{NativeObjectCopyRecord, TapeFileRecord};
```
(Merge into the existing `pub use index::{...}` line rather than duplicating it; drop any name already present.)

- [ ] **Step 2: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `pool_ops.rs` (add `use remanence_state::{NativeObjectCopyRecord, TapeFileRecord};` to that module's imports):

```rust
fn decoded(object_id: &str, tape_file_number: u32, first_body_lba: u64) -> super::DecodedLocator {
    super::DecodedLocator {
        tape_uuid: [7u8; 16],
        object_id: object_id.to_string(),
        tape_file_number,
        first_body_lba,
        content_sha256: [0u8; 32],
    }
}

fn copy(object_id: &str, tape_file_number: u32, first_body_lba: u64) -> NativeObjectCopyRecord {
    NativeObjectCopyRecord {
        object_id: object_id.to_string(),
        tape_uuid: vec![7u8; 16],
        tape_file_number,
        first_body_lba,
        first_parity_data_ordinal: None,
        protected_until_ordinal: None,
        status: "committed".to_string(),
        pool_id: Some("scenario-a".to_string()),
    }
}

fn obj_tape_file(object_id: &str, tape_file_number: u32, block_count: u64) -> TapeFileRecord {
    TapeFileRecord {
        tape_uuid: vec![7u8; 16],
        tape_file_number,
        kind: "object".to_string(),
        block_count,
        object_id: Some(object_id.to_string()),
    }
}

#[test]
fn plan_from_records_resolves_block_count_and_size() {
    let loc = decoded("obj-1", 3, 16);
    let plan = super::plan_from_records(
        &[copy("obj-1", 3, 16)],
        &[obj_tape_file("obj-1", 3, 7)],
        Some(65536),
        &loc,
    )
    .expect("plan");
    assert_eq!(plan.block_count, 7);
    assert_eq!(plan.block_size_bytes, 65536);
}

#[test]
fn plan_from_records_rejects_missing_copy_file_and_size() {
    let loc = decoded("obj-1", 3, 16);
    // No matching copy.
    assert!(super::plan_from_records(&[], &[obj_tape_file("obj-1", 3, 7)], Some(65536), &loc).is_err());
    // Copy present but no matching object tape file.
    assert!(super::plan_from_records(&[copy("obj-1", 3, 16)], &[], Some(65536), &loc).is_err());
    // Tape has no recorded block size.
    assert!(super::plan_from_records(&[copy("obj-1", 3, 16)], &[obj_tape_file("obj-1", 3, 7)], None, &loc).is_err());
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p remanence-cli plan_from_records`
Expected: FAIL to compile — `plan_from_records` does not exist.

- [ ] **Step 4: Implement `plan_from_records` and wire `resolve_object_read_plan`**

In `pool_ops.rs`, add the pure function and replace `resolve_object_read_plan`'s `todo!()` body. Add `use remanence_state::{NativeObjectCopyRecord, TapeFileRecord};` to the module imports at the top.

```rust
/// Pure: derive the read plan from already-fetched catalog records,
/// validating the copy location against the locator.
fn plan_from_records(
    copies: &[NativeObjectCopyRecord],
    tape_files: &[TapeFileRecord],
    block_size: Option<u64>,
    loc: &DecodedLocator,
) -> Result<ObjectReadPlan, String> {
    copies
        .iter()
        .find(|c| {
            c.tape_uuid.as_slice() == loc.tape_uuid.as_slice()
                && c.tape_file_number == loc.tape_file_number
                && c.first_body_lba == loc.first_body_lba
        })
        .ok_or_else(|| {
            format!(
                "no catalog copy of object {} at tape_file {} lba {}",
                loc.object_id, loc.tape_file_number, loc.first_body_lba
            )
        })?;

    let tape_file = tape_files
        .iter()
        .find(|f| {
            f.tape_file_number == loc.tape_file_number
                && f.object_id.as_deref() == Some(loc.object_id.as_str())
        })
        .ok_or_else(|| {
            format!(
                "no object tape file {} for object {}",
                loc.tape_file_number, loc.object_id
            )
        })?;

    let block_size_bytes = u32::try_from(
        block_size.ok_or_else(|| "tape has no recorded block size".to_string())?,
    )
    .map_err(|_| "tape block size exceeds u32".to_string())?;

    Ok(ObjectReadPlan {
        block_count: tape_file.block_count,
        block_size_bytes,
    })
}

/// Fetch the catalog records and resolve the read plan.
fn resolve_object_read_plan(
    index: &CatalogIndex,
    loc: &DecodedLocator,
) -> Result<ObjectReadPlan, String> {
    let copies = index
        .find_native_object_copies(&loc.object_id)
        .map_err(|e| format!("catalog: {e}"))?;
    let tape_files = index
        .list_tape_files(&loc.tape_uuid)
        .map_err(|e| format!("catalog: {e}"))?;
    let block_size = index
        .get_tape(&loc.tape_uuid)
        .map_err(|e| format!("catalog: {e}"))?
        .and_then(|t| t.block_size);
    plan_from_records(&copies, &tape_files, block_size, loc)
}
```

Remove the `#[allow(dead_code, unused_variables)]` above `resolve_object_read_plan`, and remove `#[allow(dead_code)]` from `struct ObjectReadPlan` (now used).

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p remanence-cli plan_from_records`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
cargo clippy -p remanence-cli --all-targets -- -D warnings
git add crates/remanence-cli/src/pool_ops.rs crates/remanence-state/src/lib.rs
git commit -m "Implement catalog read-plan resolution for archive read"
```

---

### Task 5: `CapturePayloadSink`

**Files:**
- Modify: `crates/remanence-cli/src/pool_ops.rs` (replace the sink `todo!()`s + `finish`)

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `pool_ops.rs`:

```rust
fn stream_entry(path: &str) -> remanence_format::RemTarStreamEntry {
    remanence_format::RemTarStreamEntry {
        path: path.to_string(),
        size_bytes: 0,
        first_chunk_lba: None,
        chunk_count: 0,
        data_offset: 0,
        pax_records: std::collections::BTreeMap::new(),
    }
}

#[test]
fn capture_payload_sink_extracts_single_entry_and_hashes() {
    use remanence_format::RemTarEntrySink;
    use sha2::{Digest, Sha256};

    let mut buf: Vec<u8> = Vec::new();
    let mut sink = super::CapturePayloadSink::new(&mut buf);

    // Manifest entry is skipped.
    let manifest = stream_entry(remanence_format::model::MANIFEST_PATH);
    sink.begin_file(&manifest).unwrap();
    sink.write_file_data(b"CBORCBOR").unwrap();
    sink.end_file(&manifest).unwrap();

    // The one real payload entry is captured.
    let file = stream_entry("hello.txt");
    sink.begin_file(&file).unwrap();
    sink.write_file_data(b"hel").unwrap();
    sink.write_file_data(b"lo").unwrap();
    sink.end_file(&file).unwrap();

    let (bytes_written, digest) = sink.finish().expect("finish");
    assert_eq!(bytes_written, 5);
    assert_eq!(buf, b"hello");
    let expected: [u8; 32] = Sha256::digest(b"hello").into();
    assert_eq!(digest, expected);
}

#[test]
fn capture_payload_sink_rejects_zero_and_multiple_entries() {
    use remanence_format::RemTarEntrySink;

    // Zero payload entries (manifest only).
    let mut buf0: Vec<u8> = Vec::new();
    let mut sink0 = super::CapturePayloadSink::new(&mut buf0);
    let manifest = stream_entry(remanence_format::model::MANIFEST_PATH);
    sink0.begin_file(&manifest).unwrap();
    sink0.end_file(&manifest).unwrap();
    assert!(sink0.finish().is_err());

    // Two payload entries.
    let mut buf2: Vec<u8> = Vec::new();
    let mut sink2 = super::CapturePayloadSink::new(&mut buf2);
    for name in ["a.txt", "b.txt"] {
        let e = stream_entry(name);
        sink2.begin_file(&e).unwrap();
        sink2.write_file_data(b"x").unwrap();
        sink2.end_file(&e).unwrap();
    }
    assert!(sink2.finish().is_err());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p remanence-cli capture_payload_sink`
Expected: FAIL — sink methods are `todo!()`.

- [ ] **Step 3: Implement**

In `pool_ops.rs`, add `use remanence_format::model::MANIFEST_PATH;` to the top imports. Replace the `CapturePayloadSink::finish` and `RemTarEntrySink` method bodies:

```rust
    /// Finalize: require exactly one payload entry, return (bytes, digest).
    fn finish(mut self) -> Result<(u64, [u8; 32]), String> {
        if self.payload_entries == 0 {
            return Err("object contains no payload entry".to_string());
        }
        if self.payload_entries > 1 {
            return Err(format!(
                "object contains {} payload entries; single-file restore only (no --path in v1)",
                self.payload_entries
            ));
        }
        self.out
            .flush()
            .map_err(|e| format!("flush --out: {e}"))?;
        let digest: [u8; 32] = self.hasher.finalize().into();
        Ok((self.bytes_written, digest))
    }
```

```rust
impl<W: Write> RemTarEntrySink for CapturePayloadSink<W> {
    fn begin_file(&mut self, entry: &RemTarStreamEntry) -> Result<(), FormatError> {
        if entry.path == MANIFEST_PATH {
            self.capturing = false;
            return Ok(());
        }
        self.payload_entries += 1;
        self.capturing = true;
        Ok(())
    }

    fn write_file_data(&mut self, bytes: &[u8]) -> Result<(), FormatError> {
        if !self.capturing {
            return Ok(()); // manifest payload — ignored
        }
        self.hasher.update(bytes);
        self.bytes_written += bytes.len() as u64;
        self.out
            .write_all(bytes)
            .map_err(|e| FormatError::parse(format!("write payload to --out: {e}")))?;
        Ok(())
    }

    fn end_file(&mut self, _entry: &RemTarStreamEntry) -> Result<(), FormatError> {
        self.capturing = false;
        Ok(())
    }
}
```

Remove `#[allow(dead_code)]` from `struct CapturePayloadSink` and from its `impl` block (now used).

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p remanence-cli capture_payload_sink`
Expected: PASS (both tests).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
cargo clippy -p remanence-cli --all-targets -- -D warnings
git add crates/remanence-cli/src/pool_ops.rs
git commit -m "Implement CapturePayloadSink for archive read"
```

---

### Task 6: Full verification + integration

All helpers are now real, so `run_archive_read` is fully functional. Verify the whole workspace, then the hardware round-trip.

**Files:**
- (verification only; no source changes expected)

- [ ] **Step 1: Confirm no `todo!()` remains in the archive-read code**

Run: `grep -n "todo!()" crates/remanence-cli/src/pool_ops.rs`
Expected: no matches.

- [ ] **Step 2: Full-workspace gates**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
Expected: all pass (no `#[allow(dead_code)]` should remain on the archive-read items; if clippy flags an unfulfilled allow, remove it).

- [ ] **Step 3: Commit (if Step 2 required any cleanup)**

```bash
git add -A
git commit -m "archive read: workspace verification cleanup"
```
(Skip if the working tree is clean.)

- [ ] **Step 4: Hardware round-trip (manual, requires the akash QuadStor fixture)**

Build + cap the binary, then exercise A.5 → A.9 by hand:
```bash
cargo build --release -p remanence-cli
sudo setcap cap_sys_rawio+ep target/release/rem-debug
# Write a known file (A.5), capture the locator line:
LOC=$(target/release/rem-debug archive write --library 7CBAD9CF74 --allow 7CBAD9CF74 \
  --file /etc/hostname --pool scenario-a --config /var/lib/replica/rem/config.toml --json)
# Read it back (A.9):
target/release/rem-debug archive read --library 7CBAD9CF74 --allow 7CBAD9CF74 \
  --locator "$LOC" --out /tmp/a9-restored.bin --config /var/lib/replica/rem/config.toml
# Verify bit-equality:
cmp /etc/hostname /tmp/a9-restored.bin && echo "A.9 byte-equal OK"
```
Expected: the read prints a `{"...","verified":true}` receipt and `cmp` reports the files identical.

- [ ] **Step 5: Record the result in the journal**

Append a dated entry noting the A.9 hardware round-trip result (or that it is pending hardware access).

---

### System-side follow-up (OUT of this plan's scope — `~/system`, not remanence)

To run scenario A.9 through the harness, the seam `rem.tape.read_object` must flip from `Stub` to a real CLI-subprocess impl. This lives in `~/system` (the harness), not remanence, so it is **not** part of Codex's implementation of this plan. Sketch for whoever wires it:
- `~/system/bindings.toml`: set `"rem.tape.read_object" = "Real(cli-subprocess)"`.
- `~/system/harness/seams/rem.py`: add `_real_read_object(locator)` that shells `rem-debug archive read --locator <json> --out <tmp> ...` (decoding the canonical locator via `locator.decode_ids` per the seam's existing note) and returns `Path(tmp).read_bytes()`.

---

## Self-Review

**Spec coverage** (against `docs/tape-read-object-design-v0.1.md`):
- Command surface `rem-debug archive read` + args + gating → Task 1. ✓
- Receipt JSON + `--out` → `run_archive_read` (skeleton, present) exercised via Task 6. ✓
- Decode locator (hex→bytes) → Tasks 2, 3. ✓
- Catalog read-length resolution → Task 4. ✓
- Payload extraction (skip `MANIFEST_PATH`, single entry) + SHA-verify → Task 5. ✓
- Mount / identity / fixed-block / locate / stream → `run_archive_read` body (present), verified Task 6. ✓
- Parity: single read path, no special-casing → no parity code added (by design). ✓
- Error handling (parse, catalog miss, mismatch, zero/multi entry, sha mismatch) → Tasks 3/4/5 + body. ✓
- Testing (pure unit + manual hardware) → Tasks 2–5 (unit) + Task 6 Step 4 (hardware). ✓
- Out of scope (recovery, multi-file, daemon) → not implemented, by design. ✓

**Placeholder scan:** no TBD/TODO; every code step shows full code; commands have expected output. The one conditional ("if the record types aren't re-exported, add the `pub use`") is a concrete grep-then-act step, not a placeholder.

**Type consistency:** `DecodedLocator` fields (`tape_uuid: [u8;16]`, `object_id: String`, `tape_file_number: u32`, `first_body_lba: u64`, `content_sha256: [u8;32]`) are used identically in Tasks 3, 4, 5. `ObjectReadPlan { block_count: u64, block_size_bytes: u32 }` matches its use in `run_archive_read` (`block_size_bytes as usize` for `stream_rem_tar_object`). `plan_from_records` signature is identical between its definition (Task 4 Step 4) and its tests (Task 4 Step 2). `CapturePayloadSink::new`/`finish` and the `RemTarEntrySink` methods match the skeleton's signatures. `pool_ops::ArchiveReadArgs` fields (`library`, `locator`, `out`, `config`) match the dispatch mapping in Task 1 Step 6.

**Verified against code:** `TapeUuid = [u8;16]`; `NativeObjectCopyRecord`/`TapeFileRecord`/`TapeRecord.block_size: Option<u64>` field names confirmed; `RemTarStreamEntry`/`RemTarEntrySink`/`MANIFEST_PATH` confirmed in `remanence-format`; the `archive write` wiring sites (`RemArchiveCommand`, `ArchiveCommand`, `tape_target`, `is_dump_command`, `source`, `format`, dispatch, `run_archive_tape_command`) confirmed in `lib.rs`; skeleton already `cargo check` + clippy clean.
