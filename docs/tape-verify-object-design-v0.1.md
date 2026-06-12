# Tape Verify-Object (B.7 `rem.tape.verify_object`) Design v0.1

Status: design decision. Adds a streaming tape-object verifier
(`rem-debug archive verify`) and refactors A.9's read path to share its
streamer. Closes scenario-B step B.7. Builds directly on
`docs/tape-read-object-design-v0.1.md` (A.9 `archive read`) and reuses
`remanence-format` streaming + `remanence-library` block I/O.

## Contract

Scenario-B's seam (`~/system/harness/seams/rem.py`):
`verify_object(locator, expected_sha256) -> {verified, expected_sha256, actual_sha256}`.
Its contract is **stricter than `read_object`**: *stream bytes from tape into a
hash accumulator without materialising the restored object on disk or in memory*.
`expected_sha256` is the catalog's recorded **asset** hash (B.2 = the original
file's SHA-256), so verification hashes the **de-tarred payload** — the same
single-non-manifest-entry extraction A.9 already performs — and compares it to the
caller's authoritative hash. The point is integrity: *do the bytes at this tape
locator still hash to what the catalog says?*

## The streaming guarantee — satisfied structurally

A.9's `CapturePayloadSink<W: Write>` already streams: `stream_rem_tar_object`
reads one fixed `block_size` block at a time and the sink hashes each chunk as it
arrives — it never buffers the whole object. The *only* reason `archive read`
fails the verify contract is that it writes the payload to `--out` (disk). Point
the sink's writer at **`std::io::sink()`** (discards every byte) and the same code
becomes a compliant verifier: nothing reaches disk, and memory stays at
O(`block_size`) + the `Sha256` state, independent of object size.

## Architecture: extract a shared streamer (DRY)

A.9's `run_archive_read` body is refactored into one helper, generic over the sink
writer, with `read` and `verify` as thin wrappers. (`read`'s observable behaviour
is unchanged.)

```rust
struct TapeObjectRef<'a> { library: &'a str, config: &'a Path, locator_json: &'a str }
struct TapeStreamOutcome { object_id: String, locator_content_sha256: [u8;32], payload_bytes: u64, actual_sha256: [u8;32] }

fn stream_tape_object<W: Write>(
    report: &DiscoveryReport,
    target: &TapeObjectRef<'_>,
    allow: &[String], allow_derived: &[String],
    sink_writer: W,
    err: &mut dyn Write,
) -> Result<TapeStreamOutcome, ExitCode>;
```

`stream_tape_object` does the existing decode → `resolve_object_read_plan` →
open library → `load_tape_by_uuid` → rewind + `verify_tape_identity` → fixed-block
config → `CapturePayloadSink::new(sink_writer)` → `space(tape_file_number,
Filemarks)` → `stream_rem_tar_object` → `finish`, returning the outcome.

- **`run_archive_read`**: `sink_writer = File::create(--out)`; `verified =
  outcome.actual_sha256 == outcome.locator_content_sha256`; existing read receipt.
- **`run_archive_verify`** *(new)*: `sink_writer = std::io::sink()`; `verified =
  outcome.actual_sha256 == expected_sha256`; verify receipt below.

A `TapeObjectRef` groups the three identity fields so the helper stays within a
sane argument count (see §10).

## CLI surface

`rem-debug archive verify` — break-glass, `rem-debug`-gated like `read`/`write`
(it physically mounts and reads the cartridge: `tape_target() => Some(library)`).
Args mirror `archive read` minus `--out`, plus `--expected-sha256`:
`--library <serial>`, `--locator <json>`, `--expected-sha256 <hex>`,
`--config <path>`. `--expected-sha256` is decoded with the existing `hex_to_bytes`
to `[u8;32]`. Wiring mirrors `read` site-for-site: `RemArchiveVerifyArgs` +
`ArchiveVerifyArgs` + `From` impls + `ArchiveCommand::Verify` + the
`tape_target`/`is_dump_command`/`source`/`format` arms + the dispatch branch +
the two `run_archive_tape_command` `unreachable!` arms.

## Receipt + exit

Always print one JSON line to stdout (so the seam can read both hashes even on
mismatch):
```json
{"verified":true,"expected_sha256":"ab…","actual_sha256":"ab…"}
```
Exit `0` if verified, `1` if not. The seam parses the receipt, checks `verified`,
and raises its own `AssertionError` quoting `expected_sha256`/`actual_sha256`.

## Error handling

Mirrors `read` (locator parse, catalog miss, locator↔catalog mismatch, mount,
identity, space/stream, zero/multi payload entries) — all surfaced by the shared
`stream_tape_object` as a non-zero `ExitCode`. Plus an `--expected-sha256`
hex/length decode error before any I/O. A hash mismatch is **not** an error
(`verified:false` + exit 1); only operational failures are errors.

## Testing

- **Pure/unit (no hardware):** `build_verify_receipt(expected, actual)` — match →
  `verified:true`; mismatch → `verified:false` with both hashes; plus the
  `--expected-sha256` decode-failure path. `hex_to_bytes` and
  `CapturePayloadSink` (incl. a `Vec` writer) are already unit-tested from A.9;
  `io::sink()` is the trivial discard case.
- **CLI parse:** `archive verify` parses into `ArchiveCommand::Verify`.
- **Hardware/manual:** scenario-B round-trip — `archive write` then `archive
  verify` against the just-written locator on the QuadStor fixture; receipt shows
  `verified:true` with `actual == expected`, and a deliberately-wrong
  `--expected-sha256` yields `verified:false` + exit 1.

## Scope

**In:** the shared `stream_tape_object<W>` refactor + `archive verify` (single
logical file, healthy no-parity/parity tape, payload hash vs `--expected-sha256`).

**Out (fast-follow / elsewhere):** sidecar parity recovery; multi-file selection
(`--path`); daemon `ReadSessionService` verify; the `~/system` seam flip
(`_real_verify_object`, bindings) — system-side; and the manual hardware run.

**Sequencing:** B.7 refactors A.9's code, so it lands **after** A.9 (`archive
read`) is committed (A.9 is currently implemented but pending Gemini review).

## §10 — Rust design verification

Verified against `cargo check -p remanence-cli` and
`cargo clippy -p remanence-cli --all-targets -- -D warnings` (both clean) on
2026-06-02, using an additive compiling skeleton in
`crates/remanence-cli/src/pool_ops.rs` (`TapeObjectRef`, `TapeStreamOutcome`,
`stream_tape_object<W>` with its borrow-sensitive body written for real,
`ArchiveVerifyArgs`, `ArchiveVerifyReceipt`, `build_verify_receipt`,
`run_archive_verify` calling the streamer with `std::io::sink()`). The skeleton
was **removed after verification** (design-only) because A.9 is still in-flight in
the working tree; the implementation plan recreates it.

Five-category result:
1. **Module privacy** — all new items local to `pool_ops.rs`, calling `pub` APIs
   (`load_tape_by_uuid`, `stream_rem_tar_object`, `CapturePayloadSink`); no
   cross-module private access. Pass.
2. **`!Send` in threading/async** — synchronous CLI; no thread/`tokio::spawn`. Pass.
3. **Reactor-registration timing** — no tokio-aware types. Pass.
4. **Borrowed-handle plumbing** — verified by the compiled generic body:
   `resolve_object_read_plan` is read into an owned `TapeStreamOutcome`/plan before
   the drive is acquired; `DriveHandleSource(&mut drive)` is taken sequentially
   (identity, then space+stream); the sink owns the moved `W`; the owned outcome is
   returned after drive+sink drop. The generic `W: Write` type-checks with both
   `File` and `std::io::Sink`. Pass.
5. **Trait/method visibility traps** — `stream_tape_object`/`run_archive_verify`
   live with the state they touch; `io::sink()` is std; all callees `pub`. Pass.

Additional finding (clippy, resolved): `stream_tape_object` initially took 8
arguments (`clippy::too_many_arguments`). Resolved — not silenced — by grouping
`library`/`config`/`locator_json` into `TapeObjectRef`, bringing it to 6.
